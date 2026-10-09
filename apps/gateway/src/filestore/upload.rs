//! Files API upload pipeline shared by `POST /v1/files` (inference keys) and
//! the dashboard upload (browser sessions): a streamed multipart form
//! (`purpose`, optional `expires_after[anchor]`/`expires_after[seconds]`,
//! then `file` as the last part) written straight into the encrypted store.
//!
//! Nothing is buffered beyond one network chunk plus the first 4 KiB of the
//! file (basic type sniffing); contents never reach logs, audit or metrics.
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;
use chrono::TimeDelta;
use futures::{Stream, StreamExt};
use uuid::Uuid;

use super::{
    ByteStream, FileStoreError, Purpose,
    files::{FileError, FileStorage, NewFile, QuotaMode, StoredFile, normalize_content_type},
    multipart::{FormError, FormReader, boundary},
};

const MIB: u64 = 1024 * 1024;
/// Form framing allowed on top of the file bytes.
pub const FORM_OVERHEAD: u64 = 64 * 1024;
/// One upload may take at most this long (a dropped upload releases its
/// reservation and partial object at once).
pub const UPLOAD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60 * 60);
/// OpenAI `expires_after.seconds` bounds (1 hour to 30 days).
pub const EXPIRES_AFTER_SECONDS: std::ops::RangeInclusive<i64> = 3600..=2_592_000;
const SNIFF_BYTES: usize = 4096;

/// Files API server configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilesApiLimits {
    /// `GATEWAY_FILES_MAX_BYTES` (default 200 MiB, 1 KiB–8 GiB).
    pub max_bytes: u64,
}
impl Default for FilesApiLimits {
    fn default() -> Self {
        Self {
            max_bytes: 200 * MIB,
        }
    }
}
impl FilesApiLimits {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let mut limits = Self::default();
        if let Some(v) = get("GATEWAY_FILES_MAX_BYTES") {
            let n: u64 = v
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("GATEWAY_FILES_MAX_BYTES must be an integer"))?;
            anyhow::ensure!(
                (1024..=8 * 1024 * MIB).contains(&n),
                "GATEWAY_FILES_MAX_BYTES out of range (1 KiB to 8 GiB)"
            );
            limits.max_bytes = n;
        }
        Ok(limits)
    }
    /// HTTP body cap of an upload route.
    pub fn body_bytes(&self) -> usize {
        usize::try_from(self.max_bytes + FORM_OVERHEAD).unwrap_or(usize::MAX)
    }
}
static LIMITS: OnceLock<FilesApiLimits> = OnceLock::new();
/// Set the process configuration once at startup (before serving).
pub fn configure(limits: FilesApiLimits) -> anyhow::Result<()> {
    LIMITS
        .set(limits)
        .map_err(|_| anyhow::anyhow!("files API limits already configured"))
}
pub fn limits() -> FilesApiLimits {
    LIMITS.get().copied().unwrap_or_default()
}

/// Who uploads. The workspace comes from the validated credential, never the form.
#[derive(Clone, Copy, Debug)]
pub struct Uploader {
    pub workspace_id: Uuid,
    pub api_key_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
}

/// Upload refusals (fixed messages; never client content).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadError {
    /// No file store is configured (`GATEWAY_FILE_STORE=off`).
    StoreOff,
    /// The purpose's group is turned off in Admin › Settings › Storage.
    PurposeOff,
    Invalid {
        message: &'static str,
        param: Option<&'static str>,
    },
    /// A recognized OpenAI purpose the gateway does not hold (`fine-tune`).
    UnsupportedPurpose,
    TooLarge,
    QuotaExceeded,
    Unavailable,
}

fn invalid(message: &'static str, param: Option<&'static str>) -> UploadError {
    UploadError::Invalid { message, param }
}

/// Maps an OpenAI upload purpose to the store purpose.
pub fn store_purpose(api: &str) -> Result<Purpose, UploadError> {
    match api {
        "fine-tune" => Err(UploadError::UnsupportedPurpose),
        p => Purpose::from_openai(p).ok_or(invalid(
            "purpose must be one of batch, user_data, vision, assistants or evals",
            Some("purpose"),
        )),
    }
}

/// Basic content validation on the first bytes: batch input must look like
/// JSONL text, vision files must be PNG, JPEG, GIF or WebP. Returns the
/// sniffed media type (vision only).
fn sniff(api_purpose: &str, head: &[u8]) -> Result<Option<&'static str>, UploadError> {
    match api_purpose {
        "batch" => {
            let text = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
            let first = text.iter().find(|b| !b.is_ascii_whitespace());
            if head.contains(&0) || first != Some(&b'{') {
                return Err(invalid(
                    "Batch input must be a JSONL file of request objects",
                    Some("file"),
                ));
            }
            Ok(None)
        }
        "vision" => {
            let kind = if head.starts_with(b"\x89PNG\r\n\x1a\n") {
                "image/png"
            } else if head.starts_with(b"\xff\xd8\xff") {
                "image/jpeg"
            } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
                "image/gif"
            } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
                "image/webp"
            } else {
                return Err(invalid(
                    "Vision files must be PNG, JPEG, GIF or WebP images",
                    Some("file"),
                ));
            };
            Ok(Some(kind))
        }
        _ => Ok(None),
    }
}

type Failure = Arc<Mutex<Option<UploadError>>>;

fn fail(slot: &Failure, error: UploadError) -> FileStoreError {
    if let Ok(mut s) = slot.lock() {
        s.get_or_insert(error);
    }
    FileStoreError::Source
}

/// The file part as a store body: form errors, empty files and (when the
/// purpose is already known) failed sniffs end the stream with an error, so
/// the store discards everything. The first bytes are kept in `head` for a
/// later sniff when the purpose arrives after the file.
fn checked_body(
    part: impl Stream<Item = Result<Bytes, FormError>> + Send + 'static,
    api_purpose: Option<&'static str>,
    failure: Failure,
    head_out: Arc<Mutex<Vec<u8>>>,
) -> ByteStream {
    async_stream::stream! {
        let mut part = Box::pin(part);
        let mut head: Vec<u8> = Vec::new();
        let mut held: Vec<Bytes> = Vec::new();
        let mut checked = false;
        let mut total = 0u64;
        loop {
            let item = part.next().await;
            let ended = item.is_none();
            match item {
                Some(Ok(chunk)) => {
                    total += chunk.len() as u64;
                    if checked {
                        yield Ok(chunk);
                        continue;
                    }
                    let take = (SNIFF_BYTES - head.len()).min(chunk.len());
                    head.extend_from_slice(&chunk[..take]);
                    held.push(chunk);
                }
                Some(Err(FormError::TooLarge)) => {
                    yield Err(fail(&failure, UploadError::TooLarge));
                    return;
                }
                Some(Err(_)) => {
                    yield Err(fail(&failure, malformed()));
                    return;
                }
                None => {}
            }
            if !checked && (head.len() >= SNIFF_BYTES || ended) {
                if total == 0 {
                    yield Err(fail(&failure, invalid("The file is empty", Some("file"))));
                    return;
                }
                if let Some(Err(e)) = api_purpose.map(|p| sniff(p, &head)) {
                    yield Err(fail(&failure, e));
                    return;
                }
                if let Ok(mut out) = head_out.lock() {
                    *out = std::mem::take(&mut head);
                }
                checked = true;
                for chunk in held.drain(..) {
                    yield Ok(chunk);
                }
            }
            if ended {
                return;
            }
        }
    }
    .boxed()
}

fn malformed() -> UploadError {
    invalid("Malformed multipart body", None)
}

/// Form fields other than the file.
#[derive(Default)]
struct Fields {
    purpose: Option<String>,
    anchor: Option<String>,
    seconds: Option<String>,
}

impl Fields {
    fn slot(&mut self, name: &str) -> Result<&mut Option<String>, UploadError> {
        let slot = match name {
            "purpose" => &mut self.purpose,
            "expires_after[anchor]" => &mut self.anchor,
            "expires_after[seconds]" => &mut self.seconds,
            _ => return Err(invalid("Unknown form field", None)),
        };
        if slot.is_some() {
            return Err(invalid("Duplicate form field", None));
        }
        Ok(slot)
    }

    /// `(OpenAI purpose, store purpose, expires_after)`.
    fn validate(&self) -> Result<(&'static str, Purpose, Option<TimeDelta>), UploadError> {
        let requested = self
            .purpose
            .as_deref()
            .ok_or(invalid("purpose is required", Some("purpose")))?;
        let store_purpose = store_purpose(requested)?;
        let api_purpose = super::files::API_PURPOSES
            .into_iter()
            .find(|p| *p == requested)
            .ok_or(invalid("Unsupported purpose", Some("purpose")))?;
        let expires_after = match (self.anchor.as_deref(), self.seconds.as_deref()) {
            (None, None) => None,
            (Some("created_at"), Some(s)) => Some(TimeDelta::seconds(
                s.trim()
                    .parse()
                    .ok()
                    .filter(|s| EXPIRES_AFTER_SECONDS.contains(s))
                    .ok_or(invalid(
                        "expires_after.seconds must be between 3600 and 2592000",
                        Some("expires_after"),
                    ))?,
            )),
            _ => {
                return Err(invalid(
                    "expires_after needs anchor \"created_at\" and seconds",
                    Some("expires_after"),
                ));
            }
        };
        Ok((api_purpose, store_purpose, expires_after))
    }
}

async fn read_field<S, E>(
    form: &mut FormReader<S>,
    fields: &mut Fields,
    name: &str,
) -> Result<(), UploadError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin + Send + 'static,
{
    let slot = fields.slot(name)?;
    *slot = Some(form.text(64).await.map_err(form_error)?);
    Ok(())
}

fn form_error(e: FormError) -> UploadError {
    match e {
        FormError::TooLarge => UploadError::TooLarge,
        _ => malformed(),
    }
}

async fn accepts(files: &FileStorage, purpose: Purpose) -> Result<(), UploadError> {
    match files.accepts(purpose).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(UploadError::PurposeOff),
        Err(_) => Err(UploadError::Unavailable),
    }
}

/// Reads the form and stores the file. Every refusal before or during the
/// upload leaves nothing behind (partial objects are deleted, the pending row
/// is marked deleted and its quota reservation released).
///
/// Field order: OpenAI's Python SDK sends `purpose` first; its Node SDK sends
/// `file` first. With `purpose` first, the purpose is validated and its
/// content sniffed while streaming. With `file` first, the file is staged
/// under a provisional purpose (batch input for `.jsonl` names, otherwise a
/// user file), the trailing fields are read, and the file is then committed,
/// moved to the right purpose (a streamed re-encryption, never buffered) or
/// discarded.
pub async fn receive<S, E>(
    files: &FileStorage,
    who: Uploader,
    content_type: Option<&str>,
    body: S,
    max_bytes: u64,
) -> Result<StoredFile, UploadError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin + Send + 'static,
{
    let boundary = content_type
        .and_then(boundary)
        .ok_or(invalid("Expected a multipart/form-data body", None))?;
    let mut form = FormReader::new(body, &boundary, max_bytes.saturating_add(FORM_OVERHEAD));
    let mut fields = Fields::default();
    let header = loop {
        let Some(part) = form.next_part().await.map_err(form_error)? else {
            return Err(invalid("file is required", Some("file")));
        };
        match (part.name.as_str(), part.filename.is_some()) {
            ("file", true) => break part,
            ("file", false) => return Err(invalid("file must be a file part", Some("file"))),
            (_, true) => return Err(invalid("Only one file part is allowed", None)),
            (name, false) => read_field(&mut form, &mut fields, name).await?,
        }
    };
    if files.runtime().store().is_none() {
        return Err(UploadError::StoreOff);
    }
    let known = fields
        .purpose
        .is_some()
        .then(|| fields.validate())
        .transpose()?;
    if let Some((_, purpose, _)) = known {
        accepts(files, purpose).await?;
    }
    let filename = header
        .filename
        .as_deref()
        .and_then(super::files::sanitize_filename)
        .unwrap_or_else(|| "file".to_owned());
    // Provisional store purpose while the real one is unknown.
    let staged_purpose = match known {
        Some((_, p, _)) => p,
        None if filename.to_ascii_lowercase().ends_with(".jsonl") => Purpose::BatchInput,
        None => Purpose::UserFile,
    };
    let client_type = header
        .content_type
        .as_deref()
        .and_then(normalize_content_type)
        .filter(|t| t != "application/octet-stream")
        .filter(|t| known.is_none_or(|k| k.0 != "vision") || t.starts_with("image/"));
    let failure: Failure = Arc::default();
    let head: Arc<Mutex<Vec<u8>>> = Arc::default();
    let rest = Arc::new(Mutex::new(None));
    let part: futures::stream::BoxStream<'static, Result<Bytes, FormError>> = if known.is_some() {
        form.into_last_part().boxed()
    } else {
        form.into_part(rest.clone()).boxed()
    };
    let body = checked_body(part, known.map(|k| k.0), failure.clone(), head.clone());
    let new = NewFile {
        created_by_api_key_id: who.api_key_id,
        created_by_user_id: who.user_id,
        filename: Some(filename),
        content_type: client_type,
        max_bytes: Some(max_bytes),
        api_purpose: Some(known.map_or(String::new(), |k| k.0.to_owned())),
        expires_after: known.and_then(|k| k.2),
        quota: QuotaMode::Enforce,
        ..NewFile::new(staged_purpose, Some(who.workspace_id))
    };
    let deadline = tokio::time::Instant::now() + UPLOAD_DEADLINE;
    let staged = tokio::time::timeout_at(deadline, files.stage(new, body)).await;
    let reported = failure.lock().ok().and_then(|s| *s);
    let staged = match staged {
        Err(_) => return Err(invalid("Upload took too long", Some("file"))),
        Ok(Ok(staged)) => staged,
        Ok(Err(e)) => {
            return Err(match (reported, e) {
                (_, FileError::QuotaExceeded) => UploadError::QuotaExceeded,
                (_, FileError::TooLarge) => UploadError::TooLarge,
                (Some(r), _) => r,
                (None, FileError::Disabled) => UploadError::StoreOff,
                (None, FileError::Invalid) => invalid("Invalid file", None),
                (None, FileError::Store(FileStoreError::Source)) => {
                    invalid("The upload was interrupted", Some("file"))
                }
                (None, _) => UploadError::Unavailable,
            });
        }
    };
    let finished = async {
        if known.is_some() {
            return files
                .commit(staged, None, None)
                .await
                .map_err(|_| UploadError::Unavailable);
        }
        // The file came first: read the fields that follow, then decide.
        let decided = async {
            let mut form = rest
                .lock()
                .ok()
                .and_then(|mut s| s.take())
                .ok_or_else(malformed)?;
            while let Some(part) = form.next_part().await.map_err(form_error)? {
                if part.filename.is_some() {
                    return Err(invalid("Only one file part is allowed", None));
                }
                read_field(&mut form, &mut fields, &part.name).await?;
            }
            let (api, purpose, expires_after) = fields.validate()?;
            accepts(files, purpose).await?;
            let head = head.lock().map(|h| h.clone()).unwrap_or_default();
            sniff(api, &head)?;
            Ok((api, purpose, expires_after))
        }
        .await;
        let (api, purpose, expires_after) = match decided {
            Ok(d) => d,
            Err(e) => {
                files.discard(staged).await;
                return Err(e);
            }
        };
        let staged = if purpose != staged.purpose() {
            files
                .restage(staged, purpose, Some(max_bytes))
                .await
                .map_err(|e| match e {
                    FileError::TooLarge => UploadError::TooLarge,
                    _ => UploadError::Unavailable,
                })?
        } else {
            staged
        };
        files
            .commit(staged, Some(api), expires_after)
            .await
            .map_err(|_| UploadError::Unavailable)
    };
    tokio::time::timeout_at(deadline, finished)
        .await
        .map_err(|_| invalid("Upload took too long", Some("file")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purposes_map_to_store_purposes() {
        assert_eq!(store_purpose("batch"), Ok(Purpose::BatchInput));
        for p in ["user_data", "vision", "assistants", "evals"] {
            assert_eq!(store_purpose(p), Ok(Purpose::UserFile));
        }
        assert_eq!(
            store_purpose("fine-tune"),
            Err(UploadError::UnsupportedPurpose)
        );
        assert!(matches!(
            store_purpose("batch_output"),
            Err(UploadError::Invalid { .. })
        ));
    }

    #[test]
    fn sniffing_is_basic() {
        assert!(sniff("batch", b"\xef\xbb\xbf \n{\"custom_id\":1}").is_ok());
        assert!(sniff("batch", b"[1,2]").is_err());
        assert!(sniff("batch", b"{\"a\":\"\0\"}").is_err());
        assert_eq!(
            sniff("vision", b"\x89PNG\r\n\x1a\nrest"),
            Ok(Some("image/png"))
        );
        assert_eq!(
            sniff("vision", b"RIFF\0\0\0\0WEBPVP8 "),
            Ok(Some("image/webp"))
        );
        assert!(sniff("vision", b"<svg/>").is_err());
        assert_eq!(sniff("user_data", b"\0\x01anything"), Ok(None));
    }

    #[test]
    fn limits_parse_and_bound() {
        let l = FilesApiLimits::from_lookup(|_| None).unwrap();
        assert_eq!(l.max_bytes, 200 * MIB);
        let l = FilesApiLimits::from_lookup(|_| Some("2048".into())).unwrap();
        assert_eq!(l.max_bytes, 2048);
        assert!(FilesApiLimits::from_lookup(|_| Some("10".into())).is_err());
        assert!(FilesApiLimits::from_lookup(|_| Some("x".into())).is_err());
    }
}

//! Installation logo (Admin › Settings › General › Logo; migration 0023).
//!
//! An uploaded PNG, JPEG or WebP image (never SVG), at most 512 KiB, stored
//! encrypted in the file store (purpose `branding`, installation scope, never
//! expires). The magic bytes must match the declared type, the container
//! structure must parse to its end (no trailing data) and the dimensions are
//! read from the image header. Markup anywhere in the file is refused, so a
//! polyglot never becomes a branding object.
//!
//! The current logo is served same-origin and without a session at
//! `GET /api/v1/branding/logo` (the sign-in page needs it before login), with
//! `nosniff`, `default-src 'none'`, an ETag and a short public cache. Replacing
//! or removing the logo deletes the previous object after the settings change
//! commits. Admin writes (audited), Auditor reads.
use super::*;
use crate::filestore::{FileError, FileStoreRuntime, NewFile, Purpose, files::FileStorage};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, header},
    routing::put,
};
use futures::StreamExt;

/// The public, unauthenticated logo endpoint.
pub(crate) const LOGO_PATH: &str = "/api/v1/branding/logo";
pub(crate) const MAX_BYTES: usize = 512 * 1024;
const SIDES: std::ops::RangeInclusive<u32> = 16..=4096;
pub(super) const LOGO_TYPE: &str = "The logo must be a PNG, JPEG or WebP image";
pub(super) const LOGO_TOO_LARGE: &str = "The logo must be 512 KiB or smaller";
pub(super) const LOGO_INVALID: &str = "The logo file is not a valid image";
pub(super) const LOGO_DIMENSIONS: &str = "The logo must be 16 to 4096 pixels on each side";
const STORE_UNAVAILABLE: &str = "File storage unavailable";

/// `{url, updated_at}` of the current logo over `installation_settings s`.
/// The URL carries a version so a replaced logo is never served from cache.
pub(crate) const PUBLIC_LOGO_SQL: &str = "CASE WHEN s.branding_logo_file_id IS NULL THEN NULL ELSE jsonb_build_object('url','/api/v1/branding/logo?v='||left(replace(s.branding_logo_file_id::text,'-',''),12),'updated_at',s.branding_logo_updated_at) END";
/// Settings view: also the stored dimensions.
const SETTINGS_LOGO_SQL: &str = "CASE WHEN s.branding_logo_file_id IS NULL THEN NULL ELSE jsonb_build_object('url','/api/v1/branding/logo?v='||left(replace(s.branding_logo_file_id::text,'-',''),12),'updated_at',s.branding_logo_updated_at,'width',s.branding_logo_width,'height',s.branding_logo_height) END";

pub(super) fn routes() -> Router<Store> {
    Router::new().route(
        "/api/v1/platform/settings/general/logo",
        put(upload).delete(remove),
    )
}
/// Mounted without the session layer.
pub(super) fn public_routes() -> Router<Store> {
    Router::new().route(LOGO_PATH, get(serve))
}

fn runtime(ext: Option<Extension<FileStoreRuntime>>) -> FileStoreRuntime {
    ext.map(|Extension(r)| r).unwrap_or_default()
}

/// The current logo for `/me` and the sign-in config: `{url, updated_at}`,
/// or null when none is set or the file store is off (nothing could be served).
pub(crate) async fn public_logo<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    rt: &FileStoreRuntime,
) -> Result<Value, sqlx::Error> {
    if rt.store().is_none() {
        return Ok(Value::Null);
    }
    sqlx::query_scalar(&format!(
        "SELECT {PUBLIC_LOGO_SQL} FROM installation_settings s WHERE s.singleton"
    ))
    .fetch_one(executor)
    .await
    .map(|v: Option<Value>| v.unwrap_or(Value::Null))
}

/// Sign-in page branding: `(logo, installation_name)`. The display name is
/// only included with a logo (it is the logo's alt text); otherwise null.
pub(crate) async fn sign_in_branding(
    pool: &sqlx::PgPool,
    rt: &FileStoreRuntime,
) -> Result<(Value, Value), sqlx::Error> {
    let logo = public_logo(pool, rt).await?;
    if logo.is_null() {
        return Ok((logo, Value::Null));
    }
    let name: String = sqlx::query_scalar("SELECT name FROM installation WHERE singleton")
        .fetch_one(pool)
        .await?;
    Ok((logo, Value::String(name)))
}

/// The settings view of the logo and what an upload accepts.
pub(super) async fn settings_json(
    tx: &mut Transaction<'_, Postgres>,
    rt: &FileStoreRuntime,
) -> Result<(Value, Value), ApiError> {
    let logo: Option<Value> = sqlx::query_scalar(&format!(
        "SELECT {SETTINGS_LOGO_SQL} FROM installation_settings s WHERE s.singleton"
    ))
    .fetch_one(&mut **tx)
    .await?;
    let upload = json!({
        "available": rt.store().is_some(),
        "max_bytes": MAX_BYTES,
        "content_types": ImageKind::ALL.map(ImageKind::content_type),
        "min_side": SIDES.start(),
        "max_side": SIDES.end(),
        "recommended_side": 64,
    });
    Ok((logo.unwrap_or(Value::Null), upload))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageKind {
    Png,
    Jpeg,
    Webp,
}
impl ImageKind {
    const ALL: [Self; 3] = [Self::Png, Self::Jpeg, Self::Webp];
    pub(crate) fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Webp => "webp",
        }
    }
    fn from_content_type(value: &str) -> Option<Self> {
        let essence = crate::filestore::files::normalize_content_type(value)?;
        Self::ALL.into_iter().find(|k| k.content_type() == essence)
    }
    fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(&PNG_SIGNATURE) {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LogoImage {
    pub(crate) kind: ImageKind,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogoError {
    /// Not declared as, or not actually, PNG/JPEG/WebP (SVG included).
    Type,
    TooLarge,
    /// Malformed structure, trailing data, animation or embedded markup.
    Invalid,
    Dimensions,
}
impl From<LogoError> for ApiError {
    fn from(e: LogoError) -> Self {
        match e {
            LogoError::Type => ApiError(StatusCode::UNSUPPORTED_MEDIA_TYPE, LOGO_TYPE),
            LogoError::TooLarge => ApiError(StatusCode::PAYLOAD_TOO_LARGE, LOGO_TOO_LARGE),
            LogoError::Invalid => ApiError(StatusCode::BAD_REQUEST, LOGO_INVALID),
            LogoError::Dimensions => ApiError(StatusCode::BAD_REQUEST, LOGO_DIMENSIONS),
        }
    }
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
/// Markup that would make the bytes meaningful to a browser as a document.
const MARKUP: [&[u8]; 9] = [
    b"<svg",
    b"<script",
    b"<html",
    b"<!doctype",
    b"<iframe",
    b"<body",
    b"<object",
    b"<embed",
    b"<foreignobject",
];

/// Validates an uploaded logo: the declared type, magic bytes, container
/// structure, dimensions and the absence of markup.
pub(crate) fn inspect(declared: Option<&str>, bytes: &[u8]) -> Result<LogoImage, LogoError> {
    if bytes.len() > MAX_BYTES {
        return Err(LogoError::TooLarge);
    }
    let declared = declared
        .and_then(ImageKind::from_content_type)
        .ok_or(LogoError::Type)?;
    let kind = ImageKind::sniff(bytes).ok_or(LogoError::Type)?;
    if kind != declared {
        return Err(LogoError::Type);
    }
    if MARKUP.iter().any(|m| contains_ignore_case(bytes, m)) {
        return Err(LogoError::Invalid);
    }
    let (width, height) = match kind {
        ImageKind::Png => png_size(bytes),
        ImageKind::Jpeg => jpeg_size(bytes),
        ImageKind::Webp => webp_size(bytes),
    }
    .ok_or(LogoError::Invalid)?;
    if !SIDES.contains(&width) || !SIDES.contains(&height) {
        return Err(LogoError::Dimensions);
    }
    Ok(LogoImage {
        kind,
        width,
        height,
    })
}

fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}
fn be16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_be_bytes(
        b.get(at..at + 2)?.try_into().ok()?,
    )))
}
fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn le16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_le_bytes(
        b.get(at..at + 2)?.try_into().ok()?,
    )))
}
fn le24(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 3)?;
    Some(u32::from(s[0]) | u32::from(s[1]) << 8 | u32::from(s[2]) << 16)
}
fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// IHDR first, at least one IDAT, IEND last with nothing after it; no APNG.
fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    if be32(b, 8)? != 13 || b.get(12..16)? != b"IHDR" {
        return None;
    }
    let size = (be32(b, 16)?, be32(b, 20)?);
    let (mut pos, mut data) = (8usize, false);
    loop {
        let len = usize::try_from(be32(b, pos)?).ok()?;
        let kind = b.get(pos + 4..pos + 8)?;
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            return None;
        }
        let end = pos.checked_add(12)?.checked_add(len)?;
        if end > b.len() || kind == b"acTL" {
            return None;
        }
        data |= kind == b"IDAT";
        if kind == b"IEND" {
            return (end == b.len() && data).then_some(size);
        }
        pos = end;
    }
}

/// SOI, segments up to the first frame header (SOF), and EOI as the last bytes.
fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    if !b.ends_with(&[0xFF, 0xD9]) {
        return None;
    }
    let mut pos = 2usize;
    loop {
        if *b.get(pos)? != 0xFF {
            return None;
        }
        while *b.get(pos + 1)? == 0xFF {
            pos += 1;
        }
        let marker = *b.get(pos + 1)?;
        match marker {
            0x01 | 0xD0..=0xD7 => {
                pos += 2;
                continue;
            }
            // A second SOI, EOI or scan data before any frame header.
            0xD8..=0xDA => return None,
            _ => {}
        }
        let len = usize::try_from(be16(b, pos + 2)?).ok()?;
        if len < 2 || pos + 2 + len > b.len() {
            return None;
        }
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            if len < 8 {
                return None;
            }
            return Some((be16(b, pos + 7)?, be16(b, pos + 5)?));
        }
        pos += 2 + len;
    }
}

/// RIFF size equal to the file, every chunk in bounds, a still VP8/VP8L/VP8X image.
fn webp_size(b: &[u8]) -> Option<(u32, u32)> {
    if usize::try_from(le32(b, 4)?).ok()?.checked_add(8)? != b.len() {
        return None;
    }
    let mut pos = 12usize;
    while pos < b.len() {
        let len = usize::try_from(le32(b, pos + 4)?).ok()?;
        let end = pos.checked_add(8)?.checked_add(len)?.checked_add(len & 1)?;
        if end > b.len()
            || !b
                .get(pos..pos + 4)?
                .iter()
                .all(|c| c.is_ascii_graphic() || *c == b' ')
        {
            return None;
        }
        pos = end;
    }
    let data = 20usize;
    let size = match b.get(12..16)? {
        b"VP8 " => {
            if b.get(data + 3..data + 6)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            (le16(b, data + 6)? & 0x3FFF, le16(b, data + 8)? & 0x3FFF)
        }
        b"VP8L" => {
            if *b.get(data)? != 0x2F {
                return None;
            }
            let bits = le32(b, data + 1)?;
            ((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1)
        }
        b"VP8X" => {
            // Animation flag: logos are still images.
            if b.get(data)? & 0x02 != 0 {
                return None;
            }
            (le24(b, data + 4)? + 1, le24(b, data + 7)? + 1)
        }
        _ => return None,
    };
    Some(size)
}

fn store_unavailable() -> ApiError {
    ApiError(StatusCode::SERVICE_UNAVAILABLE, STORE_UNAVAILABLE)
}

/// `PUT /platform/settings/general/logo`: the raw image as the body with its
/// `Content-Type`. Replaces any current logo and clears the deprecated `logo_url`.
async fn upload(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    ext: Option<Extension<FileStoreRuntime>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    let rt = runtime(ext);
    // Authorize first, then store outside the installation lock.
    settings::write_tx(&s, &u).await?.commit().await?;
    if rt.store().is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            settings::storage::STORAGE_OFF,
        ));
    }
    let declared = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let image = inspect(declared, &body)?;
    let size = body.len();
    let files = FileStorage::new(s.clone(), rt.clone());
    let new = NewFile {
        created_by_user_id: Some(u.user_id),
        filename: Some(format!("logo.{}", image.kind.as_str())),
        content_type: Some(image.kind.content_type().to_owned()),
        max_bytes: Some(MAX_BYTES as u64),
        ..NewFile::new(Purpose::Branding, None)
    };
    let file = files
        .create(new, futures::stream::iter([Ok(body)]).boxed())
        .await
        .map_err(|e| match e {
            FileError::Disabled => ApiError(StatusCode::CONFLICT, settings::storage::STORAGE_OFF),
            _ => store_unavailable(),
        })?;
    let saved = async {
        let mut tx = settings::write_tx(&s, &u).await?;
        let previous: Option<Uuid> = sqlx::query_scalar(
            "SELECT branding_logo_file_id FROM installation_settings WHERE singleton",
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("UPDATE installation_settings SET branding_logo_file_id=$1,branding_logo_updated_at=now(),branding_logo_width=$2,branding_logo_height=$3,logo_url=NULL,updated_at=now(),updated_by=$4 WHERE singleton")
            .bind(file.id)
            .bind(i32::try_from(image.width).map_err(|_| invalid())?)
            .bind(i32::try_from(image.height).map_err(|_| invalid())?)
            .bind(u.user_id)
            .execute(&mut *tx)
            .await?;
        audit(
            &mut tx,
            &u,
            None,
            "settings.logo_uploaded",
            "installation_settings",
            Some(file.id),
            json!({"kind": image.kind.as_str(), "count": size}),
        )
        .await?;
        let v = settings::general_json(&mut tx, &rt).await?;
        tx.commit().await?;
        Ok::<_, ApiError>((v, previous))
    }
    .await;
    match saved {
        Ok((v, previous)) => {
            if let Some(previous) = previous.filter(|p| *p != file.id) {
                // A failed delete leaves the object expired for the sweeper.
                let _ = files.delete(previous, None).await;
            }
            Ok(Json(v))
        }
        Err(e) => {
            let _ = files.delete(file.id, None).await;
            Err(e)
        }
    }
}

/// `DELETE /platform/settings/general/logo`: back to the Portal mark. Works
/// with the store off too (the object is then deleted by the sweeper later).
async fn remove(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    ext: Option<Extension<FileStoreRuntime>>,
) -> ApiResult {
    let rt = runtime(ext);
    let mut tx = settings::write_tx(&s, &u).await?;
    let previous: Option<Uuid> = sqlx::query_scalar(
        "SELECT branding_logo_file_id FROM installation_settings WHERE singleton",
    )
    .fetch_one(&mut *tx)
    .await?;
    if let Some(previous) = previous {
        sqlx::query("UPDATE installation_settings SET branding_logo_file_id=NULL,branding_logo_updated_at=NULL,branding_logo_width=NULL,branding_logo_height=NULL,updated_at=now(),updated_by=$1 WHERE singleton")
            .bind(u.user_id)
            .execute(&mut *tx)
            .await?;
        audit(
            &mut tx,
            &u,
            None,
            "settings.logo_removed",
            "installation_settings",
            Some(previous),
            json!({}),
        )
        .await?;
    }
    let v = settings::general_json(&mut tx, &rt).await?;
    tx.commit().await?;
    if let Some(previous) = previous {
        let _ = FileStorage::new(s.clone(), rt).delete(previous, None).await;
    }
    Ok(Json(v))
}

fn not_found() -> Response {
    missing().into_response()
}

/// `GET /api/v1/branding/logo`: public and unauthenticated. 404 when unset or
/// when the file store is off.
async fn serve(
    State(s): State<Store>,
    ext: Option<Extension<FileStoreRuntime>>,
    headers: HeaderMap,
) -> Response {
    let rt = runtime(ext);
    if rt.store().is_none() {
        return not_found();
    }
    let id: Option<Uuid> = match sqlx::query_scalar(
        "SELECT branding_logo_file_id FROM installation_settings WHERE singleton",
    )
    .fetch_one(&s.pool)
    .await
    {
        Ok(id) => id,
        Err(_) => return store_unavailable().into_response(),
    };
    let Some(id) = id else {
        return not_found();
    };
    let files = FileStorage::new(s.clone(), rt);
    let file = match files.get(id, None).await {
        Ok(Some(f)) if f.purpose == Purpose::Branding => f,
        Ok(_) => return not_found(),
        Err(_) => return store_unavailable().into_response(),
    };
    let Some(kind) = file
        .content_type
        .as_deref()
        .and_then(ImageKind::from_content_type)
    else {
        return not_found();
    };
    let etag = format!("\"{}\"", hex::encode(file.sha256));
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .map(|t| t.trim().trim_start_matches("W/"))
                .any(|t| t == etag || t == "*")
        });
    let body = if fresh {
        None
    } else {
        // Read the whole (small) object first: an integrity failure must never
        // be served as a truncated image.
        let mut stream = match files.open(file.id, None).await {
            Ok((_, stream)) => stream,
            Err(FileError::NotFound) => return not_found(),
            Err(_) => return store_unavailable().into_response(),
        };
        let mut bytes =
            Vec::with_capacity(usize::try_from(file.size_bytes).unwrap_or(0).min(MAX_BYTES));
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) if bytes.len() + chunk.len() <= MAX_BYTES => {
                    bytes.extend_from_slice(&chunk)
                }
                _ => return store_unavailable().into_response(),
            }
        }
        Some(bytes)
    };
    let mut response = match body {
        None => StatusCode::NOT_MODIFIED.into_response(),
        Some(bytes) => {
            let mut r = Body::from(bytes).into_response();
            r.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(kind.content_type()),
            );
            r
        }
    };
    let h = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'"),
    );
    h.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    // Keeps this Cache-Control (the request middleware sets no-store otherwise).
    response.extensions_mut().insert(crate::http::PublicCache);
    response
}

/// Minimal well-formed images for tests (structure only; pixel data is not decoded).
#[cfg(test)]
pub(super) mod fixtures {
    use super::PNG_SIGNATURE;

    pub(crate) fn png(width: u32, height: u32) -> Vec<u8> {
        let mut b = PNG_SIGNATURE.to_vec();
        let mut chunk = |kind: &[u8], data: &[u8]| {
            b.extend_from_slice(&(data.len() as u32).to_be_bytes());
            b.extend_from_slice(kind);
            b.extend_from_slice(data);
            b.extend_from_slice(&[0, 0, 0, 0]);
        };
        let mut ihdr = width.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(b"IHDR", &ihdr);
        chunk(b"IDAT", &[0x78, 0x9C, 0x03, 0x00]);
        chunk(b"IEND", &[]);
        b
    }
    pub(crate) fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        b.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        b.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        b.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0x12, 0x34, 0xFF, 0xD9]);
        b
    }
    pub(crate) fn webp_lossless(width: u32, height: u32) -> Vec<u8> {
        let bits = (width - 1) | ((height - 1) << 14);
        let mut data = vec![0x2F];
        data.extend_from_slice(&bits.to_le_bytes());
        data.push(0);
        let mut b = b"RIFF".to_vec();
        b.extend_from_slice(&((4 + 8 + data.len()) as u32).to_le_bytes());
        b.extend_from_slice(b"WEBPVP8L");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&data);
        b
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{jpeg, png, webp_lossless};
    use super::*;

    #[test]
    fn accepts_png_jpeg_and_webp_with_their_dimensions() {
        assert_eq!(
            inspect(Some("image/png"), &png(64, 48)),
            Ok(LogoImage {
                kind: ImageKind::Png,
                width: 64,
                height: 48
            })
        );
        assert_eq!(
            inspect(Some("image/jpeg"), &jpeg(120, 80)).map(|i| (i.width, i.height)),
            Ok((120, 80))
        );
        assert_eq!(
            inspect(Some("image/webp; charset=binary"), &webp_lossless(100, 200))
                .map(|i| (i.kind, i.width, i.height)),
            Ok((ImageKind::Webp, 100, 200))
        );
    }

    #[test]
    fn rejects_svg_mismatched_types_and_unknown_formats() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"></svg>"#;
        for declared in ["image/svg+xml", "image/png", "text/html"] {
            assert_eq!(
                inspect(Some(declared), svg),
                Err(LogoError::Type),
                "{declared}"
            );
        }
        assert_eq!(inspect(None, &png(64, 64)), Err(LogoError::Type));
        assert_eq!(
            inspect(Some("image/jpeg"), &png(64, 64)),
            Err(LogoError::Type)
        );
        assert_eq!(
            inspect(Some("image/gif"), b"GIF89a....."),
            Err(LogoError::Type)
        );
    }

    #[test]
    fn rejects_polyglots_trailing_data_and_broken_structure() {
        // Valid PNG with an HTML/SVG payload in a text chunk.
        let mut text = png(64, 64);
        let iend = text.len() - 12;
        let payload = b"Comment\0<SVG onload=alert(1)>";
        let mut chunk = (payload.len() as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(b"tEXt");
        chunk.extend_from_slice(payload);
        chunk.extend_from_slice(&[0; 4]);
        text.splice(iend..iend, chunk);
        assert_eq!(inspect(Some("image/png"), &text), Err(LogoError::Invalid));
        // Data after IEND / EOI / outside RIFF.
        let mut tail = png(64, 64);
        tail.extend_from_slice(b"<html>");
        assert_eq!(inspect(Some("image/png"), &tail), Err(LogoError::Invalid));
        let mut tail = png(64, 64);
        tail.extend_from_slice(b"PK\x03\x04zip");
        assert_eq!(inspect(Some("image/png"), &tail), Err(LogoError::Invalid));
        let mut j = jpeg(64, 64);
        j.extend_from_slice(b"<script>x</script>");
        assert_eq!(inspect(Some("image/jpeg"), &j), Err(LogoError::Invalid));
        let mut w = webp_lossless(64, 64);
        w.extend_from_slice(&[0, 0]);
        assert_eq!(inspect(Some("image/webp"), &w), Err(LogoError::Invalid));
        // Truncated and animated images.
        let p = png(64, 64);
        assert_eq!(
            inspect(Some("image/png"), &p[..p.len() - 4]),
            Err(LogoError::Invalid)
        );
        assert_eq!(
            inspect(Some("image/png"), &p[..30]),
            Err(LogoError::Invalid)
        );
        let mut apng = png(64, 64);
        let at = 8 + 25;
        apng.splice(
            at..at,
            [
                0, 0, 0, 8, b'a', b'c', b'T', b'L', 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0,
            ],
        );
        assert_eq!(inspect(Some("image/png"), &apng), Err(LogoError::Invalid));
    }

    #[test]
    fn enforces_size_and_dimensions() {
        let mut big = png(64, 64);
        big.resize(MAX_BYTES + 1, 0);
        assert_eq!(inspect(Some("image/png"), &big), Err(LogoError::TooLarge));
        assert_eq!(
            inspect(Some("image/png"), &png(8, 64)),
            Err(LogoError::Dimensions)
        );
        assert_eq!(
            inspect(Some("image/png"), &png(64, 5000)),
            Err(LogoError::Dimensions)
        );
        assert_eq!(
            inspect(Some("image/jpeg"), &jpeg(0, 64)),
            Err(LogoError::Dimensions)
        );
    }
}

//! Streaming `multipart/form-data` reader for file uploads (`POST /v1/files`
//! and the dashboard upload). Holds at most one network chunk plus a part
//! header (≤ 8 KiB) in memory; the file part is handed to the store as a
//! stream, never buffered.
use bytes::Bytes;
use futures::{Stream, StreamExt};

const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Why a form was refused. Messages are fixed strings (never client input).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormError {
    Malformed,
    /// The body exceeded the reader's byte limit.
    TooLarge,
    /// The client body failed (disconnect, transport limit).
    Body,
}

/// One part header.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartHeader {
    pub name: String,
    /// `Some` when the part declared a filename (possibly empty).
    pub filename: Option<String>,
    pub content_type: Option<String>,
}

/// The `boundary` parameter of a `multipart/form-data` content type.
pub fn boundary(content_type: &str) -> Option<String> {
    let mut parts = content_type.split(';');
    if !parts
        .next()?
        .trim()
        .eq_ignore_ascii_case("multipart/form-data")
    {
        return None;
    }
    parts.find_map(|p| {
        let (k, v) = p.split_once('=')?;
        if !k.trim().eq_ignore_ascii_case("boundary") {
            return None;
        }
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(v);
        // RFC 2046: 1..70 characters from a restricted set.
        ((1..=70).contains(&v.len())
            && v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"'()+_,-./:=? ".contains(&b)))
        .then(|| v.to_owned())
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Incremental reader over a request body stream.
pub struct FormReader<S> {
    body: S,
    buf: Vec<u8>,
    /// `\r\n--boundary`
    delimiter: Vec<u8>,
    eof: bool,
    part_done: bool,
    finished: bool,
    received: u64,
    max_bytes: u64,
    started: bool,
}

impl<S, E> FormReader<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin + Send + 'static,
{
    pub fn new(body: S, boundary: &str, max_bytes: u64) -> Self {
        Self {
            body,
            buf: Vec::new(),
            delimiter: format!("\r\n--{boundary}").into_bytes(),
            eof: false,
            part_done: true,
            finished: false,
            received: 0,
            max_bytes,
            started: false,
        }
    }

    async fn fill(&mut self) -> Result<bool, FormError> {
        if self.eof {
            return Ok(false);
        }
        match self.body.next().await {
            Some(Ok(chunk)) => {
                self.received = self.received.saturating_add(chunk.len() as u64);
                if self.received > self.max_bytes {
                    return Err(FormError::TooLarge);
                }
                self.buf.extend_from_slice(&chunk);
                Ok(true)
            }
            Some(Err(_)) => Err(FormError::Body),
            None => {
                self.eof = true;
                Ok(false)
            }
        }
    }

    /// Advances to the next part header; `None` after the closing delimiter.
    pub async fn next_part(&mut self) -> Result<Option<PartHeader>, FormError> {
        const BAD: FormError = FormError::Malformed;
        if self.finished {
            return Ok(None);
        }
        if !self.part_done {
            return Err(BAD);
        }
        if !self.started {
            let first = self.delimiter[2..].to_vec();
            while self.buf.len() < first.len() {
                if !self.fill().await? {
                    return Err(BAD);
                }
            }
            if !self.buf.starts_with(&first) {
                return Err(BAD);
            }
            self.buf.drain(..first.len());
            self.started = true;
        }
        while self.buf.len() < 2 {
            if !self.fill().await? {
                return Err(BAD);
            }
        }
        if self.buf.starts_with(b"--") {
            // Closing delimiter; only an optional CRLF may follow.
            self.buf.drain(..2);
            while self.fill().await? {
                if self.buf.len() > 2 {
                    return Err(BAD);
                }
            }
            if !(self.buf.is_empty() || self.buf == b"\r\n") {
                return Err(BAD);
            }
            self.finished = true;
            return Ok(None);
        }
        if !self.buf.starts_with(b"\r\n") {
            return Err(BAD);
        }
        self.buf.drain(..2);
        let end = loop {
            if let Some(end) = find(&self.buf, b"\r\n\r\n") {
                break end;
            }
            if self.buf.len() > MAX_HEADER_BYTES || !self.fill().await? {
                return Err(BAD);
            }
        };
        if end > MAX_HEADER_BYTES {
            return Err(BAD);
        }
        let headers = std::str::from_utf8(&self.buf[..end]).map_err(|_| BAD)?;
        let header = part_header(headers)?;
        self.buf.drain(..end + 4);
        self.part_done = false;
        Ok(Some(header))
    }

    /// Next data bytes of the current part; `None` when the part ended.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, FormError> {
        if self.part_done {
            return Ok(None);
        }
        loop {
            if let Some(pos) = find(&self.buf, &self.delimiter) {
                let data: Vec<u8> = self.buf.drain(..pos).collect();
                self.buf.drain(..self.delimiter.len());
                self.part_done = true;
                return Ok((!data.is_empty()).then(|| Bytes::from(data)));
            }
            // Keep a possible partial delimiter at the tail.
            let keep = self.delimiter.len() - 1;
            if self.buf.len() > keep {
                let n = self.buf.len() - keep;
                let data: Vec<u8> = self.buf.drain(..n).collect();
                return Ok(Some(Bytes::from(data)));
            }
            if !self.fill().await? {
                return Err(FormError::Malformed);
            }
        }
    }

    /// A small text part (at most `max` bytes, UTF-8).
    pub async fn text(&mut self, max: usize) -> Result<String, FormError> {
        let mut out = Vec::new();
        while let Some(data) = self.chunk().await? {
            out.extend_from_slice(&data);
            if out.len() > max {
                return Err(FormError::Malformed);
            }
        }
        String::from_utf8(out).map_err(|_| FormError::Malformed)
    }

    /// The current part's data as a stream that, after the part, requires the
    /// closing delimiter (the file must be the last part). Any form error ends
    /// the stream with an error item, so a store never commits a partial form.
    pub fn into_last_part(self) -> impl Stream<Item = Result<Bytes, FormError>> + Send + 'static {
        async_stream::stream! {
            let mut form = self;
            loop {
                match form.chunk().await {
                    Ok(Some(data)) => yield Ok(data),
                    Ok(None) => break,
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }
            match form.next_part().await {
                Ok(None) => {}
                Ok(Some(_)) => yield Err(FormError::Malformed),
                Err(e) => yield Err(e),
            }
        }
    }
}

impl<S, E> FormReader<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin + Send + 'static,
{
    /// The current part's data as a stream; when the part ends the reader is
    /// put back into `slot` so the caller can read the parts that follow
    /// (clients that send the file before other fields).
    pub fn into_part(
        self,
        slot: std::sync::Arc<std::sync::Mutex<Option<Self>>>,
    ) -> impl Stream<Item = Result<Bytes, FormError>> + Send + 'static {
        async_stream::stream! {
            let mut form = self;
            loop {
                match form.chunk().await {
                    Ok(Some(data)) => yield Ok(data),
                    Ok(None) => break,
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }
            if let Ok(mut s) = slot.lock() {
                *s = Some(form);
            }
        }
    }
}

/// Splits `a=b; c="d;e"` respecting quotes.
fn params(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut current, mut quoted, mut escaped) = (String::new(), false, false);
    for c in value.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => {
                current.push(c);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            ';' if !quoted => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    out.push(current);
    out
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    match v.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(inner) => {
            let mut out = String::new();
            let mut escaped = false;
            for c in inner.chars() {
                if escaped || c != '\\' {
                    out.push(c);
                    escaped = false;
                } else {
                    escaped = true;
                }
            }
            out
        }
        None => v.to_owned(),
    }
}

/// RFC 5987 `UTF-8''percent-encoded` value.
fn ext_value(v: &str) -> Option<String> {
    let (charset, rest) = v.trim().split_once('\'')?;
    let (_, encoded) = rest.split_once('\'')?;
    if !charset.eq_ignore_ascii_case("utf-8") {
        return None;
    }
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `Content-Disposition: form-data; name=...; filename=...` and an optional
/// `Content-Type`. Other headers are refused.
fn part_header(headers: &str) -> Result<PartHeader, FormError> {
    const BAD: FormError = FormError::Malformed;
    let mut header = PartHeader::default();
    let (mut name, mut disposition, mut star) = (None, false, None);
    for line in headers.split("\r\n") {
        if line.chars().any(|c| c.is_control() && c != '\t') {
            return Err(BAD);
        }
        let (key, value) = line.split_once(':').ok_or(BAD)?;
        match key.trim().to_ascii_lowercase().as_str() {
            "content-disposition" if !disposition => {
                disposition = true;
                let items = params(value);
                let mut items = items.iter();
                if !items
                    .next()
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("form-data"))
                {
                    return Err(BAD);
                }
                for item in items {
                    let (k, v) = item.split_once('=').ok_or(BAD)?;
                    match k.trim().to_ascii_lowercase().as_str() {
                        "name" if name.is_none() => name = Some(unquote(v)),
                        "filename" if header.filename.is_none() => {
                            header.filename = Some(unquote(v));
                        }
                        "filename*" if star.is_none() => star = Some(ext_value(v).ok_or(BAD)?),
                        _ => return Err(BAD),
                    }
                }
            }
            "content-type" if header.content_type.is_none() => {
                header.content_type = Some(value.trim().to_owned());
            }
            _ => return Err(BAD),
        }
    }
    if star.is_some() {
        header.filename = star;
    }
    header.name = name.ok_or(BAD)?;
    if header.name.is_empty() || header.name.len() > 64 {
        return Err(BAD);
    }
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(boundary: &str, parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, filename, data) in parts {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match filename {
                Some(f) => out.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\nContent-Type: application/jsonl\r\n\r\n").as_bytes(),
                ),
                None => out.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                ),
            }
            out.extend_from_slice(data);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        out
    }

    fn chunked(
        body: Vec<u8>,
        size: usize,
    ) -> impl Stream<Item = Result<Bytes, std::convert::Infallible>> + Unpin + Send + 'static {
        let chunks: Vec<_> = body
            .chunks(size)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        futures::stream::iter(chunks)
    }

    #[test]
    fn boundaries_are_strict() {
        assert_eq!(
            boundary("multipart/form-data; boundary=abc").as_deref(),
            Some("abc")
        );
        assert_eq!(
            boundary("multipart/form-data; boundary=\"a b\"").as_deref(),
            Some("a b")
        );
        assert_eq!(boundary("application/json; boundary=abc"), None);
        assert_eq!(boundary("multipart/form-data"), None);
        assert_eq!(
            boundary(&format!("multipart/form-data; boundary={}", "x".repeat(71))),
            None
        );
    }

    #[tokio::test]
    async fn streams_the_last_part_at_any_chunking() {
        let payload: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let body = form(
            "XyZ",
            &[
                ("purpose", None, b"batch"),
                ("file", Some("in.jsonl"), &payload),
            ],
        );
        for size in [1, 2, 7, 64, 1000, 65536] {
            let mut f = FormReader::new(chunked(body.clone(), size), "XyZ", 1 << 20);
            let p = f.next_part().await.unwrap().unwrap();
            assert_eq!((p.name.as_str(), p.filename.as_deref()), ("purpose", None));
            assert_eq!(f.text(64).await.unwrap(), "batch");
            let p = f.next_part().await.unwrap().unwrap();
            assert_eq!(p.filename.as_deref(), Some("in.jsonl"));
            assert_eq!(p.content_type.as_deref(), Some("application/jsonl"));
            let mut s = Box::pin(f.into_last_part());
            let mut got = Vec::new();
            while let Some(c) = s.next().await {
                got.extend_from_slice(&c.unwrap());
            }
            assert_eq!(got, payload, "chunk size {size}");
        }
    }

    #[tokio::test]
    async fn trailing_parts_and_limits_fail_the_file_stream() {
        let body = form(
            "b",
            &[
                ("file", Some("a.txt"), b"hello"),
                ("purpose", None, b"batch"),
            ],
        );
        let mut f = FormReader::new(chunked(body, 3), "b", 1 << 20);
        f.next_part().await.unwrap();
        let items: Vec<_> = Box::pin(f.into_last_part()).collect().await;
        assert_eq!(items.last(), Some(&Err(FormError::Malformed)));

        let body = form("b", &[("file", Some("a.txt"), &[7u8; 5000])]);
        let mut f = FormReader::new(chunked(body, 100), "b", 1000);
        f.next_part().await.unwrap();
        let items: Vec<_> = Box::pin(f.into_last_part()).collect().await;
        assert_eq!(items.last(), Some(&Err(FormError::TooLarge)));
    }

    #[test]
    fn headers_keep_quoted_and_extended_filenames() {
        let h = part_header(
            "Content-Disposition: form-data; name=\"file\"; filename=\"a;b \\\"c\\\".jsonl\"",
        )
        .unwrap();
        assert_eq!(h.filename.as_deref(), Some("a;b \"c\".jsonl"));
        let h = part_header("Content-Disposition: form-data; name=file; filename=\"x\"; filename*=UTF-8''%C3%A9t%C3%A9.txt").unwrap();
        assert_eq!(h.filename.as_deref(), Some("été.txt"));
        assert!(part_header("Content-Disposition: attachment; name=\"file\"").is_err());
        assert!(part_header("Content-Disposition: form-data; name=\"a\"\r\nX-Other: 1").is_err());
        assert!(part_header("Content-Disposition: form-data; name=\"\"").is_err());
    }
}

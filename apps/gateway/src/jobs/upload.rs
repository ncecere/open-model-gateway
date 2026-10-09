//! Streaming `multipart/form-data` reader and batch-line validation for
//! `POST /v1/files` (`purpose=batch`). Nothing is buffered beyond one line
//! (≤ [`BATCH_MAX_LINE_BYTES`]) plus one network chunk; nothing is stored or
//! logged. The form must be exactly `purpose` then `file`.
use axum::body::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value};

use super::types::*;
use crate::inference::error::InferenceError;

type Result<T> = std::result::Result<T, InferenceError>;
const BAD: InferenceError = InferenceError::InvalidRequest;
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// One part header: form field name and whether it declared a filename.
pub(crate) struct PartHeader {
    pub name: String,
    pub has_filename: bool,
}

/// Incremental reader over a request body stream.
pub(crate) struct StreamingForm<S> {
    body: S,
    buf: Vec<u8>,
    /// `\r\n--boundary`
    delimiter: Vec<u8>,
    eof: bool,
    /// The current part's data ended (delimiter consumed).
    part_done: bool,
    finished: bool,
    received: u64,
    max_bytes: u64,
    started: bool,
}
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
impl<S, E> StreamingForm<S>
where
    S: Stream<Item = std::result::Result<Bytes, E>> + Unpin,
{
    pub(crate) fn new(body: S, boundary: &str, max_bytes: u64) -> Self {
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
    /// Read one more network chunk; `false` at end of body.
    async fn fill(&mut self) -> Result<bool> {
        if self.eof {
            return Ok(false);
        }
        match self.body.next().await {
            Some(Ok(chunk)) => {
                self.received = self.received.saturating_add(chunk.len() as u64);
                if self.received > self.max_bytes {
                    return Err(InferenceError::InvalidRequest);
                }
                self.buf.extend_from_slice(&chunk);
                Ok(true)
            }
            // A body limit or client abort: never a complete form.
            Some(Err(_)) => Err(InferenceError::InvalidRequest),
            None => {
                self.eof = true;
                Ok(false)
            }
        }
    }
    /// Advance to the next part header; `None` after the closing delimiter.
    pub(crate) async fn next_part(&mut self) -> Result<Option<PartHeader>> {
        if self.finished {
            return Ok(None);
        }
        if !self.part_done {
            return Err(BAD);
        }
        if !self.started {
            // The body starts with `--boundary` (no preamble).
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
    pub(crate) async fn chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if self.part_done {
            return Ok(None);
        }
        loop {
            if let Some(pos) = find(&self.buf, &self.delimiter) {
                let data: Vec<u8> = self.buf.drain(..pos).collect();
                self.buf.drain(..self.delimiter.len());
                self.part_done = true;
                return Ok((!data.is_empty()).then_some(data));
            }
            // Keep a possible partial delimiter at the tail.
            let keep = self.delimiter.len() - 1;
            if self.buf.len() > keep {
                let n = self.buf.len() - keep;
                let data: Vec<u8> = self.buf.drain(..n).collect();
                return Ok(Some(data));
            }
            if !self.fill().await? {
                return Err(BAD);
            }
        }
    }
    /// A small text part (at most `max` bytes).
    pub(crate) async fn text(&mut self, max: usize) -> Result<String> {
        let mut out = Vec::new();
        while let Some(data) = self.chunk().await? {
            out.extend_from_slice(&data);
            if out.len() > max {
                return Err(BAD);
            }
        }
        String::from_utf8(out).map_err(|_| BAD)
    }
}

/// `Content-Disposition: form-data; name=...; filename=...` and an optional
/// `Content-Type`; nothing else.
fn part_header(headers: &str) -> Result<PartHeader> {
    let mut name = None;
    let mut has_filename = false;
    let mut disposition = false;
    for line in headers.split("\r\n") {
        if line.chars().any(|c| c.is_control() && c != '\t') {
            return Err(BAD);
        }
        let (key, value) = line.split_once(':').ok_or(BAD)?;
        match key.trim().to_ascii_lowercase().as_str() {
            "content-disposition" if !disposition => {
                disposition = true;
                let mut items = value.split(';');
                if !items
                    .next()
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("form-data"))
                {
                    return Err(BAD);
                }
                for item in items {
                    let (k, v) = item.split_once('=').ok_or(BAD)?;
                    let v = v.trim();
                    let v = v
                        .strip_prefix('"')
                        .and_then(|v| v.strip_suffix('"'))
                        .unwrap_or(v);
                    match k.trim().to_ascii_lowercase().as_str() {
                        "name" if name.is_none() => name = Some(v.to_owned()),
                        "filename" | "filename*" => has_filename = true,
                        _ => return Err(BAD),
                    }
                }
            }
            "content-type" => {}
            _ => return Err(BAD),
        }
    }
    let name = name.ok_or(BAD)?;
    if name.is_empty() || name.len() > 64 {
        return Err(BAD);
    }
    Ok(PartHeader { name, has_filename })
}

/// Splits a part's data into lines without holding more than one line.
#[derive(Default)]
pub(crate) struct Lines {
    pending: Vec<u8>,
}
impl Lines {
    /// Complete lines in `data` (without `\n`); the remainder stays pending.
    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut rest = data;
        while let Some(pos) = rest.iter().position(|b| *b == b'\n') {
            self.pending.extend_from_slice(&rest[..pos]);
            if self.pending.len() > BATCH_MAX_LINE_BYTES {
                return Err(BAD);
            }
            out.push(std::mem::take(&mut self.pending));
            rest = &rest[pos + 1..];
        }
        self.pending.extend_from_slice(rest);
        if self.pending.len() > BATCH_MAX_LINE_BYTES {
            return Err(BAD);
        }
        Ok(out)
    }
    /// The final line without a trailing newline, if any.
    pub(crate) fn finish(&mut self) -> Option<Vec<u8>> {
        let last = std::mem::take(&mut self.pending);
        (!last.is_empty()).then_some(last)
    }
}

/// One validated batch line.
pub(crate) struct Line {
    pub model: String,
    pub max_output_tokens: u32,
    body: Map<String, Value>,
}
impl Line {
    /// The line re-encoded for the provider with its upstream model id.
    pub(crate) fn encode(mut self, upstream_model: &str) -> Result<Vec<u8>> {
        let custom_id = self.body.remove("custom_id");
        let mut request = self.body.remove("body").ok_or(BAD)?;
        request["model"] = Value::String(upstream_model.to_owned());
        let mut out = serde_json::to_vec(&serde_json::json!({
            "custom_id": custom_id,
            "method": "POST",
            "url": BATCH_ENDPOINT,
            "body": request,
        }))
        .map_err(|_| BAD)?;
        out.push(b'\n');
        Ok(out)
    }
}

/// Strict line contract: `{custom_id, method:"POST", url, body}` with a
/// Chat Completions body naming `model` (the same for every line) and
/// exactly one positive `max_completion_tokens`/`max_tokens`. Features whose
/// cost the line's maximum cannot bound (several choices, audio output,
/// built-in web search, predicted outputs) and streaming are rejected.
pub(crate) fn validate_line(raw: &[u8], model: Option<&str>) -> Result<Line> {
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let Value::Object(top) = serde_json::from_slice::<Value>(raw).map_err(|_| BAD)? else {
        return Err(BAD);
    };
    if top.len() != 4
        || !top
            .keys()
            .all(|k| matches!(k.as_str(), "custom_id" | "method" | "url" | "body"))
    {
        return Err(BAD);
    }
    if !top["custom_id"]
        .as_str()
        .is_some_and(|s| !s.is_empty() && s.len() <= 512)
        || top["method"] != "POST"
    {
        return Err(BAD);
    }
    if top["url"] != BATCH_ENDPOINT {
        // Other batch endpoints are representable but not supported here.
        return Err(if top["url"].is_string() {
            InferenceError::Unsupported
        } else {
            BAD
        });
    }
    let body = top["body"].as_object().ok_or(BAD)?;
    let line_model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.trim().is_empty() && m.len() <= 200)
        .ok_or(BAD)?;
    if model.is_some_and(|m| m != line_model) || !body.get("messages").is_some_and(Value::is_array)
    {
        return Err(BAD);
    }
    let maxima: Vec<u32> = ["max_completion_tokens", "max_tokens"]
        .into_iter()
        .filter_map(|k| body.get(k))
        .map(|v| {
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0)
                .ok_or(BAD)
        })
        .collect::<Result<_>>()?;
    let [max_output_tokens] = maxima[..] else {
        // An unbounded line cannot be reserved for.
        return Err(BAD);
    };
    let unsupported = body.get("stream").is_some_and(|v| v != &Value::Bool(false))
        || body.contains_key("stream_options")
        || body.get("n").is_some_and(|v| v.as_u64() != Some(1))
        || body.contains_key("web_search_options")
        || body.contains_key("audio")
        || body.contains_key("prediction")
        || body.get("modalities").is_some_and(|m| {
            m.as_array()
                .is_none_or(|a| a.iter().any(|v| v.as_str() != Some("text")))
        });
    if unsupported {
        return Err(InferenceError::Unsupported);
    }
    Ok(Line {
        model: line_model.to_owned(),
        max_output_tokens,
        body: top,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(v: Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }
    fn ok() -> Value {
        json!({"custom_id":"r1","method":"POST","url":"/v1/chat/completions","body":{"model":"gw-batch","messages":[{"role":"user","content":"hi"}],"max_completion_tokens":64}})
    }
    #[test]
    fn lines_are_strict_and_bounded() {
        let l = validate_line(&line(ok()), None).unwrap();
        assert_eq!((l.model.as_str(), l.max_output_tokens), ("gw-batch", 64));
        let encoded: Value = serde_json::from_slice(&l.encode("gpt-x").unwrap()).unwrap();
        assert_eq!(encoded["body"]["model"], "gpt-x");
        assert_eq!(encoded["custom_id"], "r1");
        assert!(validate_line(&line(ok()), Some("other")).is_err());
        let bad = |f: &dyn Fn(&mut Value)| {
            let mut v = ok();
            f(&mut v);
            validate_line(&line(v), None).err()
        };
        assert_eq!(bad(&|v| v["method"] = json!("GET")), Some(BAD));
        assert_eq!(bad(&|v| v["extra"] = json!(1)), Some(BAD));
        assert_eq!(bad(&|v| v["custom_id"] = json!("")), Some(BAD));
        assert_eq!(
            bad(&|v| v["url"] = json!("/v1/embeddings")),
            Some(InferenceError::Unsupported)
        );
        assert_eq!(
            bad(&|v| {
                v["body"]
                    .as_object_mut()
                    .unwrap()
                    .remove("max_completion_tokens");
            }),
            Some(BAD)
        );
        assert_eq!(bad(&|v| v["body"]["max_tokens"] = json!(10)), Some(BAD));
        assert_eq!(
            bad(&|v| v["body"]["max_completion_tokens"] = json!(0)),
            Some(BAD)
        );
        assert_eq!(
            bad(&|v| v["body"]["max_completion_tokens"] = json!(1.5)),
            Some(BAD)
        );
        assert_eq!(
            bad(&|v| v["body"]["n"] = json!(2)),
            Some(InferenceError::Unsupported)
        );
        assert_eq!(
            bad(&|v| v["body"]["stream"] = json!(true)),
            Some(InferenceError::Unsupported)
        );
        assert_eq!(
            bad(&|v| v["body"]["modalities"] = json!(["text", "audio"])),
            Some(InferenceError::Unsupported)
        );
        assert_eq!(
            bad(&|v| v["body"]["web_search_options"] = json!({})),
            Some(InferenceError::Unsupported)
        );
        assert!(bad(&|v| v["body"]["n"] = json!(1)).is_none());
        assert!(bad(&|v| v["body"]["stream"] = json!(false)).is_none());
        assert!(validate_line(b"not json", None).is_err());
        assert!(validate_line(b"[1]", None).is_err());
    }
    #[test]
    fn line_splitter_holds_one_line() {
        let mut l = Lines::default();
        assert_eq!(l.push(b"ab\ncd").unwrap(), vec![b"ab".to_vec()]);
        assert_eq!(l.push(b"e\n\nf").unwrap(), vec![b"cde".to_vec(), vec![]]);
        assert_eq!(l.finish(), Some(b"f".to_vec()));
        assert_eq!(l.finish(), None);
        let mut big = Lines::default();
        assert!(big.push(&vec![b'x'; BATCH_MAX_LINE_BYTES + 1]).is_err());
    }
    fn form(parts: &[(&str, bool, &[u8])]) -> Vec<u8> {
        let mut b = Vec::new();
        for (name, file, data) in parts {
            b.extend_from_slice(b"--BOUND\r\nContent-Disposition: form-data; name=\"");
            b.extend_from_slice(name.as_bytes());
            b.extend_from_slice(b"\"");
            if *file {
                b.extend_from_slice(b"; filename=\"x.jsonl\"\r\nContent-Type: application/jsonl");
            }
            b.extend_from_slice(b"\r\n\r\n");
            b.extend_from_slice(data);
            b.extend_from_slice(b"\r\n");
        }
        b.extend_from_slice(b"--BOUND--\r\n");
        b
    }
    async fn read_all(body: Vec<u8>, chunk: usize) -> Result<Vec<(String, Vec<u8>)>> {
        let chunks: Vec<std::result::Result<Bytes, std::io::Error>> = body
            .chunks(chunk)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        let mut f = StreamingForm::new(futures_util::stream::iter(chunks), "BOUND", 1 << 20);
        let mut out = Vec::new();
        while let Some(h) = f.next_part().await? {
            let mut data = Vec::new();
            while let Some(c) = f.chunk().await? {
                data.extend_from_slice(&c);
            }
            out.push((h.name, data));
        }
        Ok(out)
    }
    #[tokio::test]
    async fn streaming_form_handles_any_chunking() {
        let content = b"{\"a\":1}\r\n--BOUN not a delimiter\n{\"b\":2}\n";
        let body = form(&[("purpose", false, b"batch"), ("file", true, content)]);
        for chunk in [1, 2, 3, 7, 64, 4096] {
            let parts = read_all(body.clone(), chunk).await.unwrap();
            assert_eq!(parts.len(), 2);
            assert_eq!(parts[0], ("purpose".into(), b"batch".to_vec()));
            assert_eq!(parts[1].1, content.to_vec());
        }
        assert!(read_all(b"garbage".to_vec(), 3).await.is_err());
        let mut truncated = form(&[("file", true, b"x")]);
        truncated.truncate(truncated.len() - 8);
        assert!(read_all(truncated, 5).await.is_err());
        let mut extra = form(&[("file", true, b"x")]);
        extra.extend_from_slice(b"junk");
        assert!(read_all(extra, 5).await.is_err());
    }
    #[tokio::test]
    async fn streaming_form_enforces_the_byte_cap() {
        let body = form(&[("file", true, &[b'x'; 4096])]);
        let chunks = vec![Ok::<_, std::io::Error>(Bytes::from(body))];
        let mut f = StreamingForm::new(futures_util::stream::iter(chunks), "BOUND", 1024);
        assert!(f.next_part().await.is_err());
    }
}

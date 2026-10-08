//! Strict, bounded `multipart/form-data` (RFC 7578) parser over a fully
//! buffered body. Each part may carry only `Content-Disposition: form-data`
//! (`name`, optional `filename`/`filename*`) and an optional `Content-Type`.
//! There is no preamble, no epilogue beyond a final CRLF, and no nested or
//! encoded parts. Filenames are returned for extension checks only.
use crate::inference::error::InferenceError;

const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_NAME_BYTES: usize = 64;
const MAX_FILENAME_BYTES: usize = 255;

type Result<T> = std::result::Result<T, InferenceError>;
const BAD: InferenceError = InferenceError::InvalidRequest;

pub struct Part<'a> {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: &'a [u8],
}

/// The boundary of a `multipart/form-data` content type.
pub fn boundary(content_type: &str) -> Result<String> {
    let mut items = content_type.split(';');
    if !items
        .next()
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("multipart/form-data"))
    {
        return Err(BAD);
    }
    let mut found = None;
    for item in items {
        let (key, value) = item.split_once('=').ok_or(BAD)?;
        if !key.trim().eq_ignore_ascii_case("boundary") {
            continue;
        }
        let value = value.trim();
        let value = match value.strip_prefix('"') {
            Some(v) => v.strip_suffix('"').ok_or(BAD)?,
            None => value,
        };
        let valid = (1..=70).contains(&value.len())
            && !value.ends_with(' ')
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"'()+_,-./:=? ".contains(&b));
        if !valid || found.replace(value.to_owned()).is_some() {
            return Err(BAD);
        }
    }
    found.ok_or(BAD)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let first = *needle.first()?;
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        let p = hay[i..=hay.len() - needle.len()]
            .iter()
            .position(|&b| b == first)?;
        i += p;
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Split `; key=value` parameters; values are tokens or quoted strings.
fn params(s: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut rest = s.trim_start();
    while !rest.is_empty() {
        rest = rest.strip_prefix(';').ok_or(BAD)?.trim_start();
        let eq = rest.find('=').ok_or(BAD)?;
        let key = rest[..eq].trim().to_ascii_lowercase();
        rest = rest[eq + 1..].trim_start();
        let value;
        if let Some(quoted) = rest.strip_prefix('"') {
            let mut v = String::new();
            let mut chars = quoted.char_indices();
            let mut end = None;
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => v.push(chars.next().ok_or(BAD)?.1),
                    '"' => {
                        end = Some(i + 1);
                        break;
                    }
                    c => v.push(c),
                }
            }
            rest = quoted[end.ok_or(BAD)?..].trim_start();
            value = v;
        } else {
            let end = rest.find(';').unwrap_or(rest.len());
            value = rest[..end].trim().to_owned();
            rest = &rest[end..];
        }
        if key.is_empty() || out.iter().any(|(k, _)| *k == key) {
            return Err(BAD);
        }
        out.push((key, value));
    }
    Ok(out)
}

fn part<'a>(headers: &[u8], data: &'a [u8]) -> Result<Part<'a>> {
    let headers = std::str::from_utf8(headers).map_err(|_| BAD)?;
    let mut disposition = None;
    let mut content_type = None;
    for line in headers.split("\r\n") {
        if line.chars().any(|c| c.is_control() && c != '\t') {
            return Err(BAD);
        }
        let (name, value) = line.split_once(':').ok_or(BAD)?;
        if name.is_empty() || name.contains([' ', '\t']) {
            return Err(BAD);
        }
        let slot = match name.to_ascii_lowercase().as_str() {
            "content-disposition" => &mut disposition,
            "content-type" => &mut content_type,
            _ => return Err(BAD),
        };
        if slot.replace(value.trim().to_owned()).is_some() {
            return Err(BAD);
        }
    }
    let disposition = disposition.ok_or(BAD)?;
    let kind_end = disposition.find(';').unwrap_or(disposition.len());
    if !disposition[..kind_end]
        .trim()
        .eq_ignore_ascii_case("form-data")
    {
        return Err(BAD);
    }
    let mut name = None;
    let mut filename = None;
    for (key, value) in params(&disposition[kind_end..])? {
        match key.as_str() {
            "name" => name = Some(value),
            "filename" => {
                filename.get_or_insert(value);
            }
            // RFC 5987 `charset'lang'pct-encoded`: only the tail matters
            // (extension); it takes precedence like in browsers.
            "filename*" => {
                let tail = value.rsplit('\'').next().unwrap_or("").to_owned();
                filename = Some(tail);
            }
            _ => return Err(BAD),
        }
    }
    let name = name.ok_or(BAD)?;
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || !name.bytes().all(|b| b.is_ascii_graphic() && b != b'"')
    {
        return Err(BAD);
    }
    if let Some(f) = &filename
        && (f.is_empty() || f.len() > MAX_FILENAME_BYTES || f.chars().any(char::is_control))
    {
        return Err(BAD);
    }
    Ok(Part {
        name,
        filename,
        content_type: content_type.map(|t| {
            t.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        }),
        data,
    })
}

/// Parse all parts (at most `max_parts`).
pub fn parse<'a>(body: &'a [u8], boundary: &str, max_parts: usize) -> Result<Vec<Part<'a>>> {
    let delimiter = format!("--{boundary}");
    let next = format!("\r\n--{boundary}");
    if !body.starts_with(delimiter.as_bytes()) {
        return Err(BAD);
    }
    let mut pos = delimiter.len();
    let mut parts = Vec::new();
    loop {
        let rest = &body[pos..];
        if let Some(epilogue) = rest.strip_prefix(b"--") {
            if epilogue.is_empty() || epilogue == b"\r\n" {
                return Ok(parts);
            }
            return Err(BAD);
        }
        let rest = rest.strip_prefix(b"\r\n").ok_or(BAD)?;
        pos += 2;
        let window = &rest[..rest.len().min(MAX_HEADER_BYTES + 4)];
        let header_end = find(window, b"\r\n\r\n").ok_or(BAD)?;
        let content = pos + header_end + 4;
        let end = find(&body[content..], next.as_bytes()).ok_or(BAD)?;
        parts.push(part(&rest[..header_end], &body[content..content + end])?);
        if parts.len() > max_parts {
            return Err(BAD);
        }
        pos = content + end + next.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (headers, data) in parts {
            out.extend_from_slice(b"--XyZ\r\n");
            out.extend_from_slice(headers.as_bytes());
            out.extend_from_slice(b"\r\n\r\n");
            out.extend_from_slice(data);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"--XyZ--\r\n");
        out
    }

    #[test]
    fn boundary_parsing_is_strict() {
        assert_eq!(
            boundary("multipart/form-data; boundary=XyZ").unwrap(),
            "XyZ"
        );
        assert_eq!(
            boundary("Multipart/Form-Data; charset=utf-8; boundary=\"a b\"").unwrap(),
            "a b"
        );
        for bad in [
            "application/json",
            "multipart/mixed; boundary=x",
            "multipart/form-data",
            "multipart/form-data; boundary=",
            "multipart/form-data; boundary=a; boundary=b",
            "multipart/form-data; boundary=\"unterminated",
            "multipart/form-data; boundary=bad\u{7f}",
        ] {
            assert!(boundary(bad).is_err(), "{bad}");
        }
        assert!(boundary(&format!("multipart/form-data; boundary={}", "x".repeat(71))).is_err());
    }

    #[test]
    fn parses_fields_files_and_binary_with_crlf() {
        let data = b"RIFF\r\n--XY\r\n\0\xff";
        let b = body(&[
            (
                "Content-Disposition: form-data; name=\"model\"",
                b"whisper-1",
            ),
            (
                "Content-Disposition: form-data; name=\"file\"; filename=\"C:\\\\x\\\\a \\\"b\\\".wav\"\r\nContent-Type: Audio/WAV; rate=8000",
                data,
            ),
        ]);
        let parts = parse(&b, "XyZ", 8).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            (parts[0].name.as_str(), parts[0].data),
            ("model", &b"whisper-1"[..])
        );
        assert_eq!(parts[1].filename.as_deref(), Some("C:\\x\\a \"b\".wav"));
        assert_eq!(parts[1].content_type.as_deref(), Some("audio/wav"));
        assert_eq!(parts[1].data, data);
        let star = body(&[(
            "content-disposition: form-data; name=file; filename*=UTF-8''%E2%9C%93.mp3",
            b"x",
        )]);
        assert_eq!(
            parse(&star, "XyZ", 8).unwrap()[0].filename.as_deref(),
            Some("%E2%9C%93.mp3")
        );
        // Empty form.
        assert_eq!(parse(b"--XyZ--", "XyZ", 8).unwrap().len(), 0);
    }

    #[test]
    fn rejects_malformed_bodies() {
        let ok = "Content-Disposition: form-data; name=\"a\"";
        let cases: Vec<Vec<u8>> = vec![
            b"preamble\r\n--XyZ--".to_vec(),
            body(&[("Content-Disposition: attachment; name=\"a\"", b"x")]),
            body(&[("Content-Disposition: form-data", b"x")]),
            body(&[("Content-Disposition: form-data; name=\"\"", b"x")]),
            body(&[(
                "Content-Disposition: form-data; name=\"a\"; name=\"b\"",
                b"x",
            )]),
            body(&[("Content-Disposition: form-data; name=\"a\"; size=3", b"x")]),
            body(&[(&format!("{ok}\r\nContent-Transfer-Encoding: base64"), b"x")]),
            body(&[(&format!("{ok}\r\n{ok}"), b"x")]),
            body(&[(&format!("{ok}\r\nX-Bad\u{1}: y"), b"x")]),
            body(&[(
                "Content-Disposition: form-data; name=\"a\"; filename=\"\"",
                b"x",
            )]),
            body(&[(
                &format!(
                    "Content-Disposition: form-data; name=\"a\"; filename=\"{}\"",
                    "f".repeat(256)
                ),
                b"x",
            )]),
            // Missing terminating delimiter.
            b"--XyZ\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nx".to_vec(),
            // Epilogue junk.
            [body(&[(ok, b"x")]), b"junk".to_vec()].concat(),
            // Headers without the blank line within the bound.
            [b"--XyZ\r\n".to_vec(), vec![b'a'; MAX_HEADER_BYTES + 10]].concat(),
        ];
        for (i, case) in cases.iter().enumerate() {
            assert!(parse(case, "XyZ", 8).is_err(), "case {i}");
        }
        let many: Vec<(&str, &[u8])> = vec![(ok, b"x"); 3];
        assert!(parse(&body(&many), "XyZ", 2).is_err());
    }
}

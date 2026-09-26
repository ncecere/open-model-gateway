//! Bounded, cancellation-on-drop HTTP/SSE primitives for native adapters.
use crate::inference::error::InferenceError;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
pub(super) type Result<T> = std::result::Result<T, InferenceError>;
pub(super) const BODY_LIMIT: usize = 4 * 1024 * 1024;
const SSE_LIMIT: usize = 1024 * 1024;
pub(super) fn transport(error: reqwest::Error) -> InferenceError {
    if error.is_timeout() {
        InferenceError::Timeout
    } else {
        InferenceError::UpstreamUnavailable
    }
}
pub(super) fn status(status: reqwest::StatusCode) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    Err(match status.as_u16() {
        401 | 403 | 300..=399 => InferenceError::Configuration,
        429 => InferenceError::Busy,
        408 => InferenceError::Timeout,
        400..=499 => InferenceError::UpstreamRejected,
        500..=599 => InferenceError::UpstreamUnavailable,
        _ => InferenceError::InvalidUpstream,
    })
}
pub(super) async fn body(response: reqwest::Response) -> Result<Value> {
    if response
        .content_length()
        .is_some_and(|n| n > BODY_LIMIT as u64)
    {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport)?;
        if chunk.len() > BODY_LIMIT - bytes.len() {
            return Err(InferenceError::InvalidUpstream);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| InferenceError::InvalidUpstream)
}
pub(super) fn check_sse(response: &reqwest::Response) -> Result<()> {
    if !response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
        })
    {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(())
}
#[derive(Default)]
struct Decoder {
    line: Vec<u8>,
    data: Vec<u8>,
    size: usize,
    skip_lf: bool,
    started: bool,
}
impl Decoder {
    fn push(&mut self, byte: u8) -> Result<Option<Vec<u8>>> {
        // Count wire bytes, including the LF half of CRLF. A trailing LF
        // after a frame boundary is conservatively charged to the next frame.
        self.size += 1;
        if self.size > SSE_LIMIT {
            return Err(InferenceError::InvalidUpstream);
        }
        if self.skip_lf {
            self.skip_lf = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }
        if byte != b'\r' && byte != b'\n' {
            self.line.push(byte);
            return Ok(None);
        }
        self.skip_lf = byte == b'\r';
        let line = std::str::from_utf8(&self.line).map_err(|_| InferenceError::InvalidUpstream)?;
        let line = if !self.started {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        self.started = true;
        if line.is_empty() {
            self.line.clear();
            self.size = 0;
            if self.data.is_empty() {
                return Ok(None);
            }
            self.data.pop();
            return Ok(Some(std::mem::take(&mut self.data)));
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            self.data
                .extend_from_slice(value.strip_prefix(' ').unwrap_or(value).as_bytes());
            self.data.push(b'\n');
        }
        self.line.clear();
        Ok(None)
    }
}
pub(super) fn frames(response: reqwest::Response) -> impl Stream<Item = Result<Value>> + Send {
    async_stream::try_stream! {
        let mut stream = response.bytes_stream(); let mut decoder = Decoder::default();
        while let Some(chunk) = stream.next().await {
            for byte in chunk.map_err(transport)?.iter() {
                if let Some(data) = decoder.push(*byte)? {
                    yield serde_json::from_slice::<Value>(&data).map_err(|_| InferenceError::InvalidUpstream)?;
                }
            }
        }
        // A protocol's explicit terminal event must cause the consumer to return first.
        Err(InferenceError::InvalidUpstream)?;
    }
}
pub(super) fn string(value: &Value) -> Result<&str> {
    value.as_str().ok_or(InferenceError::InvalidUpstream)
}
pub(super) fn nonempty(value: &Value) -> Result<String> {
    let s = string(value)?;
    if s.is_empty() {
        Err(InferenceError::InvalidUpstream)
    } else {
        Ok(s.into())
    }
}
pub(super) fn fields(value: &Value, allowed: &[&str]) -> Result<()> {
    let object = value.as_object().ok_or(InferenceError::InvalidUpstream)?;
    if object.iter().any(|(k, v)| {
        !allowed.contains(&k.as_str())
            && !v.is_null()
            && v.as_array().is_none_or(|v| !v.is_empty())
            && v.as_str() != Some("")
    }) {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(())
}
pub(super) fn usage(value: &Value) -> Result<crate::inference::types::Usage> {
    if !value.is_null() && !value.is_object() {
        return Err(InferenceError::InvalidUpstream);
    }
    let count = |key| {
        if value[key].is_null() {
            Ok(None)
        } else {
            value[key]
                .as_u64()
                .map(Some)
                .ok_or(InferenceError::InvalidUpstream)
        }
    };
    Ok(crate::inference::types::Usage {
        input_tokens: count("input_tokens")?,
        output_tokens: count("output_tokens")?,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_transport_drop_cancels_body_and_frames() {
        use axum::{
            Router,
            body::{Body, Bytes},
            http::Response,
        };
        use std::{convert::Infallible, sync::Arc, time::Duration};
        struct Guard(Arc<tokio::sync::Notify>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.notify_one();
            }
        }
        for streaming in [false, true] {
            let dropped = Arc::new(tokio::sync::Notify::new());
            let began = Arc::new(tokio::sync::Notify::new());
            let d = dropped.clone();
            let b = began.clone();
            let app = Router::new().fallback(move || {
                let guard = Guard(d.clone()); let began = b.clone();
                async move {
                    let stream = async_stream::stream! {
                        let _guard = guard; began.notify_one();
                        yield Ok::<_,Infallible>(Bytes::from_static(b"data: {\"type\":\"ping\"}\n\n"));
                        std::future::pending::<()>().await;
                    };
                    Response::builder().header("content-type","text/event-stream").body(Body::from_stream(stream)).unwrap()
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let response = reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(url)
                .send()
                .await
                .unwrap();
            if streaming {
                let mut stream = Box::pin(frames(response));
                assert!(stream.next().await.unwrap().is_ok());
                drop(stream);
            } else {
                let mut future = Box::pin(body(response));
                tokio::select! { _ = &mut future => panic!("body unexpectedly completed"), _ = began.notified() => {} }
                drop(future);
            }
            let cancelled = tokio::time::timeout(Duration::from_secs(3), dropped.notified()).await;
            task.abort();
            assert!(cancelled.is_ok(), "consumer drop must cancel upstream");
        }
    }
    #[test]
    fn malformed_utf8_and_usage_are_rejected() {
        let mut d = Decoder::default();
        d.push(0xff).unwrap();
        assert!(d.push(b'\n').is_err());
        assert!(usage(&serde_json::json!({"input_tokens":-1})).is_err());
        assert!(usage(&serde_json::json!("bad")).is_err());
        assert_eq!(
            usage(&Value::Null).unwrap(),
            crate::inference::types::Usage::default()
        );
    }
    #[test]
    fn framing_utf8_crlf_multiline_and_bound() {
        let mut d = Decoder::default();
        let mut frames = vec![];
        for b in "\u{feff}:comment\r\ndata: {\r\ndata: \"text\":\"世界\"}\r\n\r\n".bytes() {
            if let Some(v) = d.push(b).unwrap() {
                frames.push(v);
            }
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&frames[0]).unwrap()["text"],
            "世界"
        );
        let mut d = Decoder::default();
        for _ in 0..SSE_LIMIT {
            d.push(b'x').unwrap();
        }
        assert!(d.push(b'x').is_err());
    }
}

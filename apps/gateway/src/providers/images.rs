//! Shared image-generation wire helpers for native adapters: a separately
//! capped response reader, strict `data[]` decoding (base64 + magic + count)
//! and the workload's meter set. Image bytes and prompts are never logged.
use futures_util::StreamExt;
use serde_json::Value;

use super::framing::transport;
use crate::{
    billing::{MeterUsage, MeterVariant},
    inference::{
        error::InferenceError,
        evidence,
        images::{
            GeneratedImage, IMAGE_MAX_REVISED_PROMPT_BYTES, ImageMediaType, ImageRequest,
            ImageResponse, decode_base64,
        },
        types::Usage,
    },
};

type Result<T> = std::result::Result<T, InferenceError>;

/// Read a success body up to `limit` bytes (the image response cap, not the
/// shared 4 MiB provider cap). Dropping the future drops the connection.
pub(crate) async fn read_bounded(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport)?;
        if chunk.len() > limit - bytes.len() {
            return Err(InferenceError::InvalidUpstream);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Image workload meters: `images` returned (unknown when the body has no
/// `data` array), one request, semantic zeros for meters images cannot use.
pub(crate) fn meters(images: Option<u64>) -> MeterUsage {
    MeterUsage {
        output_images: images,
        input_characters: Some(0),
        input_audio_seconds_ms: Some(0),
        output_audio_seconds_ms: Some(0),
        search_units: Some(0),
        requests: Some(1),
        output_video_seconds_ms: None,
    }
}

fn empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// Strictly decode one `data[]` item. URLs are never accepted (the gateway
/// requested base64 and does not host files); unknown content fields fail.
fn image(item: &Value, format: Option<ImageMediaType>) -> Result<GeneratedImage> {
    let object = item.as_object().ok_or(InferenceError::InvalidUpstream)?;
    for (key, value) in object {
        let ok = match key.as_str() {
            "b64_json" => true,
            "revised_prompt" => value.is_null() || value.is_string(),
            "media_type" => value.is_null() || value.is_string(),
            // OpenAI per-image metadata identifier.
            "generation_id" => value.is_null() || value.is_string(),
            _ => empty(value),
        };
        if !ok {
            return Err(InferenceError::InvalidUpstream);
        }
    }
    let b64 = item["b64_json"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(InferenceError::InvalidUpstream)?;
    let bytes = decode_base64(b64).ok_or(InferenceError::InvalidUpstream)?;
    let media_type = ImageMediaType::sniff(&bytes).ok_or(InferenceError::InvalidUpstream)?;
    drop(bytes);
    if let Some(declared) = item["media_type"].as_str()
        && ImageMediaType::parse(declared) != Some(media_type)
    {
        return Err(InferenceError::InvalidUpstream);
    }
    if format.is_some_and(|f| f != media_type) {
        return Err(InferenceError::InvalidUpstream);
    }
    let revised_prompt = match item["revised_prompt"].as_str() {
        Some(p) if p.len() > IMAGE_MAX_REVISED_PROMPT_BYTES => {
            return Err(InferenceError::InvalidUpstream);
        }
        Some(p) if !p.is_empty() => Some(p.to_owned()),
        _ => None,
    };
    Ok(GeneratedImage {
        b64_json: b64.to_owned(),
        media_type,
        revised_prompt,
    })
}

fn images(
    value: &Value,
    request: &ImageRequest,
    format: Option<ImageMediaType>,
) -> Result<Vec<GeneratedImage>> {
    let data = value["data"]
        .as_array()
        .ok_or(InferenceError::InvalidUpstream)?;
    if data.len() != request.n as usize {
        return Err(InferenceError::InvalidUpstream);
    }
    data.iter().map(|item| image(item, format)).collect()
}

/// Build the response from an already error-checked JSON object. `usage`
/// carries the adapter's normalized token usage; meters and the variant are
/// attached here. A structural failure keeps the usage (including the count
/// of images the provider returned) as evidence for the failed attempt.
pub(crate) fn decode(
    value: &Value,
    request: &ImageRequest,
    mut usage: Usage,
    variant: Option<&str>,
    format: Result<Option<ImageMediaType>>,
) -> Result<ImageResponse> {
    usage.meters = Some(meters(value["data"].as_array().map(|a| a.len() as u64)));
    usage.output_image_variant = variant.and_then(MeterVariant::new);
    let created = value["created"]
        .as_u64()
        .unwrap_or_else(|| chrono::Utc::now().timestamp().max(0) as u64);
    let result = format
        .and_then(|format| images(value, request, format))
        .map(|images| ImageResponse {
            created,
            images,
            usage,
        });
    evidence::preserve(result, || Some(Ok(usage)))
}

/// Local mock upstream shared by the image adapter contract tests.
#[cfg(test)]
pub(crate) mod mock {
    use axum::{
        Router,
        body::{Body, Bytes},
        http::{HeaderMap, Response, Uri, header},
    };
    use serde_json::Value;
    use std::{
        convert::Infallible,
        sync::{Arc, Mutex},
    };

    pub struct Captured {
        pub headers: HeaderMap,
        pub path: String,
        pub body: Value,
    }
    pub struct Mock {
        pub base: String,
        pub requests: Arc<Mutex<Vec<Captured>>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Mock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    /// Notifies when the response body (and so the connection) is dropped.
    struct DropSignal(Arc<tokio::sync::Notify>);
    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }
    impl Mock {
        /// `prefix` is the API base path (`/v1` or `/api/v1`).
        pub async fn serve(prefix: &str, status: u16, body: Vec<u8>) -> Self {
            Self::start(prefix, status, Some(body), None).await
        }
        pub async fn json(prefix: &str, value: Value) -> Self {
            Self::serve(prefix, 200, serde_json::to_vec(&value).unwrap()).await
        }
        /// Sends one byte and then never completes; signals `dropped` when
        /// the client goes away.
        pub async fn pending(prefix: &str, dropped: Arc<tokio::sync::Notify>) -> Self {
            Self::start(prefix, 200, None, Some(dropped)).await
        }
        async fn start(
            prefix: &str,
            status: u16,
            body: Option<Vec<u8>>,
            dropped: Option<Arc<tokio::sync::Notify>>,
        ) -> Self {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let capture = requests.clone();
            let app = Router::new().fallback(move |headers: HeaderMap, uri: Uri, raw: Bytes| {
                let capture = capture.clone();
                let body = body.clone();
                let dropped = dropped.clone();
                async move {
                    capture.lock().unwrap().push(Captured {
                        headers,
                        path: uri.path().to_owned(),
                        body: serde_json::from_slice(&raw).unwrap_or(Value::Null),
                    });
                    let builder = Response::builder()
                        .status(status)
                        .header(header::CONTENT_TYPE, "application/json");
                    match (body, dropped) {
                        (Some(body), _) => builder.body(Body::from(body)).unwrap(),
                        (None, dropped) => {
                            let signal = dropped.map(DropSignal);
                            let stream = async_stream::stream! {
                                let _signal = signal;
                                yield Ok::<_, Infallible>(Bytes::from_static(b"{"));
                                std::future::pending::<()>().await;
                            };
                            builder.body(Body::from_stream(stream)).unwrap()
                        }
                    }
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}{prefix}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                base,
                requests,
                task,
            }
        }
        pub fn last(&self) -> (HeaderMap, String, Value) {
            let requests = self.requests.lock().unwrap();
            let c = requests.last().expect("no upstream request");
            (c.headers.clone(), c.path.clone(), c.body.clone())
        }
        pub fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::images::fixtures::*;
    use serde_json::json;

    fn request(n: u32) -> ImageRequest {
        ImageRequest {
            model: "m".into(),
            prompt: "p".into(),
            n,
            size: None,
            quality: None,
            seed: None,
            max_response_bytes: 1 << 20,
        }
    }
    #[test]
    fn strict_data_items() {
        let ok = json!({"created": 5, "data": [{"b64_json": PNG_1X1, "revised_prompt": "r", "media_type": "image/png"}]});
        let r = decode(&ok, &request(1), Usage::default(), Some("1k"), Ok(None)).unwrap();
        assert_eq!(r.created, 5);
        assert_eq!(r.images[0].media_type, ImageMediaType::Png);
        assert_eq!(r.images[0].revised_prompt.as_deref(), Some("r"));
        assert_eq!(r.usage.output_image_variant.unwrap().as_str(), "1k");
        assert_eq!(r.usage.meters, Some(meters(Some(1))));
        assert!(r.valid_for(&request(1)));
        let bad = |v: Value, f: Option<ImageMediaType>| {
            decode(&v, &request(1), Usage::default(), None, Ok(f)).err()
        };
        let invalid = Some(InferenceError::InvalidUpstream);
        assert_eq!(
            bad(json!({"data": [{"url": "https://x/y.png"}]}), None),
            invalid
        );
        assert_eq!(
            bad(
                json!({"data": [{"b64_json": PNG_1X1, "url": "https://x"}]}),
                None
            ),
            invalid
        );
        assert_eq!(
            bad(json!({"data": [{"b64_json": "not base64!"}]}), None),
            invalid
        );
        assert_eq!(
            bad(json!({"data": [{"b64_json": "aGVsbG8gd29ybGQh"}]}), None),
            invalid
        );
        assert_eq!(
            bad(
                json!({"data": [{"b64_json": PNG_1X1, "media_type": "image/jpeg"}]}),
                None
            ),
            invalid
        );
        assert_eq!(
            bad(
                json!({"data": [{"b64_json": PNG_1X1}]}),
                Some(ImageMediaType::Jpeg)
            ),
            invalid
        );
        assert_eq!(
            bad(
                json!({"data": [{"b64_json": PNG_1X1}, {"b64_json": JPEG_1X1}]}),
                None
            ),
            invalid
        );
        assert_eq!(bad(json!({"data": []}), None), invalid);
        assert_eq!(
            bad(
                json!({"data": [{"b64_json": PNG_1X1, "extra": {"a": 1}}]}),
                None
            ),
            invalid
        );
        assert_eq!(bad(json!({"images": []}), None), invalid);
        // Known metadata and empty unknowns are tolerated.
        assert!(
            decode(
                &json!({"data": [{"b64_json": JPEG_1X1, "generation_id": "g", "extra": null}]}),
                &request(1),
                Usage::default(),
                None,
                Ok(Some(ImageMediaType::Jpeg)),
            )
            .is_ok()
        );
    }
    #[tokio::test]
    async fn invalid_bodies_keep_count_and_token_evidence() {
        let usage = Usage {
            input_tokens: Some(9),
            output_tokens: Some(272),
            ..Default::default()
        };
        let (out, seen) = evidence::capture(async {
            decode(
                &json!({"data": [{"b64_json": PNG_1X1}, {"b64_json": PNG_1X1}]}),
                &request(1),
                usage,
                None,
                Ok(None),
            )
        })
        .await;
        assert!(out.is_err());
        let seen = seen.unwrap();
        assert_eq!(seen.output_tokens, Some(272));
        assert_eq!(seen.meters.unwrap().output_images, Some(2));
        let (_, seen) = evidence::capture(async {
            decode(
                &json!({"oops": 1}),
                &request(1),
                Usage::default(),
                None,
                Ok(None),
            )
        })
        .await;
        assert_eq!(seen.unwrap().meters.unwrap().output_images, None);
    }
}

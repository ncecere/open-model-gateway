//! `POST /v1/images/generations` (OpenAI-compatible subset):
//! `{model, prompt, n?, size?, quality?, response_format?:"b64_json", seed?}` →
//! `{created, data:[{b64_json, revised_prompt?}], usage?}`.
//!
//! Only base64 responses exist: `response_format:"url"` is an explicit
//! `unsupported_capability` because the gateway never hosts files. Unknown
//! fields (`output_format`, `background`, `style`, `user`, ...) are rejected.
//! Prompts and images are never logged.
use crate::{
    auth::Principal,
    http::RequestId,
    inference::{
        Engine,
        error::InferenceError,
        images::{ImageQuality, ImageRequest, ImageResponse, ImageSize},
    },
};
use axum::{
    Extension, Json,
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    model: String,
    prompt: String,
    n: Option<u32>,
    size: Option<String>,
    quality: Option<String>,
    response_format: Option<String>,
    seed: Option<u64>,
}
impl Request {
    fn normalize(self, max_response_bytes: usize) -> Result<ImageRequest, InferenceError> {
        match self.response_format.as_deref() {
            None | Some("b64_json") => {}
            Some("url") => return Err(InferenceError::Unsupported),
            Some(_) => return Err(InferenceError::InvalidRequest),
        }
        let size = self
            .size
            .as_deref()
            .map(|s| ImageSize::parse(s).ok_or(InferenceError::InvalidRequest))
            .transpose()?;
        let quality = self
            .quality
            .as_deref()
            .map(|q| ImageQuality::parse(q).ok_or(InferenceError::InvalidRequest))
            .transpose()?;
        let request = ImageRequest {
            model: self.model,
            prompt: self.prompt,
            n: self.n.unwrap_or(1),
            size,
            quality,
            seed: self.seed,
            max_response_bytes,
        };
        request.validate()?;
        Ok(request)
    }
}

fn error(e: InferenceError) -> Response {
    super::workload_error(e, StatusCode::BAD_REQUEST)
}

pub async fn handle(
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    input: Result<Json<Request>, JsonRejection>,
) -> Response {
    let cap = engine.limits().workloads.images_response_bytes;
    let request = match input {
        Ok(Json(wire)) => match wire.normalize(cap) {
            Ok(request) => request,
            Err(e) => return error(e),
        },
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return super::payload_too_large(),
        Err(_) => return error(InferenceError::InvalidRequest),
    };
    match engine
        .execute_workload(principal, request, request_id.0)
        .await
    {
        Ok(response) => Json(render(response)).into_response(),
        Err(e) => error(e),
    }
}

fn render(response: ImageResponse) -> Value {
    let data: Vec<Value> = response
        .images
        .into_iter()
        .map(|i| {
            let mut v = json!({"b64_json": i.b64_json});
            if let Some(p) = i.revised_prompt {
                v["revised_prompt"] = json!(p);
            }
            v
        })
        .collect();
    let mut body = json!({"created": response.created, "data": data});
    // Observed counters only; unknown is omitted, never a fabricated zero.
    let u = response.usage;
    let mut usage = serde_json::Map::new();
    if let Some(n) = u.input_tokens {
        usage.insert("input_tokens".into(), n.into());
    }
    if let Some(n) = u.output_tokens {
        usage.insert("output_tokens".into(), n.into());
    }
    if let (Some(i), Some(o)) = (u.input_tokens, u.output_tokens)
        && let Some(total) = i.checked_add(o)
    {
        usage.insert("total_tokens".into(), total.into());
    }
    if !usage.is_empty() {
        body["usage"] = Value::Object(usage);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::{
        images::{GeneratedImage, ImageMediaType, ImageTier, fixtures::PNG_1X1},
        types::Usage,
    };
    fn parse(value: Value) -> Result<ImageRequest, InferenceError> {
        serde_json::from_value::<Request>(value)
            .map_err(|_| InferenceError::InvalidRequest)?
            .normalize(1 << 20)
    }
    #[test]
    fn strict_request_shape_and_bounds() {
        let r = parse(json!({"model":"m","prompt":"a red dot"})).unwrap();
        assert_eq!((r.n, r.size, r.quality, r.seed), (1, None, None, None));
        assert_eq!(r.max_response_bytes, 1 << 20);
        let r = parse(json!({"model":"m","prompt":"p","n":4,"size":"1.5k","quality":"low","response_format":"b64_json","seed":7})).unwrap();
        assert_eq!(r.size, Some(ImageSize::Tier(ImageTier::K1_5)));
        assert_eq!(r.quality, Some(ImageQuality::Low));
        assert_eq!(r.seed, Some(7));
        assert_eq!(
            parse(json!({"model":"m","prompt":"p","response_format":"url"})).err(),
            Some(InferenceError::Unsupported)
        );
        for bad in [
            json!({"model":"m","prompt":"p","n":0}),
            json!({"model":"m","prompt":"p","n":5}),
            json!({"model":"m","prompt":"p","n":-1}),
            json!({"model":"m","prompt":""}),
            json!({"model":"m","prompt":"x".repeat(32_001)}),
            json!({"model":"m","prompt":"p","size":"huge"}),
            json!({"model":"m","prompt":"p","quality":"ultra"}),
            json!({"model":"m","prompt":"p","response_format":"png"}),
            json!({"model":"m","prompt":"p","seed":-1}),
            json!({"model":"m","prompt":"p","output_format":"jpeg"}),
            json!({"model":"m","prompt":"p","user":"u"}),
            json!({"model":"m","prompt":"p","provider":{"only":["x"]}}),
            json!({"prompt":"p"}),
        ] {
            assert_eq!(
                parse(bad.clone()).err(),
                Some(InferenceError::InvalidRequest),
                "{bad}"
            );
        }
    }
    #[test]
    fn renders_base64_only_and_never_fakes_usage() {
        let image = || GeneratedImage {
            b64_json: PNG_1X1.into(),
            media_type: ImageMediaType::Png,
            revised_prompt: None,
        };
        let v = render(ImageResponse {
            created: 9,
            images: vec![image()],
            usage: Usage::default(),
        });
        assert_eq!(v, json!({"created":9,"data":[{"b64_json":PNG_1X1}]}));
        let mut revised = image();
        revised.revised_prompt = Some("a red dot, centered".into());
        let v = render(ImageResponse {
            created: 9,
            images: vec![revised],
            usage: Usage {
                input_tokens: Some(9),
                output_tokens: Some(272),
                ..Default::default()
            },
        });
        assert_eq!(v["data"][0]["revised_prompt"], "a red dot, centered");
        assert_eq!(
            v["usage"],
            json!({"input_tokens":9,"output_tokens":272,"total_tokens":281})
        );
        assert!(v["data"][0].get("url").is_none());
    }
}

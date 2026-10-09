//! String-only, non-streaming /v1/embeddings frontend. No arbitrary passthrough.
use crate::{
    auth::Principal,
    http::RequestId,
    inference::{
        Engine,
        error::InferenceError,
        types::{EmbeddingRequest, EmbeddingResponse},
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
#[serde(untagged)]
enum Input {
    Text(String),
    Batch(Vec<String>),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    model: String,
    input: Input,
    encoding_format: Option<String>,
    dimensions: Option<u32>,
}
impl Request {
    fn normalize(self) -> Result<EmbeddingRequest, InferenceError> {
        if self
            .encoding_format
            .as_deref()
            .is_some_and(|s| s != "float")
        {
            return Err(InferenceError::InvalidRequest);
        }
        let input = match self.input {
            Input::Text(s) => vec![s],
            Input::Batch(v) => v,
        };
        let request = EmbeddingRequest {
            model: self.model,
            input,
            dimensions: self.dimensions,
        };
        crate::providers::embeddings::validate(&request)?;
        Ok(request)
    }
}
pub async fn handle(
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    input: Result<Json<Request>, JsonRejection>,
) -> Response {
    let request=match input {
        Ok(Json(wire))=>match wire.normalize(){Ok(request)=>request,Err(e)=>return super::chat_completions::error_response(e)},
        Err(e) if e.status()==StatusCode::PAYLOAD_TOO_LARGE=>return (StatusCode::PAYLOAD_TOO_LARGE,Json(json!({"error":{"message":"Invalid inference request","type":"invalid_request","code":"invalid_request","param":null}}))).into_response(),
        Err(_)=>return super::chat_completions::error_response(InferenceError::InvalidRequest),
    };
    let model = request.model.clone();
    match engine
        .execute_embeddings(principal, request, request_id.0)
        .await
    {
        Ok(response) => match render(response, &model) {
            Ok(value) => Json(value).into_response(),
            Err(e) => super::chat_completions::error_response(e),
        },
        Err(e) => super::chat_completions::error_response(e),
    }
}
/// `/v1/batches` line body (`crate::jobs::lines`).
pub(crate) fn batch_request(body: Value) -> Result<EmbeddingRequest, InferenceError> {
    serde_json::from_value::<Request>(body)
        .map_err(|_| InferenceError::InvalidRequest)?
        .normalize()
}
/// `/v1/batches` line result body.
pub(crate) fn batch_response(
    response: EmbeddingResponse,
    model: &str,
) -> Result<Value, InferenceError> {
    render(response, model)
}
fn render(response: EmbeddingResponse, model: &str) -> Result<Value, InferenceError> {
    let data: Vec<_> = response
        .embeddings
        .into_iter()
        .enumerate()
        .map(|(index, embedding)| json!({"object":"embedding","index":index,"embedding":embedding}))
        .collect();
    let mut value = json!({"object":"list","model":model,"data":data});
    if let Some(n) = response.usage.input_tokens {
        value["usage"] = json!({"prompt_tokens":n,"total_tokens":n});
    }
    if serde_json::to_vec(&value)
        .map_err(|_| InferenceError::InvalidUpstream)?
        .len()
        > 4 * 1024 * 1024
    {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(value)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_frontend_validation_and_bounds() {
        for value in [
            json!({"model":"m","input":[1,2]}),
            json!({"model":"m","input":[]}),
            json!({"model":"m","input":""}),
            json!({"model":"m","input":"x","encoding_format":"base64"}),
            json!({"model":"m","input":"x","dimensions":0}),
            json!({"model":"m","input":"x","dimensions":16385}),
            json!({"model":"m","input":"x","user":"private"}),
            json!({"model":"m","input":"x","stream":true}),
        ] {
            let r = serde_json::from_value::<Request>(value)
                .map_err(|_| InferenceError::InvalidRequest)
                .and_then(Request::normalize);
            assert!(r.is_err());
        }
        let r = serde_json::from_value::<Request>(
            json!({"model":"m","input":["a","b"],"dimensions":2,"encoding_format":"float"}),
        )
        .unwrap()
        .normalize()
        .unwrap();
        assert_eq!(r.input.len(), 2);
        let r = serde_json::from_value::<Request>(json!({"model":"m","input":vec!["a";129]}))
            .unwrap()
            .normalize();
        assert!(r.is_err());
    }
    #[test]
    fn numeric_usage_or_omission_never_fake_zero() {
        let response = EmbeddingResponse {
            embeddings: vec![vec![1., 2.]],
            usage: crate::inference::types::Usage::default(),
        };
        let value = render(response, "public").unwrap();
        assert!(value.get("usage").is_none());
        assert_eq!(value["model"], "public");
        let response = EmbeddingResponse {
            embeddings: vec![vec![1., 2.]],
            usage: crate::inference::types::Usage {
                input_tokens: Some(2),
                output_tokens: Some(0),
                billing: None,
                ..Default::default()
            },
        };
        let value = render(response, "public").unwrap();
        assert_eq!(value["usage"]["prompt_tokens"], 2);
        assert!(value["usage"].get("completion_tokens").is_none());
    }
}

//! String-only, non-streaming /v1/embeddings frontend. No arbitrary passthrough.
use crate::{
    auth::Principal,
    http::RequestId,
    inference::{
        Engine,
        error::InferenceError,
        types::{ApiProtocol, Deployment, EmbeddingRequest, EmbeddingResponse, Usage},
        workload::{Workload, WorkloadAdmission},
    },
    providers::ProviderAdapter,
};
use async_trait::async_trait;
use axum::{
    Extension, Json,
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
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
    encoding_format: Option<Encoding>,
    dimensions: Option<u32>,
}
#[derive(Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Encoding {
    #[default]
    Float,
    Base64,
}

// Client-only context: adapters still receive the normalized float contract.
#[derive(Clone)]
struct ClientRequest {
    request: EmbeddingRequest,
    encoding: Encoding,
}
struct PreparedResponse {
    body: Option<Value>,
    usage: Usage,
}
#[async_trait]
impl Workload for ClientRequest {
    type Response = PreparedResponse;
    const PROTOCOL: ApiProtocol = ApiProtocol::Embeddings;
    fn model(&self) -> &str {
        &self.request.model
    }
    fn validate(&self) -> Result<(), InferenceError> {
        self.request.validate()
    }
    fn admission(&self) -> WorkloadAdmission {
        self.request.admission()
    }
    fn supported_by(&self, adapter: &dyn ProviderAdapter, target: &Deployment) -> bool {
        self.request.supported_by(adapter, target)
    }
    async fn dispatch(
        self,
        adapter: &dyn ProviderAdapter,
        target: &Deployment,
    ) -> Result<PreparedResponse, InferenceError> {
        let response = self.request.clone().dispatch(adapter, target).await?;
        let usage = response.usage;
        // Validate and format before the engine records success. Keep usage even
        // when the local representation exceeds its cap or vectors are invalid.
        let body = if self.request.valid_response(&response) {
            render(response, &self.request.model, self.encoding).ok()
        } else {
            None
        };
        Ok(PreparedResponse { body, usage })
    }
    fn usage(response: &PreparedResponse) -> Usage {
        response.usage
    }
    fn valid_response(&self, response: &PreparedResponse) -> bool {
        response.body.is_some()
    }
}
impl Request {
    fn normalize(self) -> Result<ClientRequest, InferenceError> {
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
        Ok(ClientRequest {
            request,
            encoding: self.encoding_format.unwrap_or_default(),
        })
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
    match engine
        .execute_workload(principal, request, request_id.0)
        .await
    {
        Ok(PreparedResponse {
            body: Some(value), ..
        }) => Json(value).into_response(),
        Ok(_) => super::chat_completions::error_response(InferenceError::InvalidUpstream),
        Err(e) => super::chat_completions::error_response(e),
    }
}
/// `/v1/batches` line body (`crate::jobs::lines`).
pub(crate) fn batch_request(body: Value) -> Result<EmbeddingRequest, InferenceError> {
    let client = serde_json::from_value::<Request>(body)
        .map_err(|_| InferenceError::InvalidRequest)?
        .normalize()?;
    // Batch lines currently persist only normalized requests, not client encoding.
    // Preserve their explicit refusal rather than silently returning float arrays.
    if client.encoding != Encoding::Float {
        return Err(InferenceError::Unsupported);
    }
    Ok(client.request)
}
/// `/v1/batches` line result body.
pub(crate) fn batch_response(
    response: EmbeddingResponse,
    model: &str,
) -> Result<Value, InferenceError> {
    render(response, model, Encoding::Float)
}
fn render(
    response: EmbeddingResponse,
    model: &str,
    encoding: Encoding,
) -> Result<Value, InferenceError> {
    let data: Vec<_> = response
        .embeddings
        .into_iter()
        .enumerate()
        .map(|(index, embedding)| {
            let embedding = match encoding {
                Encoding::Float => json!(embedding),
                Encoding::Base64 => {
                    let bytes: Vec<_> = embedding.into_iter().flat_map(f32::to_le_bytes).collect();
                    Value::String(STANDARD.encode(bytes))
                }
            };
            json!({"object":"embedding","index":index,"embedding":embedding})
        })
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
#[cfg(all(test, feature = "integration-tests"))]
#[path = "embedding_contract_tests.rs"]
mod contract_tests;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_frontend_validation_and_bounds() {
        for value in [
            json!({"model":"m","input":[1,2]}),
            json!({"model":"m","input":[]}),
            json!({"model":"m","input":""}),
            json!({"model":"m","input":"x","encoding_format":"unknown"}),
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
        assert_eq!(r.request.input.len(), 2);
        let r = serde_json::from_value::<Request>(json!({"model":"m","input":vec!["a";129]}))
            .unwrap()
            .normalize();
        assert!(r.is_err());
    }
    #[test]
    fn encodings_are_typed_client_context_and_batches_never_drop_base64() {
        for body in [
            json!({"model":"m","input":"x"}),
            json!({"model":"m","input":"x","encoding_format":null}),
            json!({"model":"m","input":"x","encoding_format":"float"}),
        ] {
            let client = serde_json::from_value::<Request>(body.clone())
                .unwrap()
                .normalize()
                .unwrap();
            assert!(client.encoding == Encoding::Float);
            assert!(batch_request(body).is_ok());
        }
        let body = json!({"model":"m","input":"x","encoding_format":"base64"});
        assert!(
            serde_json::from_value::<Request>(body.clone())
                .unwrap()
                .normalize()
                .unwrap()
                .encoding
                == Encoding::Base64
        );
        assert!(matches!(
            batch_request(body),
            Err(InferenceError::Unsupported)
        ));
    }
    #[test]
    fn base64_is_padded_little_endian_float32_including_sign_and_extremes() {
        let vector = vec![
            1.0,
            -2.0,
            -0.0,
            f32::MAX,
            f32::MIN,
            f32::MIN_POSITIVE,
            f32::from_bits(1),
        ];
        let expected: Vec<_> = vector.iter().flat_map(|n| n.to_le_bytes()).collect();
        let body = render(
            EmbeddingResponse {
                embeddings: vec![vector],
                usage: Usage::default(),
            },
            "public",
            Encoding::Base64,
        )
        .unwrap();
        let text = body["data"][0]["embedding"].as_str().unwrap();
        assert!(text.ends_with("=="));
        assert_eq!(STANDARD.decode(text).unwrap(), expected);
        assert!(body.get("usage").is_none());
    }
    struct NonfiniteAdapter(f32);
    #[async_trait]
    impl ProviderAdapter for NonfiniteAdapter {
        fn id(&self) -> &'static str {
            "nonfinite_fixture"
        }
        fn capabilities(&self) -> crate::inference::types::Capabilities {
            crate::inference::types::Capabilities {
                text_chat: false,
                streaming: false,
                tools: false,
            }
        }
        async fn execute(
            &self,
            _: &Deployment,
            _: crate::inference::types::ChatRequest,
        ) -> Result<crate::inference::types::ProviderOutput, InferenceError> {
            Err(InferenceError::Unsupported)
        }
        async fn execute_embeddings(
            &self,
            _: &Deployment,
            _: EmbeddingRequest,
        ) -> Result<EmbeddingResponse, InferenceError> {
            Ok(EmbeddingResponse {
                embeddings: vec![vec![self.0]],
                usage: Usage {
                    input_tokens: Some(7),
                    output_tokens: Some(0),
                    ..Usage::default()
                },
            })
        }
    }
    #[tokio::test]
    async fn invalid_normalized_numbers_cannot_be_formatted_but_keep_usage() {
        let target = Deployment {
            id: uuid::Uuid::new_v4(),
            provider: "nonfinite_fixture".into(),
            upstream_model: "private".into(),
            credential_ref: "none".into(),
            endpoint: None,
            region: None,
            supported_protocols: vec!["embeddings".into()],
        };
        for n in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for encoding in ["float", "base64"] {
                let request = serde_json::from_value::<Request>(
                    json!({"model":"m","input":"x","encoding_format":encoding}),
                )
                .unwrap()
                .normalize()
                .unwrap();
                let response = request
                    .clone()
                    .dispatch(&NonfiniteAdapter(n), &target)
                    .await
                    .unwrap();
                assert!(!request.valid_response(&response));
                assert!(response.body.is_none());
                assert_eq!(response.usage.input_tokens, Some(7));
                assert_eq!(response.usage.output_tokens, Some(0));
            }
        }
    }
    #[test]
    fn numeric_usage_or_omission_never_fake_zero() {
        let response = EmbeddingResponse {
            embeddings: vec![vec![1., 2.]],
            usage: crate::inference::types::Usage::default(),
        };
        let value = render(response, "public", Encoding::Float).unwrap();
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
        let value = render(response, "public", Encoding::Float).unwrap();
        assert_eq!(value["usage"]["prompt_tokens"], 2);
        assert!(value["usage"].get("completion_tokens").is_none());
    }
}

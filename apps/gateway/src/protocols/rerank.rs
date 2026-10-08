//! `POST /v1/rerank` (Cohere v2 / Jina-style subset):
//! `{model, query, documents:[string], top_n?}` →
//! `{object:"list", id, model, results:[{index, relevance_score}], usage}`.
//! Documents are never echoed back; unknown fields are rejected.
use crate::{
    auth::Principal,
    http::RequestId,
    inference::{
        Engine,
        error::InferenceError,
        types::{RerankRequest, RerankResponse},
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
    query: String,
    documents: Vec<String>,
    top_n: Option<u32>,
}
impl Request {
    fn normalize(self) -> Result<RerankRequest, InferenceError> {
        let request = RerankRequest {
            model: self.model,
            query: self.query,
            documents: self.documents,
            top_n: self.top_n,
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
    let request = match input {
        Ok(Json(wire)) => match wire.normalize() {
            Ok(request) => request,
            Err(e) => return error(e),
        },
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return super::payload_too_large(),
        Err(_) => return error(InferenceError::InvalidRequest),
    };
    let model = request.model.clone();
    match engine
        .execute_workload(principal, request, request_id.0)
        .await
    {
        Ok(response) => Json(render(response, &model, request_id.0)).into_response(),
        Err(e) => error(e),
    }
}

fn render(response: RerankResponse, model: &str, id: uuid::Uuid) -> Value {
    let results: Vec<Value> = response
        .results
        .iter()
        .map(|r| json!({"index": r.index, "relevance_score": r.relevance_score}))
        .collect();
    // Observed counters only; unknown is omitted, never a fabricated zero.
    let mut usage = json!({});
    if let Some(n) = response.usage.input_tokens {
        usage["total_tokens"] = n.into();
    }
    if let Some(n) = response.usage.meters.and_then(|m| m.search_units) {
        usage["search_units"] = n.into();
    }
    json!({"object":"list","id":id.to_string(),"model":model,"results":results,"usage":usage})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::types::{RerankResult, Usage};
    fn parse(value: Value) -> Result<RerankRequest, InferenceError> {
        serde_json::from_value::<Request>(value)
            .map_err(|_| InferenceError::InvalidRequest)?
            .normalize()
    }
    #[test]
    fn strict_request_shape_and_bounds() {
        let r = parse(json!({"model":"m","query":"cat","documents":["a","b"],"top_n":1})).unwrap();
        assert_eq!((r.documents.len(), r.top_n), (2, Some(1)));
        for bad in [
            json!({"model":"m","query":"cat","documents":[]}),
            json!({"model":"m","query":"cat","documents":[{"text":"a"}]}),
            json!({"model":"m","query":"cat","documents":["a"],"top_n":0}),
            json!({"model":"m","query":"cat","documents":["a"],"return_documents":true}),
            json!({"model":"m","query":"cat","documents":["a"],"provider":{"only":["x"]}}),
            json!({"model":"m","query":"","documents":["a"]}),
            json!({"model":"","query":"q","documents":["a"]}),
            json!({"model":"m","query":"q","documents":vec!["a"; 1001]}),
        ] {
            assert_eq!(
                parse(bad.clone()).err(),
                Some(InferenceError::InvalidRequest),
                "{bad}"
            );
        }
    }
    #[test]
    fn renders_scores_without_documents_and_never_fakes_usage() {
        let id = uuid::Uuid::new_v4();
        let mut response = RerankResponse {
            results: vec![RerankResult {
                index: 1,
                relevance_score: 0.5,
            }],
            usage: Usage::default(),
        };
        let v = render(response, "public", id);
        assert_eq!(v["results"], json!([{"index":1,"relevance_score":0.5}]));
        assert_eq!(v["usage"], json!({}));
        assert_eq!(v["object"], "list");
        response = RerankResponse {
            results: vec![],
            usage: crate::providers::metering::input_only(Some(25)),
        };
        response.usage.meters = Some(crate::providers::metering::text_workload_meters(Some(1)));
        let v = render(response, "public", id);
        assert_eq!(v["usage"], json!({"total_tokens":25,"search_units":1}));
    }
}

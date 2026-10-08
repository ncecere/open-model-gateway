//! `POST /v1/systemone`: the TypeSafe System One contract, so the TypeSafe SDK
//! works unchanged with `base_url = https://<gateway>`. Request
//! `{model, state, questions}` (no client `provider`/routing fields); response
//! `{id, model, answers, usage:{input_tokens, output_tokens}}` with numeric
//! answer fields. Validation failures are 422 like TypeSafe. Gateway billing
//! stays in the financial APIs, never in this body.
use std::collections::BTreeMap;

use crate::{
    auth::Principal,
    http::RequestId,
    inference::{
        Engine,
        error::InferenceError,
        types::{Question, QuestionKind, SystemoneRequest, SystemoneResponse},
    },
};
use axum::{
    Extension, Json,
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireQuestion {
    #[serde(rename = "type")]
    kind: QuestionKind,
    instructions: Value,
    #[serde(default)]
    criteria: Option<Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    model: String,
    state: Value,
    questions: BTreeMap<String, WireQuestion>,
}
impl Request {
    fn normalize(self) -> Result<SystemoneRequest, InferenceError> {
        let request = SystemoneRequest {
            model: self.model,
            state: self.state,
            questions: self
                .questions
                .into_iter()
                .map(|(k, q)| {
                    (
                        k,
                        Question {
                            kind: q.kind,
                            instructions: q.instructions,
                            criteria: q.criteria,
                        },
                    )
                })
                .collect(),
        };
        request.validate()?;
        Ok(request)
    }
}

fn error(e: InferenceError) -> Response {
    super::workload_error(e, StatusCode::UNPROCESSABLE_ENTITY)
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

fn render(response: SystemoneResponse, model: &str, id: uuid::Uuid) -> Value {
    let answers: Map<String, Value> = response
        .answers
        .iter()
        .map(|(k, a)| (k.clone(), a.to_json()))
        .collect();
    // The engine only returns responses with both counters observed.
    json!({
        "id": id.to_string(),
        "model": model,
        "answers": answers,
        "usage": {
            "input_tokens": response.usage.input_tokens,
            "output_tokens": response.usage.output_tokens,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::types::{Answer, Usage};
    fn parse(value: Value) -> Result<SystemoneRequest, InferenceError> {
        serde_json::from_value::<Request>(value)
            .map_err(|_| InferenceError::InvalidRequest)?
            .normalize()
    }
    #[test]
    fn typesafe_request_shape_is_strict() {
        let ok = json!({"model":"jev-latest","state":"hello there","questions":{
            "is_q":{"type":"noul","instructions":"Is the text a greeting?"},
            "lang":{"type":"choice","instructions":"Language?","criteria":{"en":"English","fr":"French"}},
            "tone":{"type":"score","instructions":"Tone","criteria":["casual","formal"]}}});
        let r = parse(ok.clone()).unwrap();
        assert_eq!(r.questions.len(), 3);
        for (field, value) in [
            ("provider", json!({"only":["x"]})),
            ("user", json!("someone")),
            ("stream", json!(true)),
        ] {
            let mut bad = ok.clone();
            bad[field] = value;
            assert_eq!(
                parse(bad).err(),
                Some(InferenceError::InvalidRequest),
                "{field}"
            );
        }
        let mut bad = ok.clone();
        bad["questions"]["is_q"]["type"] = json!("bool");
        assert!(parse(bad).is_err());
        let mut bad = ok.clone();
        bad["questions"]["is_q"]["extra"] = json!(1);
        assert!(parse(bad).is_err());
        let mut images = ok;
        images["state"] =
            json!([{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]);
        assert_eq!(parse(images).err(), Some(InferenceError::Unsupported));
    }
    #[test]
    fn renders_typesafe_numeric_shape() {
        let id = uuid::Uuid::new_v4();
        let v = render(
            SystemoneResponse {
                answers: BTreeMap::from([("is_q".to_owned(), Answer::Noul { noul: 0.9566 })]),
                usage: Usage {
                    input_tokens: Some(145),
                    output_tokens: Some(0),
                    ..Default::default()
                },
            },
            "company/clef",
            id,
        );
        assert_eq!(
            v,
            json!({"id":id.to_string(),"model":"company/clef","answers":{"is_q":{"type":"noul","noul":0.9566}},"usage":{"input_tokens":145,"output_tokens":0}})
        );
    }
    #[test]
    fn validation_is_422_and_unsupported_is_400() {
        assert_eq!(
            error(InferenceError::InvalidRequest).status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            error(InferenceError::Unsupported).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            error(InferenceError::ModelUnavailable).status(),
            StatusCode::NOT_FOUND
        );
    }
}

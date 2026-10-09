pub mod audio;
pub mod batches;
pub mod chat_completions;
pub mod embeddings;
pub mod images;
pub mod messages;
pub mod realtime;
pub mod rerank;
pub mod responses;
pub mod systemone;
pub mod videos;

use crate::inference::error::InferenceError;
use axum::{
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};

/// Shared client-protocol error response. Budget/accounting denials keep HTTP 429
/// (OpenAI `insufficient_quota` convention) but are not transient, so they carry
/// `x-should-retry: false`, which the official OpenAI/Anthropic SDKs honor. A
/// token reservation that exceeds a tokens-per-minute limit is equally permanent.
fn error_with_body(error: InferenceError, body: serde_json::Value) -> Response {
    let mut response = (responses::status(error), axum::Json(body)).into_response();
    if error.is_non_retryable_denial() {
        response
            .headers_mut()
            .insert("x-should-retry", HeaderValue::from_static("false"));
    }
    response
}

fn http_status(error: InferenceError) -> StatusCode {
    match error {
        InferenceError::InvalidRequest | InferenceError::UpstreamRejected => {
            StatusCode::BAD_REQUEST
        }
        InferenceError::ModelUnavailable => StatusCode::NOT_FOUND,
        InferenceError::Unsupported => StatusCode::NOT_IMPLEMENTED,
        InferenceError::Busy
        | InferenceError::BudgetExceeded(_)
        | InferenceError::UnresolvedUsage(_)
        | InferenceError::TokenReservationExceedsLimit(_)
        | InferenceError::JobLimitExceeded(_) => StatusCode::TOO_MANY_REQUESTS,
        InferenceError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        InferenceError::InvalidUpstream | InferenceError::UpstreamUnavailable => {
            StatusCode::BAD_GATEWAY
        }
        InferenceError::Configuration | InferenceError::Storage => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Non-generation workload error: a model/protocol pair no deployment can
/// serve is an explicit client error (400 `unsupported_capability`), and
/// request validation uses `validation_status` (System One: 422).
fn workload_error(error: InferenceError, validation_status: StatusCode) -> Response {
    let mut response = error_with_body(error, openai_error_body(error));
    match error {
        InferenceError::Unsupported => *response.status_mut() = StatusCode::BAD_REQUEST,
        InferenceError::InvalidRequest => *response.status_mut() = validation_status,
        _ => {}
    }
    response
}
fn payload_too_large() -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        axum::Json(serde_json::json!({"error":{"message":"Request body too large","type":"invalid_request_error","code":"payload_too_large","param":null}})),
    )
        .into_response()
}

/// OpenAI-wire error envelope. Budget denials use `insufficient_quota` as the
/// type with a gateway-specific `code`; a token reservation above a
/// tokens-per-minute limit is a `rate_limit_error` with its own code; other
/// errors keep type == code.
fn openai_error_body(error: InferenceError) -> serde_json::Value {
    let kind = if error.is_budget_denial() {
        "insufficient_quota"
    } else if matches!(
        error,
        InferenceError::TokenReservationExceedsLimit(_) | InferenceError::JobLimitExceeded(_)
    ) {
        "rate_limit_error"
    } else {
        error.code()
    };
    serde_json::json!({"error":{"message":error.message(),"type":kind,"code":error.code(),"param":null}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::error::LimitScope;
    #[test]
    fn budget_denials_are_distinct_non_retryable_429s() {
        let budget = InferenceError::BudgetExceeded(LimitScope::Workspace);
        let response = error_with_body(budget, openai_error_body(budget));
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["x-should-retry"], "false");
        let body = openai_error_body(budget);
        assert_eq!(body["error"]["type"], "insufficient_quota");
        assert_eq!(body["error"]["code"], "budget_exceeded");
        let unresolved = openai_error_body(InferenceError::UnresolvedUsage(LimitScope::Workspace));
        assert_eq!(unresolved["error"]["code"], "unresolved_usage");
        assert!(
            unresolved["error"]["message"]
                .as_str()
                .unwrap()
                .contains("until reconciled")
        );
        let busy = error_with_body(
            InferenceError::Busy,
            openai_error_body(InferenceError::Busy),
        );
        assert_eq!(busy.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(busy.headers().get("x-should-retry").is_none());
        assert_eq!(
            openai_error_body(InferenceError::Busy)["error"]["type"],
            "rate_limit_error"
        );
        for scope in [
            LimitScope::ApiKey,
            LimitScope::Workspace,
            LimitScope::Installation,
        ] {
            let ceiling = InferenceError::TokenReservationExceedsLimit(scope);
            let response = error_with_body(ceiling, openai_error_body(ceiling));
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(response.headers()["x-should-retry"], "false");
            let body = openai_error_body(ceiling);
            assert_eq!(body["error"]["type"], "rate_limit_error");
            assert_eq!(body["error"]["code"], "token_reservation_exceeds_limit");
            let message = body["error"]["message"].as_str().unwrap();
            assert!(message.contains("tokens-per-minute limit"));
            assert!(message.contains("lower the price ceilings"));
            assert!(!message.contains("concurrency"));
            assert!(!message.chars().any(|c| c.is_ascii_digit()));
        }
    }
}

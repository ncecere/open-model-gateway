pub mod audio;
pub mod batches;
pub mod chat_completions;
pub mod embeddings;
pub mod files;
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
/// A model whose every route is cooling down is a retryable 503 with `Retry-After`.
fn error_with_body(error: InferenceError, body: serde_json::Value) -> Response {
    let mut response = (responses::status(error), axum::Json(body)).into_response();
    if error.is_non_retryable_denial() {
        response
            .headers_mut()
            .insert("x-should-retry", HeaderValue::from_static("false"));
    }
    if let Some(seconds) = error.retry_after_seconds() {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, HeaderValue::from(seconds));
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
        InferenceError::Configuration
        | InferenceError::PriceUnbounded
        | InferenceError::Storage
        | InferenceError::RouteCoolingDown(_) => StatusCode::SERVICE_UNAVAILABLE,
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

/// HTTP status and body a `/v1/batches` line reports for `error`: exactly what
/// the endpoint returns interactively (Anthropic shape for `/v1/messages`).
pub(crate) fn batch_line_error(anthropic: bool, error: InferenceError) -> (u16, serde_json::Value) {
    let body = if anthropic {
        messages::batch_error_body(error)
    } else {
        openai_error_body(error)
    };
    (responses::status(error).as_u16(), body)
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
    } else if matches!(error, InferenceError::RouteCoolingDown(_)) {
        // OpenAI's type for transient server-side unavailability (HTTP 5xx).
        "server_error"
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
        for scope in [LimitScope::ApiKey, LimitScope::Workspace] {
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

    #[test]
    fn cooling_down_is_a_retryable_503_not_model_not_found() {
        let error = InferenceError::RouteCoolingDown(17);
        let response = error_with_body(error, openai_error_body(error));
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "17");
        assert!(response.headers().get("x-should-retry").is_none());
        let body = openai_error_body(error);
        assert_eq!(body["error"]["type"], "server_error");
        assert_eq!(body["error"]["code"], "model_temporarily_unavailable");
        // Anthropic shape: same status/header, transient error type.
        let anthropic = messages::batch_error_body(error);
        assert_eq!(anthropic["error"]["type"], "overloaded_error");
        // Batch lines report the interactive status.
        assert_eq!(batch_line_error(false, error).0, 503);
        // Zero is never sent as Retry-After.
        let zero = error_with_body(
            InferenceError::RouteCoolingDown(0),
            openai_error_body(InferenceError::RouteCoolingDown(0)),
        );
        assert_eq!(zero.headers()["retry-after"], "1");
        // Other errors carry no Retry-After.
        let missing = error_with_body(
            InferenceError::ModelUnavailable,
            openai_error_body(InferenceError::ModelUnavailable),
        );
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert!(missing.headers().get("retry-after").is_none());
    }
}

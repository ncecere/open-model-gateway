//! A provider error that arrives inside a stream that was already returned
//! (after the HTTP 200) is classified as the same error would be as an HTTP
//! response: the adapter's own HTTP status table, applied to the status the
//! error's documented name stands for. Anthropic, OpenAI (chat,
//! Responses, the local profiles) and OpenRouter all go through [`classify`],
//! so the stream path and the response path cannot drift apart. Bedrock's
//! exception frames are named by a header instead (`bedrock/stream.rs`).
//!
//! Only documented names and numeric statuses are read. The message text is
//! never consulted here (the Anthropic `prompt is too long` refinement is the
//! HTTP path's own and runs inside its classifier), and an unrecognised name
//! is [`InferenceError::UpstreamUnavailable`], never a success and never a
//! client fault: a client-fault class does not count against routing health
//! and a rejection could settle as a free known zero, and an unknown failure
//! is evidence of neither.
use crate::inference::error::InferenceError;
use serde_json::Value;

/// The HTTP status a documented error name stands for, in the order the
/// providers document them.
///
/// - Anthropic, "Errors": `invalid_request_error` 400, `authentication_error`
///   401, `billing_error` 402, `permission_error` 403, `not_found_error` 404,
///   `request_too_large` 413, `rate_limit_error` 429, `api_error` 500,
///   `timeout_error` 504, `overloaded_error` 529.
/// - OpenAI, "Error codes" and the Responses `ResponseError.code` list:
///   `rate_limit_exceeded` and `insufficient_quota` 429, `server_error` 500,
///   `context_length_exceeded`, `invalid_prompt` and the `invalid_image*`
///   family 400, `invalid_api_key` 401, `model_not_found` 404.
/// - `prompt_too_long`, `too_many_tokens` and `overloaded` are the extra
///   names `framing::classified_error` already recognises on the HTTP path
///   (400, 400, 529).
fn documented_status(name: &str) -> Option<u16> {
    Some(match name {
        "invalid_request_error"
        | "context_length_exceeded"
        | "prompt_too_long"
        | "too_many_tokens"
        | "invalid_prompt"
        | "invalid_image"
        | "invalid_image_format"
        | "invalid_base64_image"
        | "invalid_image_url"
        | "image_too_large"
        | "image_too_small"
        | "image_parse_error"
        | "image_content_policy_violation"
        | "invalid_image_mode"
        | "image_file_too_large"
        | "unsupported_image_media_type"
        | "empty_image_file"
        | "failed_to_download_image"
        | "image_file_not_found" => 400,
        "authentication_error" | "invalid_api_key" => 401,
        "billing_error" => 402,
        "permission_error" => 403,
        "not_found_error" | "model_not_found" => 404,
        "request_too_large" => 413,
        "rate_limit_error" | "rate_limit_exceeded" | "insufficient_quota" => 429,
        "api_error" | "server_error" => 500,
        "timeout_error" => 504,
        "overloaded_error" | "overloaded" => 529,
        _ => return None,
    })
}

/// The status `error` (the provider's error object) stands for: its numeric
/// `code` (an HTTP status, as OpenRouter, vLLM and llama.cpp send it), else the
/// documented name in `code`, else in `type`. An explicit `code` wins even
/// when it is unknown or not a string, as in `framing::classified_error`: a
/// secondary `type` or quoted client text never reinterprets it.
fn equivalent_status(error: &Value) -> Option<u16> {
    let name = match error.get("code").filter(|v| !v.is_null()) {
        Some(Value::Number(code)) => return code.as_u64().and_then(|c| u16::try_from(c).ok()),
        Some(Value::String(code)) => code.as_str(),
        Some(_) => return None,
        None => error.get("type")?.as_str()?,
    };
    documented_status(name)
}

/// The class of the provider error object `error`: `as_http` is the adapter's
/// classification of an HTTP error status (its own status table, and for a
/// Messages client the refinements its response path adds), applied to the
/// status the error stands for; an unrecognised error is
/// `UpstreamUnavailable`.
pub(super) fn classify(
    error: &Value,
    as_http: impl FnOnce(u16) -> InferenceError,
) -> InferenceError {
    equivalent_status(error).map_or(InferenceError::UpstreamUnavailable, as_http)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_name_stands_for_the_status_its_provider_documents() {
        for (error, status) in [
            (json!({"type":"overloaded_error"}), Some(529)),
            (json!({"type":"rate_limit_error"}), Some(429)),
            (json!({"type":"api_error"}), Some(500)),
            (json!({"type":"invalid_request_error"}), Some(400)),
            (json!({"type":"authentication_error"}), Some(401)),
            (json!({"type":"permission_error"}), Some(403)),
            (json!({"type":"not_found_error"}), Some(404)),
            (json!({"code":"rate_limit_exceeded"}), Some(429)),
            (json!({"code":"server_error"}), Some(500)),
            (json!({"code":"invalid_prompt"}), Some(400)),
            (json!({"code":503}), Some(503)),
            (json!({"code":"tokens","type":"rate_limit_error"}), None),
            // An explicit code wins over a type, even a non-string one.
            (json!({"code":7.5,"type":"rate_limit_error"}), None),
            (json!({"code":true,"type":"rate_limit_error"}), None),
            (json!({"code":null,"type":"rate_limit_error"}), Some(429)),
            (json!({"code":-1}), None),
            (json!({"code":70000}), None),
            (json!({"type":"Rate_Limit_Error"}), None),
            (json!({"message":"rate_limit_error"}), None),
            (json!("rate_limit_error"), None),
            (json!(null), None),
        ] {
            assert_eq!(equivalent_status(&error), status, "{error}");
        }
    }

    #[test]
    fn an_unrecognised_error_is_an_unavailable_provider_not_a_client_fault() {
        for error in [
            json!({"type":"something_new"}),
            json!({"code":"surprise"}),
            json!({"message":"slow down"}),
            json!({}),
            json!("text"),
        ] {
            assert_eq!(
                classify(&error, |_| InferenceError::UpstreamRejected),
                InferenceError::UpstreamUnavailable,
                "{error}"
            );
        }
    }
}

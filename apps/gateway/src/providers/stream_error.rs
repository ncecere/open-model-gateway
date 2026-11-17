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
//! and an unknown failure is not evidence of one. Settlement is unchanged:
//! an error inside a returned stream always retains the hold.
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
pub(crate) const DOCUMENTED: &[(&str, u16)] = &[
    ("invalid_request_error", 400),
    ("context_length_exceeded", 400),
    ("prompt_too_long", 400),
    ("too_many_tokens", 400),
    ("invalid_prompt", 400),
    ("invalid_image", 400),
    ("invalid_image_format", 400),
    ("invalid_base64_image", 400),
    ("invalid_image_url", 400),
    ("image_too_large", 400),
    ("image_too_small", 400),
    ("image_parse_error", 400),
    ("image_content_policy_violation", 400),
    ("invalid_image_mode", 400),
    ("image_file_too_large", 400),
    ("unsupported_image_media_type", 400),
    ("empty_image_file", 400),
    ("failed_to_download_image", 400),
    ("image_file_not_found", 400),
    ("authentication_error", 401),
    ("invalid_api_key", 401),
    ("billing_error", 402),
    ("permission_error", 403),
    ("not_found_error", 404),
    ("model_not_found", 404),
    ("request_too_large", 413),
    ("rate_limit_error", 429),
    ("rate_limit_exceeded", 429),
    ("insufficient_quota", 429),
    ("api_error", 500),
    ("server_error", 500),
    ("timeout_error", 504),
    ("overloaded_error", 529),
    ("overloaded", 529),
];
fn documented_status(name: &str) -> Option<u16> {
    DOCUMENTED
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, status)| *status)
}

/// A numeric code is authoritative (invalid numbers and non-string values
/// fail closed). A string code is a three-ASCII-digit status or a documented
/// name; an empty or unknown string falls back to the documented type.
fn equivalent_status(error: &Value) -> Option<u16> {
    match error.get("code").filter(|v| !v.is_null()) {
        Some(Value::Number(code)) => return code.as_u64().and_then(|c| u16::try_from(c).ok()),
        Some(Value::String(code)) => {
            if code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit()) {
                return code.parse().ok();
            }
            if let Some(status) = documented_status(code) {
                return Some(status);
            }
        }
        Some(_) => return None,
        None => {}
    }
    documented_status(error.get("type")?.as_str()?)
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

/// A provider `error` in a stream frame is classified as
/// the same error is as an HTTP response: the status
/// its documented `code` or `type` stands for, then the response path's own
/// table (`check_status`, which is `framing::status`), and for a Messages
/// client `generation_error` as `generation_status` applies it.
pub(super) fn openai_chat(value: &Value, messages: bool) -> InferenceError {
    classify(&value["error"], |code| {
        if messages {
            super::framing::generation_error(code, Some(value), super::framing::classified_error)
        } else {
            super::framing::status_error(code)
        }
    })
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
            (
                json!({"code":"tokens","type":"rate_limit_error"}),
                Some(429),
            ),
            (json!({"code":"","type":"invalid_request_error"}), Some(400)),
            (json!({"code":"400","type":"rate_limit_error"}), Some(400)),
            (json!({"code":"429"}), Some(429)),
            (json!({"code":"000"}), Some(0)),
            (
                json!({"code":" 400","type":"invalid_request_error"}),
                Some(400),
            ),
            (json!({"code":"４００"}), None),
            (json!({"code":"40"}), None),
            (json!({"code":"0400"}), None),
            (json!({"code":{},"type":"rate_limit_error"}), None),
            (json!({"code":[],"type":"rate_limit_error"}), None),
            // An explicit code wins over a type, even a non-string one.
            (json!({"code":7.5,"type":"rate_limit_error"}), None),
            (json!({"code":true,"type":"rate_limit_error"}), None),
            (json!({"code":null,"type":"rate_limit_error"}), Some(429)),
            (json!({"code":-1,"type":"rate_limit_error"}), None),
            (json!({"code":70000,"type":"rate_limit_error"}), None),
            (json!({"type":"Rate_Limit_Error"}), None),
            (json!({"message":"rate_limit_error"}), None),
            (json!("rate_limit_error"), None),
            (json!(null), None),
        ] {
            assert_eq!(equivalent_status(&error), status, "{error}");
        }
    }

    use proptest::prelude::*;
    proptest! {
        #![proptest_config(crate::proptest_support_tests::config(256))]
        #[test]
        fn arbitrary_json_never_panics(error in crate::proptest_support_tests::arb_json()) {
            let class = classify(&error, super::super::framing::status_error);
            prop_assert!(matches!(class,
                InferenceError::InvalidUpstream | InferenceError::Configuration |
                InferenceError::Busy | InferenceError::Timeout |
                InferenceError::UpstreamRejected | InferenceError::UpstreamUnavailable));
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

//! Batch input and result lines (OpenAI Batch API shapes). Input lines are
//! `{custom_id, method:"POST", url, body}`; every body is validated with the
//! endpoint's normal inference contract. Results are
//! `{id, custom_id, response:{status_code, request_id, body}, error}`.
//! Nothing here logs or stores content; error messages are fixed strings that
//! never echo client or provider text.
use serde_json::{Value, json};
use uuid::Uuid;

use super::types::*;
use crate::inference::error::InferenceError;

/// Splits a byte stream into lines without holding more than one line.
pub(crate) struct Lines {
    pending: Vec<u8>,
    limit: usize,
}
impl Default for Lines {
    fn default() -> Self {
        Self::with_limit(BATCH_MAX_LINE_BYTES)
    }
}
/// A line longer than the reader's limit.
#[derive(Debug)]
pub(crate) struct LineTooLong;
impl Lines {
    pub(crate) fn with_limit(limit: usize) -> Self {
        Self {
            pending: Vec::new(),
            limit,
        }
    }
    /// Complete lines in `data` (without `\n`); the remainder stays pending.
    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>, LineTooLong> {
        let mut out = Vec::new();
        let mut rest = data;
        while let Some(pos) = rest.iter().position(|b| *b == b'\n') {
            self.pending.extend_from_slice(&rest[..pos]);
            if self.pending.len() > self.limit {
                return Err(LineTooLong);
            }
            out.push(std::mem::take(&mut self.pending));
            rest = &rest[pos + 1..];
        }
        self.pending.extend_from_slice(rest);
        if self.pending.len() > self.limit {
            return Err(LineTooLong);
        }
        Ok(out)
    }
    /// The final line without a trailing newline, if any.
    pub(crate) fn finish(&mut self) -> Option<Vec<u8>> {
        let last = std::mem::take(&mut self.pending);
        (!last.is_empty()).then_some(last)
    }
}
/// Blank lines (only whitespace) are skipped, never counted.
pub(crate) fn is_blank(line: &[u8]) -> bool {
    line.iter().all(u8::is_ascii_whitespace)
}

/// Why an input line is invalid: a stable code and a fixed message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineError {
    InvalidJson,
    InvalidShape,
    InvalidCustomId,
    DuplicateCustomId,
    InvalidMethod,
    WrongUrl,
    InvalidBody,
    UnsupportedFeature,
    Unbounded,
    ModelNotFound,
    ModelUnsupported,
    Unpriced,
    OutputLimit,
    LineTooLarge,
    TooManyLines,
    EmptyFile,
}
impl LineError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::InvalidShape => "invalid_line",
            Self::InvalidCustomId => "invalid_custom_id",
            Self::DuplicateCustomId => "duplicate_custom_id",
            Self::InvalidMethod => "invalid_method",
            Self::WrongUrl => "invalid_url",
            Self::InvalidBody => "invalid_body",
            Self::UnsupportedFeature => "unsupported_feature",
            Self::Unbounded => "missing_max_tokens",
            Self::ModelNotFound => "model_not_found",
            Self::ModelUnsupported => "model_unsupported",
            Self::Unpriced => "model_not_priced",
            Self::OutputLimit => "max_tokens_too_large",
            Self::LineTooLarge => "line_too_large",
            Self::TooManyLines => "too_many_lines",
            Self::EmptyFile => "empty_file",
        }
    }
    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidJson => "The line is not valid JSON.",
            Self::InvalidShape => {
                "Each line must be exactly {custom_id, method, url, body} with an object body."
            }
            Self::InvalidCustomId => "custom_id must be a non-empty string of at most 512 bytes.",
            Self::DuplicateCustomId => "custom_id must be unique within the file.",
            Self::InvalidMethod => "method must be POST.",
            Self::WrongUrl => "url must equal the batch endpoint.",
            Self::InvalidBody => "The request body is not valid for this endpoint.",
            Self::UnsupportedFeature => "The request uses a feature batches do not support.",
            Self::Unbounded => {
                "The request must set its output maximum (max_completion_tokens, max_output_tokens or max_tokens)."
            }
            Self::ModelNotFound => "The model is not available to this API key.",
            Self::ModelUnsupported => "The model does not serve this batch endpoint.",
            Self::Unpriced => {
                "The model has no price with ceilings, so the batch cannot be reserved for."
            }
            Self::OutputLimit => "The output maximum exceeds the model's output ceiling.",
            Self::LineTooLarge => "The line exceeds 4 MiB.",
            Self::TooManyLines => "A batch may contain at most 50,000 requests.",
            Self::EmptyFile => "The file contains no requests.",
        }
    }
}

/// A parsed input line. No `Debug`: it is content.
pub(crate) struct InputLine {
    pub custom_id: String,
    pub request: BatchRequest,
}

/// Parse and validate one input line against `endpoint`'s contract. Lines
/// must be bounded: generation lines need an explicit output maximum.
pub(crate) fn parse_line(raw: &[u8], endpoint: BatchEndpoint) -> Result<InputLine, LineError> {
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let Value::Object(mut top) =
        serde_json::from_slice::<Value>(raw).map_err(|_| LineError::InvalidJson)?
    else {
        return Err(LineError::InvalidShape);
    };
    if top.len() != 4
        || !top
            .keys()
            .all(|k| matches!(k.as_str(), "custom_id" | "method" | "url" | "body"))
        || !top["body"].is_object()
    {
        return Err(LineError::InvalidShape);
    }
    let custom_id = top["custom_id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 512)
        .ok_or(LineError::InvalidCustomId)?
        .to_owned();
    if top["method"] != "POST" {
        return Err(LineError::InvalidMethod);
    }
    if top["url"] != endpoint.as_str() {
        return Err(LineError::WrongUrl);
    }
    let body = top.remove("body").unwrap_or(Value::Null);
    let map = |e: InferenceError| match e {
        InferenceError::Unsupported => LineError::UnsupportedFeature,
        _ => LineError::InvalidBody,
    };
    let request = match endpoint {
        BatchEndpoint::ChatCompletions => BatchRequest::Chat(
            crate::protocols::chat_completions::batch_request(body).map_err(map)?,
        ),
        BatchEndpoint::Responses => {
            BatchRequest::Chat(crate::protocols::responses::batch_request(body).map_err(map)?)
        }
        BatchEndpoint::Messages => {
            BatchRequest::Chat(crate::protocols::messages::batch_request(body).map_err(map)?)
        }
        BatchEndpoint::Embeddings => BatchRequest::Embeddings(
            crate::protocols::embeddings::batch_request(body).map_err(map)?,
        ),
    };
    if let BatchRequest::Chat(r) = &request
        && r.max_output_tokens.is_none_or(|n| n == 0)
    {
        return Err(LineError::Unbounded);
    }
    Ok(InputLine { custom_id, request })
}

/// Only the `custom_id` of an already-validated line (result mapping).
pub(crate) fn custom_id(raw: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Id {
        custom_id: String,
    }
    serde_json::from_slice::<Id>(raw.strip_suffix(b"\r").unwrap_or(raw))
        .ok()
        .map(|i| i.custom_id)
}

fn request_line_id(request_id: Option<Uuid>) -> String {
    format!(
        "batch_req_{}",
        request_id.unwrap_or_else(Uuid::new_v4).simple()
    )
}

/// The client-facing response body of a successful line, in the endpoint's
/// normal response shape with the public model name.
pub(crate) fn response_body(
    endpoint: BatchEndpoint,
    request_id: Uuid,
    model: &str,
    response: BatchResponse,
) -> Result<Value, InferenceError> {
    let created = chrono::Utc::now().timestamp().max(0) as u64;
    let hex = request_id.simple();
    match (endpoint, response) {
        (BatchEndpoint::ChatCompletions, BatchResponse::Chat(r)) => {
            Ok(crate::protocols::chat_completions::batch_response(
                &format!("chatcmpl-{hex}"),
                created,
                model,
                r,
            ))
        }
        (BatchEndpoint::Responses, BatchResponse::Chat(r)) => Ok(
            crate::protocols::responses::batch_response(&format!("resp_{hex}"), created, model, &r),
        ),
        (BatchEndpoint::Messages, BatchResponse::Chat(r)) => {
            crate::protocols::messages::batch_response(&format!("msg_{hex}"), model, &r)
        }
        (BatchEndpoint::Embeddings, BatchResponse::Embeddings(r)) => {
            crate::protocols::embeddings::batch_response(r, model)
        }
        _ => Err(InferenceError::InvalidUpstream),
    }
}

/// A successful result line.
pub(crate) fn success_line(custom_id: &str, request_id: Option<Uuid>, body: Value) -> Value {
    json!({
        "id": request_line_id(request_id),
        "custom_id": custom_id,
        "response": {
            "status_code": 200,
            "request_id": request_id.map(|r| r.to_string()),
            "body": body,
        },
        "error": null,
    })
}

/// A line the gateway answered with an error (the same status and body the
/// endpoint returns interactively).
pub(crate) fn failure_line(
    custom_id: &str,
    request_id: Option<Uuid>,
    endpoint: BatchEndpoint,
    error: InferenceError,
) -> Value {
    let (status, body) =
        crate::protocols::batch_line_error(endpoint == BatchEndpoint::Messages, error);
    json!({
        "id": request_line_id(request_id),
        "custom_id": custom_id,
        "response": {
            "status_code": status,
            "request_id": request_id.map(|r| r.to_string()),
            "body": body,
        },
        "error": null,
    })
}

/// A line the provider answered with an error (native batches): its status
/// and sanitized code, with a fixed message.
pub(crate) fn provider_failure_line(
    custom_id: &str,
    endpoint: BatchEndpoint,
    status: u16,
    code: &ErrorCode,
) -> Value {
    let message = "The provider rejected this request.";
    let body = if endpoint == BatchEndpoint::Messages {
        json!({"type":"error","error":{"type":code.as_str(),"message":message}})
    } else {
        json!({"error":{"message":message,"type":code.as_str(),"code":code.as_str(),"param":null}})
    };
    json!({
        "id": request_line_id(None),
        "custom_id": custom_id,
        "response": {"status_code": status, "request_id": null, "body": body},
        "error": null,
    })
}

/// Why a line has no response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotRun {
    Cancelled,
    Expired,
    BudgetExceeded,
    Failed,
    /// It ran, but its result was lost (the gateway stopped before storing it).
    ResultUnavailable,
}
impl NotRun {
    pub fn code(self) -> &'static str {
        match self {
            Self::Cancelled => "batch_cancelled",
            Self::Expired => "batch_expired",
            Self::BudgetExceeded => "budget_exceeded",
            Self::Failed => "batch_failed",
            Self::ResultUnavailable => "result_unavailable",
        }
    }
    fn message(self) -> &'static str {
        match self {
            Self::Cancelled => "This request was not run because the batch was cancelled.",
            Self::Expired => "This request was not run before the batch expired.",
            Self::BudgetExceeded => "This request was not run because a budget was exhausted.",
            Self::Failed => "This request was not run because the batch failed.",
            Self::ResultUnavailable => {
                "This request was started, but the gateway stopped before its result was stored."
            }
        }
    }
}
/// A line without a response (error file).
pub(crate) fn not_run_line(custom_id: &str, reason: NotRun) -> Value {
    json!({
        "id": request_line_id(None),
        "custom_id": custom_id,
        "response": null,
        "error": {"code": reason.code(), "message": reason.message()},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }
    fn chat() -> Value {
        json!({"custom_id":"r1","method":"POST","url":"/v1/chat/completions","body":{"model":"m","messages":[{"role":"user","content":"hi"}],"max_completion_tokens":64}})
    }
    #[test]
    fn lines_are_strict_and_bounded() {
        let l = parse_line(&line(chat()), BatchEndpoint::ChatCompletions).unwrap();
        assert_eq!(l.custom_id, "r1");
        assert_eq!(l.request.model(), "m");
        assert_eq!(l.request.max_output(), 64);
        let bad = |f: &dyn Fn(&mut Value)| {
            let mut v = chat();
            f(&mut v);
            parse_line(&line(v), BatchEndpoint::ChatCompletions).err()
        };
        assert_eq!(
            bad(&|v| v["method"] = json!("GET")),
            Some(LineError::InvalidMethod)
        );
        assert_eq!(
            bad(&|v| v["extra"] = json!(1)),
            Some(LineError::InvalidShape)
        );
        assert_eq!(
            bad(&|v| v["custom_id"] = json!("")),
            Some(LineError::InvalidCustomId)
        );
        assert_eq!(
            bad(&|v| v["url"] = json!("/v1/embeddings")),
            Some(LineError::WrongUrl)
        );
        assert_eq!(
            bad(&|v| {
                v["body"]
                    .as_object_mut()
                    .unwrap()
                    .remove("max_completion_tokens");
            }),
            Some(LineError::Unbounded)
        );
        assert_eq!(
            bad(&|v| v["body"]["stream"] = json!(true)),
            Some(LineError::InvalidBody)
        );
        assert_eq!(
            bad(&|v| v["body"]["n"] = json!(2)),
            Some(LineError::InvalidBody)
        );
        // The legacy `max_tokens` name is accepted; both only when equal.
        let mut legacy = chat();
        let body = legacy["body"].as_object_mut().unwrap();
        body.remove("max_completion_tokens");
        body.insert("max_tokens".into(), json!(9));
        let l = parse_line(&line(legacy), BatchEndpoint::ChatCompletions).unwrap();
        assert_eq!(l.request.max_output(), 9);
        assert_eq!(
            bad(&|v| v["body"]["max_tokens"] = json!(10)),
            Some(LineError::InvalidBody)
        );
        assert_eq!(
            parse_line(b"not json", BatchEndpoint::ChatCompletions).err(),
            Some(LineError::InvalidJson)
        );
        assert_eq!(
            parse_line(b"[1]", BatchEndpoint::ChatCompletions).err(),
            Some(LineError::InvalidShape)
        );
    }
    #[test]
    fn every_endpoint_uses_its_interactive_contract() {
        let messages = json!({"custom_id":"a","method":"POST","url":"/v1/messages","body":{"model":"m","max_tokens":5,"messages":[{"role":"user","content":"hi"}]}});
        assert!(parse_line(&line(messages), BatchEndpoint::Messages).is_ok());
        let responses = json!({"custom_id":"a","method":"POST","url":"/v1/responses","body":{"model":"m","input":"hi","max_output_tokens":5}});
        assert!(parse_line(&line(responses.clone()), BatchEndpoint::Responses).is_ok());
        let mut unbounded = responses;
        unbounded["body"]
            .as_object_mut()
            .unwrap()
            .remove("max_output_tokens");
        assert_eq!(
            parse_line(&line(unbounded), BatchEndpoint::Responses).err(),
            Some(LineError::Unbounded)
        );
        let embeddings = json!({"custom_id":"a","method":"POST","url":"/v1/embeddings","body":{"model":"e","input":["x","y"]}});
        let l = parse_line(&line(embeddings), BatchEndpoint::Embeddings).unwrap();
        assert_eq!(l.request.max_output(), 0);
    }
    #[test]
    fn result_lines_never_echo_errors_or_content() {
        let f = failure_line(
            "c",
            Some(Uuid::nil()),
            BatchEndpoint::ChatCompletions,
            InferenceError::UpstreamUnavailable,
        );
        assert_eq!(f["response"]["status_code"], 502);
        assert_eq!(f["custom_id"], "c");
        let m = failure_line("c", None, BatchEndpoint::Messages, InferenceError::Busy);
        assert_eq!(m["response"]["body"]["type"], "error");
        let n = not_run_line("c", NotRun::BudgetExceeded);
        assert_eq!(n["error"]["code"], "budget_exceeded");
        assert!(n["response"].is_null());
        let p = provider_failure_line(
            "c",
            BatchEndpoint::ChatCompletions,
            400,
            &ErrorCode::parse("Bad Thing: secret"),
        );
        assert_eq!(p["response"]["body"]["error"]["code"], "upstream_failed");
    }
    #[test]
    fn line_splitter_holds_one_line() {
        let mut l = Lines::default();
        assert_eq!(l.push(b"ab\ncd").unwrap(), vec![b"ab".to_vec()]);
        assert_eq!(l.push(b"e\n\nf").unwrap(), vec![b"cde".to_vec(), vec![]]);
        assert_eq!(l.finish(), Some(b"f".to_vec()));
        assert_eq!(l.finish(), None);
        let mut big = Lines::default();
        assert!(big.push(&vec![b'x'; BATCH_MAX_LINE_BYTES + 1]).is_err());
        assert!(is_blank(b"  \r"));
    }
}

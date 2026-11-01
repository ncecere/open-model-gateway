//! Error vocabulary, envelopes and reusable responses of the inference contract,
//! generated from source so a new `InferenceError` variant or job code cannot
//! be missing from the artifact.
//!
//! Four native shapes exist and none is the management `ErrorEnvelope`
//! (`error.code` = HTTP status string, `error.reason`):
//! * OpenAI: `{error:{message,type,code,param:null}}`, `code` a machine code
//!   (`protocols/mod.rs` `openai_error_body`; `http.rs` `api_error`).
//! * Anthropic: `{type:"error",error:{type,message}}`, NO machine code
//!   (`protocols/messages.rs` `error_body`, `http_error`).
//! * Jobs: OpenAI-shaped with `permission_error`/`invalid_request_error` types
//!   and the `JobError::{Conflict,Invalid,Forbidden}` codes
//!   (`protocols/videos.rs` `job_error`).
//! * Files: OpenAI-shaped with a nullable string `param` (`protocols/files.rs`).
use std::{collections::BTreeSet, path::Path};

use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};
use syn::visit::Visit;

use crate::{
    inference::error::{FieldName, InferenceError, LimitScope},
    protocols::batch_line_error,
};

/// Every `InferenceError` variant (both scopes of the scoped ones). The guard
/// below fails to COMPILE when a variant is added, so the vocabulary cannot
/// silently miss it.
pub(super) fn all() -> Vec<InferenceError> {
    let field = FieldName::parse("field").expect("valid field name");
    let mut all = vec![
        InferenceError::InvalidRequest,
        InferenceError::InvalidField("field"),
        InferenceError::UnknownField(field),
        InferenceError::ContextLength,
        InferenceError::Overloaded,
        InferenceError::Unauthenticated,
        InferenceError::ModelUnavailable,
        InferenceError::Unsupported,
        InferenceError::Configuration,
        InferenceError::Busy,
        InferenceError::PriceUnbounded,
        InferenceError::RouteCoolingDown(1),
        InferenceError::Timeout,
        InferenceError::UpstreamRejected,
        InferenceError::UpstreamUnavailable,
        InferenceError::InvalidUpstream,
        InferenceError::Storage,
    ];
    for scope in [LimitScope::Workspace, LimitScope::ApiKey] {
        all.extend([
            InferenceError::BudgetExceeded(scope),
            InferenceError::UnresolvedUsage(scope),
            InferenceError::TokenReservationExceedsLimit(scope),
            InferenceError::JobLimitExceeded(scope),
        ]);
    }
    all
}
pub(super) const fn guard(error: InferenceError) -> usize {
    match error {
        InferenceError::InvalidRequest => 0,
        InferenceError::InvalidField(_) => 1,
        InferenceError::UnknownField(_) => 2,
        InferenceError::ContextLength => 3,
        InferenceError::Overloaded => 4,
        InferenceError::Unauthenticated => 5,
        InferenceError::ModelUnavailable => 6,
        InferenceError::Unsupported => 7,
        InferenceError::Configuration => 8,
        InferenceError::Busy => 9,
        InferenceError::BudgetExceeded(_) => 10,
        InferenceError::UnresolvedUsage(_) => 11,
        InferenceError::TokenReservationExceedsLimit(_) => 12,
        InferenceError::PriceUnbounded => 13,
        InferenceError::JobLimitExceeded(_) => 14,
        InferenceError::RouteCoolingDown(_) => 15,
        InferenceError::Timeout => 16,
        InferenceError::UpstreamRejected => 17,
        InferenceError::UpstreamUnavailable => 18,
        InferenceError::InvalidUpstream => 19,
        InferenceError::Storage => 20,
    }
}
const _: fn(InferenceError) -> usize = guard;

/// OpenAI-shaped errors that do not come from an `InferenceError`:
/// `(type, code, file that renders it)`. Each literal must still occur in that
/// file (`errors_tests.rs`), so a stale entry fails.
pub(super) const OPENAI_TRANSPORT: &[(&str, &str, &str)] = &[
    ("api_error", "api_error", "src/http.rs"),
    ("not_found_error", "not_found_error", "src/http.rs"),
    (
        "invalid_request_error",
        "payload_too_large",
        "src/protocols/mod.rs",
    ),
    // `POST /v1/embeddings` answers an oversized streamed body with this
    // type/code, unlike every other route (src/protocols/embeddings.rs).
    (
        "invalid_request",
        "invalid_request",
        "src/protocols/embeddings.rs",
    ),
];
/// Anthropic `error.type` values rendered outside `error_body`.
pub(super) const ANTHROPIC_TRANSPORT: &[(&str, &str)] = &[
    ("request_too_large", "src/protocols/messages.rs"),
    ("invalid_request_error", "src/protocols/messages.rs"),
];

pub(super) struct Vocabulary {
    pub(super) openai_types: BTreeSet<String>,
    pub(super) openai_codes: BTreeSet<String>,
    pub(super) anthropic_types: BTreeSet<String>,
    pub(super) job_codes: BTreeSet<String>,
}

pub(super) fn vocabulary() -> Result<Vocabulary> {
    let mut v = Vocabulary {
        openai_types: BTreeSet::new(),
        openai_codes: BTreeSet::new(),
        anthropic_types: BTreeSet::new(),
        job_codes: job_codes()?,
    };
    for error in all() {
        // The real renderers, not a copy of their mapping.
        let (_, openai) = batch_line_error(false, error);
        let (_, anthropic) = batch_line_error(true, error);
        let text = |value: &Value, key: &str| -> Result<String> {
            Ok(value["error"][key]
                .as_str()
                .with_context(|| format!("renderer produced no error.{key}"))?
                .to_owned())
        };
        ensure!(
            openai["error"]["code"] == error.code(),
            "the OpenAI renderer disagrees with InferenceError::code"
        );
        v.openai_types.insert(text(&openai, "type")?);
        v.openai_codes.insert(error.code().to_owned());
        v.anthropic_types.insert(text(&anthropic, "type")?);
    }
    for (kind, code, _) in OPENAI_TRANSPORT {
        v.openai_types.insert((*kind).to_owned());
        v.openai_codes.insert((*code).to_owned());
    }
    for (kind, _) in ANTHROPIC_TRANSPORT {
        v.anthropic_types.insert((*kind).to_owned());
    }
    Ok(v)
}

// ------------------------------------------------------------ job codes ----

/// `JobError::{Conflict,Invalid,Forbidden}("code", "message")` literals of the
/// non-test sources, plus `not_found` (`JobError::NotFound`). An argument that
/// is not a string literal fails the scan instead of being guessed.
pub(super) fn job_codes() -> Result<BTreeSet<String>> {
    let mut codes = BTreeSet::from(["not_found".to_owned()]);
    let mut files = Vec::new();
    source_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/jobs"),
        &mut files,
    )?;
    for file in files {
        scan_source(&std::fs::read_to_string(&file)?, &mut codes)
            .with_context(|| file.display().to_string())?;
    }
    Ok(codes)
}
/// The literal job codes of one source file (non-test modules only).
pub(super) fn scan_source(source: &str, codes: &mut BTreeSet<String>) -> Result<()> {
    let source = syn::parse_file(source)?;
    let mut visitor = JobCodes {
        codes,
        errors: Vec::new(),
    };
    visitor.visit_file(&source);
    ensure!(visitor.errors.is_empty(), "{}", visitor.errors.join("; "));
    Ok(())
}
fn source_files(directory: &Path, files: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        let name = path.file_name().unwrap().to_string_lossy();
        if name.contains("test") {
            continue;
        }
        if path.is_dir() {
            source_files(&path, files)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    files.sort();
    Ok(())
}
struct JobCodes<'a> {
    codes: &'a mut BTreeSet<String>,
    errors: Vec<String>,
}
impl<'ast> Visit<'ast> for JobCodes<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        // `#[cfg(test)]` modules build fixtures, not served errors.
        if item.attrs.iter().any(|attr| {
            attr.path().is_ident("cfg")
                && attr.parse_args::<syn::Meta>().is_ok_and(
                    |meta| matches!(meta, syn::Meta::Path(path) if path.is_ident("test")),
                )
        }) {
            return;
        }
        syn::visit::visit_item_mod(self, item);
    }
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = &*call.func {
            let segments: Vec<String> = path
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            if let [.., owner, variant] = segments.as_slice()
                && owner == "JobError"
                && ["Conflict", "Invalid", "Forbidden"].contains(&variant.as_str())
            {
                match call.args.first() {
                    Some(syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(code),
                        ..
                    })) => {
                        self.codes.insert(code.value());
                    }
                    _ => self.errors.push(
                        "unsupported JobError code expression; inventory it explicitly".into(),
                    ),
                }
            }
        }
        syn::visit::visit_expr_call(self, call);
    }
}

// ------------------------------------------------------ shared components ----

fn strings(values: &BTreeSet<String>) -> Vec<&String> {
    values.iter().collect()
}

fn envelope(kind: Value, code: Value, param: Value) -> Value {
    json!({"type":"object","required":["error"],"additionalProperties":false,
        "properties":{"error":{"type":"object","required":["message","type","code","param"],
            "additionalProperties":false,
            "properties":{"message":{"type":"string"},"type":kind,"code":code,"param":param}}}})
}

/// Generated schemas, headers and reusable responses.
pub(super) fn components() -> Result<Value> {
    let v = vocabulary()?;
    let mut schemas = Map::new();
    schemas.insert("OpenAiErrorType".into(), json!({"type":"string","enum":strings(&v.openai_types),
        "description":"OpenAI error.type. Generated from the renderers: budget denials are insufficient_quota, token-reservation and job-limit denials rate_limit_error, a cooling route server_error, every other InferenceError uses its code as its type, plus the transport kinds of OPENAI_TRANSPORT."}));
    schemas.insert("OpenAiErrorCode".into(), json!({"type":"string","enum":strings(&v.openai_codes),
        "description":"Gateway machine code, generated from InferenceError::code over every variant plus the transport codes of OPENAI_TRANSPORT. Clients branch on this, never on the message."}));
    schemas.insert("AnthropicErrorType".into(), json!({"type":"string","enum":strings(&v.anthropic_types),
        "description":"Anthropic error.type. Anthropic's shape has no machine code, so a budget denial is a rate_limit_error distinguishable only by its message."}));
    schemas.insert("JobErrorCode".into(), json!({"type":"string","enum":strings(&v.job_codes),
        "description":"Code of a job error: not_found plus the JobError::{Conflict,Invalid,Forbidden} literals scanned from src/jobs."}));
    schemas.insert(
        "OpenAiErrorEnvelope".into(),
        {
            let mut schema = envelope(
                json!({"$ref":"#/components/schemas/OpenAiErrorType"}),
                json!({"$ref":"#/components/schemas/OpenAiErrorCode"}),
                json!({"type":"null"}),
            );
            schema["description"] = json!("OpenAI-shaped inference error. Never the management ErrorEnvelope: error.code is a gateway machine code, not the HTTP status, and there is no error.reason.");
            schema
        },
    );
    schemas.insert(
        "JobErrorEnvelope".into(),
        {
            let mut schema = envelope(
                json!({"type":"string","enum":["invalid_request_error","permission_error"]}),
                json!({"$ref":"#/components/schemas/JobErrorCode"}),
                json!({"type":"null"}),
            );
            schema["description"] = json!("Job (video and batch) error: 404 and 400 are invalid_request_error, 403 permission_error (protocols/videos.rs job_error). Inference errors of a job route use OpenAiErrorEnvelope.");
            schema
        },
    );
    schemas.insert(
        "FileErrorEnvelope".into(),
        {
            let mut schema = envelope(
                json!({"type":"string","enum":["invalid_request_error","permission_error","insufficient_quota","service_unavailable"]}),
                json!({"type":"string","pattern":"^[a-z][a-z0-9_]*$"}),
                json!({"type":["string","null"],"pattern":"^[a-z_]+$"}),
            );
            schema["description"] = json!("Files API error (protocols/files.rs): param names the offending field, or is null. The exhaustive code list is documented with the Files operations.");
            schema
        },
    );
    schemas.insert("AnthropicErrorEnvelope".into(), json!({"type":"object","required":["type","error"],"additionalProperties":false,
        "description":"Anthropic-shaped /v1/messages error (protocols/messages.rs). It carries NO gateway machine code: unsupported_capability, budget_exceeded and the rest cannot be told apart except by their message.",
        "properties":{"type":{"const":"error"},"error":{"type":"object","required":["type","message"],"additionalProperties":false,
            "properties":{"type":{"$ref":"#/components/schemas/AnthropicErrorType"},"message":{"type":"string"}}}}}));

    let headers = json!({
        "RequestId":{"description":"The gateway's request ID (UUIDv7), also the attempt ID of a root request.","required":true,"schema":{"type":"string","format":"uuid"}},
        "NoStore":{"description":"Everything is uncacheable.","required":true,"schema":{"const":"no-store"}},
        "ShouldRetryFalse":{"description":"Permanent refusals (unsupported capability, budget, price or token-reservation denials) tell SDKs not to retry.","required":false,"schema":{"const":"false"}},
        "RetryAfter":{"description":"Seconds to wait; only a cooling-down route sends it.","required":false,"schema":{"type":"string","pattern":"^[1-9][0-9]*$"}},
        "WwwAuthenticate":{"description":"Always `Bearer` on a 401.","required":true,"schema":{"const":"Bearer"}}
    });
    let header = |names: &[&str]| -> Value {
        let mut map = Map::new();
        map.insert(
            "x-request-id".into(),
            json!({"$ref":"#/components/headers/RequestId"}),
        );
        map.insert(
            "cache-control".into(),
            json!({"$ref":"#/components/headers/NoStore"}),
        );
        for name in names {
            let (wire, component) = match *name {
                "should-retry" => ("x-should-retry", "ShouldRetryFalse"),
                "retry-after" => ("retry-after", "RetryAfter"),
                "www-authenticate" => ("www-authenticate", "WwwAuthenticate"),
                other => panic!("unknown shared header {other}"),
            };
            map.insert(
                wire.into(),
                json!({"$ref":format!("#/components/headers/{component}")}),
            );
        }
        Value::Object(map)
    };
    let openai = |description: &str, extra: &[&str]| -> Value {
        json!({"description":description,"headers":header(extra),"content":{"application/json":{"schema":{"$ref":"#/components/schemas/OpenAiErrorEnvelope"}}}})
    };
    let responses = json!({
        "OpenAiBadRequest":openai("HTTP 400 invalid_request_error for a closed-field, bound or type violation, or an explicit unsupported_capability on non-generation workloads; error.code distinguishes them. System One uses its separate HTTP 422 response for validation.", &["should-retry"]),
        "OpenAiUnauthorized":openai("Missing, malformed, duplicate, unknown, revoked or no-longer-valid inference key (always authentication_error)", &["www-authenticate"]),
        "OpenAiNotFound":openai("The model does not exist or is not available to this key and workspace (model_not_found)", &[]),
        "OpenAiPayloadTooLarge":json!({"description":"The request body exceeds the route's cap. A declared Content-Length over the cap is refused by the body-limit layer with the plain text `length limit exceeded`, before any handler runs; a streamed body is refused by the handler with this JSON error (a different type/code on /v1/embeddings).","headers":header(&[]),"content":{"text/plain":{"schema":{"const":"length limit exceeded"}},"application/json":{"schema":{"$ref":"#/components/schemas/OpenAiErrorEnvelope"}}}}),
        "OpenAiTooManyRequests":openai("Rate or concurrency limit (rate_limit_error), or a permanent budget, unresolved-usage or token-reservation denial (x-should-retry: false)", &["should-retry"]),
        "OpenAiNotImplemented":openai("A capability no registered deployment supports (unsupported_capability); permanent", &["should-retry"]),
        "OpenAiBadGateway":openai("The provider was unavailable or returned an invalid or incomplete response", &[]),
        "OpenAiUnavailable":openai("Authentication, catalog or accounting storage unavailable, provider configuration unavailable, an unbounded price, or every route cooling down (Retry-After)", &["should-retry","retry-after"]),
        "OpenAiGatewayTimeout":openai("The inference deadline was exceeded", &[]),
        "OpenAiOverloaded":openai("The provider reported an overload (HTTP 529)", &[]),
    });
    Ok(json!({"schemas":schemas,"headers":headers,"responses":responses}))
}

// ----------------------------------------------------------- drift checks ----

/// Envelope components every operation of a path may reference.
fn allowed_envelopes(path: &str) -> &'static [&'static str] {
    if path == "/v1/messages" {
        &["AnthropicErrorEnvelope"]
    } else if path.starts_with("/v1/videos") || path.starts_with("/v1/batches") {
        &["JobErrorEnvelope", "OpenAiErrorEnvelope"]
    } else if path.starts_with("/v1/files") {
        &["FileErrorEnvelope", "OpenAiErrorEnvelope"]
    } else {
        &["OpenAiErrorEnvelope"]
    }
}
const ENVELOPES: &[&str] = &[
    "OpenAiErrorEnvelope",
    "AnthropicErrorEnvelope",
    "JobErrorEnvelope",
    "FileErrorEnvelope",
];

fn references(
    document: &Value,
    node: &Value,
    out: &mut BTreeSet<String>,
    seen: &mut BTreeSet<String>,
) {
    match node {
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
                if let Some(name) = reference.strip_prefix("#/components/schemas/") {
                    out.insert(name.to_owned());
                } else if seen.insert(reference.to_owned())
                    && let Some(target) = reference
                        .strip_prefix('#')
                        .and_then(|pointer| document.pointer(pointer))
                {
                    references(document, target, out, seen);
                }
            }
            for child in map.values() {
                references(document, child, out, seen);
            }
        }
        Value::Array(items) => {
            for child in items {
                references(document, child, out, seen);
            }
        }
        _ => {}
    }
}

/// Generated components must be byte-for-byte what source dictates, the
/// management `ErrorEnvelope` must not appear, and each operation may use only
/// the envelope family of its protocol.
pub(super) fn verify_components(document: &Value) -> Result<()> {
    let generated = components()?;
    let schemas = document["components"]["schemas"]
        .as_object()
        .context("schemas missing")?;
    ensure!(
        !schemas.contains_key("ErrorEnvelope"),
        "the management ErrorEnvelope must not be reused by the inference contract"
    );
    for (name, schema) in generated["schemas"].as_object().unwrap() {
        ensure!(
            schemas.get(name) == Some(schema),
            "undocumented error vocabulary or envelope drift: {name}"
        );
    }
    for kind in ["headers", "responses"] {
        ensure!(
            document["components"][kind] == generated[kind],
            "shared {kind} drift"
        );
    }
    let mut every = BTreeSet::new();
    references(document, document, &mut every, &mut BTreeSet::new());
    ensure!(
        !every.contains("ErrorEnvelope"),
        "a reference to the management ErrorEnvelope"
    );
    for (path, item) in document["paths"].as_object().context("paths missing")? {
        let allowed = allowed_envelopes(path);
        for (method, operation) in item.as_object().context("path item")? {
            let mut used = BTreeSet::new();
            references(
                document,
                &operation["responses"],
                &mut used,
                &mut BTreeSet::new(),
            );
            for name in used.iter().filter(|n| ENVELOPES.contains(&n.as_str())) {
                ensure!(
                    allowed.contains(&name.as_str()),
                    "{method} {path} uses {name}, the envelope of another protocol"
                );
            }
        }
    }
    Ok(())
}

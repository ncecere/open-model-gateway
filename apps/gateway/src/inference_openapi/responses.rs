//! Raw-wire contract checks of the inference contract: status, headers, bytes.
//! A COPY of the management `validate` shape, not a parameterisation of it: the
//! management validator insists that `error.code` equals the HTTP status string,
//! which is exactly wrong here (an inference `error.code` is a machine code), and
//! its template matching, completed-area registry and artifact are management's.
//! Everything else is shared: the offline validator, `describe` (which never
//! prints an instance value), quality rules, coverage and vectors.
use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

#[cfg(feature = "integration-tests")]
use crate::management::openapi::{
    http_vectors::{self, Probe},
    responses::{Captured, PositiveFixtures},
};
use crate::management::openapi::{quality, responses as shared, validation};

/// Completed areas of PR1. Each is gated by an executed fixture group
/// (`fixtures_tests.rs`); the other areas stay counted placeholders.
pub(super) const SIMPLE_JSON_AREAS: &[&str] = &[
    "models",
    "embeddings",
    "rerank",
    "systemone",
    "images",
    "realtime/client_secrets",
    "realtime/calls",
];
pub(super) const GENERATION_AREAS: &[&str] = &["chat", "responses", "messages"];
pub(super) const AREA_GROUPS: &[(&str, &[&str])] = &[
    ("SIMPLE_JSON_AREAS", SIMPLE_JSON_AREAS),
    ("GENERATION_AREAS", GENERATION_AREAS),
];

/// Area of a template: the first segment after `/v1/`, except `realtime`,
/// whose two refusals and WebSocket upgrade are separate areas.
pub(super) fn area(template: &str) -> &str {
    let Some(rest) = template.strip_prefix("/v1/") else {
        return "";
    };
    let take = if rest.starts_with("realtime/") { 2 } else { 1 };
    let end = rest
        .match_indices('/')
        .nth(take - 1)
        .map_or(rest.len(), |(i, _)| i);
    &rest[..end]
}
pub(super) fn completed(area: &str) -> bool {
    AREA_GROUPS.iter().any(|(_, group)| group.contains(&area))
}

/// The committed artifact, read once (the file is generated test-only bytes).
pub(super) fn artifact() -> &'static Value {
    static DOCUMENT: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    DOCUMENT.get_or_init(|| {
        serde_json::from_slice(
            &std::fs::read(super::generator::artifact_path()).expect("inference artifact"),
        )
        .expect("inference artifact is JSON")
    })
}

fn resolved<'a>(document: &'a Value, value: &'a Value) -> Result<&'a Value> {
    let mut value = value;
    for _ in 0..8 {
        let Some(reference) = value.get("$ref") else {
            return Ok(value);
        };
        let reference = reference.as_str().context("reference must be a string")?;
        ensure!(reference.starts_with("#/"), "external reference refused");
        value = document
            .pointer(&reference[1..])
            .context("unresolved reference")?;
    }
    anyhow::bail!("reference chain too deep")
}

/// Compile a schema against the whole document (so local `$ref`s resolve).
/// Only the immutable artifact is cached, by pointer identity: a mutated
/// document control always compiles its own.
fn validator(document: &Value, schema: &Value) -> Result<std::sync::Arc<jsonschema::Validator>> {
    fn compile(document: &Value, schema: &Value) -> Result<std::sync::Arc<jsonschema::Validator>> {
        let mut root = document.clone();
        root["$defs"] = json!({"observed":schema});
        root["$ref"] = json!("#/$defs/observed");
        Ok(std::sync::Arc::new(validation::compile_schema(&root)?))
    }
    if !std::ptr::eq(document, artifact()) {
        return compile(document, schema);
    }
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<String, std::sync::Arc<jsonschema::Validator>>>,
    > = std::sync::OnceLock::new();
    let key = serde_json::to_string(schema)?;
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    if let Some(validator) = cache.get(&key) {
        return Ok(validator.clone());
    }
    let validator = compile(document, schema)?;
    cache.insert(key, validator.clone());
    Ok(validator)
}

pub(super) fn validate_value(document: &Value, schema: &Value, body: &Value) -> Result<()> {
    validator(document, schema)?
        .validate(body)
        .map_err(|e| anyhow::anyhow!(shared::describe(&e)))
}

fn validate_headers(document: &Value, definitions: &Value, headers: &HeaderMap) -> Result<()> {
    for (name, header) in definitions.as_object().into_iter().flatten() {
        let header = resolved(document, header)?;
        let values = headers.get_all(name.as_str()).iter().collect::<Vec<_>>();
        ensure!(
            header["required"] != true || !values.is_empty(),
            "missing response header {name}"
        );
        for value in values {
            // Never echo the value: only the header name and the violated schema.
            let value = value
                .to_str()
                .map_err(|_| anyhow::anyhow!("response header {name} is not visible ASCII"))?;
            validate_value(document, &header["schema"], &json!(value))
                .with_context(|| format!("response header {name}"))?;
        }
    }
    Ok(())
}

fn media_schema<'a>(response: &'a Value, headers: &HeaderMap) -> Result<(&'a str, &'a Value)> {
    let media = headers
        .get("content-type")
        .context("missing content-type")?
        .to_str()
        .map_err(|_| anyhow::anyhow!("content-type is not visible ASCII"))?
        .split(';')
        .next()
        .unwrap()
        .trim()
        .to_ascii_lowercase();
    let content = response["content"]
        .as_object()
        .context("content must be an object")?;
    let (name, value) = content
        .get_key_value(media.as_str())
        .context("undocumented response content-type")?;
    Ok((name, value.get("schema").context("media schema missing")?))
}

/// Exact METHOD/template and status of a COMPLETED operation, never an
/// operation inferred from the body.
pub(super) fn validate(
    document: &Value,
    method: &str,
    template: &str,
    status: StatusCode,
    headers: &HeaderMap,
    bytes: &[u8],
) -> Result<()> {
    require_complete(document, shared::operation(document, method, template)?)?;
    validate_documented(document, method, template, status, headers, bytes)
}

/// [`validate`] for a documented status of any operation, including an
/// incomplete one (the Messages transport errors are documented already).
pub(super) fn validate_documented(
    document: &Value,
    method: &str,
    template: &str,
    status: StatusCode,
    headers: &HeaderMap,
    bytes: &[u8],
) -> Result<()> {
    validate_exchange(document, method, template, status, Some(headers), bytes)
}

/// Status and JSON body only, for a response taken straight from a handler,
/// below the transport layer that adds `x-request-id` and `Cache-Control`.
pub(super) fn validate_body(
    document: &Value,
    method: &str,
    template: &str,
    status: StatusCode,
    bytes: &[u8],
) -> Result<()> {
    validate_exchange(document, method, template, status, None, bytes)
}

fn validate_exchange(
    document: &Value,
    method: &str,
    template: &str,
    status: StatusCode,
    headers: Option<&HeaderMap>,
    bytes: &[u8],
) -> Result<()> {
    let op = shared::operation(document, method, template)?;
    let response = op["responses"]
        .get(status.as_str())
        .with_context(|| format!("undocumented status {status} for {method} {template}"))?;
    let response = resolved(document, response)?;
    if let Some(headers) = headers {
        validate_headers(document, &response["headers"], headers)?;
    }
    if response.get("content").is_none() {
        ensure!(
            bytes.is_empty(),
            "documented empty response contained bytes (JSON null is not empty)"
        );
        return Ok(());
    }
    ensure!(!bytes.is_empty(), "documented representation is empty");
    let (media, schema) = match headers {
        Some(headers) => media_schema(response, headers)?,
        None => (
            "application/json",
            response["content"]["application/json"]
                .get("schema")
                .context("documented response has no JSON schema")?,
        ),
    };
    if media == "application/json" || media.ends_with("+json") {
        let body: Value = serde_json::from_slice(bytes)
            .map_err(|e| anyhow::anyhow!("response is not JSON ({:?})", e.classify()))?;
        validate_value(document, schema, &body)?;
    } else if media == "text/event-stream" {
        let outcome = super::generation::validate_sse(document, template, bytes, true)?;
        ensure!(
            outcome.positive(),
            "truncated generation SSE cannot earn positive coverage"
        );
    } else if media.starts_with("text/") {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("text response is not UTF-8"))?;
        validate_value(document, schema, &json!(text))?;
    } else {
        anyhow::bail!("media type {media} is not supported by the simple-JSON areas");
    }
    Ok(())
}

pub(super) fn require_complete(document: &Value, operation: &Value) -> Result<()> {
    fn walk(document: &Value, value: &Value, seen: &mut BTreeSet<String>) -> Result<()> {
        ensure!(
            value["x-omg-schema-status"] != "incomplete",
            "completed operation references an incomplete schema"
        );
        if let Some(reference) = value.get("$ref").and_then(Value::as_str)
            && seen.insert(reference.to_owned())
        {
            walk(document, resolved(document, value)?, seen)?;
        }
        match value {
            Value::Object(map) => {
                for child in map.values() {
                    walk(document, child, seen)?;
                }
            }
            Value::Array(values) => {
                for child in values {
                    walk(document, child, seen)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    static CHECKED: std::sync::OnceLock<std::sync::Mutex<BTreeSet<String>>> =
        std::sync::OnceLock::new();
    let cacheable = std::ptr::eq(document, artifact());
    let id = operation["operationId"]
        .as_str()
        .context("operation ID missing")?;
    if cacheable
        && CHECKED
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .contains(id)
    {
        return Ok(());
    }
    walk(document, operation, &mut BTreeSet::new())?;
    quality::operation(document, operation)?;
    if cacheable {
        CHECKED
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(id.to_owned());
    }
    Ok(())
}

/// Resolve a concrete request path to its documented template (never the
/// concrete path in a message: it can carry identifiers).
#[cfg(feature = "integration-tests")]
pub(super) fn template(method: &str, path: &str) -> Result<Option<String>> {
    let path = path.split('?').next().unwrap();
    let verb = method.to_ascii_lowercase();
    let mut candidates = artifact()["paths"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(template, item)| {
            let a = template.split('/').collect::<Vec<_>>();
            let b = path.split('/').collect::<Vec<_>>();
            item.get(&verb).is_some()
                && a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(a, b)| (a.starts_with('{') && a.ends_with('}')) || *a == b)
        })
        .map(|(p, _)| p.clone())
        .collect::<Vec<_>>();
    let specificity = |path: &str| {
        path.split('/')
            .map(|s| !s.starts_with('{'))
            .collect::<Vec<_>>()
    };
    candidates.sort_by_key(|path| std::cmp::Reverse(specificity(path)));
    if candidates.len() > 1 {
        ensure!(
            specificity(&candidates[0]) != specificity(&candidates[1]),
            "ambiguous operation template"
        );
    }
    Ok(candidates.into_iter().next())
}

#[cfg(feature = "integration-tests")]
fn query_names(path: &str) -> Vec<String> {
    path.split_once('?')
        .map(|(_, query)| {
            query
                .split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| pair.split('=').next().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Check one real exchange of a completed area against the committed artifact.
#[cfg(feature = "integration-tests")]
pub(super) fn check(
    method: &str,
    path: &str,
    status: StatusCode,
    headers: &HeaderMap,
    bytes: &[u8],
) -> Result<()> {
    if let Some(template) = template(method, path)?
        && completed(area(&template))
    {
        validate(artifact(), method, &template, status, headers, bytes)
            .with_context(|| format!("{method} {template} (status {})", status.as_u16()))?;
    }
    Ok(())
}

/// Status, headers and raw bytes BEFORE any JSON decoding; a mismatch panics
/// with the already-sanitized contract error and nothing captured.
#[cfg(feature = "integration-tests")]
pub(super) async fn capture(
    method: &str,
    path: &str,
    response: axum::response::Response,
) -> Captured {
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024).await.unwrap();
    check(method, path, parts.status, &parts.headers, &bytes).unwrap_or_else(|e| shared::refuse(e));
    Captured {
        status: parts.status,
        headers: parts.headers,
        bytes,
        method: method.into(),
        template: template(method, path).unwrap(),
        query: query_names(path),
    }
}

/// A status-derived input verdict that also reads 501 and 413 as refusals.
#[cfg(feature = "integration-tests")]
pub(super) fn probe(captured: Captured) -> Probe {
    Probe::refusing(captured, &[413, 501])
}

/// The shared coverage engine, fed through this contract's validator.
#[cfg(feature = "integration-tests")]
#[derive(Default)]
pub(super) struct Fixtures(PositiveFixtures);
#[cfg(feature = "integration-tests")]
impl Fixtures {
    pub(super) fn observe(&mut self, captured: &Captured) {
        self.0.observe_with(artifact(), captured, validate);
    }
    /// Every operation of the given areas must have earned positive credit and
    /// the coverage and boundary-vector gates must hold.
    pub(super) fn finish(&self, areas: &[&str]) {
        finish(&self.0, artifact(), areas).unwrap_or_else(|e| shared::refuse(e));
    }
    /// Run the HTTP boundary vectors of a JSON body (one run per wire form).
    pub(super) async fn body_vectors(
        &mut self,
        method: &str,
        template: &str,
        valid: Value,
        seeds: &[(&str, Value)],
        candidates: &[(&str, Value)],
        send: impl AsyncFn(Value) -> Probe,
    ) {
        let own = |pairs: &[(&str, Value)]| -> Vec<(String, Value)> {
            pairs
                .iter()
                .map(|(pointer, value)| ((*pointer).to_owned(), value.clone()))
                .collect()
        };
        let constraints = |body: &Value| {
            !GENERATION_AREAS.contains(&area(template))
                || super::generation::validate_request(artifact(), template, body).is_ok()
        };
        let run = http_vectors::run_body_with_constraints(
            artifact(),
            method,
            template,
            valid,
            &own(seeds),
            &own(candidates),
            (&send, &constraints),
        )
        .await
        .unwrap_or_else(|e| shared::refuse(e));
        for exchange in &run.exchanges {
            self.observe(exchange);
        }
        self.0.record_vectors(method, template, "body");
        self.0.record_reach(method, template, &run.reached);
    }
}

#[cfg(feature = "integration-tests")]
fn finish(fixtures: &PositiveFixtures, document: &Value, areas: &[&str]) -> Result<()> {
    ensure!(!areas.is_empty(), "no completed areas requested");
    let paths = document["paths"].as_object().context("paths")?;
    for wanted in areas {
        ensure!(
            paths.keys().any(|path| area(path) == *wanted),
            "area {wanted} has no operations in the artifact"
        );
    }
    let mut expected = BTreeSet::new();
    for (path, item) in paths {
        if areas.contains(&area(path)) {
            for (method, op) in item.as_object().context("operation map")? {
                require_complete(document, op)?;
                expected.insert(format!("{} {path}", method.to_ascii_uppercase()));
            }
        }
    }
    let observed: BTreeSet<String> = fixtures
        .observed_operations()
        .iter()
        .filter(|key| areas.contains(&area(key.split_once(' ').unwrap().1)))
        .cloned()
        .collect();
    ensure!(
        expected == observed,
        "positive fixture inventory differs: missing {:?}; extra {:?}",
        expected.difference(&observed).collect::<Vec<_>>(),
        observed.difference(&expected).collect::<Vec<_>>()
    );
    fixtures.check_contract_coverage(document, &expected)
}

/// The embedding vectors of a response are in the representation the REQUEST
/// asked for (`protocols/embeddings.rs` `render`): float arrays by default, or
/// for `encoding_format: base64` padded base64 of little-endian float32 values,
/// four bytes per dimension, every vector of one length. The schema allows both
/// representations at every vector, so this cross-check carries what OpenAPI
/// cannot express. Messages name the vector index, never a value.
pub(super) fn embedding_encoding_matches(request: &Value, response: &Value) -> Result<()> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    // Object form `encoding_format`, or serde's positional form (index 2).
    let format = request
        .get("encoding_format")
        .or_else(|| request.get(2))
        .and_then(Value::as_str)
        .unwrap_or("float");
    let input = request.get("input").or_else(|| request.get(1));
    let inputs = match input {
        Some(Value::Array(items)) => items.len(),
        _ => 1,
    };
    let data = response["data"]
        .as_array()
        .context("response has no data array")?;
    ensure!(data.len() == inputs, "one vector per input is required");
    let mut dimensions = None;
    for (index, item) in data.iter().enumerate() {
        ensure!(
            item["index"] == index,
            "vector {index} is out of input order"
        );
        let length = match (format, &item["embedding"]) {
            ("base64", Value::String(text)) => {
                let bytes = STANDARD
                    .decode(text)
                    .map_err(|_| anyhow::anyhow!("vector {index} is not valid base64"))?;
                ensure!(
                    !bytes.is_empty() && bytes.len() % 4 == 0,
                    "vector {index} is not whole float32 values"
                );
                bytes.len() / 4
            }
            ("float", Value::Array(numbers)) => {
                ensure!(
                    numbers.iter().all(Value::is_number),
                    "vector {index} has a non-number"
                );
                numbers.len()
            }
            (wanted, _) => {
                anyhow::bail!("vector {index} is not in the requested {wanted} representation")
            }
        };
        ensure!(
            dimensions
                .replace(length)
                .is_none_or(|first| first == length),
            "vectors differ in length"
        );
    }
    Ok(())
}

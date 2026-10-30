//! Raw-wire contract checks. Never infer an empty body from a failed JSON decode.
use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

// Enabled only when the area's schemas AND executed fixture suite land together.
// Each #81 slice owns one group; its executed fixture test finishes exactly that
// group. Add a group here, never another slice's names, to complete new areas.
pub(crate) const IDENTITY_SELF_SERVICE_AREAS: &[&str] = &[
    "auth",
    "branding",
    "invitations",
    "me",
    "access-request-targets",
];
pub(crate) const SLICE5_AREAS: &[&str] = &[
    "platform/alerts",
    "platform/installation",
    "platform/server-policy",
    "platform/settings",
    "workspaces/{ws}/alerts",
];
pub(crate) const AREA_GROUPS: &[(&str, &[&str])] = &[
    ("IDENTITY_SELF_SERVICE_AREAS", IDENTITY_SELF_SERVICE_AREAS),
    ("SLICE5_AREAS", SLICE5_AREAS),
];

/// Whether an area's schemas are complete and gated by an executed fixture group.
pub(crate) fn completed(area: &str) -> bool {
    AREA_GROUPS.iter().any(|(_, group)| group.contains(&area))
}

pub(crate) fn artifact() -> &'static Value {
    static DOCUMENT: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    DOCUMENT.get_or_init(|| {
        serde_json::from_str(include_str!(
            "../../../../docs/openapi/management.openapi.json"
        ))
        .unwrap()
    })
}

pub(super) fn operation<'a>(
    document: &'a Value,
    method: &str,
    template: &str,
) -> Result<&'a Value> {
    let verb = method.to_ascii_lowercase();
    let item = &document["paths"][template];
    item.get(&verb)
        .or_else(|| (method == "HEAD").then(|| item.get("get")).flatten())
        .with_context(|| format!("undocumented operation {method} {template}"))
}

fn resolved<'a>(document: &'a Value, value: &'a Value) -> Result<&'a Value> {
    if let Some(reference) = value.get("$ref") {
        let reference = reference.as_str().context("reference must be a string")?;
        ensure!(reference.starts_with("#/"), "external reference refused");
        return document
            .pointer(&reference[1..])
            .context("unresolved reference");
    }
    Ok(value)
}

pub(super) fn validator(
    document: &Value,
    schema: &Value,
) -> Result<std::sync::Arc<jsonschema::Validator>> {
    fn compile(document: &Value, schema: &Value) -> Result<std::sync::Arc<jsonschema::Validator>> {
        let mut root = document.clone();
        root["$defs"] = json!({"observed":schema});
        root["$ref"] = json!("#/$defs/observed");
        Ok(std::sync::Arc::new(super::validation::compile_schema(
            &root,
        )?))
    }
    // Pointer identity, not value equality: mutated-document controls must
    // always compile their own document and can never reuse a trusted result.
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

/// Every `jsonschema` error kind. A failure is named by one of these static
/// strings, never by the validator's own text or payload.
const ERROR_KINDS: &[&str] = &[
    "AdditionalItems",
    "AdditionalProperties",
    "AnyOf",
    "BacktrackLimitExceeded",
    "RegexEngineFailure",
    "Constant",
    "Contains",
    "ContentEncoding",
    "ContentMediaType",
    "Custom",
    "Enum",
    "ExclusiveMaximum",
    "ExclusiveMinimum",
    "FalseSchema",
    "Format",
    "FromUtf8",
    "MaxItems",
    "Maximum",
    "MaxLength",
    "MaxProperties",
    "MinItems",
    "Minimum",
    "MinLength",
    "MinProperties",
    "MultipleOf",
    "Not",
    "OneOfMultipleValid",
    "OneOfNotValid",
    "Pattern",
    "PropertyNames",
    "Required",
    "Type",
    "UnevaluatedItems",
    "UnevaluatedProperties",
    "UniqueItems",
    "Referencing",
];

/// Describe a schema violation WITHOUT any instance value: only the error kind
/// (a static name), the instance JSON pointer and the schema pointer. The
/// validator's own `Display` embeds the offending instance (tokens, emails,
/// identifiers from real responses) and must never reach a panic or log.
pub(crate) fn describe(error: &jsonschema::ValidationError<'_>) -> String {
    let debug = format!("{:?}", error.kind());
    let name = debug
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default();
    let kind = ERROR_KINDS
        .iter()
        .copied()
        .find(|kind| *kind == name)
        .unwrap_or("Other");
    let pointer = |location: String| {
        if location.is_empty() {
            "(root)".to_owned()
        } else {
            location
        }
    };
    format!(
        "wire schema mismatch: kind {kind} at instance {} (schema {})",
        pointer(error.instance_path().to_string()),
        pointer(error.schema_path().to_string())
    )
}

pub(crate) fn validate_value(document: &Value, schema: &Value, body: &Value) -> Result<()> {
    validator(document, schema)?
        .validate(body)
        .map_err(|e| anyhow::anyhow!(describe(&e)))
}

fn validate_headers(document: &Value, definitions: &Value, headers: &HeaderMap) -> Result<()> {
    for (name, header) in definitions.as_object().into_iter().flatten() {
        let header = resolved(document, header)?;
        let values = headers.get_all(name).iter().collect::<Vec<_>>();
        ensure!(
            header["required"] != true || !values.is_empty(),
            "missing response header {name}"
        );
        for value in values {
            // Never echo the header value: only its name and the violated schema.
            let value = value
                .to_str()
                .map_err(|_| anyhow::anyhow!("response header {name} is not visible ASCII"))?;
            validate_value(document, &header["schema"], &json!(value))
                .with_context(|| format!("response header {name}"))?;
        }
    }
    Ok(())
}

/// Exact METHOD/template, never an operation inferred from the response body.
pub(crate) fn validate(
    document: &Value,
    method: &str,
    template: &str,
    status: StatusCode,
    headers: &HeaderMap,
    bytes: &[u8],
) -> Result<()> {
    let op = operation(document, method, template)?;
    require_complete(document, op)?;
    let response = op["responses"]
        .get(status.as_str())
        .with_context(|| format!("undocumented status {status} for {method} {template}"))?;
    let response = resolved(document, response)?;
    validate_headers(document, &response["headers"], headers)?;
    for name in response["x-omg-required-cookies"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let prefix = format!(
            "{}=",
            name.as_str().context("cookie name must be a string")?
        );
        ensure!(
            headers
                .get_all("set-cookie")
                .iter()
                .any(|v| v.to_str().is_ok_and(|v| v.starts_with(&prefix))),
            "missing response cookie {prefix}"
        );
    }
    if method == "HEAD" {
        ensure!(bytes.is_empty(), "implicit HEAD must have no body");
        // HEAD carries GET's representation headers, but no representation bytes.
        if response.get("content").is_some() {
            media_schema(response, headers)?;
        }
        return Ok(());
    }
    if response.get("content").is_none() {
        ensure!(
            bytes.is_empty(),
            "documented empty response contained bytes (JSON null is not empty)"
        );
        return Ok(());
    }
    let (media, schema) = media_schema(response, headers)?;
    ensure!(!bytes.is_empty(), "documented representation is empty");
    if media == "application/json" || media.ends_with("+json") {
        let body: Value = serde_json::from_slice(bytes)
            .map_err(|e| anyhow::anyhow!("response is not JSON ({:?})", e.classify()))?;
        if let Some(error) = body.get("error").filter(|v| v.is_object()) {
            ensure!(
                error["code"] == status.as_str(),
                "error envelope code differs from HTTP status"
            );
        }
        validate_value(document, schema, &body)?;
        for variant in response["x-omg-body-header-requirements"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if validator(document, &variant["schema"])?.is_valid(&body) {
                validate_headers(document, &variant["headers"], headers)?;
            }
        }
    } else if media.starts_with("text/") {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("text response is not UTF-8"))?;
        validate_value(document, schema, &json!(text))?;
    } else {
        let schema = resolved(document, schema)?;
        ensure!(
            schema["type"] == "string" && schema["format"] == "binary",
            "binary response needs an explicit binary schema"
        );
        ensure!(
            bytes.len() as u64 >= schema["minLength"].as_u64().unwrap_or(0),
            "binary response too short"
        );
        if let Some(max) = schema["maxLength"].as_u64() {
            ensure!(bytes.len() as u64 <= max, "binary response too long");
        }
        if schema["x-omg-validate-image"] == true {
            crate::management::branding::inspect(Some(media), bytes).map_err(|_| {
                anyhow::anyhow!("binary image does not match its documented format and media type")
            })?;
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

fn require_complete(document: &Value, operation: &Value) -> Result<()> {
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
    // Only the immutable artifact can reuse completed-operation verification;
    // all mutated-document controls rerun reference AND anti-vacuity checks.
    static CHECKED: std::sync::OnceLock<std::sync::Mutex<BTreeSet<String>>> =
        std::sync::OnceLock::new();
    if std::ptr::eq(document, artifact()) {
        let id = operation["operationId"]
            .as_str()
            .context("operation ID missing")?;
        let mut checked = CHECKED.get_or_init(Default::default).lock().unwrap();
        if checked.contains(id) {
            return Ok(());
        }
        walk(document, operation, &mut BTreeSet::new())?;
        super::quality::operation(document, operation)?;
        checked.insert(id.to_owned());
        return Ok(());
    }
    walk(document, operation, &mut BTreeSet::new())?;
    super::quality::operation(document, operation)
}

fn require_positives(expected: &BTreeSet<String>, observed: &BTreeSet<String>) -> Result<()> {
    ensure!(
        expected == observed,
        "positive fixture inventory differs: missing {:?}; extra {:?}",
        expected.difference(observed).collect::<Vec<_>>(),
        observed.difference(expected).collect::<Vec<_>>()
    );
    Ok(())
}

/// Coverage area of a template: `platform/<resource>`,
/// `workspaces/{ws}/<resource>` (or `workspaces/{ws}` itself, or
/// the `workspaces` collection), otherwise the top-level segment such as `me`.
/// Always a contiguous substring of the template after `/api/v1/`.
pub(crate) fn area(template: &str) -> &str {
    let Some(rest) = template.strip_prefix("/api/v1/") else {
        return "";
    };
    let take = match rest.split('/').next() {
        Some("platform") => 2,
        Some("workspaces") => 3,
        _ => 1,
    };
    let end = rest
        .match_indices('/')
        .nth(take - 1)
        .map_or(rest.len(), |(i, _)| i);
    &rest[..end]
}

/// Resolve a concrete request only at capture sites; validation itself requires
/// the exact template. Match Axum's static-before-parameter precedence (for
/// example /platform/models/compare versus /platform/models/{id}); equally
/// specific ambiguous declarations are errors, not first-match wins.
fn template(method: &str, path: &str) -> Result<Option<String>> {
    let path = path.split('?').next().unwrap();
    let mut candidates = artifact()["paths"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(template, item)| {
            let verb = method.to_ascii_lowercase();
            let a = template.split('/').collect::<Vec<_>>();
            let b = path.split('/').collect::<Vec<_>>();
            (item.get(verb).is_some() || (method == "HEAD" && item.get("get").is_some()))
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

pub(crate) struct Captured {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub bytes: axum::body::Bytes,
    pub(super) method: String,
    pub(super) template: Option<String>,
    /// Query parameter NAMES of the request (never their values).
    pub(super) query: Vec<String>,
}

/// Names (not values) of the query parameters in a request target.
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

#[cfg(feature = "integration-tests")]
pub(crate) async fn capture(
    method: &str,
    path: &str,
    response: axum::response::Response,
) -> Captured {
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024).await.unwrap();
    check(method, path, parts.status, &parts.headers, &bytes).unwrap_or_else(|e| refuse(e));
    Captured {
        status: parts.status,
        headers: parts.headers,
        bytes,
        method: method.into(),
        template: template(method, path).unwrap(),
        query: query_names(path),
    }
}

pub(crate) fn check(
    method: &str,
    path: &str,
    status: StatusCode,
    headers: &HeaderMap,
    bytes: &[u8],
) -> Result<()> {
    if let Some(template) = template(method, path)?
        && completed(area(&template))
    {
        // The TEMPLATE, never the concrete path (it can carry user/workspace ids).
        validate(artifact(), method, &template, status, headers, bytes)
            .with_context(|| format!("{method} {template} (status {})", status.as_u16()))?;
    }
    Ok(())
}

/// Panic with the already-sanitized contract error (method, template, status,
/// kind and JSON pointers) and nothing captured from the exchange.
pub(crate) fn refuse(error: anyhow::Error) -> ! {
    panic!("contract check failed: {error:#}")
}

#[cfg(feature = "integration-tests")]
pub(crate) async fn capture_http(
    method: &str,
    path: &str,
    response: reqwest::Response,
) -> Captured {
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.bytes().await.unwrap();
    check(method, path, status, &headers, &bytes).unwrap_or_else(|e| refuse(e));
    Captured {
        status,
        headers,
        bytes,
        method: method.into(),
        template: template(method, path).unwrap(),
        query: query_names(path),
    }
}

/// Preserve response extensions as well as the raw representation for callers
/// that still need to inspect redirect/cookie or cache metadata.
#[cfg(feature = "integration-tests")]
pub(crate) async fn checked_response(
    method: &str,
    path: &str,
    response: axum::response::Response,
) -> axum::response::Response {
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024).await.unwrap();
    check(method, path, parts.status, &parts.headers, &bytes).unwrap_or_else(|e| refuse(e));
    axum::response::Response::from_parts(parts, axum::body::Body::from(bytes))
}

fn positive(response: &Value, status: StatusCode, headers: &HeaderMap, bytes: &[u8]) -> bool {
    // 304/HEAD/other bodyless replies cannot establish representation coverage.
    // Only explicitly reviewed empty successes and OIDC redirects earn credit.
    let represented = response.get("content").is_some() && !bytes.is_empty();
    let permitted = (status.is_success() && represented)
        || (status == StatusCode::NO_CONTENT
            && bytes.is_empty()
            && response["x-omg-positive-kind"] == "empty")
        || (status == StatusCode::SEE_OTHER
            && bytes.is_empty()
            && response["x-omg-positive-kind"] == "redirect")
        || (status == StatusCode::GONE && represented && response["x-omg-positive"] == true);
    let cookie = response["x-omg-positive-cookie"]
        .as_str()
        .is_none_or(|name| {
            let prefix = format!("{name}=");
            headers.get_all("set-cookie").iter().any(|h| {
                h.to_str().is_ok_and(|h| {
                    h.strip_prefix(&prefix)
                        .and_then(|v| v.split(';').next())
                        .is_some_and(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
                })
            })
        });
    permitted && cookie
}

fn json_media(headers: &HeaderMap) -> bool {
    headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
        .is_some_and(|media| media == "application/json" || media.ends_with("+json"))
}

/// What real handlers were seen to do. Positive observations earn operation,
/// query-parameter, array-population and union-branch coverage; executed HTTP
/// boundary vectors are recorded per operation exactly like positives.
#[derive(Default)]
pub(crate) struct PositiveFixtures {
    operations: BTreeSet<String>,
    coverage: super::coverage::Observed,
    vectors: std::collections::BTreeMap<String, BTreeSet<&'static str>>,
    /// Declared request locations (schema paths) each operation's body vector
    /// runs carried, by `METHOD template`; see `request_reach`.
    reach: std::collections::BTreeMap<String, BTreeSet<String>>,
}
impl PositiveFixtures {
    /// Record that every HTTP boundary vector of `kind` (`query` or `body`)
    /// for the exact operation ran and matched the handler.
    pub(super) fn record_vectors(&mut self, method: &str, template: &str, kind: &'static str) {
        self.vectors
            .entry(format!("{method} {template}"))
            .or_default()
            .insert(kind);
    }
    /// The declared request locations one body vector run carried (its valid body
    /// and seeded bases); runs of one operation accumulate.
    pub(super) fn record_reach(
        &mut self,
        method: &str,
        template: &str,
        reached: &BTreeSet<String>,
    ) {
        self.reach
            .entry(format!("{method} {template}"))
            .or_default()
            .extend(reached.iter().cloned());
    }
    /// Coverage and vector gaps for the given `METHOD template` operations.
    pub(super) fn check_contract_coverage(
        &self,
        document: &Value,
        expected: &BTreeSet<String>,
    ) -> Result<()> {
        let mut failures = Vec::new();
        for key in expected {
            let (method, template) = key.split_once(' ').context("operation key")?;
            let op = operation(document, method, template)?;
            if let Err(error) = self.coverage.check(document, key, op) {
                failures.push(format!("{error:#}"));
            }
            let ran = self.vectors.get(key);
            let has = |kind: &str| ran.is_some_and(|ran| ran.contains(kind));
            if !super::coverage::documented_queries(document, op)?.is_empty() && !has("query") {
                failures.push(format!(
                    "{key}: documented query parameters have no executed HTTP boundary vectors (call http_vectors::query_vectors)"
                ));
            }
            if op.get("requestBody").is_some() && !has("body") {
                failures.push(format!(
                    "{key}: the request body has no executed HTTP boundary vectors (call http_vectors::body_vectors_http)"
                ));
            }
            // Every declared request location must have been carried by some
            // valid body or seed of the operation (or be reviewed-exempt).
            if has("body")
                && let Some(schema) = op["requestBody"]["content"]["application/json"].get("schema")
            {
                let reached = self.reach.get(key).cloned().unwrap_or_default();
                for (location, kind) in super::request_reach::unreached(document, schema, &reached)?
                {
                    failures.push(format!(
                        "{key}: declared request location `{location}` ({kind}) was never walked by a vector run: {}, or annotate it with {}",
                        super::request_reach::how_to_reach(&location, kind),
                        super::request_reach::UNPROBED
                    ));
                }
                for location in super::request_reach::stale(document, schema, &reached)? {
                    failures.push(format!(
                        "{key}: request location `{location}` carries {} but a vector run reached it: the exemption is stale, remove it",
                        super::request_reach::UNPROBED
                    ));
                }
            }
        }
        ensure!(failures.is_empty(), "{}", failures.join("\n"));
        Ok(())
    }
    pub(crate) fn observe(&mut self, captured: &Captured) {
        self.observe_in(artifact(), captured);
    }
    /// `observe` against an explicit document (synthetic documents in controls).
    pub(super) fn observe_in(&mut self, document: &Value, captured: &Captured) {
        let template = captured
            .template
            .as_deref()
            .expect("fixture operation must be documented");
        validate(
            document,
            &captured.method,
            template,
            captured.status,
            &captured.headers,
            &captured.bytes,
        )
        .unwrap_or_else(|e| {
            refuse(e.context(format!(
                "{} {template} (status {})",
                captured.method,
                captured.status.as_u16()
            )))
        });
        let op = operation(document, &captured.method, template).unwrap();
        let response = resolved(document, &op["responses"][captured.status.as_str()]).unwrap();
        // A denial (including the callback's cookie-clearing 303) never earns
        // coverage, nor does an implicit HEAD earn a GET.
        if positive(
            response,
            captured.status,
            &captured.headers,
            &captured.bytes,
        ) && captured.method != "HEAD"
        {
            let key = format!("{} {template}", captured.method);
            self.operations.insert(key.clone());
            self.coverage.observe_query(&key, &captured.query);
            if captured.status.is_success()
                && json_media(&captured.headers)
                && let Ok(body) = serde_json::from_slice::<Value>(&captured.bytes)
            {
                self.coverage
                    .observe_body(document, &key, op, captured.status.as_u16(), &body)
                    .unwrap_or_else(|e| refuse(e.context(key.clone())));
            }
        }
    }
    pub(crate) fn finish(&self, areas: &[&str]) {
        require_known_areas(areas).unwrap();
        let mut expected = BTreeSet::new();
        for (path, item) in artifact()["paths"].as_object().unwrap() {
            if areas.contains(&area(path)) {
                for (method, op) in item.as_object().unwrap() {
                    require_complete(artifact(), op).unwrap();
                    expected.insert(format!("{} {path}", method.to_ascii_uppercase()));
                }
            }
        }
        assert!(!expected.is_empty(), "empty completed-area inventory");
        let observed = self
            .operations
            .iter()
            .filter(|key| areas.contains(&area(key.split_once(' ').unwrap().1)))
            .cloned()
            .collect();
        require_positives(&expected, &observed).unwrap_or_else(|e| refuse(e));
        self.check_contract_coverage(artifact(), &expected)
            .unwrap_or_else(|e| refuse(e));
    }
}

/// Every requested or registered area must own at least one artifact operation:
/// a misspelled area (for example `{workspace}` instead of the artifact's `{ws}`)
/// must fail rather than be silently satisfied by its valid neighbours.
pub(crate) fn require_known_areas(areas: &[&str]) -> Result<()> {
    ensure!(!areas.is_empty(), "no completed areas requested");
    let paths = artifact()["paths"].as_object().unwrap();
    for wanted in areas {
        ensure!(
            paths.keys().any(|path| area(path) == *wanted),
            "area {wanted} has no operations in the artifact"
        );
    }
    Ok(())
}

#[path = "review_tests.rs"]
pub(super) mod review_tests;

#[cfg(test)]
mod tests {
    #[test]
    fn areas_are_fine_grained_and_groups_are_disjoint() {
        for (template, want) in [
            ("/api/v1/me", "me"),
            ("/api/v1/me/sessions/{session}", "me"),
            ("/api/v1/auth/callback", "auth"),
            ("/api/v1/platform/users", "platform/users"),
            (
                "/api/v1/platform/users/{user}/roles/{role}",
                "platform/users",
            ),
            ("/api/v1/workspaces", "workspaces"),
            ("/api/v1/workspaces/{workspace}", "workspaces/{workspace}"),
            (
                "/api/v1/workspaces/{workspace}/keys",
                "workspaces/{workspace}/keys",
            ),
            (
                "/api/v1/workspaces/{workspace}/keys/{key}/rotate",
                "workspaces/{workspace}/keys",
            ),
            ("/health/ready", ""),
        ] {
            assert_eq!(super::area(template), want, "{template}");
        }
        assert!(super::require_known_areas(&["branding"]).is_ok());
        assert!(
            super::require_known_areas(&["branding", "workspaces/{workspace}/keys"]).is_err(),
            "an unknown area beside a valid one must be rejected"
        );
        assert!(super::require_known_areas(&[]).is_err());
        let mut seen = std::collections::BTreeSet::new();
        for (_, group) in super::AREA_GROUPS {
            assert!(!group.is_empty());
            super::require_known_areas(group).unwrap();
            for area in *group {
                assert!(
                    seen.insert(*area),
                    "area {area} is owned by two #81 slice groups"
                );
            }
        }
    }
    use super::*;
    fn document() -> Value {
        json!({"paths":{"/api/v1/me":{"get":{
            "x-omg-request-schema":{"$ref":"#/components/schemas/Request"},
            "x-omg-response-schema":{"$ref":"#/components/schemas/Response"},
            "responses":{"200":{"description":"OK","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Response"}}}},
                "403":{"description":"Denied","content":{"application/json":{"schema":{"type":"object","required":["error"],"properties":{"error":{"type":"object","required":["code","message"],"properties":{"code":{"type":"string"},"message":{"type":"string"}},"additionalProperties":false}},"additionalProperties":false}}}}}
        }}},"components":{"schemas":{"Request":false,"Response":{"type":"object","required":["ok"],"properties":{"ok":{"const":true}},"additionalProperties":false}}}})
    }
    fn headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("content-type", "application/json".parse().unwrap());
        h
    }
    #[test]
    fn management_openapi_response_positive_and_wire_controls() {
        let good = document();
        validate(
            &good,
            "GET",
            "/api/v1/me",
            StatusCode::OK,
            &headers(),
            b"{\"ok\":true}",
        )
        .unwrap();
        validate(&good, "HEAD", "/api/v1/me", StatusCode::OK, &headers(), b"").unwrap();
        assert!(
            validate(
                &good,
                "POST",
                "/api/v1/me",
                StatusCode::OK,
                &headers(),
                b"{\"ok\":true}"
            )
            .is_err()
        );
        assert!(
            validate(
                &good,
                "GET",
                "/api/v1/me",
                StatusCode::OK,
                &HeaderMap::new(),
                b"{\"ok\":true}"
            )
            .is_err()
        );
        let mut d = good.clone();
        d["paths"]["/api/v1/me"]["get"]["responses"]["204"] = json!({"description":"Empty"});
        validate(
            &d,
            "GET",
            "/api/v1/me",
            StatusCode::NO_CONTENT,
            &HeaderMap::new(),
            b"",
        )
        .unwrap();
        assert!(
            validate(
                &d,
                "GET",
                "/api/v1/me",
                StatusCode::NO_CONTENT,
                &headers(),
                b"null"
            )
            .is_err()
        );
        d["paths"]["/api/v1/me"]["get"]["responses"]["200"]["headers"] =
            json!({"ETag":{"required":true,"schema":{"const":"abc"}}});
        assert!(
            validate(
                &d,
                "GET",
                "/api/v1/me",
                StatusCode::OK,
                &headers(),
                b"{\"ok\":true}"
            )
            .is_err()
        );
        let mut h = headers();
        h.insert("etag", "abc".parse().unwrap());
        validate(
            &d,
            "GET",
            "/api/v1/me",
            StatusCode::OK,
            &h,
            b"{\"ok\":true}",
        )
        .unwrap();
        for (media, schema, body) in [
            ("text/plain", json!({"const":"hello"}), b"hello".as_slice()),
            (
                "image/png",
                json!({"type":"string","format":"binary","minLength":2}),
                b"\x89PNG".as_slice(),
            ),
        ] {
            d["paths"]["/api/v1/me"]["get"]["responses"]["200"] =
                json!({"description":"Representation","content":{media:{"schema":schema}}});
            h.insert("content-type", media.parse().unwrap());
            validate(&d, "GET", "/api/v1/me", StatusCode::OK, &h, body).unwrap();
            assert!(validate(&d, "GET", "/api/v1/me", StatusCode::OK, &h, b"").is_err());
        }
        assert_eq!(
            template("GET", "/api/v1/me?x=1").unwrap().as_deref(),
            Some("/api/v1/me")
        );
        assert_eq!(
            template("GET", "/api/v1/platform/models/compare")
                .unwrap()
                .as_deref(),
            Some("/api/v1/platform/models/compare")
        );
        check(
            "GET",
            "/api/v1/platform/not-declared",
            StatusCode::NOT_FOUND,
            &HeaderMap::new(),
            b"",
        )
        .unwrap();
        let observed = BTreeSet::from(["GET /api/v1/me".into()]);
        require_positives(&observed, &observed).unwrap();
    }
    #[test]
    fn management_openapi_completed_areas_have_no_incomplete_references() {
        let mut count = 0;
        for (path, item) in artifact()["paths"].as_object().unwrap() {
            if completed(area(path)) {
                for op in item.as_object().unwrap().values() {
                    require_complete(artifact(), op).unwrap();
                    assert!(
                        op["responses"].get("default").is_none(),
                        "completed operation needs exact statuses"
                    );
                    count += 1;
                }
            }
        }
        assert_eq!(count, 59);
    }
    #[test]
    fn management_openapi_response_rejects_incomplete_completed_slot() {
        let mut d = document();
        d["components"]["schemas"]["Request"] = json!({"x-omg-schema-status":"incomplete"});
        assert!(require_complete(&d, &d["paths"]["/api/v1/me"]["get"]).is_err());
    }
    #[test]
    fn management_openapi_response_denials_and_cookie_clearing_redirects_are_not_positive() {
        let response =
            json!({"x-omg-positive-cookie":"omg_session","x-omg-positive-kind":"redirect"});
        let mut h = HeaderMap::new();
        h.append(
            "set-cookie",
            "omg_session=; Path=/; Max-Age=0".parse().unwrap(),
        );
        assert!(!positive(&response, StatusCode::SEE_OTHER, &h, b""));
        assert!(!positive(
            &json!({"x-omg-positive":true}),
            StatusCode::FORBIDDEN,
            &h,
            b""
        ));
        assert!(!positive(&json!({}), StatusCode::UNAUTHORIZED, &h, b""));
        h.append(
            "set-cookie",
            format!("omg_session={}; Path=/", "a".repeat(64))
                .parse()
                .unwrap(),
        );
        assert!(positive(&response, StatusCode::SEE_OTHER, &h, b""));
        assert!(!positive(&json!({}), StatusCode::NO_CONTENT, &h, b""));
        assert!(positive(
            &json!({"x-omg-positive-kind":"empty"}),
            StatusCode::NO_CONTENT,
            &h,
            b""
        ));
        assert!(positive(
            &json!({"x-omg-positive":true,"content":{"application/json":{}}}),
            StatusCode::GONE,
            &h,
            b"{\"error\":{\"code\":\"410\"}}"
        ));
    }
    #[test]
    fn management_openapi_response_requires_executed_positive_fixture() {
        assert!(
            require_positives(&BTreeSet::from(["GET /api/v1/me".into()]), &BTreeSet::new())
                .is_err()
        );
    }
    #[test]
    fn management_openapi_response_rejects_status_body_mismatch() {
        for (status, body) in [
            (StatusCode::CREATED, b"{\"ok\":true}".as_slice()),
            (StatusCode::OK, b"null".as_slice()),
            (StatusCode::OK, b"".as_slice()),
        ] {
            assert!(validate(&document(), "GET", "/api/v1/me", status, &headers(), body).is_err());
        }
    }
    #[test]
    fn management_openapi_response_rejects_envelope_code_mismatch() {
        assert!(
            validate(
                &document(),
                "GET",
                "/api/v1/me",
                StatusCode::FORBIDDEN,
                &headers(),
                b"{\"error\":{\"code\":\"401\",\"message\":\"Denied\"}}"
            )
            .is_err()
        );
    }
}

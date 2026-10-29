//! HTTP boundary vectors: for every documented query parameter and request body
//! the SCHEMA's verdict on an input must equal the real HANDLER's verdict over
//! HTTP (served router or loopback client), not merely the DTO's serde verdict.
//!
//! * Vectors derive from the artifact's schemas: bounds and bounds +/- 1,
//!   `maxLength + 1`, pattern and format violations, enum members and
//!   non-members, wrong types, empty values, omission and (bodies) null where
//!   not nullable and unknown fields, top level and nested.
//! * The schema is the oracle; the handler "accepts" when it got past input
//!   validation (2xx/3xx, or a domain refusal 404/409) and "refuses" with 400 or
//!   422. Any other status (401/403/413/415/429/5xx) is a harness failure, never
//!   silently counted. A fixture whose status cannot tell input refusal from a
//!   domain refusal supplies its own verdict through [`Probe::decided`].
//! * Executing a full set is recorded per operation; `finish` refuses completed
//!   operations with query parameters or a request body that never ran theirs.
//!
//! Failure messages name method, template, vector kind, the schema-declared
//! parameter name or JSON pointer and the status code: never a sent value.
use super::{
    coverage,
    responses::{self, Captured, PositiveFixtures},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// One executed request and the handler's verdict on its input.
pub(crate) struct Probe {
    pub(crate) captured: Captured,
    accepted: Option<bool>,
}
impl From<Captured> for Probe {
    /// The verdict is derived from the status (see the module comment).
    fn from(captured: Captured) -> Self {
        Self {
            captured,
            accepted: None,
        }
    }
}
impl Probe {
    /// An explicit verdict for handlers whose status is ambiguous (for example a
    /// callback that answers 400 both for malformed input and for a refused login).
    pub(crate) fn decided(captured: Captured, accepted: bool) -> Self {
        Self {
            captured,
            accepted: Some(accepted),
        }
    }
    fn verdict(&self) -> Result<bool> {
        if let Some(accepted) = self.accepted {
            return Ok(accepted);
        }
        match self.captured.status.as_u16() {
            400 | 422 => Ok(false),
            200..=399 | 404 | 409 => Ok(true),
            other => bail!("unexpected status {other}: not an input verdict"),
        }
    }
}

// ---------------------------------------------------------------------------
// Schema walking
// ---------------------------------------------------------------------------

fn resolve<'a>(document: &'a Value, mut node: &'a Value) -> Result<&'a Value> {
    for _ in 0..16 {
        match coverage::follow(document, node)? {
            Some((target, _)) => node = target,
            None => return Ok(node),
        }
    }
    bail!("reference chain too deep")
}

fn valid(document: &Value, schema: &Value, instance: &Value) -> Result<bool> {
    Ok(responses::validator(document, schema)?.is_valid(instance))
}

/// The schema nodes that constrain `value`: the node itself, its `allOf` members,
/// the `oneOf`/`anyOf` branches the value matches and the active conditional arm.
fn applicable<'a>(
    document: &'a Value,
    node: &'a Value,
    value: &Value,
    out: &mut Vec<&'a Value>,
    depth: usize,
) -> Result<()> {
    ensure!(depth < 16, "schema nesting too deep");
    let node = resolve(document, node)?;
    out.push(node);
    for branch in node["allOf"].as_array().into_iter().flatten() {
        applicable(document, branch, value, out, depth + 1)?;
    }
    for keyword in ["oneOf", "anyOf"] {
        for branch in node[keyword].as_array().into_iter().flatten() {
            if valid(document, branch, value)? {
                applicable(document, branch, value, out, depth + 1)?;
            }
        }
    }
    if let Some(condition) = node.get("if") {
        let keyword = if valid(document, condition, value)? {
            "then"
        } else {
            "else"
        };
        if let Some(branch) = node.get(keyword) {
            applicable(document, branch, value, out, depth + 1)?;
        }
    }
    Ok(())
}

fn first<'a>(nodes: &[&'a Value], key: &str) -> Option<&'a Value> {
    nodes.iter().find_map(|node| node.get(key))
}
fn types<'a>(nodes: &[&'a Value]) -> BTreeSet<&'a str> {
    let mut out = BTreeSet::new();
    for node in nodes {
        match &node["type"] {
            Value::String(t) => {
                out.insert(t.as_str());
            }
            Value::Array(all) => out.extend(all.iter().filter_map(Value::as_str)),
            _ => {}
        }
    }
    out
}
fn repeat_to(seed: &str, length: usize) -> String {
    let chars: Vec<char> = seed.chars().collect();
    let chars = if chars.is_empty() { vec!['a'] } else { chars };
    (0..length).map(|i| chars[i % chars.len()]).collect()
}

/// Longest string vector (bytes of characters) the harness will send.
const LONGEST: u64 = 100_000;

/// Candidate replacement values for one location, by the constraints that apply.
/// `query` candidates are rendered as strings; JSON wrong types do not exist there.
fn scalar_mutations(
    document: &Value,
    schema: &Value,
    nodes: &[&Value],
    value: &Value,
    query: bool,
) -> Result<Vec<(&'static str, Value)>> {
    let mut out: Vec<(&'static str, Value)> = Vec::new();
    let kinds = types(nodes);
    if !query {
        for wrong in [
            json!(null),
            json!(true),
            json!(1),
            json!("wrong"),
            json!([]),
            json!({}),
        ] {
            // A candidate of an ALLOWED JSON type is a value vector, not a type vector.
            let allowed = match &wrong {
                Value::Null => false, // always probed: nullable fields must accept it
                Value::Bool(_) => kinds.contains("boolean"),
                Value::Number(_) => kinds.contains("integer") || kinds.contains("number"),
                Value::String(_) => kinds.contains("string"),
                Value::Array(_) => kinds.contains("array"),
                Value::Object(_) => kinds.contains("object"),
            };
            if !allowed {
                out.push((
                    if wrong.is_null() {
                        "null"
                    } else {
                        "wrong type"
                    },
                    wrong,
                ));
            }
        }
        if kinds.contains("string") {
            out.push(("empty", json!("")));
        }
    } else {
        if kinds.contains("integer") {
            for raw in ["abc", "true", "9999999999999999999999"] {
                out.push(("wrong type", json!(raw)));
            }
            out.push(("non-integer", json!("1.5")));
        }
        if kinds.contains("number") {
            out.push(("wrong type", json!("abc")));
        }
        if kinds.contains("boolean") {
            for raw in ["yes", "1", "TRUE"] {
                out.push(("wrong type", json!(raw)));
            }
        }
        out.push(("empty", json!("")));
    }
    if let Some(members) = first(nodes, "enum").and_then(Value::as_array) {
        out.push(("enum non-member", json!("~not-a-member~")));
        for member in members.iter().filter(|m| !m.is_null() && *m != value) {
            out.push(("enum member", member.clone()));
        }
    }
    // Numeric bounds, each side and its neighbour.
    let number = |key: &str| first(nodes, key).and_then(Value::as_i64);
    if kinds.contains("integer") || kinds.contains("number") {
        let mut push = |kind: &'static str, n: i64| out.push((kind, json!(n)));
        if let Some(min) = number("minimum") {
            push("minimum", min);
            push("minimum-1", min.saturating_sub(1));
        }
        if let Some(max) = number("maximum") {
            push("maximum", max);
            push("maximum+1", max.saturating_add(1));
        }
        if let Some(min) = number("exclusiveMinimum") {
            push("exclusiveMinimum", min);
            push("exclusiveMinimum+1", min.saturating_add(1));
        }
        if let Some(max) = number("exclusiveMaximum") {
            push("exclusiveMaximum", max);
            push("exclusiveMaximum-1", max.saturating_sub(1));
        }
        if !query && kinds.contains("integer") {
            out.push(("non-integer", json!(1.5)));
        }
    }
    // String bounds are derived from the VALID value so the alphabet stays valid.
    if kinds.contains("string") {
        let seed = value.as_str().unwrap_or("a");
        if let Some(min) = first(nodes, "minLength").and_then(Value::as_u64)
            && min <= LONGEST
        {
            out.push(("minLength", json!(repeat_to(seed, min as usize))));
            if min > 0 {
                out.push(("minLength-1", json!(repeat_to(seed, min as usize - 1))));
            }
        }
        if let Some(max) = first(nodes, "maxLength").and_then(Value::as_u64)
            && max < LONGEST
        {
            out.push(("maxLength", json!(repeat_to(seed, max as usize))));
            out.push(("maxLength+1", json!(repeat_to(seed, max as usize + 1))));
        }
        if first(nodes, "pattern").is_some() {
            let upper = seed.to_uppercase();
            let lower = seed.to_lowercase();
            let mut replaced = seed.chars();
            replaced.next();
            for candidate in [
                format!("{seed}!"),
                format!("!{seed}"),
                format!("!{}", replaced.as_str()),
                upper,
                lower,
                " ".to_owned(),
                "~".to_owned(),
                "\u{e9}".to_owned(),
            ] {
                // Keep only genuine violations: this is the pattern-violation vector.
                if !valid(document, schema, &json!(candidate))? {
                    out.push(("pattern violation", json!(candidate)));
                }
            }
        }
        if let Some(format) = first(nodes, "format").and_then(Value::as_str) {
            let bad: &[&str] = match format {
                "date" => &["2024-02-30", "2024-13-01", "not-a-date"],
                "date-time" => &["yesterday", "2024-01-01"],
                "uuid" => &["not-a-uuid"],
                "email" => &["not-an-email"],
                "uri" => &["not a uri"],
                _ => &[],
            };
            for candidate in bad {
                out.push(("format violation", json!(candidate)));
            }
        }
        if query {
            for candidate in ["a b", "\u{e9}\u{2713}", "a&b=c#d"] {
                out.push(("special characters", json!(candidate)));
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Query vectors
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Edit {
    Omit,
    Set(String),
}
#[derive(Clone, Debug)]
pub(super) struct QueryCase {
    pub(super) param: String,
    pub(super) kind: &'static str,
    pub(super) edit: Edit,
    pub(super) accept: bool,
}

fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// A raw query string value as the typed JSON the schema describes (query
/// values always arrive as strings): canonical integers, numbers and booleans only.
fn coerce(kinds: &BTreeSet<&str>, raw: &str) -> Value {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let unsigned = raw.strip_prefix('-').unwrap_or(raw);
    let canonical = digits(unsigned) && (unsigned == "0" || !unsigned.starts_with('0'));
    if kinds.contains("integer") && canonical && raw != "-0" {
        if let Ok(n) = raw.parse::<i64>() {
            return json!(n);
        }
        if let Ok(n) = raw.parse::<u64>() {
            return json!(n);
        }
    }
    if kinds.contains("number") {
        let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, "1"));
        if digits(whole) && digits(fraction) && (whole == "0" || !whole.starts_with('0')) {
            if let Ok(n) = raw.parse::<i64>() {
                return json!(n);
            }
            if let Ok(n) = raw.parse::<f64>() {
                return json!(n);
            }
        }
    }
    if kinds.contains("boolean") && (raw == "true" || raw == "false") {
        return json!(raw == "true");
    }
    json!(raw)
}

fn query_parameters<'a>(
    document: &'a Value,
    operation: &'a Value,
) -> Result<Vec<(String, bool, &'a Value)>> {
    let mut out = Vec::new();
    for parameter in operation["parameters"].as_array().into_iter().flatten() {
        let parameter = resolve(document, parameter)?;
        if parameter["in"] == "query" {
            out.push((
                parameter["name"]
                    .as_str()
                    .context("query parameter needs a name")?
                    .to_owned(),
                parameter["required"] == true,
                parameter
                    .get("schema")
                    .context("query parameter needs a schema")?,
            ));
        }
    }
    Ok(out)
}

/// Every query vector of an operation, given one VALID value per parameter.
pub(super) fn query_cases(
    document: &Value,
    operation: &Value,
    base: &[(String, String)],
) -> Result<Vec<QueryCase>> {
    let parameters = query_parameters(document, operation)?;
    ensure!(
        !parameters.is_empty(),
        "the operation documents no query parameters"
    );
    let mut out = Vec::new();
    for (name, required, schema) in &parameters {
        let raw = base
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .context("the valid base request must carry every documented query parameter")?;
        let mut nodes = Vec::new();
        let kinds = {
            applicable(document, schema, &json!(raw), &mut nodes, 0)?;
            types(&nodes)
        };
        ensure!(
            valid(document, schema, &coerce(&kinds, raw))?,
            "the valid base value of a query parameter must satisfy its schema"
        );
        let mut seen = BTreeSet::from([raw.to_owned()]);
        out.push(QueryCase {
            param: name.clone(),
            kind: "omitted",
            edit: Edit::Omit,
            accept: !required,
        });
        for (kind, value) in scalar_mutations(document, schema, &nodes, &coerce(&kinds, raw), true)?
        {
            let raw = render(&value);
            if seen.insert(raw.clone()) {
                let accept = valid(document, schema, &coerce(&kinds, &raw))?;
                out.push(QueryCase {
                    param: name.clone(),
                    kind,
                    edit: Edit::Set(raw),
                    accept,
                });
            }
        }
    }
    for name in base.iter().map(|(n, _)| n) {
        ensure!(
            parameters.iter().any(|(p, ..)| p == name),
            "the valid base request carries an undocumented query parameter"
        );
    }
    Ok(out)
}

fn encode(pairs: &[(String, String)]) -> String {
    // Percent-encode everything outside the RFC 3986 unreserved set.
    fn escape(text: &str) -> String {
        text.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    char::from(b).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
    pairs
        .iter()
        .map(|(name, value)| format!("{}={}", escape(name), escape(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn same_operation(captured: &Captured, method: &str, template: &str) -> Result<()> {
    ensure!(
        captured.method == method && captured.template.as_deref() == Some(template),
        "a vector request reached a different operation than {method} {template}"
    );
    Ok(())
}

fn mismatch(
    method: &str,
    template: &str,
    kind: &str,
    location: &str,
    schema_accepts: bool,
    probe: &Probe,
    handler_accepts: bool,
) -> anyhow::Error {
    let word = |accepts: bool| if accepts { "accepts" } else { "refuses" };
    anyhow::anyhow!(
        "{method} {template}: {kind} vector at {location}: schema {} but handler {} (status {})",
        word(schema_accepts),
        word(handler_accepts),
        probe.captured.status.as_u16()
    )
}

/// Run every query vector; returns every exchange (so the caller can observe them).
pub(super) async fn run_query<C>(
    document: &Value,
    method: &str,
    template: &str,
    fresh: &impl AsyncFn() -> (C, Vec<(String, String)>),
    send: &impl AsyncFn(&C, String) -> Probe,
) -> Result<Vec<Captured>> {
    let operation = responses::operation(document, method, template)?;
    let (_, base) = fresh().await;
    let cases = query_cases(document, operation, &base)?;
    let mut exchanges = Vec::new();
    // The all-valid request is the positive control for every other vector.
    let mut plan: Vec<(Option<&QueryCase>, &str, String, bool)> =
        vec![(None, "valid", "(all)".into(), true)];
    for case in &cases {
        plan.push((Some(case), case.kind, case.param.clone(), case.accept));
    }
    let mut accepted_seen = false;
    for (case, kind, location, expected) in plan {
        let (context, mut pairs) = fresh().await;
        if let Some(case) = case {
            pairs.retain(|(name, _)| !(name == &case.param && matches!(case.edit, Edit::Omit)));
            if let Edit::Set(raw) = &case.edit {
                for pair in pairs.iter_mut().filter(|(name, _)| name == &case.param) {
                    pair.1 = raw.clone();
                }
            }
        }
        let probe = send(&context, encode(&pairs)).await;
        same_operation(&probe.captured, method, template)?;
        let handler = probe
            .verdict()
            .with_context(|| format!("{method} {template}: {kind} vector at {location}"))?;
        if handler != expected {
            return Err(mismatch(
                method, template, kind, &location, expected, &probe, handler,
            ));
        }
        accepted_seen |= handler;
        exchanges.push(probe.captured);
    }
    ensure!(accepted_seen, "no query vector was accepted by the handler");
    Ok(exchanges)
}

/// Query boundary vectors for one operation over real HTTP.
///
/// `fresh` yields, per request, a context and a VALID value for every documented
/// query parameter (a login attempt for a callback, for example); `send` issues
/// the request target's query string and returns the captured exchange.
pub(crate) async fn query_vectors<C>(
    fixtures: &mut PositiveFixtures,
    method: &str,
    template: &str,
    fresh: impl AsyncFn() -> (C, Vec<(String, String)>),
    send: impl AsyncFn(&C, String) -> Probe,
) {
    let exchanges = run_query(responses::artifact(), method, template, &fresh, &send)
        .await
        .unwrap_or_else(|e| responses::refuse(e));
    for exchange in &exchanges {
        fixtures.observe(exchange);
    }
    fixtures.record_vectors(method, template, "query");
}

/// [`query_vectors`] for operations whose valid request is a fixed query.
pub(crate) async fn query_vectors_fixed(
    fixtures: &mut PositiveFixtures,
    method: &str,
    template: &str,
    base: &[(&str, &str)],
    send: impl AsyncFn(String) -> Captured,
) {
    let base: Vec<(String, String)> = base
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect();
    query_vectors(
        fixtures,
        method,
        template,
        async || ((), base.clone()),
        async |_: &(), query| Probe::from(send(query).await),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Body vectors
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Step {
    Key(String),
    Index(usize),
}
#[derive(Clone, Debug)]
pub(super) struct BodyCase {
    pub(super) pointer: String,
    pub(super) kind: &'static str,
    pub(super) body: Value,
    pub(super) accept: bool,
}

fn set(root: &Value, path: &[Step], new: Option<Value>) -> Value {
    let mut out = root.clone();
    let Some((last, parents)) = path.split_last() else {
        return new.unwrap_or(Value::Null);
    };
    let mut at = &mut out;
    for step in parents {
        at = match step {
            Step::Key(name) => &mut at[name.as_str()],
            Step::Index(i) => &mut at[*i],
        };
    }
    match (last, new) {
        (Step::Key(name), Some(v)) => {
            at[name.as_str()] = v;
        }
        (Step::Key(name), None) => {
            if let Some(map) = at.as_object_mut() {
                map.remove(name);
            }
        }
        (Step::Index(i), Some(v)) => {
            at[*i] = v;
        }
        (Step::Index(i), None) => {
            if let Some(items) = at.as_array_mut()
                && *i < items.len()
            {
                items.remove(*i);
            }
        }
    }
    out
}
fn pointer(path: &[Step]) -> String {
    if path.is_empty() {
        return "(root)".into();
    }
    path.iter()
        .map(|step| match step {
            Step::Key(name) => format!("/{name}"),
            Step::Index(i) => format!("/{i}"),
        })
        .collect()
}

struct Walk<'a> {
    document: &'a Value,
    root_schema: &'a Value,
    valid_body: &'a Value,
    seen: BTreeSet<String>,
    out: Vec<BodyCase>,
}
impl Walk<'_> {
    fn push(&mut self, path: &[Step], kind: &'static str, new: Option<Value>) -> Result<()> {
        let body = set(self.valid_body, path, new);
        if body == *self.valid_body || !self.seen.insert(serde_json::to_string(&body)?) {
            return Ok(());
        }
        let accept = valid(self.document, self.root_schema, &body)?;
        self.out.push(BodyCase {
            pointer: pointer(path),
            kind,
            body,
            accept,
        });
        Ok(())
    }
    fn walk(&mut self, schema: &Value, value: &Value, path: &mut Vec<Step>) -> Result<()> {
        ensure!(path.len() < 12, "request body nesting too deep");
        let mut nodes = Vec::new();
        applicable(self.document, schema, value, &mut nodes, 0)?;
        for (kind, new) in scalar_mutations(self.document, schema, &nodes, value, false)? {
            self.push(path, kind, Some(new))?;
        }
        if !path.is_empty() {
            self.push(path, "omitted", None)?;
        }
        match value {
            Value::Object(map) => {
                let mut unknown = map.clone();
                unknown.insert("unexpected_configuration".into(), json!(true));
                self.push(path, "unknown field", Some(Value::Object(unknown)))?;
                for (name, child) in map {
                    // Only schema-declared names are walked (and reported).
                    if let Some(child_schema) = first_property(&nodes, name) {
                        path.push(Step::Key(name.clone()));
                        self.walk(child_schema, child, path)?;
                        path.pop();
                    }
                }
            }
            Value::Array(items) => {
                let element = items.first();
                if let Some(first) = element {
                    let min = first_u64(&nodes, "minItems");
                    let max = first_u64(&nodes, "maxItems");
                    for (kind, length) in [
                        ("minItems", min),
                        ("minItems-1", min.and_then(|n| n.checked_sub(1))),
                        ("maxItems", max),
                        ("maxItems+1", max.map(|n| n + 1)),
                    ] {
                        if let Some(length) = length.filter(|n| *n <= 1000) {
                            let array = vec![first.clone(); length as usize];
                            self.push(path, kind, Some(Value::Array(array)))?;
                        }
                    }
                }
                self.push(path, "empty array", Some(json!([])))?;
                if let (Some(first), Some(item_schema)) = (items.first(), item_schema(&nodes, 0)) {
                    path.push(Step::Index(0));
                    self.walk(item_schema, first, path)?;
                    path.pop();
                }
            }
            _ => {}
        }
        Ok(())
    }
}
fn first_u64(nodes: &[&Value], key: &str) -> Option<u64> {
    first(nodes, key).and_then(Value::as_u64)
}
fn first_property<'a>(nodes: &[&'a Value], name: &str) -> Option<&'a Value> {
    nodes.iter().find_map(|node| node["properties"].get(name))
}
fn item_schema<'a>(nodes: &[&'a Value], index: usize) -> Option<&'a Value> {
    nodes.iter().find_map(|node| {
        node["prefixItems"]
            .get(index)
            .or_else(|| node.get("items").filter(|v| v.is_object()))
    })
}

fn body_schema(operation: &Value) -> Result<&Value> {
    operation["requestBody"]["content"]["application/json"]
        .get("schema")
        .context("the operation documents no JSON request body")
}

/// Every body vector derived from the operation's request schema and a VALID body.
pub(super) fn body_cases(
    document: &Value,
    operation: &Value,
    valid_body: &Value,
) -> Result<Vec<BodyCase>> {
    let schema = body_schema(operation)?;
    ensure!(
        valid(document, schema, valid_body)?,
        "the valid request body must satisfy its schema"
    );
    let mut walk = Walk {
        document,
        root_schema: schema,
        valid_body,
        seen: BTreeSet::new(),
        out: Vec::new(),
    };
    walk.walk(schema, valid_body, &mut Vec::new())?;
    Ok(walk.out)
}

pub(super) async fn run_body(
    document: &Value,
    method: &str,
    template: &str,
    valid_body: Value,
    send: &impl AsyncFn(Value) -> Probe,
) -> Result<Vec<Captured>> {
    let operation = responses::operation(document, method, template)?;
    let cases = body_cases(document, operation, &valid_body)?;
    let mut plan = vec![("valid", "(root)".to_owned(), valid_body.clone(), true)];
    plan.extend(
        cases
            .into_iter()
            .map(|case| (case.kind, case.pointer, case.body, case.accept)),
    );
    let mut exchanges = Vec::new();
    let mut accepted_seen = false;
    for (kind, location, body, expected) in plan {
        let probe = send(body).await;
        same_operation(&probe.captured, method, template)?;
        let handler = probe
            .verdict()
            .with_context(|| format!("{method} {template}: {kind} vector at {location}"))?;
        if handler != expected {
            return Err(mismatch(
                method, template, kind, &location, expected, &probe, handler,
            ));
        }
        accepted_seen |= handler;
        exchanges.push(probe.captured);
    }
    ensure!(accepted_seen, "no body vector was accepted by the handler");
    Ok(exchanges)
}

/// Body boundary vectors for one operation over real HTTP: top-level and nested
/// wrong types, null where not nullable, enum members and non-members, bounds,
/// omissions and unknown fields. `valid` must satisfy the schema; supply optional
/// nested fields in it to have them walked too.
pub(crate) async fn body_vectors_http(
    fixtures: &mut PositiveFixtures,
    method: &str,
    template: &str,
    valid: Value,
    send: impl AsyncFn(Value) -> Probe,
) {
    let exchanges = run_body(responses::artifact(), method, template, valid, &send)
        .await
        .unwrap_or_else(|e| responses::refuse(e));
    for exchange in &exchanges {
        fixtures.observe(exchange);
    }
    fixtures.record_vectors(method, template, "body");
}

// ---------------------------------------------------------------------------
// Raw (non-JSON) body vectors: uploads
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(super) struct BinaryCase {
    pub(super) kind: &'static str,
    pub(super) media: String,
    pub(super) bytes: Vec<u8>,
    pub(super) accept: bool,
}

/// The schema's verdict on a raw body of declared `media`: the documented length
/// bounds and, where the schema asks for it, the real image inspection (the same
/// rule responses.rs applies to binary representations).
pub(crate) fn binary_accepts(schema: &Value, media: &str, bytes: &[u8]) -> bool {
    bytes.len() as u64 >= schema["minLength"].as_u64().unwrap_or(0)
        && schema["maxLength"]
            .as_u64()
            .is_none_or(|max| bytes.len() as u64 <= max)
        && (schema["x-omg-validate-image"] != true
            || crate::management::branding::inspect(Some(media), bytes).is_ok())
}

/// Every raw-body vector of an operation, derived from the binary schema of
/// `media` and a VALID body: empty, one byte over `maxLength`, truncated, with a
/// trailing byte and the same bytes under every other documented media type, plus
/// caller-supplied `extra` bodies whose verdict the schema also decides.
pub(super) fn binary_cases(
    operation: &Value,
    media: &str,
    valid_body: &[u8],
    extra: &[(&'static str, Vec<u8>)],
) -> Result<Vec<BinaryCase>> {
    let content = operation["requestBody"]["content"]
        .as_object()
        .context("the operation documents no request body")?;
    let schema = content
        .get(media)
        .and_then(|c| c.get("schema"))
        .context("the operation documents no body for that media type")?;
    ensure!(
        schema["type"] == "string" && schema["format"] == "binary",
        "the request body of that media type is not a binary schema"
    );
    ensure!(
        binary_accepts(schema, media, valid_body),
        "the valid raw body must satisfy its schema"
    );
    let mut out = Vec::new();
    let mut push = |kind: &'static str, media: &str, bytes: Vec<u8>| {
        let accept = binary_accepts(schema, media, &bytes);
        out.push(BinaryCase {
            kind,
            media: media.to_owned(),
            bytes,
            accept,
        });
    };
    push("empty", media, Vec::new());
    if let Some(max) = schema["maxLength"].as_u64()
        && max < 64 * 1024 * 1024
    {
        let mut over = valid_body.to_vec();
        over.resize(max as usize + 1, 0);
        push("maxLength+1", media, over);
    }
    if valid_body.len() > 1 {
        push(
            "truncated",
            media,
            valid_body[..valid_body.len() / 2].to_vec(),
        );
    }
    let mut trailing = valid_body.to_vec();
    trailing.push(0);
    push("trailing byte", media, trailing);
    for other in content.keys().filter(|other| other.as_str() != media) {
        push("other media type", other, valid_body.to_vec());
    }
    for (kind, bytes) in extra {
        push(kind, media, bytes.clone());
    }
    Ok(out)
}

pub(super) async fn run_binary(
    document: &Value,
    method: &str,
    template: &str,
    media: &str,
    valid_body: Vec<u8>,
    extra: &[(&'static str, Vec<u8>)],
    send: &impl AsyncFn(String, Vec<u8>) -> Probe,
) -> Result<Vec<Captured>> {
    let operation = responses::operation(document, method, template)?;
    let cases = binary_cases(operation, media, &valid_body, extra)?;
    let mut plan = vec![("valid", media.to_owned(), valid_body, true)];
    plan.extend(
        cases
            .into_iter()
            .map(|case| (case.kind, case.media, case.bytes, case.accept)),
    );
    let mut exchanges = Vec::new();
    let mut accepted_seen = false;
    for (kind, declared, bytes, expected) in plan {
        let probe = send(declared, bytes).await;
        same_operation(&probe.captured, method, template)?;
        let handler = probe
            .verdict()
            .with_context(|| format!("{method} {template}: {kind} raw body vector"))?;
        if handler != expected {
            return Err(mismatch(
                method,
                template,
                kind,
                "(raw body)",
                expected,
                &probe,
                handler,
            ));
        }
        accepted_seen |= handler;
        exchanges.push(probe.captured);
    }
    ensure!(
        accepted_seen,
        "no raw body vector was accepted by the handler"
    );
    Ok(exchanges)
}

/// Raw body boundary vectors for an upload operation over real HTTP. `send` gets
/// the declared media type and the bytes; a handler whose refusals are not a plain
/// 400/422 (415, 413 ...) answers with [`Probe::decided`].
pub(crate) async fn binary_body_vectors_http(
    fixtures: &mut PositiveFixtures,
    method: &str,
    template: &str,
    media: &str,
    valid: Vec<u8>,
    extra: Vec<(&'static str, Vec<u8>)>,
    send: impl AsyncFn(String, Vec<u8>) -> Probe,
) {
    let exchanges = run_binary(
        responses::artifact(),
        method,
        template,
        media,
        valid,
        &extra,
        &send,
    )
    .await
    .unwrap_or_else(|e| responses::refuse(e));
    for exchange in &exchanges {
        fixtures.observe(exchange);
    }
    fixtures.record_vectors(method, template, "body");
}

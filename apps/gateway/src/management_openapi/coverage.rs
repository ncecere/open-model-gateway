//! Contract COVERAGE, not only conformance: what real handlers were seen to
//! accept and return, per completed operation.
//!
//! * Query parameters: every documented parameter appears in at least one
//!   POSITIVE observed request (a documented parameter that a
//!   `deny_unknown_fields` struct rejects with 400 never would).
//! * Nested population: every array location of a success response (a JSON
//!   pointer with indices wildcarded, e.g. `/data/*/outcomes`) is non-empty at
//!   least once. Exempt only with `x-omg-may-be-empty: "<reason>"` on the array.
//! * Union branches: every `oneOf`/`anyOf` branch reachable from a success
//!   response is the matching branch for at least one observed value AT THAT
//!   LOCATION. NULLABILITY is a branch too (`type: [X, "null"]`, enums with
//!   null), so unknown-versus-zero values must be seen as both. Exempt only
//!   with `x-omg-unobserved-branch: "<reason>"` on the branch (or on the
//!   nullable node).
//!
//! * A query parameter that can never ride a positive request (its handler
//!   always refuses a request carrying it) is exempt only with
//!   `x-omg-unobserved-parameter: "<reason>"` on the parameter. The exemption
//!   is STALE (and refused) once any positive request accepted the parameter.
//! * Typed map values (`patternProperties` / schema-valued
//!   `additionalProperties`) are walked at the wildcard location `<map>/*`.
//! * On a NULLABLE node `x-omg-unobserved-branch` must start with `null:` or
//!   `non-null:`, naming the one side it exempts; on a oneOf/anyOf branch it
//!   exempts that branch. Annotations are refused anywhere under a component
//!   referenced from more than one place (they would apply at every `$ref`), so
//!   exemptions live inline or in a component referenced once (`quality.rs`).
//!
//! * A STALE `x-omg-may-be-empty` (the array WAS non-empty in a positive
//!   response) or `x-omg-unobserved-branch` (that branch or nullable side WAS
//!   observed) is refused, exactly like a stale `x-omg-unobserved-parameter`.
//!
//! Messages name schema/data JSON pointers only, never instance values.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const MAY_BE_EMPTY: &str = "x-omg-may-be-empty";
pub(super) const UNOBSERVED_BRANCH: &str = "x-omg-unobserved-branch";
pub(super) const UNOBSERVED_PARAMETER: &str = "x-omg-unobserved-parameter";

#[derive(Default)]
struct OpObserved {
    queries: BTreeSet<String>,
    /// (array data pattern, schema pointer of the array) -> seen non-empty.
    /// The schema pointer keeps `items`, `prefixItems`, map values and union-arm
    /// arrays that share a data pattern apart.
    arrays: BTreeMap<(String, String), bool>,
    /// (data pattern, schema pointer of the union, branch index).
    branches: BTreeSet<(String, String, usize)>,
}
#[derive(Default)]
pub(crate) struct Observed(BTreeMap<String, OpObserved>);

fn segment(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}
fn show(pattern: &str) -> &str {
    if pattern.is_empty() {
        "(root)"
    } else {
        pattern
    }
}
/// Resolve a local `$ref`: the target and its pointer.
pub(super) fn follow<'a>(
    document: &'a Value,
    node: &'a Value,
) -> Result<Option<(&'a Value, String)>> {
    let Some(reference) = node.get("$ref") else {
        return Ok(None);
    };
    let reference = reference.as_str().context("reference must be a string")?;
    ensure!(reference.starts_with("#/"), "external reference refused");
    let target = document
        .pointer(&reference[1..])
        .context("unresolved reference")?;
    Ok(Some((target, reference[1..].to_owned())))
}
fn types(node: &Value) -> Vec<&str> {
    match &node["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(all) => all.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    }
}
fn is_array_schema(node: &Value) -> bool {
    types(node).contains(&"array")
        || node.get("items").is_some()
        || node.get("prefixItems").is_some()
}
/// `type: [X, "null"]` or an enum that lists null beside other members.
pub(super) fn nullable(node: &Value) -> bool {
    let kinds = types(node);
    (kinds.contains(&"null") && kinds.len() > 1)
        || node["enum"]
            .as_array()
            .is_some_and(|e| e.contains(&Value::Null) && e.len() > 1)
}
fn reason<'a>(node: &'a Value, key: &str) -> Option<&'a str> {
    node.get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}
/// Which side of a nullable node an `x-omg-unobserved-branch` reason exempts:
/// `Some(true)` for `null: <reason>`, `Some(false)` for `non-null: <reason>`.
/// A reason naming no side (or no reason after it) exempts nothing.
pub(super) fn exempt_side(node: &Value) -> Option<bool> {
    let text = reason(node, UNOBSERVED_BRANCH)?;
    let (side, rest) = text.split_once(':')?;
    if rest.trim().is_empty() {
        return None;
    }
    match side {
        "null" => Some(true),
        "non-null" => Some(false),
        _ => None,
    }
}
fn describe(branch: &Value) -> String {
    if let Some(reference) = branch["$ref"].as_str() {
        return reference.rsplit('/').next().unwrap_or(reference).to_owned();
    }
    let text = branch.to_string();
    if text.chars().count() > 60 {
        format!("{}...", text.chars().take(57).collect::<String>())
    } else {
        text
    }
}

/// JSON-bearing 2xx response schemas of an operation: `(status, schema)`.
fn success_schemas<'a>(
    document: &'a Value,
    operation: &'a Value,
) -> Result<Vec<(String, &'a Value)>> {
    let mut out = Vec::new();
    for (status, response) in operation["responses"].as_object().into_iter().flatten() {
        if !status.starts_with('2') {
            continue;
        }
        let response = follow(document, response)?.map_or(response, |(target, _)| target);
        for (media, content) in response["content"].as_object().into_iter().flatten() {
            if media == "application/json" || media.ends_with("+json") {
                out.push((
                    status.clone(),
                    content.get("schema").context("media schema missing")?,
                ));
            }
        }
    }
    Ok(out)
}
fn queries(document: &Value, operation: &Value, exempt_only: bool) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for parameter in operation["parameters"].as_array().into_iter().flatten() {
        let parameter = follow(document, parameter)?.map_or(parameter, |(target, _)| target);
        if parameter["in"] == "query"
            && (!exempt_only || reason(parameter, UNOBSERVED_PARAMETER).is_some())
        {
            names.insert(
                parameter["name"]
                    .as_str()
                    .context("query parameter needs a name")?
                    .to_owned(),
            );
        }
    }
    Ok(names)
}
pub(super) fn documented_queries(document: &Value, operation: &Value) -> Result<BTreeSet<String>> {
    queries(document, operation, false)
}
/// Query parameters exempt from positive acceptance, each with a reason.
fn unobservable_queries(document: &Value, operation: &Value) -> Result<BTreeSet<String>> {
    queries(document, operation, true)
}

#[derive(Default)]
struct Expected {
    /// (array data pattern, schema pointer) -> the declaring node carries a
    /// may-be-empty reason.
    arrays: BTreeMap<(String, String), bool>,
    /// (data pattern, union schema pointer) -> branch -> (description, exempt).
    branches: BTreeMap<(String, String), BTreeMap<usize, (String, bool)>>,
}

/// Static walk: every reachable array and union branch, at every data location.
fn expect(
    document: &Value,
    node: &Value,
    data: &str,
    sptr: &str,
    out: &mut Expected,
    stack: &mut Vec<String>,
) -> Result<()> {
    if let Some((target, pointer)) = follow(document, node)?
        && !stack.contains(&pointer)
    {
        stack.push(pointer.clone());
        expect(document, target, data, &pointer, out, stack)?;
        stack.pop();
    }
    if is_array_schema(node) {
        let exempt = reason(node, MAY_BE_EMPTY).is_some();
        out.arrays
            .entry((data.to_owned(), sptr.to_owned()))
            .and_modify(|all| *all &= exempt)
            .or_insert(exempt);
    }
    if nullable(node) {
        let side = exempt_side(node);
        let entry = out
            .branches
            .entry((data.to_owned(), format!("{sptr}/type")))
            .or_default();
        entry
            .entry(0)
            .or_insert(("null".into(), side == Some(true)));
        entry
            .entry(1)
            .or_insert(("non-null".into(), side == Some(false)));
    }
    for keyword in ["oneOf", "anyOf"] {
        for (i, branch) in node[keyword].as_array().into_iter().flatten().enumerate() {
            let exempt = !nullable(branch) && reason(branch, UNOBSERVED_BRANCH).is_some();
            out.branches
                .entry((data.to_owned(), format!("{sptr}/{keyword}")))
                .or_default()
                .entry(i)
                .or_insert((describe(branch), exempt));
            // An unobservable branch has unobservable contents too.
            if !exempt {
                expect(
                    document,
                    branch,
                    data,
                    &format!("{sptr}/{keyword}/{i}"),
                    out,
                    stack,
                )?;
            }
        }
    }
    for (i, branch) in node["allOf"].as_array().into_iter().flatten().enumerate() {
        expect(
            document,
            branch,
            data,
            &format!("{sptr}/allOf/{i}"),
            out,
            stack,
        )?;
    }
    for keyword in ["then", "else"] {
        if let Some(branch) = node.get(keyword) {
            expect(
                document,
                branch,
                data,
                &format!("{sptr}/{keyword}"),
                out,
                stack,
            )?;
        }
    }
    for (name, child) in node["properties"].as_object().into_iter().flatten() {
        let name = segment(name);
        expect(
            document,
            child,
            &format!("{data}/{name}"),
            &format!("{sptr}/properties/{name}"),
            out,
            stack,
        )?;
    }
    if let Some(items) = node.get("items").filter(|v| v.is_object()) {
        expect(
            document,
            items,
            &format!("{data}/*"),
            &format!("{sptr}/items"),
            out,
            stack,
        )?;
    }
    for (i, child) in node["prefixItems"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        expect(
            document,
            child,
            &format!("{data}/*"),
            &format!("{sptr}/prefixItems/{i}"),
            out,
            stack,
        )?;
    }
    // Typed map values live at the wildcard location of their map.
    for (pattern, child) in node["patternProperties"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, child)| child.is_object())
    {
        expect(
            document,
            child,
            &format!("{data}/*"),
            &format!("{sptr}/patternProperties/{}", segment(pattern)),
            out,
            stack,
        )?;
    }
    if let Some(child) = node.get("additionalProperties").filter(|v| v.is_object()) {
        expect(
            document,
            child,
            &format!("{data}/*"),
            &format!("{sptr}/additionalProperties"),
            out,
            stack,
        )?;
    }
    Ok(())
}

/// Dynamic walk: follow the OBSERVED value through the schema, crediting only
/// the branches that value actually matches (validated offline, in-document).
fn observe_node(
    document: &Value,
    node: &Value,
    instance: &Value,
    data: &str,
    sptr: &str,
    seen: &mut OpObserved,
    active: &mut Vec<(String, String)>,
) -> Result<()> {
    // A reference followed twice at the SAME data location made no progress
    // through the instance: a cycle, reported instead of overflowing the stack.
    ensure!(
        active.len() < 256,
        "schema nesting too deep at `{}`",
        show(data)
    );
    let matches = |schema: &Value| -> Result<bool> {
        Ok(super::responses::validator(document, schema)?.is_valid(instance))
    };
    if let Some((target, pointer)) = follow(document, node)? {
        let frame = (pointer.clone(), data.to_owned());
        ensure!(
            !active.contains(&frame),
            "schema reference cycle at {pointer} (data location `{}`)",
            show(data)
        );
        active.push(frame);
        let walked = observe_node(document, target, instance, data, &pointer, seen, active);
        active.pop();
        walked?;
    }
    if let Some(array) = instance.as_array()
        && is_array_schema(node)
    {
        *seen
            .arrays
            .entry((data.to_owned(), sptr.to_owned()))
            .or_insert(false) |= !array.is_empty();
    }
    if nullable(node) {
        seen.branches.insert((
            data.to_owned(),
            format!("{sptr}/type"),
            usize::from(!instance.is_null()),
        ));
    }
    for keyword in ["oneOf", "anyOf"] {
        for (i, branch) in node[keyword].as_array().into_iter().flatten().enumerate() {
            if matches(branch)? {
                seen.branches
                    .insert((data.to_owned(), format!("{sptr}/{keyword}"), i));
                observe_node(
                    document,
                    branch,
                    instance,
                    data,
                    &format!("{sptr}/{keyword}/{i}"),
                    seen,
                    active,
                )?;
            }
        }
    }
    for (i, branch) in node["allOf"].as_array().into_iter().flatten().enumerate() {
        observe_node(
            document,
            branch,
            instance,
            data,
            &format!("{sptr}/allOf/{i}"),
            seen,
            active,
        )?;
    }
    if let Some(condition) = node.get("if") {
        let keyword = if matches(condition)? { "then" } else { "else" };
        if let Some(branch) = node.get(keyword) {
            observe_node(
                document,
                branch,
                instance,
                data,
                &format!("{sptr}/{keyword}"),
                seen,
                active,
            )?;
        }
    }
    if let Some(object) = instance.as_object() {
        for (name, child) in node["properties"].as_object().into_iter().flatten() {
            if let Some(value) = object.get(name) {
                let name = segment(name);
                observe_node(
                    document,
                    child,
                    value,
                    &format!("{data}/{name}"),
                    &format!("{sptr}/properties/{name}"),
                    seen,
                    active,
                )?;
            }
        }
    }
    if let Some(object) = instance.as_object() {
        let patterns: Vec<(&String, &Value, _)> = node["patternProperties"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, child)| child.is_object())
            .map(|(pattern, child)| {
                super::validation::compile_schema(
                    &serde_json::json!({"type":"string","pattern":pattern}),
                )
                .map(|validator| (pattern, child, validator))
            })
            .collect::<Result<_>>()?;
        let declared = node["properties"].as_object();
        for (name, value) in object {
            let name_value = Value::String(name.clone());
            let mut selected = false;
            for (pattern, child, validator) in &patterns {
                if validator.is_valid(&name_value) {
                    selected = true;
                    observe_node(
                        document,
                        child,
                        value,
                        &format!("{data}/*"),
                        &format!("{sptr}/patternProperties/{}", segment(pattern)),
                        seen,
                        active,
                    )?;
                }
            }
            if !selected
                && declared.is_none_or(|d| !d.contains_key(name))
                && let Some(child) = node.get("additionalProperties").filter(|v| v.is_object())
            {
                observe_node(
                    document,
                    child,
                    value,
                    &format!("{data}/*"),
                    &format!("{sptr}/additionalProperties"),
                    seen,
                    active,
                )?;
            }
        }
    }
    if let Some(array) = instance.as_array() {
        // `items` governs only the elements AFTER the positional prefix.
        let prefix = node["prefixItems"].as_array().map_or(0, Vec::len);
        if let Some(items) = node.get("items").filter(|v| v.is_object()) {
            for value in array.iter().skip(prefix) {
                observe_node(
                    document,
                    items,
                    value,
                    &format!("{data}/*"),
                    &format!("{sptr}/items"),
                    seen,
                    active,
                )?;
            }
        }
        for (i, child) in node["prefixItems"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            if let Some(value) = array.get(i) {
                observe_node(
                    document,
                    child,
                    value,
                    &format!("{data}/*"),
                    &format!("{sptr}/prefixItems/{i}"),
                    seen,
                    active,
                )?;
            }
        }
    }
    Ok(())
}

impl Observed {
    /// Query parameter names carried by one POSITIVE request to `op_key`.
    pub(super) fn observe_query(&mut self, op_key: &str, names: &[String]) {
        self.0
            .entry(op_key.to_owned())
            .or_default()
            .queries
            .extend(names.iter().cloned());
    }

    /// A positive JSON success body of `op_key`; non-success statuses and
    /// non-JSON representations carry no coverage.
    pub(super) fn observe_body(
        &mut self,
        document: &Value,
        op_key: &str,
        operation: &Value,
        status: u16,
        body: &Value,
    ) -> Result<()> {
        let status = status.to_string();
        for (declared, schema) in success_schemas(document, operation)? {
            if declared == status {
                let seen = self.0.entry(op_key.to_owned()).or_default();
                observe_node(
                    document,
                    schema,
                    body,
                    "",
                    &format!("/response/{status}"),
                    seen,
                    &mut Vec::new(),
                )?;
            }
        }
        Ok(())
    }

    /// Fail, listing every gap, unless the operation's positive observations
    /// cover its documented query parameters, arrays and union branches.
    pub(super) fn check(&self, document: &Value, op_key: &str, operation: &Value) -> Result<()> {
        let empty = OpObserved::default();
        let seen = self.0.get(op_key).unwrap_or(&empty);
        let mut gaps = Vec::new();
        let unobservable = unobservable_queries(document, operation)?;
        for name in &unobservable {
            if seen.queries.contains(name) {
                gaps.push(format!(
                    "query parameter `{name}` carries {UNOBSERVED_PARAMETER} but a positive request accepted it: the exemption is stale, remove it"
                ));
            }
        }
        for name in documented_queries(document, operation)? {
            if !seen.queries.contains(&name) && !unobservable.contains(&name) {
                gaps.push(format!(
                    "query parameter `{name}` is documented but no positive request accepted it"
                ));
            }
        }
        let mut expected = Expected::default();
        for (status, schema) in success_schemas(document, operation)? {
            expect(
                document,
                schema,
                "",
                &format!("/response/{status}"),
                &mut expected,
                &mut Vec::new(),
            )?;
        }
        for (key, exempt) in &expected.arrays {
            let (pattern, schema) = key;
            if *exempt && seen.arrays.get(key) == Some(&true) {
                gaps.push(format!(
                    "array `{}` ({schema}) carries {MAY_BE_EMPTY} but was non-empty in a positive response: the exemption is stale, remove it",
                    show(pattern)
                ));
            }
            if !exempt && seen.arrays.get(key) != Some(&true) {
                gaps.push(format!(
                    "array `{}` ({schema}) was never non-empty in a positive response (populate it, or annotate the array with {MAY_BE_EMPTY})",
                    show(pattern)
                ));
            }
        }
        for ((data, union), branches) in &expected.branches {
            for (index, (description, exempt)) in branches {
                if *exempt
                    && seen
                        .branches
                        .contains(&(data.clone(), union.clone(), *index))
                {
                    gaps.push(format!(
                        "`{}` ({union}): branch {index} `{description}` carries {UNOBSERVED_BRANCH} but was observed: the exemption is stale, remove it",
                        show(data)
                    ));
                }
                if !exempt
                    && !seen
                        .branches
                        .contains(&(data.clone(), union.clone(), *index))
                {
                    gaps.push(format!(
                        "`{}` ({union}): branch {index} `{description}` never matched an observed value (observe it, or annotate the branch with {UNOBSERVED_BRANCH})",
                        show(data)
                    ));
                }
            }
        }
        ensure!(
            gaps.is_empty(),
            "{op_key}: contract coverage gaps:\n  - {}",
            gaps.join("\n  - ")
        );
        Ok(())
    }
}

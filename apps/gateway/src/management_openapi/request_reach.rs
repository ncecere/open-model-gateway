//! Which DECLARED request locations did the vector runs of an operation reach?
//!
//! The vector walk only mutates what is PRESENT in the valid body (or a seed):
//! an optional property the base omits, a map value, a positional index or a
//! `oneOf` arm the base does not match is silently never probed. This module
//! names those locations so `finish` can fail on them.
//!
//! A location is identified by its SCHEMA path from the request schema root
//! (references are transparent): `/properties/reason`, `/oneOf/1/prefixItems/2`,
//! `/properties/details/additionalProperties`, `/items`. Declared locations are:
//! every property, every `patternProperties` / schema-valued
//! `additionalProperties` value, every `prefixItems` index and the `items` tail
//! and every `oneOf` / `anyOf` arm. `allOf` members and `then`/`else` contents are
//! walked transparently. A location is REACHED by a base body when the body
//! carries it (and, for an arm, matches the arm).
//!
//! Exempt only with `x-omg-unprobed-location: "<reason>"` on the location's
//! schema (property, map value, element or arm); the contents of an exempt
//! location are exempt too. Messages name schema paths, never instance values.
use super::coverage::follow;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const UNPROBED: &str = "x-omg-unprobed-location";

pub(super) struct Declared {
    pub(super) kind: &'static str,
    pub(super) exempt: bool,
}

fn segment(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}

/// A reviewed reason on the schema or on any schema it references.
fn exempt(document: &Value, node: &Value) -> bool {
    let mut node = node;
    for _ in 0..16 {
        if node
            .get(UNPROBED)
            .and_then(Value::as_str)
            .is_some_and(|reason| !reason.trim().is_empty())
        {
            return true;
        }
        match follow(document, node) {
            Ok(Some((target, _))) => node = target,
            _ => return false,
        }
    }
    false
}

fn pattern_matches(pattern: &str, name: &str) -> Result<bool> {
    Ok(
        super::validation::compile_schema(&json!({"type":"string","pattern":pattern}))?
            .is_valid(&Value::String(name.to_owned())),
    )
}

fn declare_child(
    document: &Value,
    child: &Value,
    key: String,
    kind: &'static str,
    out: &mut BTreeMap<String, Declared>,
    stack: &mut Vec<String>,
    depth: usize,
) -> Result<()> {
    if !child.is_object() {
        return Ok(());
    }
    let excused = exempt(document, child);
    out.insert(
        key.clone(),
        Declared {
            kind,
            exempt: excused,
        },
    );
    if !excused {
        declare(document, child, &key, out, stack, depth + 1)?;
    }
    Ok(())
}

fn declare(
    document: &Value,
    node: &Value,
    key: &str,
    out: &mut BTreeMap<String, Declared>,
    stack: &mut Vec<String>,
    depth: usize,
) -> Result<()> {
    ensure!(depth < 48, "request schema nesting too deep");
    if let Some((target, pointer)) = follow(document, node)?
        && !stack.contains(&pointer)
    {
        stack.push(pointer);
        declare(document, target, key, out, stack, depth + 1)?;
        stack.pop();
    }
    for (name, child) in node["properties"].as_object().into_iter().flatten() {
        let child_key = format!("{key}/properties/{}", segment(name));
        declare_child(document, child, child_key, "property", out, stack, depth)?;
    }
    for (pattern, child) in node["patternProperties"].as_object().into_iter().flatten() {
        let child_key = format!("{key}/patternProperties/{}", segment(pattern));
        declare_child(document, child, child_key, "map value", out, stack, depth)?;
    }
    if let Some(child) = node.get("additionalProperties") {
        declare_child(
            document,
            child,
            format!("{key}/additionalProperties"),
            "map value",
            out,
            stack,
            depth,
        )?;
    }
    for (i, child) in node["prefixItems"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let child_key = format!("{key}/prefixItems/{i}");
        declare_child(
            document,
            child,
            child_key,
            "array element",
            out,
            stack,
            depth,
        )?;
    }
    if let Some(child) = node.get("items") {
        declare_child(
            document,
            child,
            format!("{key}/items"),
            "array element",
            out,
            stack,
            depth,
        )?;
    }
    for keyword in ["oneOf", "anyOf"] {
        for (i, child) in node[keyword].as_array().into_iter().flatten().enumerate() {
            let child_key = format!("{key}/{keyword}/{i}");
            declare_child(document, child, child_key, "union arm", out, stack, depth)?;
        }
    }
    for (i, child) in node["allOf"].as_array().into_iter().flatten().enumerate() {
        declare(
            document,
            child,
            &format!("{key}/allOf/{i}"),
            out,
            stack,
            depth + 1,
        )?;
    }
    for keyword in ["then", "else"] {
        if let Some(child) = node.get(keyword).filter(|c| c.is_object()) {
            declare(
                document,
                child,
                &format!("{key}/{keyword}"),
                out,
                stack,
                depth + 1,
            )?;
        }
    }
    Ok(())
}

/// Every declared request location of a request schema, by schema path.
pub(super) fn declared(document: &Value, schema: &Value) -> Result<BTreeMap<String, Declared>> {
    let mut out = BTreeMap::new();
    declare(document, schema, "", &mut out, &mut Vec::new(), 0)?;
    Ok(out)
}

/// What a reach computation may claim: with `walked`, only the instance
/// locations (JSON pointers, `""` for the root) a vector walk really mutated.
struct Ctx<'a> {
    document: &'a Value,
    walked: Option<&'a BTreeSet<String>>,
}
impl Ctx<'_> {
    fn counts(&self, at: &str) -> bool {
        self.walked.is_none_or(|walked| walked.contains(at))
    }
}

#[allow(clippy::too_many_arguments)]
fn visit(
    ctx: &Ctx,
    child: &Value,
    key: String,
    instance: &Value,
    at: &str,
    out: &mut BTreeSet<String>,
    stack: &mut Vec<(String, String)>,
    depth: usize,
) -> Result<()> {
    if !child.is_object() || !ctx.counts(at) {
        return Ok(());
    }
    out.insert(key.clone());
    reach(ctx, child, instance, &key, at, out, stack, depth + 1)
}

#[allow(clippy::too_many_arguments)]
fn reach(
    ctx: &Ctx,
    node: &Value,
    instance: &Value,
    key: &str,
    at: &str,
    out: &mut BTreeSet<String>,
    stack: &mut Vec<(String, String)>,
    depth: usize,
) -> Result<()> {
    let document = ctx.document;
    ensure!(depth < 48, "request body nesting too deep");
    if let Some((target, pointer)) = follow(document, node)? {
        let frame = (pointer, key.to_owned());
        if !stack.contains(&frame) {
            stack.push(frame);
            let walked = reach(ctx, target, instance, key, at, out, stack, depth + 1);
            stack.pop();
            walked?;
        }
    }
    let matches = |schema: &Value| -> Result<bool> {
        Ok(super::responses::validator(document, schema)?.is_valid(instance))
    };
    for keyword in ["oneOf", "anyOf"] {
        for (i, arm) in node[keyword].as_array().into_iter().flatten().enumerate() {
            if arm.is_object() && matches(arm)? {
                visit(
                    ctx,
                    arm,
                    format!("{key}/{keyword}/{i}"),
                    instance,
                    at,
                    out,
                    stack,
                    depth,
                )?;
            }
        }
    }
    for (i, arm) in node["allOf"].as_array().into_iter().flatten().enumerate() {
        reach(
            ctx,
            arm,
            instance,
            &format!("{key}/allOf/{i}"),
            at,
            out,
            stack,
            depth + 1,
        )?;
    }
    if let Some(condition) = node.get("if") {
        let keyword = if matches(condition)? { "then" } else { "else" };
        if let Some(branch) = node.get(keyword).filter(|b| b.is_object()) {
            reach(
                ctx,
                branch,
                instance,
                &format!("{key}/{keyword}"),
                at,
                out,
                stack,
                depth + 1,
            )?;
        }
    }
    if let Some(object) = instance.as_object() {
        let properties = node["properties"].as_object();
        let patterns: Vec<(&String, &Value)> = node["patternProperties"]
            .as_object()
            .into_iter()
            .flatten()
            .collect();
        for (name, value) in object {
            let child_at = format!("{at}/{name}");
            if let Some(child) = properties.and_then(|p| p.get(name)) {
                visit(
                    ctx,
                    child,
                    format!("{key}/properties/{}", segment(name)),
                    value,
                    &child_at,
                    out,
                    stack,
                    depth,
                )?;
            }
            let mut selected = properties.is_some_and(|p| p.contains_key(name));
            for (pattern, child) in &patterns {
                if pattern_matches(pattern, name)? {
                    selected = true;
                    visit(
                        ctx,
                        child,
                        format!("{key}/patternProperties/{}", segment(pattern)),
                        value,
                        &child_at,
                        out,
                        stack,
                        depth,
                    )?;
                }
            }
            if !selected && let Some(child) = node.get("additionalProperties") {
                visit(
                    ctx,
                    child,
                    format!("{key}/additionalProperties"),
                    value,
                    &child_at,
                    out,
                    stack,
                    depth,
                )?;
            }
        }
    }
    if let Some(array) = instance.as_array() {
        let prefix = node["prefixItems"].as_array().map_or(0, Vec::len);
        for (i, child) in node["prefixItems"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            if let Some(value) = array.get(i) {
                visit(
                    ctx,
                    child,
                    format!("{key}/prefixItems/{i}"),
                    value,
                    &format!("{at}/{i}"),
                    out,
                    stack,
                    depth,
                )?;
            }
        }
        if let Some(child) = node.get("items") {
            for (i, value) in array.iter().enumerate().skip(prefix) {
                visit(
                    ctx,
                    child,
                    format!("{key}/items"),
                    value,
                    &format!("{at}/{i}"),
                    out,
                    stack,
                    depth,
                )?;
            }
        }
    }
    Ok(())
}

/// The declared locations a base body carries (and the arms it matches).
pub(super) fn reached(
    document: &Value,
    schema: &Value,
    instance: &Value,
) -> Result<BTreeSet<String>> {
    let ctx = Ctx {
        document,
        walked: None,
    };
    let mut out = BTreeSet::new();
    reach(&ctx, schema, instance, "", "", &mut out, &mut Vec::new(), 0)?;
    Ok(out)
}

/// The declared locations of `instance` that a vector walk really MUTATED:
/// `walked` are the JSON pointers (`""` for the root) the walk generated
/// vectors for. A location the body merely contains is not reached.
pub(super) fn reached_by_walk(
    document: &Value,
    schema: &Value,
    instance: &Value,
    walked: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let ctx = Ctx {
        document,
        walked: Some(walked),
    };
    let mut out = BTreeSet::new();
    reach(&ctx, schema, instance, "", "", &mut out, &mut Vec::new(), 0)?;
    Ok(out)
}

/// `(schema path, kind)` of every declared, non-exempt location never reached.
pub(super) fn unreached(
    document: &Value,
    schema: &Value,
    reached: &BTreeSet<String>,
) -> Result<Vec<(String, &'static str)>> {
    Ok(declared(document, schema)?
        .into_iter()
        .filter(|(key, location)| !location.exempt && !reached.contains(key))
        .map(|(key, location)| (key, location.kind))
        .collect())
}

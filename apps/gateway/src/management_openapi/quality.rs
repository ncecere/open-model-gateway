//! Fail closed on vacuous completed schemas, including every reachable
//! component and inline branch. Conjunctive refinements inherit the closed
//! object's constraints; disjunctions may not add an unconstrained escape.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const ARBITRARY_JSON: &[(&str, &str)] = &[
    // alert_events.details is a stored jsonb object, not a deserialized DTO.
    // Its reviewed privacy refinements still constrain personal identities.
    (
        "/components/schemas/IdentityNotification/properties/details",
        "Stored alert_events.details is an arbitrary producer-specific JSON object; monetary and identity refinements remain enforced.",
    ),
    // Subject export returns the stored audit value verbatim. Closing or
    // inventing its keys would misdescribe historical records and new actions.
    (
        "/components/schemas/get_api_v1_me_exportResponse/properties/audit/properties/items/items/properties/metadata",
        "Stored audit_events.metadata is exported verbatim as JSON; its vocabulary depends on the audited action.",
    ),
];

fn arbitrary(location: &str, node: &Value) -> bool {
    ARBITRARY_JSON
        .iter()
        .any(|(pointer, reason)| *pointer == location && node["$comment"] == *reason)
}
fn segment(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}
fn reference<'a>(document: &'a Value, node: &Value) -> Result<(&'a Value, String)> {
    let r = node["$ref"]
        .as_str()
        .context("schema reference must be a string")?;
    ensure!(r.starts_with("#/"), "external completed schema reference");
    ensure!(
        !ARBITRARY_JSON
            .iter()
            .any(|(pointer, _)| *pointer == &r[1..]),
        "arbitrary stored JSON may not be aliased as a standalone schema"
    );
    Ok((
        document
            .pointer(&r[1..])
            .context("unresolved completed schema")?,
        r[1..].to_owned(),
    ))
}
#[derive(Clone, Default)]
struct Facts {
    bounded: bool,
    object: bool,
    array: bool,
    items: bool,
    closed: bool,
    required: bool,
    arbitrary: bool,
    properties: BTreeMap<String, Facts>,
}
impl Facts {
    fn merge(&mut self, other: Self) {
        self.bounded |= other.bounded;
        self.object |= other.object;
        self.array |= other.array;
        self.items |= other.items;
        self.closed |= other.closed;
        self.required |= other.required;
        self.arbitrary |= other.arbitrary;
        for (name, facts) in other.properties {
            self.properties.entry(name).or_default().merge(facts);
        }
    }
}
fn facts(
    document: &Value,
    node: &Value,
    location: &str,
    seen: &mut BTreeSet<String>,
) -> Result<Facts> {
    let mut result = Facts {
        bounded: node.get("type").is_some()
            || node.get("const").is_some()
            || node.get("enum").is_some()
            || node == &Value::Bool(false),
        object: node["type"] == "object"
            || node["type"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "object"))
            || node.get("properties").is_some()
            || node.get("required").is_some(),
        array: node["type"] == "array"
            || node["type"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "array"))
            || node.get("items").is_some()
            || node.get("prefixItems").is_some()
            || node.get("contains").is_some(),
        // The item schema itself is checked recursively below, with a fresh
        // domain: the enclosing array/object cannot constrain its elements.
        items: node.get("items").is_some(),
        closed: node["additionalProperties"] == false,
        required: node["required"].is_array(),
        arbitrary: arbitrary(location, node),
        ..Facts::default()
    };
    if node.get("$ref").is_some() {
        let (target, pointer) = reference(document, node)?;
        if seen.insert(pointer.clone()) {
            result.merge(facts(document, target, &pointer, seen)?);
            seen.remove(&pointer);
        }
    }
    for (i, child) in node["allOf"].as_array().into_iter().flatten().enumerate() {
        result.merge(facts(
            document,
            child,
            &format!("{location}/allOf/{i}"),
            seen,
        )?);
    }
    // A union supplies a positive domain only when every alternative does.
    for keyword in ["anyOf", "oneOf"] {
        if let Some(branches) = node[keyword].as_array() {
            let bounded = branches
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    facts(document, b, &format!("{location}/{keyword}/{i}"), seen)
                        .map(|f| f.bounded || f.arbitrary)
                })
                .collect::<Result<Vec<_>>>()?;
            result.bounded |= !bounded.is_empty() && bounded.into_iter().all(|v| v);
        }
    }
    for (name, child) in node["properties"].as_object().into_iter().flatten() {
        result
            .properties
            .entry(name.clone())
            .or_default()
            .merge(facts(
                document,
                child,
                &format!("{location}/properties/{}", segment(name)),
                seen,
            )?);
    }
    Ok(result)
}

/// Every keyword a completed schema may use: those `schema` traverses or
/// constrains, plus scalar assertions and annotations that admit no subschema.
const TRAVERSED_OR_ANNOTATION: &[&str] = &[
    "$ref",
    "type",
    "enum",
    "const",
    "format",
    "pattern",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minContains",
    "maxContains",
    "minProperties",
    "maxProperties",
    "required",
    "dependentRequired",
    "properties",
    "patternProperties",
    "additionalProperties",
    "unevaluatedProperties",
    "items",
    "prefixItems",
    "unevaluatedItems",
    "additionalItems",
    "contains",
    "propertyNames",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "description",
    "title",
    "$comment",
    "default",
    "examples",
    "deprecated",
    "readOnly",
    "writeOnly",
    "contentMediaType",
    "contentEncoding",
];

fn schema(
    document: &Value,
    node: &Value,
    location: &str,
    inherited: &Facts,
    seen: &mut BTreeSet<String>,
) -> Result<()> {
    if node == &Value::Bool(false) {
        return Ok(());
    } // explicit absence / forbidden tuple items
    ensure!(
        node.is_object(),
        "permissive boolean completed schema at {location}"
    );
    ensure!(
        !node.as_object().unwrap().is_empty(),
        "empty completed schema at {location}"
    );
    // Allow-list: a keyword this checker does not traverse (dependentSchemas,
    // $defs, $dynamicRef, ...) could hide a permissive branch, so it is refused
    // rather than silently skipped. Vendor extensions carry no validation.
    for keyword in node.as_object().unwrap().keys() {
        ensure!(
            keyword.starts_with("x-") || TRAVERSED_OR_ANNOTATION.contains(&keyword.as_str()),
            "unsupported keyword {keyword} in a completed schema at {location}"
        );
    }
    let mut effective = inherited.clone();
    effective.merge(facts(document, node, location, &mut BTreeSet::new())?);
    ensure!(
        effective.bounded || effective.arbitrary,
        "unbounded completed schema at {location}"
    );
    if effective.object && !effective.arbitrary {
        ensure!(
            effective.closed,
            "completed object is not closed at {location}"
        );
        // [] is intentional for a closed DTO whose fields all have defaults.
        ensure!(
            effective.required,
            "completed object must explicitly declare required fields at {location}"
        );
    }
    if effective.array && !effective.arbitrary {
        ensure!(
            effective.items,
            "completed array must constrain items at {location}"
        );
    }
    // Even typed wildcard values admit arbitrary property *names*. The exact
    // reviewed stored-JSON pointer, not inherited facts or a copied comment,
    // must authorize these maps. The notification monetary pattern is covered
    // by its existing details entry in ARBITRARY_JSON.
    ensure!(
        arbitrary(location, node)
            || (node["patternProperties"]
                .as_object()
                .is_none_or(|m| m.values().all(|v| v == false))
                && !node["additionalProperties"].is_object()
                && !node["unevaluatedProperties"].is_object()),
        "wildcard maps require a reviewed ARBITRARY_JSON pointer at {location}"
    );
    if node.get("$ref").is_some() {
        let (target, pointer) = reference(document, node)?;
        if seen.insert(pointer.clone()) {
            schema(document, target, &pointer, &Facts::default(), seen)?;
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        for (i, child) in node[keyword].as_array().into_iter().flatten().enumerate() {
            let context = if keyword == "prefixItems" {
                Facts::default()
            } else {
                effective.clone()
            };
            // Even a conjunctive/union branch may never be {} or true.
            schema(
                document,
                child,
                &format!("{location}/{keyword}/{i}"),
                &context,
                seen,
            )?;
        }
    }
    for (name, child) in node["properties"].as_object().into_iter().flatten() {
        schema(
            document,
            child,
            &format!("{location}/properties/{}", segment(name)),
            effective.properties.get(name).unwrap_or(&Facts::default()),
            seen,
        )?;
    }
    for (pattern, child) in node["patternProperties"].as_object().into_iter().flatten() {
        schema(
            document,
            child,
            &format!("{location}/patternProperties/{}", segment(pattern)),
            &Facts::default(),
            seen,
        )?;
    }
    for keyword in [
        "items",
        "additionalProperties",
        "unevaluatedProperties",
        "unevaluatedItems",
        "additionalItems",
        "contains",
        "propertyNames",
    ] {
        if let Some(child) = node.get(keyword) {
            schema(
                document,
                child,
                &format!("{location}/{keyword}"),
                &Facts::default(),
                seen,
            )?;
        }
    }
    for keyword in ["if", "then", "else", "not"] {
        if let Some(child) = node.get(keyword) {
            schema(
                document,
                child,
                &format!("{location}/{keyword}"),
                &effective,
                seen,
            )?;
        }
    }
    Ok(())
}

pub(super) fn completed(document: &Value) -> Result<()> {
    for (name, node) in document["components"]["schemas"]
        .as_object()
        .context("schemas missing")?
    {
        if node["x-omg-schema-status"] != "incomplete" {
            schema(
                document,
                node,
                &format!("/components/schemas/{}", segment(name)),
                &Facts::default(),
                &mut BTreeSet::new(),
            )?;
        }
    }
    // Generated reusable responses/headers are complete contracts too, even
    // when only an area that is still incomplete references them today.
    for kind in ["responses", "headers", "parameters", "requestBodies"] {
        for node in document["components"][kind]
            .as_object()
            .into_iter()
            .flat_map(|m| m.values())
        {
            operation(document, node)?;
        }
    }
    for (path, item) in document["paths"].as_object().context("paths missing")? {
        if super::responses::completed(super::responses::area(path)) {
            for op in item.as_object().context("operation map missing")?.values() {
                operation(document, op)?;
            }
        }
    }
    Ok(())
}

pub(super) fn operation(document: &Value, operation: &Value) -> Result<()> {
    fn discover(
        document: &Value,
        node: &Value,
        location: &str,
        seen: &mut BTreeSet<String>,
    ) -> Result<()> {
        if node.get("$ref").is_some() {
            let (target, pointer) = reference(document, node)?;
            if pointer.starts_with("/components/schemas/") {
                if seen.insert(pointer.clone()) {
                    schema(document, target, &pointer, &Facts::default(), seen)?;
                }
            } else if seen.insert(pointer.clone()) {
                discover(document, target, &pointer, seen)?;
            }
        }
        match node {
            Value::Object(map) => {
                for (key, child) in map {
                    let pointer = format!("{location}/{}", segment(key));
                    if key == "schema" {
                        schema(document, child, &pointer, &Facts::default(), seen)?;
                    } else {
                        discover(document, child, &pointer, seen)?;
                    }
                }
            }
            Value::Array(values) => {
                for (i, child) in values.iter().enumerate() {
                    discover(document, child, &format!("{location}/{i}"), seen)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    discover(document, operation, "/operation", &mut BTreeSet::new())
}

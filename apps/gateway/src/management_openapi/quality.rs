//! Fail closed on vacuous completed schemas, including every reachable
//! component and inline branch. Conjunctive refinements inherit the closed
//! object's constraints; disjunctions may not add an unconstrained escape.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const ARBITRARY_JSON: &[(&str, &str)] = &[
    (
        "/components/schemas/SettingsAlertEvent/properties/details",
        "EVENT_JSON returns non-ceiling alert_events.details verbatim as stored producer-specific JSON; monetary refinements apply and ceiling details use the closed shared IdentityCeilingDetails schema.",
    ),
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

/// Decimal RATE fields that legitimately carry fractions, so the integer
/// micro-USD string rule does not apply. `*suffix` entries match a name suffix.
/// Every addition needs review and a justification: an amount that is merely
/// inconvenient to constrain does not belong here.
pub(super) const DECIMAL_RATES: &[(&str, &str)] = &[
    (
        "conversion_residual_microusd",
        "Fractional micro-USD remainder of a decimal currency conversion; a rate-like decimal, not a whole-micro-USD amount.",
    ),
    (
        "*_per_million",
        "Price rates are decimal micro-USD per million tokens (they may carry fractions); they are rates, not integer amounts.",
    ),
];

fn money_name(name: &str) -> bool {
    name.ends_with("_microusd") || name.contains("_microusd_")
}
fn decimal_rate(name: &str) -> bool {
    DECIMAL_RATES
        .iter()
        .any(|(entry, _)| match entry.strip_prefix('*') {
            Some(suffix) => name.ends_with(suffix),
            None => *entry == name,
        })
}
#[derive(PartialEq)]
enum Money {
    Amount,
    Null,
    Other,
}
/// Integer micro-USD wire strings: unsigned `^(0|[1-9][0-9]*)$` or signed
/// `^(0|-?[1-9][0-9]*)$`. Probes reject leading zeros, `-0`, signs, fractions,
/// exponents and whitespace, so `.*`, `^[0-9]+$` or `^-?(0|[1-9][0-9]*)$` fail.
fn money_pattern(pattern: &Value) -> bool {
    let Some(pattern) = pattern.as_str().filter(|p| !p.is_empty()) else {
        return false;
    };
    let Ok(validator) =
        super::validation::compile_schema(&serde_json::json!({"type":"string","pattern":pattern}))
    else {
        return false;
    };
    let accepts = |v: &str| validator.is_valid(&Value::String(v.to_owned()));
    let mut ok = ["0", "1", "10", "9007199254740993"]
        .iter()
        .all(|v| accepts(v))
        && ![
            "", "abc", "1.5", "1e6", " 1", "1 ", "01", "007", "-0", "-00", "+1", "0x1", "1_0", "
1",
        ]
        .iter()
        .any(|v| accepts(v));
    if accepts("-1") {
        ok = ok
            && accepts("-10")
            && accepts("-9007199254740993")
            && !["-01", "--1", "-", "-1.5"].iter().any(|v| accepts(v));
    }
    ok
}
fn money(document: &Value, node: &Value, depth: usize) -> Result<Money> {
    ensure!(depth < 16, "money schema reference chain too deep");
    if node.get("$ref").is_some() {
        let (target, _) = reference(document, node)?;
        return money(document, target, depth + 1);
    }
    if node["type"] == "null" || node.get("const") == Some(&Value::Null) {
        return Ok(Money::Null);
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(branches) = node[keyword].as_array() {
            let kinds = branches
                .iter()
                .map(|b| money(document, b, depth + 1))
                .collect::<Result<Vec<_>>>()?;
            return Ok(if kinds.is_empty() || kinds.contains(&Money::Other) {
                Money::Other
            } else if kinds.contains(&Money::Amount) {
                Money::Amount
            } else {
                Money::Null
            });
        }
    }
    if let Some(all) = node["allOf"].as_array() {
        let mut found = false;
        for child in all {
            found |= money(document, child, depth + 1)? == Money::Amount;
        }
        return Ok(if found { Money::Amount } else { Money::Other });
    }
    let kinds = match &node["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(all) => all.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    let string_or_null =
        kinds.contains(&"string") && kinds.iter().all(|k| ["string", "null"].contains(k));
    Ok(if string_or_null && money_pattern(&node["pattern"]) {
        Money::Amount
    } else {
        Money::Other
    })
}

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
    /// Set only by a oneOf/anyOf parent for its direct branches.
    branch: bool,
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
    let in_branch = inherited.branch;
    let mut effective = inherited.clone();
    effective.branch = false;
    effective.merge(facts(document, node, location, &mut BTreeSet::new())?);
    // Coverage exemptions are reviewed claims: each needs a real reason and the
    // right kind of node, or it would silently excuse unrelated schemas.
    if let Some(value) = node.get(super::coverage::MAY_BE_EMPTY) {
        ensure!(
            value.as_str().is_some_and(|r| !r.trim().is_empty()),
            "{} needs a non-empty reason at {location}",
            super::coverage::MAY_BE_EMPTY
        );
        ensure!(
            node["type"] == "array"
                || node["type"]
                    .as_array()
                    .is_some_and(|t| t.iter().any(|v| v == "array")),
            "{} is only valid on an array schema at {location}",
            super::coverage::MAY_BE_EMPTY
        );
    }
    if let Some(value) = node.get(super::coverage::UNOBSERVED_BRANCH) {
        ensure!(
            value.as_str().is_some_and(|r| !r.trim().is_empty()),
            "{} needs a non-empty reason at {location}",
            super::coverage::UNOBSERVED_BRANCH
        );
        ensure!(
            in_branch || super::coverage::nullable(node),
            "{} is only valid on a oneOf/anyOf branch or a nullable node at {location}",
            super::coverage::UNOBSERVED_BRANCH
        );
    }
    // Reviewed arbitrary JSON still declares its JSON type (e.g. "object").
    if arbitrary(location, node) {
        ensure!(
            node["type"].is_string() || node["type"].as_array().is_some_and(|t| !t.is_empty()),
            "arbitrary stored JSON must declare a type at {location}"
        );
    }
    // Money wire values: integer micro-USD strings constrained by a pattern
    // that rejects leading zeros and `-0` (request and response alike).
    for (name, child) in node["properties"].as_object().into_iter().flatten() {
        if money_name(name) && !decimal_rate(name) {
            ensure!(
                money(document, child, 0)? != Money::Other,
                "money property `{name}` at {location} must be a string constrained by an integer micro-USD pattern (^(0|[1-9][0-9]*)$ or ^(0|-?[1-9][0-9]*)$), or a nullable form of one"
            );
        }
    }
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
            let mut context = if keyword == "prefixItems" {
                Facts::default()
            } else {
                effective.clone()
            };
            context.branch = matches!(keyword, "anyOf" | "oneOf");
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

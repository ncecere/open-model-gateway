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
    // The platform subject export (slice 2) is the same stored audit value.
    (
        "/components/schemas/AccessPlatformExport/properties/audit/properties/items/items/properties/metadata",
        "Stored audit_events.metadata is exported verbatim as JSON; its vocabulary depends on the audited action.",
    ),
    // Slice 6: audit rows are stored jsonb objects, not deserialized DTOs.
    (
        "/components/schemas/S6WorkspaceAudit/properties/metadata",
        "Stored audit_events.metadata object. audit() keeps only typed safe keys (flags, counts, closed vocabulary values) for new rows; older rows and database-trigger rows carry action-specific keys.",
    ),
    (
        "/components/schemas/S6PlatformAudit/properties/metadata",
        "Stored audit_events.metadata object. audit() keeps only typed safe keys (flags, counts, closed vocabulary values) for new rows; older rows and database-trigger rows carry action-specific keys.",
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

/// Annotations that excuse ONE gap at ONE location. Each is refused anywhere
/// under a component that more than one place references (they would silently
/// apply at every reference), and an exemption lives only on a location owned
/// by a single operation.
const EXEMPTIONS: [&str; 6] = [
    super::coverage::MAY_BE_EMPTY,
    super::coverage::UNOBSERVED_BRANCH,
    super::coverage::UNOBSERVED_PARAMETER,
    super::request_reach::UNPROBED,
    super::http_vectors::UNSENDABLE,
    super::http_vectors::PATTERN_BOUNDARIES,
];

/// `/components/<kind>/<name>` when `path` is that component or lies under it.
fn component_of(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/components/")?;
    let mut parts = rest.splitn(3, '/');
    let (kind, name) = (parts.next()?, parts.next()?);
    Some(format!("/components/{kind}/{name}"))
}

/// Every `$ref` target (a JSON pointer) with the component that holds the
/// reference (`None` for a reference outside `components`).
fn ref_sites(document: &Value) -> Vec<(String, Option<String>)> {
    fn walk(node: &Value, path: &str, out: &mut Vec<(String, Option<String>)>) {
        match node {
            Value::Object(map) => {
                if let Some(target) = map
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|r| r.strip_prefix('#'))
                {
                    out.push((target.to_owned(), component_of(path)));
                }
                for (key, child) in map {
                    // The generated slot aliases repeat the reference of the
                    // request/response schema they name: not a second place.
                    if matches!(
                        key.as_str(),
                        "x-omg-request-schema" | "x-omg-response-schema"
                    ) {
                        continue;
                    }
                    walk(child, &format!("{path}/{}", segment(key)), out);
                }
            }
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    walk(child, &format!("{path}/{i}"), out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(document, "", &mut out);
    out
}

/// How many places (capped at 2) reach `component`: each reference outside
/// `components` counts once, each reference inside another component as often as
/// that component is reached (an annotation under a component referenced once by
/// a component referenced twice still applies twice).
fn instances(
    component: &str,
    sites: &[(String, Option<String>)],
    stack: &mut Vec<String>,
) -> usize {
    if stack.iter().any(|c| c == component) {
        return 1;
    }
    stack.push(component.to_owned());
    let mut total = 0;
    for (target, holder) in sites {
        // A reference INTO the component (`#/components/schemas/S/properties/p`)
        // reaches the annotations under `S` just like a reference to `S`.
        if target != component && !target.starts_with(&format!("{component}/")) {
            continue;
        }
        total += match holder {
            None => 1,
            Some(holder) => instances(holder, sites, stack).max(1),
        };
        if total >= 2 {
            break;
        }
    }
    stack.pop();
    total
}

/// Is `location` under a component that more than one place references?
fn shared_location(document: &Value, location: &str) -> bool {
    component_of(location)
        .is_some_and(|component| instances(&component, &ref_sites(document), &mut Vec::new()) >= 2)
}

/// Component schema pointers reachable from any operation's JSON request body,
/// and the pointers of the request body schemas themselves.
fn request_schemas(document: &Value) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut roots = BTreeSet::new();
    let mut reachable = BTreeSet::new();
    let mut pending: Vec<&Value> = Vec::new();
    for item in document["paths"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
    {
        for operation in item.as_object().into_iter().flat_map(|m| m.values()) {
            let body = &operation["requestBody"];
            let body = body
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| document.pointer(r.strip_prefix('#')?))
                .unwrap_or(body);
            if let Some(schema) = body["content"]["application/json"].get("schema") {
                if let Some(target) = schema
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|r| r.strip_prefix('#'))
                {
                    roots.insert(target.to_owned());
                }
                pending.push(schema);
            }
        }
    }
    while let Some(node) = pending.pop() {
        match node {
            Value::Object(map) => {
                if let Some(target) = map
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|r| r.strip_prefix('#'))
                    && reachable.insert(target.to_owned())
                    && let Some(next) = document.pointer(target)
                {
                    pending.push(next);
                }
                pending.extend(map.values());
            }
            Value::Array(items) => pending.extend(items),
            _ => {}
        }
    }
    (roots, reachable)
}

/// Does `location` lie within a JSON request body schema?
fn in_request_schema(document: &Value, location: &str) -> bool {
    location.starts_with("/operation/requestBody")
        || location.starts_with("/components/requestBodies/")
        || component_of(location)
            .is_some_and(|component| request_schemas(document).1.contains(&component))
}

/// Is `location` a property (or positional item) declared DIRECTLY on a request
/// body's root object/array, in the root itself or in one of its union arms?
fn direct_body_property(document: &Value, location: &str) -> bool {
    let Some((head, _)) = location.rsplit_once('/') else {
        return false;
    };
    let Some(mut parent) = head
        .strip_suffix("/properties")
        .or_else(|| head.strip_suffix("/prefixItems"))
    else {
        return false;
    };
    // Strip `/oneOf/<n>`, `/anyOf/<n>`, `/allOf/<n>` arms.
    while let Some((rest, index)) = parent.rsplit_once('/') {
        let Some(arm) = ["/oneOf", "/anyOf", "/allOf"]
            .iter()
            .find_map(|keyword| rest.strip_suffix(keyword))
        else {
            break;
        };
        if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
            break;
        }
        parent = arm;
    }
    (parent.starts_with("/operation/requestBody") && parent.ends_with("/application~1json/schema"))
        || request_schemas(document).0.contains(parent)
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
    // An exemption under a component that more than one place references (at
    // any depth, through nested components too) would silently apply at every
    // reference: exemptions live inline, or in a component referenced once.
    if EXEMPTIONS.iter().any(|key| node.get(*key).is_some()) {
        ensure!(
            !shared_location(document, location),
            "an exemption annotation is refused under a shared component ({location}): it would apply everywhere the component is referenced; annotate the one inline location, or move the schema to a component referenced once"
        );
    }
    if let Some(value) = node.get(super::request_reach::UNPROBED) {
        ensure!(
            value.as_str().is_some_and(|r| !r.trim().is_empty()),
            "{} needs a non-empty reason at {location}",
            super::request_reach::UNPROBED
        );
        ensure!(
            in_request_schema(document, location),
            "{} is only valid inside a request body schema at {location}",
            super::request_reach::UNPROBED
        );
    }
    if let Some(value) = node.get(super::http_vectors::UNSENDABLE) {
        ensure!(
            value.as_str().is_some_and(|r| !r.trim().is_empty()),
            "{} needs a non-empty reason at {location}",
            super::http_vectors::UNSENDABLE
        );
        ensure!(
            direct_body_property(document, location),
            "{} is only valid on a query parameter or a direct request body property at {location}",
            super::http_vectors::UNSENDABLE
        );
    }
    if let Some(value) = node.get(super::http_vectors::PATTERN_BOUNDARIES) {
        ensure!(
            value.as_str().is_some_and(|r| !r.trim().is_empty()),
            "{} needs a non-empty reason at {location}",
            super::http_vectors::PATTERN_BOUNDARIES
        );
        ensure!(
            node["pattern"].is_string(),
            "{} is only valid on a schema with a pattern at {location}",
            super::http_vectors::PATTERN_BOUNDARIES
        );
    }
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
        let nullable = super::coverage::nullable(node);
        ensure!(
            in_branch || nullable,
            "{} is only valid on a oneOf/anyOf branch or a nullable node at {location}",
            super::coverage::UNOBSERVED_BRANCH
        );
        if nullable {
            ensure!(
                !in_branch,
                "{} on a nullable union branch is ambiguous at {location}: give the null its own branch",
                super::coverage::UNOBSERVED_BRANCH
            );
            ensure!(
                super::coverage::exempt_side(node).is_some(),
                "{} on a nullable node must read `null: <reason>` or `non-null: <reason>` at {location}",
                super::coverage::UNOBSERVED_BRANCH
            );
        }
    }
    if let Some(value) = node.get(super::http_vectors::MAX_BYTES) {
        let limit = value.as_u64().filter(|n| *n > 0);
        ensure!(
            limit.is_some(),
            "{} must be a positive integer at {location}",
            super::http_vectors::MAX_BYTES
        );
        ensure!(
            node["type"] == "string"
                || node["type"]
                    .as_array()
                    .is_some_and(|t| t.iter().any(|v| v == "string")),
            "{} is only valid on a string schema at {location}",
            super::http_vectors::MAX_BYTES
        );
        // maxLength counts characters, each at least one byte: it can never
        // permit more than the byte limit does.
        ensure!(
            node["maxLength"]
                .as_u64()
                .is_none_or(|max| Some(max) <= limit),
            "maxLength exceeds {} at {location}",
            super::http_vectors::MAX_BYTES
        );
    }
    unprobed_request_keywords(document, node, location)?;
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

/// Keywords the vector harness cannot probe: a REQUEST schema using one would
/// pass `finish` with the constraint never exercised, so they are refused there
/// (add vectors for the keyword first, or restructure the schema).
const UNPROBED_REQUEST_KEYWORDS: &[&str] = &[
    "minProperties",
    "maxProperties",
    "multipleOf",
    "propertyNames",
    "contains",
    "minContains",
    "maxContains",
    "not",
];

/// Numeric bounds the vectors can probe are integers within i64 (the vectors
/// are `bound` and `bound` +/- 1 as i64).
const NUMERIC_BOUNDS: &[&str] = &["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"];

fn unprobed_request_keywords(document: &Value, node: &Value, location: &str) -> Result<()> {
    let keyword = UNPROBED_REQUEST_KEYWORDS
        .iter()
        .find(|keyword| node.get(**keyword).is_some())
        .copied()
        .or_else(|| {
            NUMERIC_BOUNDS
                .iter()
                .find(|keyword| node.get(**keyword).is_some_and(|v| v.as_i64().is_none()))
                .copied()
        });
    if let Some(keyword) = keyword
        && in_request_schema(document, location)
    {
        let why = if NUMERIC_BOUNDS.contains(&keyword) {
            "a request bound must be an integer within i64"
        } else {
            "the vector harness cannot probe it"
        };
        anyhow::bail!("{keyword} is not allowed in a request schema at {location}: {why}");
    }
    Ok(())
}

/// `x-omg-max-bytes` is read from every APPLICABLE node of a value, so it must
/// state a limit that always applies: it is refused on a oneOf/anyOf branch (the
/// limit would hold only when that branch matches, and a sibling branch could
/// state another) and anywhere under `if`/`then`/`else`/`not` (conditional or
/// inverted). Properties, items and map values INSIDE a branch are separate
/// instances and may carry their own.
fn bytes_placement(
    document: &Value,
    node: &Value,
    location: &str,
    volatile: bool,
    branch: bool,
    seen: &mut BTreeSet<(String, bool, bool)>,
) -> Result<()> {
    if !node.is_object() {
        return Ok(());
    }
    if node.get(super::http_vectors::MAX_BYTES).is_some() {
        ensure!(
            !volatile && !branch,
            "{} is refused on a oneOf/anyOf branch and under if/then/else/not at {location}: it must state a limit that always applies",
            super::http_vectors::MAX_BYTES
        );
    }
    if let Some(target) = node
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix('#'))
        && seen.insert((target.to_owned(), volatile, branch))
        && let Some(next) = document.pointer(target)
    {
        bytes_placement(document, next, target, volatile, branch, seen)?;
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        for (i, child) in node[keyword].as_array().into_iter().flatten().enumerate() {
            bytes_placement(
                document,
                child,
                &format!("{location}/{keyword}/{i}"),
                volatile,
                branch || keyword != "allOf",
                seen,
            )?;
        }
    }
    for keyword in ["if", "then", "else", "not"] {
        if let Some(child) = node.get(keyword) {
            bytes_placement(
                document,
                child,
                &format!("{location}/{keyword}"),
                true,
                false,
                seen,
            )?;
        }
    }
    for keyword in ["properties", "patternProperties"] {
        for (name, child) in node[keyword].as_object().into_iter().flatten() {
            bytes_placement(
                document,
                child,
                &format!("{location}/{keyword}/{}", segment(name)),
                volatile,
                false,
                seen,
            )?;
        }
    }
    for (i, child) in node["prefixItems"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        bytes_placement(
            document,
            child,
            &format!("{location}/prefixItems/{i}"),
            volatile,
            false,
            seen,
        )?;
    }
    for keyword in [
        "items",
        "additionalProperties",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contains",
        "propertyNames",
    ] {
        if let Some(child) = node.get(keyword) {
            bytes_placement(
                document,
                child,
                &format!("{location}/{keyword}"),
                volatile,
                false,
                seen,
            )?;
        }
    }
    Ok(())
}

/// Run [`bytes_placement`] over every complete component and every inline schema.
fn bytes_placements(document: &Value) -> Result<()> {
    fn crawl(
        document: &Value,
        node: &Value,
        location: &str,
        seen: &mut BTreeSet<(String, bool, bool)>,
    ) -> Result<()> {
        match node {
            Value::Object(map) => {
                for (key, child) in map {
                    let at = format!("{location}/{}", segment(key));
                    if key == "schema" {
                        bytes_placement(document, child, &at, false, false, seen)?;
                    } else {
                        crawl(document, child, &at, seen)?;
                    }
                }
            }
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    crawl(document, child, &format!("{location}/{i}"), seen)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut seen = BTreeSet::new();
    for (name, node) in document["components"]["schemas"]
        .as_object()
        .into_iter()
        .flatten()
    {
        if node["x-omg-schema-status"] != "incomplete" {
            bytes_placement(
                document,
                node,
                &format!("/components/schemas/{}", segment(name)),
                false,
                false,
                &mut seen,
            )?;
        }
    }
    crawl(document, &document["paths"], "/paths", &mut seen)?;
    for kind in ["responses", "headers", "parameters", "requestBodies"] {
        crawl(
            document,
            &document["components"][kind],
            &format!("/components/{kind}"),
            &mut seen,
        )?;
    }
    Ok(())
}

pub(super) fn completed(document: &Value) -> Result<()> {
    bytes_placements(document)?;
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
    // Each reusable component is checked AT ITS OWN LOCATION, so the shared
    // component ban does not depend on an operation referencing it.
    for kind in ["responses", "headers", "parameters", "requestBodies"] {
        for (name, node) in document["components"][kind]
            .as_object()
            .into_iter()
            .flatten()
        {
            operation_at(
                document,
                node,
                &format!("/components/{kind}/{}", segment(name)),
            )?;
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
    operation_at(document, operation, "/operation")
}

/// [`operation`] for a node that lives at `location` in the document.
fn operation_at(document: &Value, operation: &Value, location: &str) -> Result<()> {
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
                if let Some(value) = map.get(super::coverage::UNOBSERVED_PARAMETER) {
                    ensure!(
                        value.as_str().is_some_and(|r| !r.trim().is_empty()),
                        "{} needs a non-empty reason at {location}",
                        super::coverage::UNOBSERVED_PARAMETER
                    );
                    ensure!(
                        map.get("in").is_some_and(|v| v == "query"),
                        "{} is only valid on a query parameter at {location}",
                        super::coverage::UNOBSERVED_PARAMETER
                    );
                }
                if let Some(value) = map.get(super::http_vectors::UNSENDABLE) {
                    ensure!(
                        value.as_str().is_some_and(|r| !r.trim().is_empty()),
                        "{} needs a non-empty reason at {location}",
                        super::http_vectors::UNSENDABLE
                    );
                    ensure!(
                        map.get("in").is_some_and(|v| v == "query"),
                        "{} is only valid on a query parameter or a direct request body property at {location}",
                        super::http_vectors::UNSENDABLE
                    );
                }
                if EXEMPTIONS.iter().any(|key| map.contains_key(*key)) && map.get("in").is_some() {
                    ensure!(
                        !shared_location(document, location),
                        "an exemption annotation is refused under a shared component ({location}): it would apply everywhere the component is referenced"
                    );
                }
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
    discover(document, operation, location, &mut BTreeSet::new())
}

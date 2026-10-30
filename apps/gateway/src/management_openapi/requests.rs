//! Request-schema/real-serde boundary vectors, not a replacement for HTTP
//! successful controls or handler semantic/authorization tests.
use serde::de::{DeserializeOwned, Error, Visitor};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeSet};

// Ask the derived deserializer for its accepted wire field names. This bounded
// gate catches a newly added optional field even when the original positive
// fixture still deserializes. Custom/flattened shapes fail rather than guess.
#[derive(Default)]
struct Fields(RefCell<Vec<&'static str>>);
impl<'de> serde::Deserializer<'de> for &Fields {
    type Error = serde::de::value::Error;
    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
        Err(Self::Error::custom("ordinary struct DTO required"))
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.replace(fields.to_vec());
        Err(Self::Error::custom("field inventory collected"))
    }
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct
        map enum identifier ignored_any
    }
}

/// Operations whose HANDLER refuses values serde accepts (documented value
/// rules: length, alphabet, exactly-one-of). The schema carries those rules, so
/// against serde alone it may be STRICTER, never looser. Each entry is a
/// reviewed claim; `finish()` still demands executed HTTP boundary vectors for
/// the operation, which compare the schema with the real handler exactly.
pub(crate) const SEMANTIC_REFINEMENTS: &[(&str, &str, &str, &[&str])] = &[
    (
        "POST",
        "/api/v1/invitations/accept",
        "the token must be exactly 64 ASCII hexadecimal characters",
        &["/token"],
    ),
    (
        "POST",
        "/api/v1/me/access-requests",
        "idempotency_key is 1 to 128 printable ASCII characters; reason is at most 1000 characters without control characters",
        &["/idempotency_key", "/reason"],
    ),
    (
        "POST",
        "/api/v1/me/notifications/read",
        "exactly one of all=true or a non-empty ids array of at most 200",
        &["/ids", "/all"],
    ),
    (
        "POST",
        "/api/v1/platform/legal-holds",
        "exactly one of user_id or workspace_id is a UUID; the reference is a short code of at most 64 characters",
        &["/user_id", "/workspace_id", "/reference"],
    ),
    (
        "POST",
        "/api/v1/platform/admin-tokens",
        "name 1 to 120 characters, one to five distinct known scopes, 1 to 90 days, an allowlist of 1 to 32 distinct networks",
        &["/name", "/scopes", "/expires_in_days", "/ip_allowlist"],
    ),
    (
        "POST",
        "/api/v1/workspaces/{ws}/keys",
        "name, 1 to 365 days, positive rate limits within 32 bits, at most four distinct budget periods with positive amounts",
        &[
            "/name",
            "/expires_in_days",
            "/requests_per_minute",
            "/tokens_per_minute",
            "/concurrent_requests",
            "/concurrent_jobs",
            "/budgets",
        ],
    ),
    (
        "PUT",
        "/api/v1/workspaces/{ws}/keys/{key}/policy",
        "positive 32-bit limits; no storage or issuance quota on a key; either budgets or the deprecated single budget",
        &[
            "/requests_per_minute",
            "/tokens_per_minute",
            "/concurrent_requests",
            "/concurrent_jobs",
            "/storage_bytes",
            "/key_issuances_per_holder",
            "/key_issuances_per_workspace",
            "/monthly_budget_microusd",
            "/budget_period",
            "/budgets",
        ],
    ),
    (
        "POST",
        "/api/v1/workspaces/{ws}/keys/{key}/rotate",
        "expires_in_days is 1 to 365",
        &["/expires_in_days"],
    ),
    (
        "POST",
        "/api/v1/workspaces/{ws}/service-accounts",
        "the name is 1 to 120 bytes of printable text that is not all whitespace",
        &["/name"],
    ),
];

/// May the contract's verdict differ from serde's? Equal always passes; a
/// stricter contract passes only for a vector whose touched pointers are EVERY
/// ONE a reviewed refined pointer (a vector that also changes an unreviewed
/// field, the extra item or the root must agree exactly); a looser contract
/// (accepting what serde rejects) never does.
pub(super) fn agrees(refined: &[&str], touched: &[String], contract: bool, serde: bool) -> bool {
    contract == serde
        || (!contract
            && !touched.is_empty()
            && touched
                .iter()
                .all(|field| refined.contains(&field.as_str())))
}

/// Every reviewed refined pointer must exist in the operation's request schema:
/// as a property of the object form AND (when the schema documents serde's
/// positional form) as a positional item at the field's index in the DTO's
/// field order. A stale or misspelled pointer would silently widen nothing and
/// excuse nothing, so it fails.
pub(super) fn refined_pointers_exist(
    component: &Value,
    fields: &[&str],
    refined: &[&str],
) -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    let arms: Vec<&Value> = match component["oneOf"].as_array() {
        Some(arms) => arms.iter().collect(),
        None => vec![component],
    };
    let object = arms
        .iter()
        .find_map(|arm| arm["properties"].as_object())
        .context("the object form of the request schema is missing")?;
    let positional = arms.iter().find_map(|arm| arm["prefixItems"].as_array());
    for pointer in refined {
        let field = pointer
            .strip_prefix('/')
            .with_context(|| format!("refined pointer {pointer} must be a /field pointer"))?;
        ensure!(
            object.contains_key(field),
            "refined pointer {pointer} is not a property of the request schema"
        );
        let index = fields
            .iter()
            .position(|name| *name == field)
            .with_context(|| format!("refined pointer {pointer} is not a DTO field"))?;
        if let Some(items) = positional {
            ensure!(
                items.len() > index,
                "refined pointer {pointer} has no positional item in the request schema"
            );
        }
    }
    Ok(())
}

pub(crate) fn vectors<T: DeserializeOwned>(method: &str, template: &str, valid: Value) {
    let pointers = SEMANTIC_REFINEMENTS
        .iter()
        .find(|(m, t, ..)| *m == method && *t == template)
        .map_or(&[][..], |(.., pointers)| *pointers);
    vectors_in::<T>(
        super::responses::artifact(),
        method,
        template,
        valid,
        pointers,
    )
    .unwrap_or_else(|e| panic!("{e:#}"));
}

/// [`vectors`] against an explicit document and reviewed refined pointers.
/// Failures name method, template, vector label and field names, never values.
pub(super) fn vectors_in<T: DeserializeOwned>(
    document: &Value,
    method: &str,
    template: &str,
    valid: Value,
    refined: &[&str],
) -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    let operation = &document["paths"][template][method.to_ascii_lowercase()];
    let schema = &operation["requestBody"]["content"]["application/json"]["schema"];
    ensure!(
        !schema.is_null(),
        "request schema must be bound to an operation"
    );
    let fields = Fields::default();
    let _ = T::deserialize(&fields);
    let actual: BTreeSet<_> = fields.0.borrow().iter().copied().collect();
    ensure!(
        !actual.is_empty(),
        "unsupported DTO deserializer: inventory explicitly"
    );
    let component = document
        .pointer(
            &schema["$ref"]
                .as_str()
                .context("request schema reference")?[1..],
        )
        .context("request schema component")?;
    let object = component
        .get("properties")
        .unwrap_or(&component["oneOf"][0]["properties"]);
    let documented: BTreeSet<_> = object
        .as_object()
        .context("ordinary struct object variant required")?
        .keys()
        .map(String::as_str)
        .collect();
    ensure!(
        actual == documented,
        "{method} {template}: DTO/schema field inventory drift"
    );
    ensure!(
        serde_json::from_value::<T>(valid.clone()).is_ok(),
        "real deserialization positive control"
    );
    let names: Vec<&str> = fields.0.borrow().clone();
    refined_pointers_exist(component, &names, refined)
        .with_context(|| format!("{method} {template}"))?;
    let mut failures = Vec::new();
    let mut compare = |label: &str, value: Value| {
        let actual = serde_json::from_value::<T>(value.clone()).is_ok();
        let contract = super::responses::validate_value(document, schema, &value).is_ok();
        let touched = touched(&names, &valid, &value);
        if !agrees(refined, &touched, contract, actual) {
            failures.push(format!(
                "{method} {template}: {label}: contract {} but serde {} (touches {})",
                if contract { "accepts" } else { "refuses" },
                if actual { "accepts" } else { "refuses" },
                touched.join(", "),
            ));
        }
    };
    compare("positive", valid.clone());
    let positional = names
        .iter()
        .map(|name| valid.get(*name).cloned().unwrap_or(Value::Null))
        .collect::<Vec<_>>();
    compare("serde positional struct representation", json!(positional));
    for length in 0..positional.len() {
        compare("positional omission", json!(&positional[..length]));
    }
    let mut extra = positional;
    extra.push(json!({"unexpected_configuration":true}));
    compare("positional unknown extra item", json!(extra));
    let mut unknown = valid.clone();
    unknown["unexpected_configuration"] = json!(true);
    compare("unknown", unknown);
    for field in valid.as_object().context("valid body is an object")?.keys() {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        compare(&format!("missing/omission {field}"), missing);
        for wrong in [
            Value::Null,
            json!(true),
            json!(1),
            json!("wrong"),
            json!([]),
            json!({}),
        ] {
            let mut probe = valid.clone();
            probe[field] = wrong;
            compare(&format!("wrong-type/null {field}"), probe);
        }
        // These DTOs have no nested object DTO. Inject one at each scalar or
        // array-item boundary so a future nested shape cannot be silently loose.
        let mut nested = valid.clone();
        if let Some(items) = nested[field].as_array_mut() {
            items[0] = json!({"unexpected_configuration":true});
        } else {
            nested[field] = json!({"unexpected_configuration":true});
        }
        compare(&format!("nested-unknown {field}"), nested);
    }
    for value in [
        Value::Null,
        json!(true),
        json!(1),
        json!("wrong"),
        json!([]),
    ] {
        compare("wrong root type / JSON null is not omission", value);
    }
    // UUID serde deliberately accepts all these spellings. JSON Schema format
    // assertions must match that parser rather than only canonical output.
    for field in ["workspace_id", "ids"] {
        if valid.get(field).is_some() {
            let id = uuid::Uuid::nil();
            for value in [
                id.to_string(),
                id.simple().to_string(),
                id.urn().to_string(),
                id.braced().to_string(),
                "not-a-uuid".into(),
            ] {
                let mut probe = valid.clone();
                probe[field] = if field == "ids" {
                    json!([value])
                } else {
                    json!(value)
                };
                compare("UUID parser boundary", probe);
            }
        }
    }
    ensure!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}

/// Top-level fields (named `/field`, `(extra)` or `(root)`) a vector changes
/// relative to the valid body, whether it is an object or serde's positional array.
pub(super) fn touched(fields: &[&str], valid: &Value, value: &Value) -> Vec<String> {
    let mut out = BTreeSet::new();
    match value {
        Value::Object(map) => {
            let base = valid.as_object().cloned().unwrap_or_default();
            for key in map.keys().chain(base.keys()) {
                if map.get(key) != base.get(key) {
                    out.insert(format!("/{key}"));
                }
            }
        }
        Value::Array(items) => {
            let base: Vec<Value> = fields
                .iter()
                .map(|name| valid.get(*name).cloned().unwrap_or(Value::Null))
                .collect();
            for i in 0..items.len().max(base.len()) {
                if items.get(i) != base.get(i) {
                    out.insert(
                        fields
                            .get(i)
                            .map_or_else(|| "(extra)".to_owned(), |n| format!("/{n}")),
                    );
                }
            }
        }
        _ => {
            out.insert("(root)".to_owned());
        }
    }
    out.into_iter().collect()
}

#[test]
fn management_openapi_invitation_and_notification_request_vectors() {
    vectors::<crate::management::members::AcceptInput>(
        "POST",
        "/api/v1/invitations/accept",
        json!({"token":"a".repeat(64)}),
    );
    vectors::<crate::management::alerts::ReadInput>(
        "POST",
        "/api/v1/me/notifications/read",
        json!({"ids":[uuid::Uuid::nil()],"all":false}),
    );
    vectors::<crate::management::alerts::ReadInput>(
        "POST",
        "/api/v1/me/notifications/read",
        json!({"all":true}),
    );
}

#[test]
fn management_openapi_request_refinements_are_reviewed_and_one_directional() {
    let t = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
    assert!(agrees(&[], &t(&[]), true, true) && agrees(&[], &t(&[]), false, false));
    assert!(
        !agrees(&[], &t(&["/a"]), false, true),
        "stricter needs a reviewed refinement"
    );
    assert!(
        agrees(&["/a"], &t(&["/a"]), false, true),
        "a reviewed refinement may be stricter on its own pointer"
    );
    assert!(
        !agrees(&["/a"], &t(&["/b"]), false, true),
        "...but only on its own pointers"
    );
    assert!(
        !agrees(&["/a"], &t(&[]), false, true),
        "an untouched vector must agree exactly"
    );
    assert!(
        !agrees(&["/a"], &t(&["(root)"]), false, true),
        "so must a wrong root"
    );
    assert!(
        !agrees(&["/a"], &t(&["/b", "/a"]), false, true),
        "...and only when EVERY touched pointer is reviewed"
    );
    assert!(agrees(&["/a", "/b"], &t(&["/b", "/a"]), false, true));
    assert!(
        !agrees(&["/a"], &t(&["/a", "(extra)"]), false, true),
        "the extra item is never refined"
    );
    assert!(!agrees(&["/a"], &t(&["/a", "(root)"]), false, true));
    assert!(
        !agrees(&["/a"], &t(&["/a"]), true, false),
        "never looser than serde"
    );
    assert!(!agrees(&[], &t(&[]), true, false));
    let document = super::responses::artifact();
    for (method, template, reason, pointers) in SEMANTIC_REFINEMENTS {
        assert!(reason.trim().len() > 20, "{template}");
        let operation = super::responses::operation(document, method, template).unwrap();
        assert!(
            operation.get("requestBody").is_some(),
            "{method} {template} has no request body to refine"
        );
        assert!(
            !pointers.is_empty(),
            "{method} {template} reviews no pointer"
        );
        // Every listed pointer exists in the request schema: as a property of the
        // object form and as an item of the positional form.
        let schema = &operation["requestBody"]["content"]["application/json"]["schema"];
        let component = document
            .pointer(&schema["$ref"].as_str().unwrap()[1..])
            .unwrap();
        let arms: Vec<&Value> = match component["oneOf"].as_array() {
            Some(arms) => arms.iter().collect(),
            None => vec![component],
        };
        let object = arms
            .iter()
            .find_map(|a| a["properties"].as_object())
            .unwrap();
        let positional = arms
            .iter()
            .find_map(|a| a["prefixItems"].as_array())
            .unwrap();
        assert!(
            positional.len() >= object.len(),
            "{method} {template}: the positional form is shorter than the object form"
        );
        for pointer in *pointers {
            assert!(
                object.contains_key(pointer.trim_start_matches('/')),
                "{method} {template}: refined pointer {pointer} is not a request property"
            );
        }
    }
}

#[test]
fn management_openapi_refined_pointers_must_exist_in_the_request_schema() {
    let component = json!({"oneOf":[
        {"type":"object","properties":{"a":{"type":"string"},"b":{"type":"string"}}},
        {"type":"array","prefixItems":[{"type":"string"},{"type":"string"}]}]});
    refined_pointers_exist(&component, &["a", "b"], &["/a", "/b"]).unwrap();
    for (refined, label) in [
        (&["/c"][..], "an unknown property"),
        (&["a"][..], "a pointer without its slash"),
    ] {
        assert!(
            refined_pointers_exist(&component, &["a", "b"], refined).is_err(),
            "accepted {label}"
        );
    }
    // A property the DTO does not have, even when the schema lists it.
    let extra = json!({"oneOf":[
        {"type":"object","properties":{"a":{"type":"string"},"z":{"type":"string"}}},
        {"type":"array","prefixItems":[{"type":"string"},{"type":"string"}]}]});
    assert!(refined_pointers_exist(&extra, &["a", "b"], &["/z"]).is_err());
    // A positional form too short to carry the pointer's index.
    let short = json!({"oneOf":[
        {"type":"object","properties":{"a":{"type":"string"},"b":{"type":"string"}}},
        {"type":"array","prefixItems":[{"type":"string"}]}]});
    refined_pointers_exist(&short, &["a", "b"], &["/a"]).unwrap();
    let text = format!(
        "{:#}",
        refined_pointers_exist(&short, &["a", "b"], &["/b"]).unwrap_err()
    );
    assert!(text.contains("/b") && text.contains("positional"), "{text}");
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Probe {
    a: String,
    b: String,
}
fn probe_document(a_is_stricter: bool) -> Value {
    let a = if a_is_stricter {
        json!({"type":"string","const":"ok"})
    } else {
        json!({"type":"string"})
    };
    json!({"openapi":"3.1.0","paths":{"/api/v1/x":{"post":{"requestBody":{"content":{"application/json":{
        "schema":{"$ref":"#/components/schemas/Probe"}}}},"responses":{"200":{"description":"ok"}}}}},
        "components":{"schemas":{"Probe":{"oneOf":[
            {"type":"object","required":["a","b"],"additionalProperties":false,
                "properties":{"a":a.clone(),"b":{"type":"string","const":"x"}}},
            {"type":"array","prefixItems":[a,{"type":"string","const":"x"}],
                "minItems":2,"maxItems":2,"items":false}]}}}})
}

#[test]
fn management_openapi_refinement_tolerance_is_scoped_to_reviewed_pointers() {
    let valid = || json!({"a":"ok","b":"x"});
    let run = |document: &Value, refined: &[&str]| {
        vectors_in::<Probe>(document, "POST", "/api/v1/x", valid(), refined)
    };
    // `b` is stricter than serde on any string but "x": reviewed for /b only.
    run(&probe_document(false), &["/b"]).unwrap();
    assert!(run(&probe_document(false), &[]).is_err());
    // `a` is ALSO stricter, and unreviewed: the old per-DTO tolerance hid it.
    let text = format!("{:#}", run(&probe_document(true), &["/b"]).unwrap_err());
    assert!(
        text.contains("POST /api/v1/x") && text.contains("/a") && text.contains("wrong-type"),
        "{text}"
    );
    assert!(!text.contains("\"ok\""), "{text}");
    // Reviewing both pointers accepts it.
    run(&probe_document(true), &["/a", "/b"]).unwrap();
    // A reviewed pointer the schema does not carry fails, whatever else passes.
    let text = format!(
        "{:#}",
        run(&probe_document(true), &["/a", "/b", "/zzz"]).unwrap_err()
    );
    assert!(
        text.contains("/zzz") && text.contains("POST /api/v1/x"),
        "{text}"
    );
    // The vectors that touch no reviewed pointer are compared exactly.
    let touched = super::requests::touched(&["a", "b"], &valid(), &json!({"a":"ok","b":"y"}));
    assert_eq!(touched, vec!["/b".to_owned()]);
    assert_eq!(
        super::requests::touched(&["a", "b"], &valid(), &json!(["ok"])),
        vec!["/b".to_owned()]
    );
    assert_eq!(
        super::requests::touched(&["a", "b"], &valid(), &json!(["ok", "x", 1])),
        vec!["(extra)".to_owned()]
    );
    assert_eq!(
        super::requests::touched(&["a", "b"], &valid(), &json!(true)),
        vec!["(root)".to_owned()]
    );
}

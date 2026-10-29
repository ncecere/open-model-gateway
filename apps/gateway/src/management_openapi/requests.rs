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
/// rules: ranges, enums, alphabets, per-kind field sets). The schema carries those
/// rules, so against serde alone it may be STRICTER, never looser. Each entry is a
/// reviewed claim; `finish()` still demands executed HTTP boundary vectors for
/// the operation, which compare the schema with the real handler exactly.
pub(crate) const SEMANTIC_REFINEMENTS: &[(&str, &str, &str)] = &[
    (
        "POST",
        "/api/v1/platform/alerts/rules",
        "the kind decides which fields are required and which are refused, with ranges, enums and at most ten emails",
    ),
    (
        "PUT",
        "/api/v1/platform/alerts/rules/{id}",
        "the kind decides which fields are required and which are refused, with ranges, enums and at most ten emails",
    ),
    (
        "POST",
        "/api/v1/workspaces/{ws}/alerts/rules",
        "the kind decides which fields are required and which are refused, with ranges, enums and at most ten emails",
    ),
    (
        "PUT",
        "/api/v1/workspaces/{ws}/alerts/rules/{id}",
        "the kind decides which fields are required and which are refused, with ranges, enums and at most ten emails",
    ),
    (
        "PUT",
        "/api/v1/platform/settings/general",
        "display name 1 to 120 characters without control characters, https URLs without credentials, lifetime 1 to 365 days",
    ),
    (
        "PUT",
        "/api/v1/platform/settings/privacy",
        "data collection is deny or allow; retention is absent or 30 to 3650 days",
    ),
    (
        "PUT",
        "/api/v1/platform/settings/email",
        "either every field absent, or host, port 1-65535, tls enum, sender and a username with its password reference; plaintext only to loopback",
    ),
    (
        "PUT",
        "/api/v1/platform/settings/storage",
        "every retention is 1 to 365 days",
    ),
    (
        "POST",
        "/api/v1/platform/alerts/webhooks",
        "a trimmed label of at most 120 characters, an approved https URL and an env: reference",
    ),
    (
        "PATCH",
        "/api/v1/platform/alerts/webhooks/{id}",
        "the same value rules as create; a rotation reference and its expiry end together",
    ),
];

/// May the contract's verdict differ from serde's? Equal always passes; a
/// stricter contract passes only for a reviewed semantic refinement; a looser
/// contract (accepting what serde rejects) never does.
pub(super) fn agrees(refined: bool, contract: bool, serde: bool) -> bool {
    contract == serde || (refined && !contract)
}

fn refined(method: &str, template: &str) -> bool {
    SEMANTIC_REFINEMENTS
        .iter()
        .any(|(m, t, _)| *m == method && *t == template)
}

pub(crate) fn vectors<T: DeserializeOwned>(method: &str, template: &str, valid: Value) {
    let document = super::responses::artifact();
    let operation = &document["paths"][template][method.to_ascii_lowercase()];
    let schema = &operation["requestBody"]["content"]["application/json"]["schema"];
    assert!(
        !schema.is_null(),
        "request schema must be bound to an operation"
    );
    boundary_vectors::<T>(method, template, schema, valid, refined(method, template));
}

/// Bind recursively reachable nested DTOs as well, so a new optional nested
/// field cannot escape the parent's positive/negative boundary vectors.
pub(crate) fn component_vectors<T: DeserializeOwned>(
    method: &str,
    template: &str,
    component: &str,
    valid: Value,
) {
    let schema = json!({"$ref":format!("#/components/schemas/{component}")});
    boundary_vectors::<T>(method, template, &schema, valid, refined(method, template));
}

fn boundary_vectors<T: DeserializeOwned>(
    method: &str,
    template: &str,
    schema: &Value,
    valid: Value,
    refined: bool,
) {
    let document = super::responses::artifact();
    let fields = Fields::default();
    let _ = T::deserialize(&fields);
    let actual: BTreeSet<_> = fields.0.borrow().iter().copied().collect();
    assert!(
        !actual.is_empty(),
        "unsupported DTO deserializer: inventory explicitly"
    );
    let mut component = schema;
    let mut seen = BTreeSet::new();
    while let Some(reference) = component["$ref"].as_str() {
        assert!(seen.insert(reference), "cyclic request component alias");
        component = document.pointer(&reference[1..]).unwrap();
    }
    let object = component
        .get("properties")
        .unwrap_or(&component["oneOf"][0]["properties"]);
    let documented: BTreeSet<_> = object
        .as_object()
        .expect("ordinary struct object variant required")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        actual, documented,
        "{method} {template}: DTO/schema field inventory drift"
    );
    assert!(
        serde_json::from_value::<T>(valid.clone()).is_ok(),
        "real deserialization positive control"
    );
    let compare = |label: &str, value: Value| {
        let actual = serde_json::from_value::<T>(value.clone()).is_ok();
        let contract = super::responses::validate_value(document, schema, &value).is_ok();
        assert!(
            agrees(refined, contract, actual),
            "{method} {template}: {label}: contract {} but serde {}",
            if contract { "accepts" } else { "refuses" },
            if actual { "accepts" } else { "refuses" },
        );
    };
    compare("positive", valid.clone());
    // Recursively probe nested DTOs as well as their parent field's type.
    // Nested schemas must bind unknown/missing/null boundaries, not merely
    // reject replacement of the entire nested object with a scalar.
    fn nested_objects(value: &Value, path: &str, found: &mut Vec<String>) {
        if let Some(object) = value.as_object() {
            if !path.is_empty() {
                found.push(path.to_owned());
            }
            for (key, child) in object {
                nested_objects(child, &format!("{path}/{key}"), found);
            }
        } else if let Some(array) = value.as_array() {
            for (index, child) in array.iter().enumerate() {
                nested_objects(child, &format!("{path}/{index}"), found);
            }
        }
    }
    let mut nested = Vec::new();
    nested_objects(&valid, "", &mut nested);
    for path in nested {
        let mut unknown = valid.clone();
        unknown.pointer_mut(&path).unwrap()["unexpected_configuration"] = json!(true);
        compare("nested object unknown field", unknown);
        for field in valid.pointer(&path).unwrap().as_object().unwrap().keys() {
            let mut missing = valid.clone();
            missing
                .pointer_mut(&path)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(field);
            compare("nested missing field", missing);
            for wrong in [
                Value::Null,
                json!("wrong"),
                json!({}),
                json!([]),
                json!(true),
                json!(1),
            ] {
                let mut probe = valid.clone();
                probe.pointer_mut(&path).unwrap()[field] = wrong;
                compare("nested wrong-type/null", probe);
            }
        }
    }
    let positional = fields
        .0
        .borrow()
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
    for field in valid.as_object().unwrap().keys() {
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
            // A money field is a String to serde and an integer-pattern string to
            // the contract; the handler (not serde) rejects `wrong`. The HTTP
            // boundary vectors compare that verdict against the real handler.
            if field.ends_with("_microusd") && wrong == json!("wrong") {
                continue;
            }
            let mut probe = valid.clone();
            probe[field] = wrong;
            compare(&format!("wrong-type/null {field}"), probe);
        }
        // These DTOs have no nested object DTO. Inject one at each scalar or
        // array-item boundary so a future nested shape cannot be silently loose.
        let mut nested = valid.clone();
        if let Some(items) = nested[field].as_array_mut() {
            if items.is_empty() {
                items.push(json!({"unexpected_configuration":true}));
            } else {
                items[0] = json!({"unexpected_configuration":true});
            }
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
    if valid.get("previous_secret_until").is_some() {
        for value in [
            "2026-10-28T00:00:00Z",
            "2026-10-28 00:00:00Z",
            "2026-10-28t00:00:00z",
            "2026-10-28T00:00:00+0000",
            "2026-02-30T00:00:00Z",
            "2026-10-28T00:00:60Z",
            "not-a-time",
        ] {
            let mut probe = valid.clone();
            probe["previous_secret_until"] = json!(value);
            compare("chrono serde timestamp boundary", probe);
        }
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
    assert!(agrees(false, true, true) && agrees(false, false, false));
    assert!(
        !agrees(false, false, true),
        "stricter needs a reviewed refinement"
    );
    assert!(
        agrees(true, false, true),
        "reviewed refinement may be stricter"
    );
    assert!(!agrees(true, true, false), "never looser than serde");
    assert!(!agrees(false, true, false));
    let document = super::responses::artifact();
    for (method, template, reason) in SEMANTIC_REFINEMENTS {
        assert!(reason.trim().len() > 20, "{template}");
        let operation = super::responses::operation(document, method, template).unwrap();
        assert!(
            operation.get("requestBody").is_some(),
            "{method} {template} has no request body to refine"
        );
    }
}

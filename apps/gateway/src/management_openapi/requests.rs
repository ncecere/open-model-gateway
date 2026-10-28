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

pub(crate) fn vectors<T: DeserializeOwned>(method: &str, template: &str, valid: Value) {
    let document = super::responses::artifact();
    let operation = &document["paths"][template][method.to_ascii_lowercase()];
    let schema = &operation["requestBody"]["content"]["application/json"]["schema"];
    assert!(
        !schema.is_null(),
        "request schema must be bound to an operation"
    );
    schema_vectors::<T>(method, template, schema, valid);
}

/// Bind independently named, recursively reachable DTOs as well as the root.
/// This catches a newly added optional nested field even when omitted from
/// the original root fixture.
pub(crate) fn component_vectors<T: DeserializeOwned>(name: &str, valid: Value) {
    schema_vectors::<T>(
        "component",
        name,
        &json!({"$ref":format!("#/components/schemas/{name}")}),
        valid,
    );
}
fn schema_vectors<T: DeserializeOwned>(method: &str, template: &str, schema: &Value, valid: Value) {
    let document = super::responses::artifact();
    let fields = Fields::default();
    let _ = T::deserialize(&fields);
    let actual: BTreeSet<_> = fields.0.borrow().iter().copied().collect();
    assert!(
        !actual.is_empty(),
        "unsupported DTO deserializer: inventory explicitly"
    );
    let component = document
        .pointer(&schema["$ref"].as_str().unwrap()[1..])
        .unwrap();
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
        assert_eq!(contract, actual, "{method} {template}: {label}: {value}");
    };
    compare("positive", valid.clone());
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
            let mut probe = valid.clone();
            probe[field] = wrong;
            compare(&format!("wrong-type/null {field}"), probe);
        }
        // Inject unknown fields at each scalar or array-item boundary;
        // recursively reachable object fields are also probed below.
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
    // Probe recursively reachable DTO fields, not just the top-level body.
    fn nested_probes(value: &Value, pointer: &str, out: &mut Vec<(String, Value)>) {
        match value {
            Value::Object(map) => {
                if !pointer.is_empty() {
                    let mut v = value.clone();
                    v["unexpected_configuration"] = json!(true);
                    out.push((pointer.to_owned(), v));
                    for name in map.keys() {
                        let mut v = value.clone();
                        v.as_object_mut().unwrap().remove(name);
                        out.push((pointer.to_owned(), v));
                        for wrong in [
                            Value::Null,
                            json!(true),
                            json!(1),
                            json!("wrong"),
                            json!([]),
                            json!({}),
                        ] {
                            let mut v = value.clone();
                            v[name] = wrong;
                            out.push((pointer.to_owned(), v));
                        }
                    }
                }
                for (name, child) in map {
                    nested_probes(
                        child,
                        &format!("{pointer}/{}", name.replace('~', "~0").replace('/', "~1")),
                        out,
                    );
                }
            }
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    nested_probes(child, &format!("{pointer}/{i}"), out);
                }
            }
            _ => {}
        }
    }
    let mut nested = Vec::new();
    nested_probes(&valid, "", &mut nested);
    for (pointer, value) in nested {
        let mut probe = valid.clone();
        *probe.pointer_mut(&pointer).unwrap() = value;
        compare(&format!("recursive DTO boundary {pointer}"), probe);
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

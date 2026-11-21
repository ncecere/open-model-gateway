//! The management validator cache (#444): content-keyed like before, but the
//! key of a node of the committed artifact is derived once, not re-serialized
//! on every validation (about 40 million times in the slice 3 fixture,
//! counted with temporary instrumentation).
use super::responses::{self, artifact};
use serde_json::{Value, json};
use std::sync::Arc;

fn some_artifact_schema() -> &'static Value {
    artifact()["components"]["schemas"]
        .as_object()
        .unwrap()
        .values()
        .find(|s| s.get("properties").is_some())
        .expect("an object schema in the artifact")
}

#[test]
fn artifact_node_keys_are_derived_once() {
    let schema = some_artifact_schema();
    responses::validator(artifact(), schema).unwrap();
    let before = responses::key_serializations();
    for _ in 0..100 {
        responses::validator(artifact(), schema).unwrap();
    }
    assert_eq!(
        responses::key_serializations(),
        before,
        "artifact node key re-serialized"
    );
    // Control: a temporary is serialized on every call.
    let copy = schema.clone();
    for _ in 0..3 {
        responses::validator(artifact(), &copy).unwrap();
    }
    assert_eq!(responses::key_serializations(), before + 3);
}

#[test]
fn keys_stay_content_based() {
    let schema = some_artifact_schema();
    let node = responses::validator(artifact(), schema).unwrap();
    // An equal temporary (not an artifact node) finds the same entry.
    let copy = schema.clone();
    let temporary = responses::validator(artifact(), &copy).unwrap();
    assert!(
        Arc::ptr_eq(&node, &temporary),
        "equal content must share an entry"
    );
    // Different content never reuses it.
    let mut changed = schema.clone();
    changed["description"] = json!("validator cache key control");
    let other = responses::validator(artifact(), &changed).unwrap();
    assert!(!Arc::ptr_eq(&node, &other));
    drop(changed);
    let refusing = json!({"type":"string","maxLength":0});
    assert!(
        !responses::validator(artifact(), &refusing)
            .unwrap()
            .is_valid(&json!("x"))
    );
    assert!(
        responses::validator(artifact(), &json!({"type":"string"}))
            .unwrap()
            .is_valid(&json!("x"))
    );
}

#[test]
fn memoized_keys_equal_the_content_key() {
    let schemas = artifact()["components"]["schemas"].as_object().unwrap();
    for schema in schemas.values().take(200) {
        let expected = responses::value_key(schema);
        for _ in 0..2 {
            assert_eq!(&*responses::cache_key(schema).unwrap(), expected);
        }
        // A clone (never an artifact node) is keyed by content on every call.
        let copy = schema.clone();
        let before = responses::key_serializations();
        assert_eq!(&*responses::cache_key(&copy).unwrap(), expected);
        assert_eq!(responses::key_serializations(), before + 1);
    }
}

/// Two different values never share a key (a shared key would drop a planned
/// vector as a duplicate), including values built to collide if an encoding
/// were not self-delimiting.
#[test]
fn value_keys_differ_exactly_when_the_values_differ() {
    let values = [
        json!(null),
        json!(true),
        json!(false),
        json!(0),
        json!(1),
        json!(-1),
        json!(1.0),
        json!(1.5),
        json!(10),
        json!("1"),
        json!(""),
        json!("n"),
        json!("t"),
        json!("#1;"),
        json!("\"1:a"),
        json!("a\"1:b"),
        json!([]),
        json!([[]]),
        json!([[], []]),
        json!([1, 0]),
        json!([10]),
        json!(["a", "b"]),
        json!(["ab"]),
        json!(["a\"1:b"]),
        json!({}),
        json!({"a": {}}),
        json!({"a": null}),
        json!({"a": "n"}),
        json!({"a": 1, "b": 2}),
        json!({"a": {"b": 2}}),
        json!({"a": [1], "b": 2}),
        json!({"a": [1, {"b": 2}]}),
        json!({"ab": 1}),
        json!({"a": "b1"}),
        json!({"": ""}),
        json!([{"": ""}]),
        json!(["", ""]),
    ];
    for left in &values {
        for right in &values {
            assert_eq!(
                responses::value_key(left) == responses::value_key(right),
                serde_json::to_string(left).unwrap() == serde_json::to_string(right).unwrap(),
                "{left} and {right}"
            );
        }
    }
}

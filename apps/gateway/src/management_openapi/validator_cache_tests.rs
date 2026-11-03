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
fn memoized_keys_equal_the_serialized_content() {
    let schemas = artifact()["components"]["schemas"].as_object().unwrap();
    for schema in schemas.values().take(200) {
        let expected = serde_json::to_string(schema).unwrap();
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

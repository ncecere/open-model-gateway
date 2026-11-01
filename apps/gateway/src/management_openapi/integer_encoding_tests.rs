use super::*;

#[tokio::test]
async fn management_openapi_integer_encoding_preserves_wire_spelling() {
    for (body, target, expected) in [
        (json!(2.0), "(root)", "2e0"),
        (json!({"a/b~c":[2.0]}), "/a~1b~0c/0", "{\"a/b~c\":[2e0]}"),
        (json!([2.0]), "/0", "[2e0]"),
        (json!(1e20), "(root)", "100000000000000000000e0"),
    ] {
        let bytes = INTEGER_WIRE
            .scope(
                (
                    target.into(),
                    "integer encoding exponent",
                    std::cell::Cell::new(false),
                ),
                async { wire_bytes(&body) },
            )
            .await;
        assert!(
            bytes == expected.as_bytes(),
            "incorrect integer exponent wire spelling"
        );
        assert!(
            serde_json::from_slice::<Value>(&bytes).is_ok(),
            "invalid exponent JSON"
        );
    }
    assert!(wire_bytes(&json!(2.0)) == b"2.0", "wire scope leaked");
}

#[test]
fn management_openapi_integer_encoding_nullable_seed() {
    let schema = json!({"type":["integer","null"],"minimum":2,"maximum":9});
    let nodes = vec![&schema];
    let mutations = scalar_mutations(&json!({}), &schema, &nodes, &Value::Null, false, 0).unwrap();
    for kind in ["integer encoding fraction", "integer encoding exponent"] {
        assert!(
            mutations
                .vectors
                .iter()
                .any(|(k, v)| *k == kind && valid(&json!({}), &schema, v).unwrap()),
            "missing nullable integer encoding vector"
        );
    }
    assert!(
        !mutations
            .vectors
            .iter()
            .any(|(k, _)| *k == "integer encoding negative zero"),
        "zero vector outside valid range"
    );
}

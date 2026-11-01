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

#[tokio::test]
async fn management_openapi_integer_encoding_accepting_handler_fails() {
    let schema = json!({"type":"integer","minimum":0,"maximum":9});
    let document = json!({"x-omg-integer-encoding":"json-integer-literal",
        "components":{"schemas":{}},"paths":{"/x":{"post":{
        "requestBody":{"required":true,"content":{"application/json":{"schema":schema}}}}}}});
    let result = run_body(&document, "POST", "/x", json!(2), &[], &async |body| {
        let bytes = wire_bytes(&body);
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();
        let status = if valid(&document, &schema, &parsed).unwrap() {
            200
        } else {
            422
        };
        Probe::from(Captured {
            status: axum::http::StatusCode::from_u16(status).unwrap(),
            headers: axum::http::HeaderMap::new(),
            bytes: bytes::Bytes::new(),
            method: "POST".into(),
            template: Some("/x".into()),
            query: vec![],
        })
    })
    .await;
    let error = result.err().expect("encoding-accepting handler must fail");
    assert!(
        format!("{error:#}").contains("documented integer encoding requires refusal"),
        "wrong refusal failure kind"
    );
}

#[test]
fn management_openapi_integer_encoding_quality_gate() {
    let good = json!({"x-omg-integer-encoding":"json-integer-literal",
        "info":{"description":super::super::quality::INTEGER_ENCODING_DESCRIPTION}});
    super::super::quality::integer_encoding(&good).unwrap();
    for pointer in ["/x-omg-integer-encoding", "/info/description"] {
        let mut bad = good.clone();
        *bad.pointer_mut(pointer).unwrap() = json!("wrong");
        assert!(
            super::super::quality::integer_encoding(&bad).is_err(),
            "missing convention quality failure"
        );
    }
}

#[tokio::test]
async fn management_openapi_integer_encoding_escaped_positional_location() {
    let schema = json!({"type":"array","prefixItems":[{"type":"object",
        "properties":{"a/b~c":{"type":"integer","minimum":0,"maximum":9}},
        "required":["a/b~c"],"additionalProperties":false}],"items":false,"minItems":1});
    let document = json!({"x-omg-integer-encoding":"json-integer-literal",
        "components":{"schemas":{}},"paths":{"/x":{"post":{
        "requestBody":{"required":true,"content":{"application/json":{"schema":schema}}}}}}});
    let operation = &document["paths"]["/x"]["post"];
    let body = json!([{"a/b~c":2}]);
    let cases = body_cases(&document, operation, &body, &[]).unwrap();
    assert!(
        cases
            .iter()
            .any(|c| c.pointer == "/0/a~1b~0c" && c.kind == "integer encoding exponent"),
        "missing escaped positional integer vector"
    );
    let seen = std::cell::RefCell::new(BTreeSet::new());
    run_body(&document, "POST", "/x", body, &[], &async |body| {
        let bytes = wire_bytes(&body);
        if bytes == br#"[{"a/b~c":2e0}]"# {
            seen.borrow_mut().insert("exponent");
        }
        if bytes == br#"[{"a/b~c":2.0}]"# {
            seen.borrow_mut().insert("fraction");
        }
        if bytes == br#"[{"a/b~c":-0.0}]"# {
            seen.borrow_mut().insert("negative zero");
        }
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();
        let accepted = valid(&document, &schema, &parsed).unwrap()
            && serde_json::from_slice::<Vec<std::collections::BTreeMap<String, u32>>>(&bytes)
                .is_ok();
        Probe::from(Captured {
            status: axum::http::StatusCode::from_u16(if accepted { 200 } else { 422 }).unwrap(),
            headers: axum::http::HeaderMap::new(),
            bytes: bytes::Bytes::new(),
            method: "POST".into(),
            template: Some("/x".into()),
            query: vec![],
        })
    })
    .await
    .unwrap();
    assert!(seen.borrow().len() == 3, "missing integer wire spellings");
}

#[test]
fn management_openapi_integer_encoding_zero_requires_whole_request_validity() {
    let schema = json!({"type":"object","properties":{
        "n":{"type":"integer","minimum":0,"maximum":9},"flag":{"type":"boolean"}},
        "required":["n","flag"],"additionalProperties":false,
        "if":{"properties":{"n":{"const":0}}},"then":{"properties":{"flag":{"const":true}}}});
    let document = json!({"components":{"schemas":{}},"requestBody":{
        "required":true,"content":{"application/json":{"schema":schema}}}});
    let cases = body_cases(&document, &document, &json!({"n":2,"flag":false}), &[]).unwrap();
    assert!(
        !cases
            .iter()
            .any(|c| c.pointer == "/n" && c.kind == "integer encoding negative zero"),
        "schema-invalid zero divergence"
    );
}

#[test]
fn management_openapi_integer_encoding_query_exception_is_scoped() {
    let operation = json!({"parameters":[
        {"in":"query","name":"offset","schema":{"type":"integer","minimum":0,"maximum":9}},
        {"in":"query","name":"count","schema":{"type":"integer","minimum":0,"maximum":9}}]});
    let document = json!({"x-omg-integer-encoding-query-exceptions":{"offset":["-0"]}});
    let base = vec![("offset".into(), "2".into()), ("count".into(), "2".into())];
    let cases = query_cases(&document, &operation, &base, &[]).unwrap();
    assert!(
        cases
            .iter()
            .any(|c| c.param == "offset" && c.kind == "query negative zero literal" && c.accept),
        "missing reviewed offset exception"
    );
    assert!(
        !cases
            .iter()
            .any(|c| c.param == "count" && c.kind == "query negative zero literal"),
        "unreviewed query exception"
    );
}

#[tokio::test]
async fn management_openapi_integer_encoding_large_fraction_wire_spelling() {
    let body = json!({"n":1_000_000_000_000_000_000.0});
    let bytes = INTEGER_WIRE
        .scope(
            (
                "/n".into(),
                "integer encoding fraction",
                std::cell::Cell::new(false),
            ),
            async { wire_bytes(&body) },
        )
        .await;
    assert!(
        bytes == br#"{"n":1000000000000000000.0}"#,
        "large fraction spelling lost"
    );
}

//! The coverage engine as `PositiveFixtures` drives it: what `observe` credits
//! and what the completed-group gate refuses. Synthetic documents only (DB-free).
use super::responses::{Captured, PositiveFixtures};
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const KEY: &str = "GET /api/v1/x";

fn document() -> Value {
    let item = json!({"type":"object","required":["name","note"],"additionalProperties":false,"properties":{
        "name":{"type":"string","maxLength":40},
        "note":{"type":["string","null"],"maxLength":40}}});
    json!({"openapi":"3.1.0","paths":{"/api/v1/x":{"get":{
        "operationId":"getX",
        "parameters":[{"in":"query","name":"limit","schema":{"type":"integer","minimum":1,"maximum":200}}],
        "responses":{"200":{"description":"ok","content":{"application/json":{"schema":
            {"type":"object","required":["data"],"additionalProperties":false,"properties":{
                "data":{"type":"array","items":item}}}}}}}}}},
        "components":{"schemas":{}}})
}
fn captured(status: StatusCode, body: &Value, query: &[&str]) -> Captured {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    Captured {
        status,
        headers,
        bytes: axum::body::Bytes::from(serde_json::to_vec(body).unwrap()),
        method: "GET".into(),
        template: Some("/api/v1/x".into()),
        query: query.iter().map(|n| (*n).to_owned()).collect(),
    }
}
fn gaps(fixtures: &PositiveFixtures, document: &Value) -> anyhow::Result<()> {
    fixtures.check_contract_coverage(document, &BTreeSet::from([KEY.to_owned()]))
}
fn message(result: anyhow::Result<()>) -> String {
    format!("{:#}", result.expect_err("a coverage gap must fail"))
}
/// Vectors are not what these controls are about.
fn with_vectors(fixtures: &mut PositiveFixtures) {
    fixtures.record_vectors("GET", "/api/v1/x", "query");
}

#[test]
fn management_openapi_wiring_positive_observations_earn_coverage() {
    let document = document();
    let mut fixtures = PositiveFixtures::default();
    with_vectors(&mut fixtures);
    // RED: nothing observed yet, so every gap is named by template and pointer.
    let gap = message(gaps(&fixtures, &document));
    for expected in [KEY, "limit", "/data", "null", "non-null"] {
        assert!(gap.contains(expected), "{expected}: {gap}");
    }
    // An empty array, a missing parameter and a null-only note still leave gaps.
    fixtures.observe_in(
        &document,
        &captured(StatusCode::OK, &json!({"data":[]}), &[]),
    );
    let gap = message(gaps(&fixtures, &document));
    assert!(
        gap.contains("/data/*/note") || gap.contains("/data"),
        "{gap}"
    );
    // GREEN: one populated positive request carrying the parameter and a null
    // note, plus another with a non-null note.
    fixtures.observe_in(
        &document,
        &captured(
            StatusCode::OK,
            &json!({"data":[{"name":"a","note":null}]}),
            &["limit"],
        ),
    );
    let gap = message(gaps(&fixtures, &document));
    assert!(
        gap.contains("/data/*/note") && gap.contains("non-null") && !gap.contains("limit"),
        "{gap}"
    );
    fixtures.observe_in(
        &document,
        &captured(
            StatusCode::OK,
            &json!({"data":[{"name":"a","note":"n"}]}),
            &[],
        ),
    );
    gaps(&fixtures, &document).unwrap();
}

#[test]
fn management_openapi_wiring_refusals_and_head_earn_no_coverage() {
    let document = document();
    let full = json!({"data":[{"name":"a","note":null},{"name":"b","note":"n"}]});
    let mut fixtures = PositiveFixtures::default();
    with_vectors(&mut fixtures);
    // A HEAD of a populated GET cannot credit the GET.
    let mut head = captured(StatusCode::OK, &full, &["limit"]);
    head.method = "HEAD".into();
    head.bytes = axum::body::Bytes::new();
    fixtures.observe_in(&document, &head);
    assert!(gaps(&fixtures, &document).is_err());
}

#[test]
fn management_openapi_wiring_gap_messages_name_pointers_never_values() {
    let document = document();
    let mut fixtures = PositiveFixtures::default();
    with_vectors(&mut fixtures);
    let secret = "tok_3fc1a9d0-secret-value";
    fixtures.observe_in(
        &document,
        &captured(
            StatusCode::OK,
            &json!({"data":[{"name":secret,"note":secret}]}),
            &[],
        ),
    );
    let gap = message(gaps(&fixtures, &document));
    assert!(!gap.contains(secret), "{gap}");
    assert!(
        gap.contains("limit") && gap.contains("/data/*/note"),
        "{gap}"
    );
}

#[test]
fn management_openapi_wiring_vectors_are_required_for_queries_and_bodies() {
    let mut document = document();
    let full = json!({"data":[{"name":"a","note":null},{"name":"b","note":"n"}]});
    let mut fixtures = PositiveFixtures::default();
    fixtures.observe_in(&document, &captured(StatusCode::OK, &full, &["limit"]));
    // RED: documented query parameter, no HTTP boundary vectors executed.
    let gap = message(gaps(&fixtures, &document));
    assert!(gap.contains(KEY) && gap.contains("query"), "{gap}");
    fixtures.record_vectors("GET", "/api/v1/x", "query");
    gaps(&fixtures, &document).unwrap();
    // A request body demands its own kind of vectors; query vectors do not satisfy it.
    document["paths"]["/api/v1/x"]["get"]["requestBody"] = json!({"required":true,"content":{
        "application/json":{"schema":{"type":"object","required":[],"additionalProperties":false,"properties":{}}}}});
    let gap = message(gaps(&fixtures, &document));
    assert!(gap.contains(KEY) && gap.contains("body"), "{gap}");
    fixtures.record_vectors("GET", "/api/v1/x", "body");
    gaps(&fixtures, &document).unwrap();
    // An operation without a query or body owes no vectors.
    document["paths"]["/api/v1/x"]["get"]
        .as_object_mut()
        .unwrap()
        .remove("requestBody");
    document["paths"]["/api/v1/x"]["get"]["parameters"] = json!([]);
    let mut bare = PositiveFixtures::default();
    bare.observe_in(&document, &captured(StatusCode::OK, &full, &[]));
    gaps(&bare, &document).unwrap();
}

//! Controls for the HTTP boundary-vector harness: real axum extractors behind
//! `oneshot`, synthetic schemas, no database. A handler that is looser or
//! stricter than its schema must FAIL, and the failure names no sent value.
use super::http_vectors::{
    Edit, Probe, binary_cases, body_cases, query_cases, run_binary, run_body, run_query,
};
use super::responses::Captured;
use axum::{
    Json, Router,
    body::Body,
    extract::Query,
    http::{Request, StatusCode, header},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt;

const TEMPLATE: &str = "/api/v1/x";

async fn exchange(app: &Router, method: &str, request: Request<Body>) -> Captured {
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    Captured {
        status: parts.status,
        headers: parts.headers,
        bytes: axum::body::to_bytes(body, 1 << 20).await.unwrap(),
        method: method.into(),
        template: Some(TEMPLATE.into()),
        query: vec![],
    }
}

fn parameter(name: &str, required: bool, schema: Value) -> Value {
    json!({"in":"query","name":name,"required":required,"schema":schema})
}
fn query_document() -> Value {
    json!({"openapi":"3.1.0","paths":{TEMPLATE:{"get":{"parameters":[
        parameter("limit", false, json!({"type":"integer","minimum":1,"maximum":4242})),
        parameter("status", false, json!({"enum":["active","all"]})),
        parameter("day", false, json!({"type":"string","format":"date"})),
        parameter("flag", false, json!({"type":"boolean"})),
        parameter("tag", true, json!({"type":"string","maxLength":8,"pattern":"^[a-z]+$"})),
    ],"responses":{"200":{"description":"ok"}}}}},"components":{"schemas":{}}})
}
fn base() -> Vec<(String, String)> {
    [
        ("limit", "10"),
        ("status", "active"),
        ("day", "2025-01-31"),
        ("flag", "true"),
        ("tag", "abc"),
    ]
    .iter()
    .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
    .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Q {
    limit: Option<i64>,
    status: Option<String>,
    day: Option<chrono::NaiveDate>,
    flag: Option<bool>,
    tag: String,
}
/// `strict` enforces exactly the documented bounds; the other knobs loosen or
/// tighten one of them.
#[derive(Clone, Copy)]
struct Handler {
    max: i64,
    min: i64,
    pattern: bool,
}
const EXACT: Handler = Handler {
    max: 4242,
    min: 1,
    pattern: true,
};
fn query_app(handler: Handler) -> Router {
    Router::new().route(
        TEMPLATE,
        get(move |Query(q): Query<Q>| async move {
            let limit = q.limit.unwrap_or(handler.min);
            let tag_ok = q.tag.len() <= 8
                && (!handler.pattern || q.tag.chars().all(|c| c.is_ascii_lowercase()))
                && !q.tag.is_empty();
            let status_ok = q
                .status
                .as_deref()
                .is_none_or(|s| s == "active" || s == "all");
            if (handler.min..=handler.max).contains(&limit) && tag_ok && status_ok {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            }
        }),
    )
}
async fn run(app: &Router, document: &Value) -> anyhow::Result<Vec<Captured>> {
    run_query(
        document,
        "GET",
        TEMPLATE,
        &async || ((), base()),
        &async |_: &(), query: String| {
            let request = Request::builder()
                .uri(format!("{TEMPLATE}?{query}"))
                .body(Body::empty())
                .unwrap();
            Probe::from(exchange(app, "GET", request).await)
        },
    )
    .await
}
fn message(result: anyhow::Result<Vec<Captured>>) -> String {
    format!("{:#}", result.err().expect("the vector run must fail"))
}

#[test]
fn management_openapi_http_vectors_query_cases_cover_the_boundaries() {
    let document = query_document();
    let operation = &document["paths"][TEMPLATE]["get"];
    let cases = query_cases(&document, operation, &base()).unwrap();
    let find = |param: &str, kind: &str| {
        cases
            .iter()
            .filter(|c| c.param == param && c.kind == kind)
            .collect::<Vec<_>>()
    };
    for (param, kind, accept) in [
        ("limit", "minimum", true),
        ("limit", "minimum-1", false),
        ("limit", "maximum", true),
        ("limit", "maximum+1", false),
        ("limit", "wrong type", false),
        ("limit", "non-integer", false),
        ("limit", "empty", false),
        ("limit", "omitted", true),
        ("status", "enum non-member", false),
        ("status", "enum member", true),
        ("day", "format violation", false),
        ("flag", "wrong type", false),
        ("tag", "maxLength", true),
        ("tag", "maxLength+1", false),
        ("tag", "pattern violation", false),
        ("tag", "omitted", false),
    ] {
        let found = find(param, kind);
        assert!(!found.is_empty(), "missing vector {kind} for {param}");
        assert!(
            found.iter().all(|c| c.accept == accept),
            "{kind} for {param} must be {}",
            if accept { "accepted" } else { "refused" }
        );
    }
    assert!(
        find("limit", "maximum+1")
            .iter()
            .any(|c| c.edit == Edit::Set("4243".into()))
    );
    // A base that omits a documented parameter, or adds an undocumented one, is refused.
    assert!(query_cases(&document, operation, &base()[..2]).is_err());
    let mut extra = base();
    extra.push(("surprise".into(), "1".into()));
    assert!(query_cases(&document, operation, &extra).is_err());
}

#[tokio::test]
async fn management_openapi_http_vectors_query_agreement_passes() {
    let document = query_document();
    let exchanges = run(&query_app(EXACT), &document).await.unwrap();
    assert!(exchanges.len() > 25, "{}", exchanges.len());
    assert!(
        exchanges
            .iter()
            .any(|c| c.status == StatusCode::BAD_REQUEST)
    );
}

#[tokio::test]
async fn management_openapi_http_vectors_query_handler_looser_than_schema_fails() {
    let document = query_document();
    let looser = Handler {
        max: i64::MAX,
        ..EXACT
    };
    let text = message(run(&query_app(looser), &document).await);
    assert!(
        text.contains("GET /api/v1/x") && text.contains("maximum+1") && text.contains("limit"),
        "{text}"
    );
    assert!(
        text.contains("schema refuses but handler accepts"),
        "{text}"
    );
    // The sent value never reaches the message.
    assert!(!text.contains("4243"), "{text}");
    let unchecked = Handler {
        pattern: false,
        ..EXACT
    };
    let text = message(run(&query_app(unchecked), &document).await);
    assert!(
        text.contains("pattern violation") && text.contains("tag"),
        "{text}"
    );
}

#[tokio::test]
async fn management_openapi_http_vectors_query_handler_stricter_than_schema_fails() {
    let document = query_document();
    let stricter = Handler { min: 2, ..EXACT };
    let text = message(run(&query_app(stricter), &document).await);
    assert!(
        text.contains("schema accepts but handler refuses") && text.contains("limit"),
        "{text}"
    );
}

#[tokio::test]
async fn management_openapi_http_vectors_unexpected_statuses_and_other_operations_fail() {
    let document = query_document();
    let limited = Router::new().route(TEMPLATE, get(|| async { StatusCode::TOO_MANY_REQUESTS }));
    let text = message(run(&limited, &document).await);
    assert!(text.contains("unexpected status 429"), "{text}");
    // An explicit verdict replaces the status-derived one.
    let verdict = run_query(
        &document,
        "GET",
        TEMPLATE,
        &async || ((), base()),
        &async |_: &(), _query: String| {
            let request = Request::builder()
                .uri(TEMPLATE)
                .body(Body::empty())
                .unwrap();
            let mut captured = exchange(&limited, "GET", request).await;
            captured.template = Some("/api/v1/elsewhere".into());
            Probe::decided(captured, true)
        },
    )
    .await;
    assert!(format!("{:#}", verdict.err().unwrap()).contains("different operation"));
}

// ----- request bodies -------------------------------------------------------

fn body_document() -> Value {
    json!({"openapi":"3.1.0","paths":{TEMPLATE:{"post":{
        "requestBody":{"required":true,"content":{"application/json":{"schema":{
            "type":"object","required":["token","count","mode"],"additionalProperties":false,"properties":{
                "token":{"type":"string","pattern":"^[0-9a-f]{8}$"},
                "count":{"type":"integer","minimum":0,"maximum":10},
                "mode":{"enum":["a","b"]},
                "note":{"type":["string","null"],"maxLength":5},
                "tags":{"type":["array","null"],"maxItems":2,"items":{"type":"string","maxLength":4}},
                "nested":{"type":["object","null"],"required":["flag"],"additionalProperties":false,"properties":{
                    "flag":{"type":"boolean"}}}}}}}},
        "responses":{"200":{"description":"ok"}}}}},"components":{"schemas":{}}})
}
fn valid_body() -> Value {
    json!({"token":"deadbeef","count":3,"mode":"a","note":"ok","tags":["x"],"nested":{"flag":true}})
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Payload {
    token: String,
    count: u8,
    mode: String,
    note: Option<String>,
    tags: Option<Vec<String>>,
    nested: Option<Nested>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Nested {
    flag: bool,
}
fn body_app(check_token: bool, max_count: u8) -> Router {
    Router::new().route(
        TEMPLATE,
        post(move |Json(b): Json<Payload>| async move {
            let token_ok = !check_token
                || (b.token.len() == 8
                    && b.token
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
            let note_ok = b.note.as_deref().is_none_or(|n| n.chars().count() <= 5);
            let tags_ok = b
                .tags
                .as_ref()
                .is_none_or(|t| t.len() <= 2 && t.iter().all(|s| s.chars().count() <= 4));
            if token_ok
                && b.count <= max_count
                && ["a", "b"].contains(&b.mode.as_str())
                && note_ok
                && tags_ok
            {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            }
        }),
    )
}
async fn run_bodies(app: &Router, document: &Value) -> anyhow::Result<Vec<Captured>> {
    run_body(
        document,
        "POST",
        TEMPLATE,
        valid_body(),
        &async |body: Value| {
            let request = Request::builder()
                .method("POST")
                .uri(TEMPLATE)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();
            Probe::from(exchange(app, "POST", request).await)
        },
    )
    .await
}

#[test]
fn management_openapi_http_vectors_body_cases_reach_nested_fields() {
    let document = body_document();
    let operation = &document["paths"][TEMPLATE]["post"];
    let cases = body_cases(&document, operation, &valid_body()).unwrap();
    let has = |pointer: &str, kind: &str, accept: bool| {
        cases
            .iter()
            .any(|c| c.pointer == pointer && c.kind == kind && c.accept == accept)
    };
    for (pointer, kind, accept) in [
        ("(root)", "null", false),
        ("(root)", "wrong type", false),
        ("(root)", "unknown field", false),
        ("/token", "wrong type", false),
        ("/token", "null", false),
        ("/token", "pattern violation", false),
        ("/token", "omitted", false),
        ("/count", "maximum", true),
        ("/count", "maximum+1", false),
        ("/count", "minimum-1", false),
        ("/count", "non-integer", false),
        ("/mode", "enum non-member", false),
        ("/mode", "enum member", true),
        ("/note", "null", true),
        ("/note", "maxLength+1", false),
        ("/note", "omitted", true),
        ("/tags", "null", true),
        ("/nested", "null", true),
        ("/tags", "maxItems+1", false),
        ("/tags/0", "wrong type", false),
        ("/tags/0", "maxLength+1", false),
        ("/nested", "unknown field", false),
        ("/nested/flag", "wrong type", false),
        ("/nested/flag", "null", false),
        ("/nested/flag", "omitted", false),
    ] {
        assert!(has(pointer, kind, accept), "missing {kind} at {pointer}");
    }
    assert!(
        cases
            .iter()
            .any(|c| c.pointer == "/tags" && c.body["tags"] == json!([]) && c.accept)
    );
    // A body that does not satisfy its own schema is not a baseline.
    assert!(body_cases(&document, operation, &json!({"token":"x"})).is_err());
}

#[tokio::test]
async fn management_openapi_http_vectors_body_agreement_passes() {
    let document = body_document();
    let exchanges = run_bodies(&body_app(true, 10), &document).await.unwrap();
    assert!(exchanges.len() > 40, "{}", exchanges.len());
}

#[tokio::test]
async fn management_openapi_http_vectors_body_disagreement_fails_without_values() {
    let document = body_document();
    let text = message(run_bodies(&body_app(false, 10), &document).await);
    assert!(
        text.contains("POST /api/v1/x")
            && text.contains("/token")
            && text.contains("schema refuses but handler accepts"),
        "{text}"
    );
    let text = message(run_bodies(&body_app(true, 200), &document).await);
    assert!(
        text.contains("/count") && text.contains("maximum+1"),
        "{text}"
    );
    assert!(!text.contains("deadbeef") && !text.contains("11"), "{text}");
}

// ---------------------------------------------------------------------------
// Raw (non-JSON) upload bodies
// ---------------------------------------------------------------------------

fn upload_document() -> Value {
    let body = json!({"type":"string","format":"binary","minLength":1,"maxLength":16});
    json!({"openapi":"3.1.0","paths":{TEMPLATE:{"put":{"requestBody":{"required":true,"content":{
        "image/png":{"schema":body},"image/jpeg":{"schema":body}}},
        "responses":{"200":{"description":"ok"}}}}},"components":{"schemas":{}}})
}
#[test]
fn management_openapi_http_vectors_binary_cases_cover_raw_uploads() {
    let document = upload_document();
    let operation = &document["paths"][TEMPLATE]["put"];
    let cases = binary_cases(
        operation,
        "image/png",
        &[7u8; 8],
        &[("oversize", vec![1u8; 32])],
    )
    .unwrap();
    let find = |kind: &str| cases.iter().find(|c| c.kind == kind).unwrap();
    assert!(!find("empty").accept && find("empty").bytes.is_empty());
    assert!(!find("maxLength+1").accept && find("maxLength+1").bytes.len() == 17);
    assert!(find("truncated").accept && find("truncated").bytes.len() == 4);
    assert!(find("trailing byte").accept && find("trailing byte").bytes.len() == 9);
    let other = find("other media type");
    assert!(other.accept && other.media == "image/jpeg");
    assert!(!find("oversize").accept);
    // A media type the operation does not document, or a body that is not valid, is refused.
    assert!(binary_cases(operation, "text/plain", &[1], &[]).is_err());
    assert!(binary_cases(operation, "image/png", &[], &[]).is_err());
    assert!(
        binary_cases(
            &json!({"requestBody":{"content":{"image/png":{"schema":{"type":"string"}}}}}),
            "image/png",
            &[1],
            &[]
        )
        .is_err()
    );
}
fn upload_app(max: usize) -> Router {
    Router::new().route(
        TEMPLATE,
        axum::routing::put(move |body: axum::body::Bytes| async move {
            if (1..=max).contains(&body.len()) {
                StatusCode::OK
            } else {
                StatusCode::PAYLOAD_TOO_LARGE
            }
        }),
    )
}
async fn run_uploads(app: &Router, document: &Value) -> anyhow::Result<Vec<Captured>> {
    run_binary(
        document,
        "PUT",
        TEMPLATE,
        "image/png",
        vec![7u8; 8],
        &[],
        &async |media: String, bytes: Vec<u8>| {
            let request = Request::builder()
                .method("PUT")
                .uri(TEMPLATE)
                .header(header::CONTENT_TYPE, media)
                .body(Body::from(bytes))
                .unwrap();
            let captured = exchange(app, "PUT", request).await;
            // 413 is an input refusal here, not a harness failure: decide it explicitly.
            let accepted = captured.status.is_success();
            Probe::decided(captured, accepted)
        },
    )
    .await
}
#[tokio::test]
async fn management_openapi_http_vectors_binary_agreement_passes() {
    let exchanges = run_uploads(&upload_app(16), &upload_document())
        .await
        .unwrap();
    assert!(exchanges.len() >= 6);
}
#[tokio::test]
async fn management_openapi_http_vectors_binary_disagreement_fails_without_values() {
    // Looser than the schema: accepts a body over maxLength.
    let text = message(run_uploads(&upload_app(32), &upload_document()).await);
    assert!(
        text.contains("maxLength+1") && text.contains("schema refuses but handler accepts"),
        "{text}"
    );
    // Stricter than the schema: refuses the valid body.
    let text = message(run_uploads(&upload_app(4), &upload_document()).await);
    assert!(
        text.contains("schema accepts but handler refuses"),
        "{text}"
    );
    assert!(!text.contains("[7, 7"), "{text}");
}

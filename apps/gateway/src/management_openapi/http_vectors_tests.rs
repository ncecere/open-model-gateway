//! Controls for the HTTP boundary-vector harness: real axum extractors behind
//! `oneshot`, synthetic schemas, no database. A handler that is looser or
//! stricter than its schema must FAIL, and the failure names no sent value.
use super::http_vectors::{Edit, Probe, body_cases, query_cases, run_body, run_query};
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
        &[],
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
    let cases = query_cases(&document, operation, &base(), &[]).unwrap();
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
    assert!(query_cases(&document, operation, &base()[..2], &[]).is_err());
    let mut extra = base();
    extra.push(("surprise".into(), "1".into()));
    assert!(query_cases(&document, operation, &extra, &[]).is_err());
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
        &[],
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
        &[],
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
    let cases = body_cases(&document, operation, &valid_body(), &[]).unwrap();
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
    assert!(body_cases(&document, operation, &json!({"token":"x"}), &[]).is_err());
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

#[test]
fn management_openapi_http_vectors_seeds_cover_parameters_absent_from_the_base() {
    let document = json!({"openapi":"3.1.0","paths":{TEMPLATE:{"get":{"parameters":[
        parameter("state", true, json!({"type":"string","pattern":"^[0-9a-f]{4}$"})),
        parameter("error", false, json!({"type":"string","maxLength":6})),
    ],"responses":{"200":{"description":"ok"}}}}},"components":{"schemas":{}}});
    let operation = &document["paths"][TEMPLATE]["get"];
    let base = vec![("state".to_owned(), "abcd".to_owned())];
    let seed = vec![("error".to_owned(), "denied".to_owned())];
    // Without a seed the optional parameter has no valid value to derive from.
    assert!(query_cases(&document, operation, &base, &[]).is_err());
    let cases = query_cases(&document, operation, &base, &seed).unwrap();
    let error: Vec<_> = cases.iter().filter(|c| c.param == "error").collect();
    assert!(error.iter().any(|c| c.kind == "maxLength+1" && !c.accept));
    // The baseline never carried `error`, so there is nothing to omit.
    assert!(error.iter().all(|c| c.kind != "omitted"));
    assert!(
        cases
            .iter()
            .any(|c| c.param == "state" && c.kind == "omitted" && !c.accept)
    );
}

// ----- engine v2: flips, seeds, pattern boundaries, positional indices, bytes ----

fn body_only(schema: Value) -> Value {
    json!({"openapi":"3.1.0","paths":{TEMPLATE:{"post":{
        "requestBody":{"required":true,"content":{"application/json":{"schema":schema}}},
        "responses":{"200":{"description":"ok"}}}}},"components":{"schemas":{}}})
}
fn cases_of(
    document: &Value,
    valid: &Value,
    seeds: &[(&str, Value)],
) -> anyhow::Result<Vec<super::http_vectors::BodyCase>> {
    let seeds: Vec<(String, Value)> = seeds
        .iter()
        .map(|(p, v)| ((*p).to_owned(), v.clone()))
        .collect();
    body_cases(
        document,
        &document["paths"][TEMPLATE]["post"],
        valid,
        &seeds,
    )
}
/// A handler that deserializes strictly into `T` (a refusal is 4xx) and then
/// applies its own value rules.
fn judge<T: serde::de::DeserializeOwned + Send + 'static>(check: fn(&T) -> bool) -> Router {
    Router::new().route(
        TEMPLATE,
        post(move |Json(v): Json<T>| async move {
            if check(&v) {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            }
        }),
    )
}
async fn run_judged(
    app: &Router,
    document: &Value,
    valid: Value,
    seeds: &[(String, Value)],
) -> anyhow::Result<Vec<Captured>> {
    run_body(
        document,
        "POST",
        TEMPLATE,
        valid,
        seeds,
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
fn has(
    cases: &[super::http_vectors::BodyCase],
    pointer: &str,
    kind: &str,
    accept: bool,
    test: impl Fn(&Value) -> bool,
) -> bool {
    cases
        .iter()
        .any(|c| c.pointer == pointer && c.kind == kind && c.accept == accept && test(&c.body))
}

/// The notification-read shape: a cross-field rule between two siblings.
fn cross_field() -> Value {
    body_only(
        json!({"type":"object","required":[],"additionalProperties":false,"properties":{
        "ids":{"type":["array","null"],"minItems":1,"maxItems":3,"items":{"type":"string","pattern":"^[0-9a-f]{4}$"}},
        "all":{"type":"boolean","default":false}},
        "oneOf":[
            {"required":["all"],"properties":{"all":{"const":true},"ids":{"type":"null"}}},
            {"required":["ids"],"properties":{"all":{"const":false},"ids":{"type":"array"}}}]}),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    ids: Option<Vec<String>>,
    #[serde(default)]
    all: bool,
}
fn read_shape_ok(b: &Read) -> bool {
    b.ids.as_ref().is_none_or(|ids| {
        (1..=3).contains(&ids.len())
            && ids.iter().all(|id| {
                id.len() == 4 && id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
            })
    })
}
/// Exactly one of `all=true` or `ids`.
fn read_exact(b: &Read) -> bool {
    read_shape_ok(b) && b.all != b.ids.is_some()
}
/// Forgets that both together are refused.
fn read_loose(b: &Read) -> bool {
    read_shape_ok(b) && (b.all || b.ids.is_some())
}

#[test]
fn management_openapi_http_vectors_flip_booleans_and_consts() {
    let document = cross_field();
    let cases = cases_of(&document, &json!({"ids":["abcd"],"all":false}), &[]).unwrap();
    // `{"ids":[x],"all":true}` reaches the handler, and the schema refuses it.
    assert!(has(&cases, "/all", "boolean flip", false, |b| {
        b["all"] == json!(true) && b["ids"] == json!(["abcd"])
    }));
    let cases = cases_of(&document, &json!({"all":true}), &[]).unwrap();
    assert!(has(&cases, "/all", "boolean flip", false, |b| {
        b["all"] == json!(false) && b.get("ids").is_none()
    }));
    // A const is flipped to a different value of its type.
    let document = body_only(json!({"type":"object","required":["kind","n"],
        "additionalProperties":false,"properties":{"kind":{"const":"fixed"},"n":{"const":7}}}));
    let cases = cases_of(&document, &json!({"kind":"fixed","n":7}), &[]).unwrap();
    assert!(has(&cases, "/kind", "const flip", false, |b| b["kind"]
        != json!("fixed")));
    assert!(has(&cases, "/n", "const flip", false, |b| b["n"] != json!(7)));
}

#[tokio::test]
async fn management_openapi_http_vectors_a_handler_missing_a_cross_field_rule_fails() {
    let document = cross_field();
    let valid = json!({"ids":["abcd"],"all":false});
    run_judged(&judge(read_exact), &document, valid.clone(), &[])
        .await
        .unwrap();
    let text = message(run_judged(&judge(read_loose), &document, valid, &[]).await);
    assert!(
        text.contains("boolean flip")
            && text.contains("/all")
            && text.contains("schema refuses but handler accepts"),
        "{text}"
    );
    assert!(!text.contains("abcd"), "{text}");
}

#[test]
fn management_openapi_http_vectors_seeds_add_declared_absent_optionals() {
    let document = cross_field();
    let valid = json!({"ids":["abcd"]});
    // Without a seed the absent optional is never sent.
    let cases = cases_of(&document, &valid, &[]).unwrap();
    assert!(
        cases
            .iter()
            .all(|c| c.pointer != "/all" || c.kind == "omitted")
    );
    let cases = cases_of(&document, &valid, &[("/all", json!(true))]).unwrap();
    assert!(has(&cases, "/all", "optional added", false, |b| {
        b["all"] == json!(true) && b["ids"] == json!(["abcd"])
    }));
    // The seeded value is walked too: its own flip is a vector.
    assert!(has(&cases, "/all", "boolean flip", true, |b| b["all"]
        == json!(false)));
    // A seed for an undeclared or already present location is a harness error
    // that names the pointer.
    for bad in ["/typo", "/ids"] {
        let error = cases_of(&document, &valid, &[(bad, json!(true))])
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains(bad), "{error:#}");
    }
    // Trailing optional positional items are seeded the same way.
    let tuple = body_only(json!({"type":"array","prefixItems":[
        {"type":"string","pattern":"^[0-9a-f]{4}$"},{"type":"boolean"}],
        "minItems":1,"maxItems":2,"items":false}));
    let cases = cases_of(&tuple, &json!(["abcd"]), &[("/1", json!(true))]).unwrap();
    assert!(has(&cases, "/1", "optional added", true, |b| *b
        == json!(["abcd", true])));
}

#[tokio::test]
async fn management_openapi_http_vectors_seeded_run_sends_the_added_optional() {
    let document = cross_field();
    let seeds = vec![("/all".to_owned(), json!(true))];
    let valid = json!({"ids":["abcd"]});
    let app = judge(read_exact);
    let exchanges = run_judged(&app, &document, valid.clone(), &seeds)
        .await
        .unwrap();
    assert!(exchanges.len() > 10, "{}", exchanges.len());
    let text = message(run_judged(&judge(read_loose), &document, valid, &seeds).await);
    assert!(
        text.contains("optional added") && text.contains("/all"),
        "{text}"
    );
}

#[test]
fn management_openapi_http_vectors_walk_every_positional_index() {
    let tuple = body_only(json!({"type":"array","prefixItems":[
        {"type":"string","pattern":"^[0-9a-f]{4}$"},
        {"type":["array","null"],"items":{"type":"string","maxLength":3}},
        {"type":"boolean"}],"minItems":3,"maxItems":3,"items":false}));
    let valid = json!(["abcd", ["x"], false]);
    let cases = cases_of(&tuple, &valid, &[]).unwrap();
    for (pointer, kind, accept) in [
        ("/0", "pattern violation", false),
        ("/1", "null", true),
        ("/1", "wrong type", false),
        ("/1/0", "maxLength+1", false),
        ("/2", "wrong type", false),
        ("/2", "boolean flip", true),
        ("/2", "null", false),
    ] {
        assert!(
            has(&cases, pointer, kind, accept, |_| true),
            "missing {kind} at {pointer}"
        );
    }
}

fn pattern_document() -> Value {
    body_only(
        json!({"type":"object","required":["key","token"],"additionalProperties":false,"properties":{
        "reason":{"type":["string","null"],"maxLength":10,"pattern":"^[^\\u0000-\\u001f\\u007f-\\u009f]*$"},
        "key":{"type":"string","pattern":"^[!-~]{1,8}$"},
        "token":{"type":"string","pattern":"^[0-9a-fA-F]{4}$"}}}),
    )
}
fn pattern_valid() -> Value {
    json!({"reason":"ok","key":"abc","token":"abcd"})
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fields {
    reason: Option<String>,
    key: String,
    token: String,
}
fn fields_ok(b: &Fields, controls: bool, key_limit: usize) -> bool {
    b.reason
        .as_ref()
        .is_none_or(|r| r.chars().count() <= 10 && (!controls || !r.chars().any(char::is_control)))
        && (1..=key_limit).contains(&b.key.len())
        && b.key.bytes().all(|c| c.is_ascii_graphic())
        && b.token.len() == 4
        && b.token.bytes().all(|c| c.is_ascii_hexdigit())
}
fn fields_exact(b: &Fields) -> bool {
    fields_ok(b, true, 8)
}
fn fields_without_control_check(b: &Fields) -> bool {
    fields_ok(b, false, 8)
}
fn fields_wide_key(b: &Fields) -> bool {
    fields_ok(b, true, 1024)
}

#[test]
fn management_openapi_http_vectors_pattern_boundaries_cover_controls_multibyte_and_lengths() {
    let cases = cases_of(&pattern_document(), &pattern_valid(), &[]).unwrap();
    for c in ['\n', '\0', '\u{7f}', '\u{85}'] {
        assert!(
            has(&cases, "/reason", "pattern violation", false, |b| {
                b["reason"].as_str().is_some_and(|r| r.contains(c))
            }),
            "no control character candidate"
        );
    }
    // Multibyte values the pattern allows are sent, and accepted by the schema.
    assert!(has(&cases, "/reason", "pattern candidate", true, |b| {
        b["reason"]
            .as_str()
            .is_some_and(|r| r.chars().any(|c| c.len_utf8() > 1))
    }));
    // Length boundaries of `^X{m,n}$`: m-1, m, n, n+1, from the valid seed's characters.
    let key_lengths = |accept: bool| -> Vec<usize> {
        cases
            .iter()
            .filter(|c| c.pointer == "/key" && c.accept == accept)
            .filter_map(|c| c.body["key"].as_str().map(str::len))
            .collect()
    };
    assert!(key_lengths(false).contains(&9), "n+1 is a vector");
    assert!(key_lengths(true).contains(&8), "n is a vector");
    let token_lengths: Vec<usize> = cases
        .iter()
        .filter(|c| c.pointer == "/token" && !c.accept)
        .filter_map(|c| c.body["token"].as_str().map(str::len))
        .collect();
    assert!(token_lengths.contains(&3) && token_lengths.contains(&5));
    // `^X+$`: the empty string is the m-1 vector.
    let plus = body_only(
        json!({"type":"object","required":["a"],"additionalProperties":false,
        "properties":{"a":{"type":"string","pattern":"^[a-z]+$"}}}),
    );
    let cases = cases_of(&plus, &json!({"a":"abc"}), &[]).unwrap();
    assert!(
        cases
            .iter()
            .any(|c| c.pointer == "/a" && !c.accept && c.body["a"] == json!(""))
    );
}

#[tokio::test]
async fn management_openapi_http_vectors_pattern_boundaries_catch_loose_handlers() {
    let run = |app: Router| async move {
        run_judged(&app, &pattern_document(), pattern_valid(), &[]).await
    };
    run(judge(fields_exact)).await.unwrap();
    let text = message(run(judge(fields_without_control_check)).await);
    assert!(
        text.contains("/reason")
            && text.contains("pattern violation")
            && text.contains("schema refuses but handler accepts"),
        "{text}"
    );
    let text = message(run(judge(fields_wide_key)).await);
    assert!(
        text.contains("/key") && text.contains("pattern violation"),
        "{text}"
    );
}

fn byte_limited(extension: bool) -> Value {
    let mut code = json!({"type":"string","minLength":1,"maxLength":8});
    if extension {
        code["x-omg-max-bytes"] = json!(8);
    }
    body_only(
        json!({"type":"object","required":["code"],"additionalProperties":false,
        "properties":{"code":code}}),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Code {
    code: String,
}
fn by_bytes(b: &Code) -> bool {
    !b.code.is_empty() && b.code.len() <= 8
}
fn by_chars(b: &Code) -> bool {
    !b.code.is_empty() && b.code.chars().count() <= 8
}

#[test]
fn management_openapi_http_vectors_byte_limits_are_honoured_by_the_oracle() {
    let valid = json!({"code":"abc"});
    let multibyte = |b: &Value, n: usize| {
        b["code"]
            .as_str()
            .is_some_and(|c| c.chars().count() == n && c.len() > c.chars().count())
    };
    // 8 two-byte characters satisfy maxLength (characters) but not 8 BYTES.
    let cases = cases_of(&byte_limited(true), &valid, &[]).unwrap();
    assert!(has(&cases, "/code", "multibyte maxLength", false, |b| {
        multibyte(b, 8)
    }));
    // 4 two-byte characters are exactly 8 bytes: accepted.
    assert!(has(&cases, "/code", "multibyte maxBytes", true, |b| {
        multibyte(b, 4)
    }));
    assert!(has(&cases, "/code", "multibyte maxBytes+1", false, |b| {
        multibyte(b, 5)
    }));
    // Without the byte annotation, characters are what the schema counts.
    let cases = cases_of(&byte_limited(false), &valid, &[]).unwrap();
    assert!(has(&cases, "/code", "multibyte maxLength", true, |b| {
        multibyte(b, 8)
    }));
    assert!(has(&cases, "/code", "multibyte maxLength+1", false, |b| {
        multibyte(b, 9)
    }));
}

#[tokio::test]
async fn management_openapi_http_vectors_a_char_byte_mismatch_fails() {
    let valid = json!({"code":"abc"});
    let run = |document: Value, check: fn(&Code) -> bool| {
        let valid = valid.clone();
        async move { run_judged(&judge(check), &document, valid, &[]).await }
    };
    // The honest pairings agree.
    run(byte_limited(true), by_bytes).await.unwrap();
    run(byte_limited(false), by_chars).await.unwrap();
    // A byte-limited handler documented only in characters is stricter.
    let text = message(run(byte_limited(false), by_bytes).await);
    assert!(
        text.contains("multibyte maxLength") && text.contains("schema accepts but handler refuses"),
        "{text}"
    );
    // A character-counting handler documented as a byte limit is looser.
    let text = message(run(byte_limited(true), by_chars).await);
    assert!(
        text.contains("multibyte maxLength") && text.contains("schema refuses but handler accepts"),
        "{text}"
    );
    assert!(!text.contains('\u{e9}'), "{text}");
}

#[tokio::test]
async fn management_openapi_http_vectors_decided_probes_still_fail_on_unexpected_statuses() {
    let document = query_document();
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let app = Router::new().route(TEMPLATE, get(move || async move { status }));
        for accepted in [true, false] {
            let result = run_query(
                &document,
                "GET",
                TEMPLATE,
                &[],
                &async || ((), base()),
                &async |_: &(), query: String| {
                    let request = Request::builder()
                        .uri(format!("{TEMPLATE}?{query}"))
                        .body(Body::empty())
                        .unwrap();
                    Probe::decided(exchange(&app, "GET", request).await, accepted)
                },
            )
            .await;
            let text = message(result);
            assert!(
                text.contains(&format!("unexpected status {}", status.as_u16())),
                "{text}"
            );
        }
    }
    // Within the expected set an explicit verdict may override the status.
    let app = Router::new().route(TEMPLATE, get(|| async { StatusCode::BAD_REQUEST }));
    let result = run_query(
        &document,
        "GET",
        TEMPLATE,
        &[],
        &async || ((), base()),
        &async |_: &(), query: String| {
            let request = Request::builder()
                .uri(format!("{TEMPLATE}?{query}"))
                .body(Body::empty())
                .unwrap();
            Probe::decided(exchange(&app, "GET", request).await, true)
        },
    )
    .await;
    // Every request is "accepted" by decision, so the schema-refused vectors mismatch,
    // proving the decision (not the 400) was used.
    assert!(message(result).contains("schema refuses but handler accepts"));
}

fn code_document(schema: Value, exempt: bool) -> Value {
    let mut parameter = parameter("code", true, schema);
    if exempt {
        parameter["x-omg-unsendable-boundary"] =
            json!("The handler caps this value far beyond what one request target can carry.");
    }
    json!({"openapi":"3.1.0","paths":{TEMPLATE:{"get":{"parameters":[parameter],
        "responses":{"200":{"description":"ok"}}}}},"components":{"schemas":{}}})
}
fn code_cases(schema: Value, exempt: bool) -> anyhow::Result<Vec<super::http_vectors::QueryCase>> {
    let document = code_document(schema, exempt);
    query_cases(
        &document,
        &document["paths"][TEMPLATE]["get"],
        &[("code".to_owned(), "good".to_owned())],
        &[],
    )
}

#[test]
fn management_openapi_http_vectors_query_sizes_use_the_real_encoded_length() {
    // 59001 unreserved characters are 59001 encoded bytes: they fit the request
    // target, so the decisive maxLength+1 vector must be sent (a worst-case
    // 3-bytes-per-character estimate used to drop it silently).
    // (Its multibyte variants cannot be sent, so the parameter carries the reviewed
    // exemption; maxLength and maxLength+1 are sent regardless.)
    let cases = code_cases(json!({"type":"string","maxLength":59_000}), true).unwrap();
    for kind in ["maxLength", "maxLength+1"] {
        assert!(
            cases
                .iter()
                .any(|c| c.kind == kind && matches!(&c.edit, Edit::Set(r) if r.len() >= 59_000)),
            "{kind} was not sent"
        );
    }
    for case in &cases {
        if let Edit::Set(raw) = &case.edit {
            let pairs = vec![("code".to_owned(), raw.clone())];
            assert!(super::http_vectors::encode(&pairs).len() <= 60_010);
        }
    }
    // Multibyte values that fit are still sent around the character limit.
    let cases = code_cases(json!({"type":"string","maxLength":9000}), false).unwrap();
    assert!(cases.iter().any(|c| c.kind == "multibyte maxLength"
        && matches!(&c.edit, Edit::Set(r) if r.starts_with('\u{e9}'))));
}

#[test]
fn management_openapi_http_vectors_an_unsendable_decisive_query_vector_fails() {
    // RED: 70000 characters cannot ride a request target of 65534 bytes.
    let text = format!(
        "{:#}",
        code_cases(json!({"type":"string","maxLength":70_000}), false)
            .expect_err("an unsendable maxLength+1 must fail")
    );
    assert!(
        text.contains("maxLength+1")
            && text.contains("code")
            && text.contains("x-omg-unsendable-boundary"),
        "{text}"
    );
    assert!(!text.contains("good"), "{text}");
    // GREEN: a reviewed reason on the parameter excuses it, and those vectors are not sent.
    let cases = code_cases(json!({"type":"string","maxLength":70_000}), true).unwrap();
    assert!(cases.iter().all(|c| c.kind != "maxLength+1"));
    // One kind sent in some variant is enough: the emoji variant of a multibyte
    // vector may be too long while the two-byte variant fits.
    code_cases(json!({"type":"string","maxLength":9000}), false).unwrap();
}

fn property_document(schema: Value) -> Value {
    body_only(
        json!({"type":"object","required":["big"],"additionalProperties":false,
        "properties":{"big":schema}}),
    )
}
fn unsendable_body(schema: Value, valid: Value) -> anyhow::Result<()> {
    cases_of(&property_document(schema), &json!({"big":valid}), &[]).map(|_| ())
}
const REASON: &str = "The handler caps this value far beyond what the harness can send.";

#[test]
fn management_openapi_http_vectors_an_unsendable_decisive_body_vector_fails() {
    for (label, schema, valid, kind) in [
        (
            "maxLength past the harness cap",
            json!({"type":"string","maxLength":150_000}),
            json!("abc"),
            "maxLength+1",
        ),
        (
            "multibyte maxLength past the multibyte cap",
            json!({"type":"string","maxLength":30_000}),
            json!("abc"),
            "multibyte maxLength",
        ),
        (
            "minLength past the harness cap",
            json!({"type":"string","minLength":150_000}),
            json!("a".repeat(150_000)),
            "minLength",
        ),
        (
            "maxBytes past the harness cap",
            json!({"type":"string","maxLength":120_000,"x-omg-max-bytes":120_000}),
            json!("abc"),
            "multibyte maxBytes",
        ),
        (
            "maxItems past the item cap",
            json!({"type":"array","maxItems":2000,"items":{"type":"string"}}),
            json!(["a"]),
            "maxItems+1",
        ),
    ] {
        // RED: nothing is sent for the decisive bound, so the run must fail.
        let error = unsendable_body(schema.clone(), valid.clone())
            .err()
            .unwrap_or_else(|| panic!("{label}: an unsendable boundary passed silently"));
        let text = format!("{error:#}");
        assert!(
            text.contains(kind)
                && text.contains("/big")
                && text.contains("x-omg-unsendable-boundary"),
            "{label}: {text}"
        );
        assert!(!text.contains("abc"), "{label}: {text}");
        // GREEN: the reviewed annotation on the direct property excuses it.
        let mut exempt = schema;
        exempt["x-omg-unsendable-boundary"] = json!(REASON);
        unsendable_body(exempt, valid).unwrap_or_else(|e| panic!("{label}: {e:#}"));
    }
    // Bounds the harness can send never need the annotation.
    unsendable_body(json!({"type":"string","maxLength":9000}), json!("abc")).unwrap();
    unsendable_body(
        json!({"type":"array","maxItems":999,"items":{"type":"string"}}),
        json!(["a"]),
    )
    .unwrap();
}

#[test]
fn management_openapi_http_vectors_non_decisive_drops_are_counted_not_silent() {
    use super::http_vectors::Notes;
    let mut notes = Notes::default();
    // A pattern boundary past the cap is counted, never a failure.
    notes.settle("/big", &["pattern length"], &[], false);
    assert_eq!((notes.minor, notes.exempt, notes.unsent.len()), (1, 0, 0));
    notes.check().unwrap();
    // A decisive kind that was sent in another variant is only counted.
    notes.settle(
        "/big",
        &["multibyte maxLength"],
        &["multibyte maxLength"],
        false,
    );
    assert_eq!((notes.minor, notes.unsent.len()), (2, 0));
    // A decisive kind with nothing sent is an exemption when reviewed, else a failure.
    notes.settle("/big", &["maxItems"], &[], true);
    assert_eq!((notes.exempt, notes.unsent.len()), (1, 0));
    notes.settle("/big", &["maxItems"], &[], false);
    let text = format!("{:#}", notes.check().unwrap_err());
    assert!(text.contains("maxItems at /big"), "{text}");
}

// ----- fail-closed v2: unreached locations, sibling consts, unbounded patterns, bytes ----

use super::request_reach;
use std::collections::BTreeSet;

fn body_schema_of(document: &Value) -> &Value {
    &document["paths"][TEMPLATE]["post"]["requestBody"]["content"]["application/json"]["schema"]
}
/// The locations a valid body (plus seeds) reaches, and what is declared but not reached.
fn reach_of(document: &Value, valid: &Value, seeds: &[(&str, Value)]) -> BTreeSet<String> {
    let seeds: Vec<(String, Value)> = seeds
        .iter()
        .map(|(p, v)| ((*p).to_owned(), v.clone()))
        .collect();
    super::http_vectors::body_plan(
        document,
        &document["paths"][TEMPLATE]["post"],
        valid,
        &seeds,
        &[],
    )
    .unwrap()
    .reached
}
fn unreached_in(document: &Value, reached: &BTreeSet<String>) -> Vec<(String, &'static str)> {
    request_reach::unreached(document, body_schema_of(document), reached).unwrap()
}
fn unreached_paths(document: &Value, reached: &BTreeSet<String>) -> Vec<String> {
    unreached_in(document, reached)
        .into_iter()
        .map(|(path, _)| path)
        .collect()
}

#[test]
fn management_openapi_http_vectors_report_declared_optional_properties_never_carried() {
    let mut optional = json!({"type":"string","maxLength":5});
    let schema = |b: &Value| {
        body_only(
            json!({"type":"object","required":["a"],"additionalProperties":false,
        "properties":{"a":{"type":"string"},"b":b}}),
        )
    };
    let document = schema(&optional);
    // RED: the valid body omits the optional `b`, so nothing ever probes it.
    let reached = reach_of(&document, &json!({"a":"x"}), &[]);
    assert_eq!(
        unreached_in(&document, &reached),
        vec![("/properties/b".to_owned(), "property")]
    );
    // GREEN: a valid body that carries it, or a seed that adds it, reaches it.
    let reached = reach_of(&document, &json!({"a":"x","b":"y"}), &[]);
    assert!(unreached_paths(&document, &reached).is_empty());
    let reached = reach_of(&document, &json!({"a":"x"}), &[("/b", json!("y"))]);
    assert!(unreached_paths(&document, &reached).is_empty());
    // A reviewed exemption excuses it (and only with a reason).
    optional["x-omg-unprobed-location"] = json!("The server derives this value itself.");
    let document = schema(&optional);
    let reached = reach_of(&document, &json!({"a":"x"}), &[]);
    assert!(unreached_paths(&document, &reached).is_empty());
    optional["x-omg-unprobed-location"] = json!(" ");
    let document = schema(&optional);
    let reached = reach_of(&document, &json!({"a":"x"}), &[]);
    assert_eq!(unreached_paths(&document, &reached), ["/properties/b"]);
}

#[test]
fn management_openapi_http_vectors_report_map_values_never_carried() {
    let map = |extra: Value| {
        let mut node = json!({"type":"object"});
        for (k, v) in extra.as_object().unwrap() {
            node[k] = v.clone();
        }
        body_only(
            json!({"type":"object","required":["m"],"additionalProperties":false,
            "properties":{"m":node}}),
        )
    };
    let additional = map(json!({"additionalProperties":{"type":"string","maxLength":4}}));
    let reached = reach_of(&additional, &json!({"m":{}}), &[]);
    assert_eq!(
        unreached_in(&additional, &reached),
        vec![("/properties/m/additionalProperties".to_owned(), "map value")]
    );
    let valid = json!({"m":{"k":"v"}});
    let reached = reach_of(&additional, &valid, &[]);
    assert!(unreached_paths(&additional, &reached).is_empty());
    // ...and the carried map value is walked like any other location.
    let cases = cases_of(&additional, &valid, &[]).unwrap();
    assert!(has(&cases, "/m/k", "maxLength+1", false, |_| true));
    assert!(has(&cases, "/m/k", "wrong type", false, |_| true));
    // patternProperties values are map values too, selected by name.
    let patterned = map(
        json!({"patternProperties":{"^x-":{"type":"integer","maximum":9}},
        "additionalProperties":false}),
    );
    let reached = reach_of(&patterned, &json!({"m":{}}), &[]);
    assert_eq!(
        unreached_paths(&patterned, &reached),
        ["/properties/m/patternProperties/^x-"]
    );
    let valid = json!({"m":{"x-a":1}});
    let reached = reach_of(&patterned, &valid, &[]);
    assert!(unreached_paths(&patterned, &reached).is_empty());
    let cases = cases_of(&patterned, &valid, &[]).unwrap();
    assert!(has(&cases, "/m/x-a", "maximum+1", false, |_| true));
}

#[test]
fn management_openapi_http_vectors_report_array_elements_beyond_the_first() {
    let tuple = body_only(json!({"type":"array","prefixItems":[
        {"type":"string"},{"type":"integer","maximum":9}],"minItems":1,"maxItems":2,"items":false}));
    let reached = reach_of(&tuple, &json!(["a"]), &[]);
    assert_eq!(
        unreached_in(&tuple, &reached),
        vec![("/prefixItems/1".to_owned(), "array element")]
    );
    let reached = reach_of(&tuple, &json!(["a", 1]), &[]);
    assert!(unreached_paths(&tuple, &reached).is_empty());
    let reached = reach_of(&tuple, &json!(["a"]), &[("/1", json!(1))]);
    assert!(unreached_paths(&tuple, &reached).is_empty());
    // The tail `items` schema after the positional prefix is a location, and its
    // first element is walked (it used to be skipped: only index 0 was).
    let tail = body_only(json!({"type":"array","prefixItems":[{"type":"string"}],
        "items":{"type":"integer","maximum":9}}));
    let reached = reach_of(&tail, &json!(["a"]), &[]);
    assert_eq!(unreached_paths(&tail, &reached), ["/items"]);
    let valid = json!(["a", 1]);
    let reached = reach_of(&tail, &valid, &[]);
    assert!(unreached_paths(&tail, &reached).is_empty());
    let cases = cases_of(&tail, &valid, &[]).unwrap();
    assert!(has(&cases, "/1", "maximum+1", false, |_| true));
}

#[test]
fn management_openapi_http_vectors_report_union_arms_the_valid_body_did_not_match() {
    let both = body_only(json!({"oneOf":[
        {"type":"object","required":["a"],"additionalProperties":false,"properties":{"a":{"type":"string"}}},
        {"type":"array","prefixItems":[{"type":"string"}],"minItems":1,"maxItems":1,"items":false}]}));
    // One run per wire form: the object form alone leaves the positional arm and its items.
    let object = reach_of(&both, &json!({"a":"x"}), &[]);
    assert_eq!(
        unreached_in(&both, &object),
        vec![
            ("/oneOf/1".to_owned(), "union arm"),
            ("/oneOf/1/prefixItems/0".to_owned(), "array element")
        ]
    );
    let positional = reach_of(&both, &json!(["x"]), &[]);
    assert_eq!(
        unreached_paths(&both, &positional),
        ["/oneOf/0", "/oneOf/0/properties/a"]
    );
    // Together (two runs of one operation) nothing is left.
    let union: BTreeSet<String> = object.union(&positional).cloned().collect();
    assert!(unreached_paths(&both, &union).is_empty());
    // An exempt arm excuses itself and its contents.
    let mut exempt = both.clone();
    exempt["paths"][TEMPLATE]["post"]["requestBody"]["content"]["application/json"]["schema"]["oneOf"]
        [1]["x-omg-unprobed-location"] = json!("The positional form is retired.");
    assert!(unreached_paths(&exempt, &object).is_empty());
}

fn consts(first: &str, second: &str) -> Value {
    body_only(
        json!({"type":"object","required":["period"],"additionalProperties":false,
        "properties":{"period":{"oneOf":[{"const":first},{"const":second}]}}}),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Period {
    period: String,
}
fn month_only(b: &Period) -> bool {
    b.period == "month"
}
fn month_or_day(b: &Period) -> bool {
    b.period == "month" || b.period == "day"
}

#[tokio::test]
async fn management_openapi_http_vectors_offer_sibling_consts_of_a_union() {
    let document = consts("month", "day");
    let valid = json!({"period":"month"});
    let cases = cases_of(&document, &valid, &[]).unwrap();
    // The sibling branch's const is sent as an enum member the schema accepts.
    assert!(has(&cases, "/period", "enum member", true, |b| b["period"]
        == json!("day")));
    assert!(has(&cases, "/period", "enum non-member", false, |_| true));
    // RED: a handler that refuses "day" disagrees with the schema.
    let text = message(run_judged(&judge(month_only), &document, valid.clone(), &[]).await);
    assert!(
        text.contains("enum member")
            && text.contains("/period")
            && text.contains("schema accepts but handler refuses"),
        "{text}"
    );
    assert!(!text.contains("day"), "{text}");
    // GREEN: a handler that honours both consts agrees.
    run_judged(&judge(month_or_day), &document, valid, &[])
        .await
        .unwrap();
    // Enum members of sibling branches are offered too.
    let document = body_only(json!({"type":"object","required":["period"],
        "additionalProperties":false,"properties":{"period":{"anyOf":[
            {"enum":["month","week"]},{"enum":["day","hour"]}]}}}));
    let cases = cases_of(&document, &json!({"period":"month"}), &[]).unwrap();
    for member in ["week", "day", "hour"] {
        assert!(
            has(&cases, "/period", "enum member", true, |b| b["period"]
                == json!(member)),
            "{member} was not offered"
        );
    }
}

fn unbounded_pattern(extra: Value) -> Value {
    let mut number = json!({"type":"string","pattern":"^(0|[1-9][0-9]{0,14})$"});
    for (k, v) in extra.as_object().unwrap() {
        number[k] = v.clone();
    }
    body_only(
        json!({"type":"object","required":["n"],"additionalProperties":false,
        "properties":{"n":number}}),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Number {
    n: String,
}
fn digits_to(limit: usize, b: &Number) -> bool {
    b.n == "0"
        || (!b.n.starts_with('0')
            && (1..=limit).contains(&b.n.len())
            && b.n.bytes().all(|c| c.is_ascii_digit()))
}
fn capped_at_15(b: &Number) -> bool {
    digits_to(15, b)
}
fn capped_at_16(b: &Number) -> bool {
    digits_to(16, b)
}
fn candidate_values() -> Vec<(String, Value)> {
    ["0", "7", &"1".repeat(15), &"1".repeat(16)]
        .iter()
        .map(|n| ("/n".to_owned(), json!(n)))
        .collect()
}
async fn run_with_candidates(
    app: &Router,
    document: &Value,
    candidates: &[(String, Value)],
) -> anyhow::Result<Vec<Captured>> {
    Ok(super::http_vectors::run_body_with(
        document,
        "POST",
        TEMPLATE,
        json!({"n":"15"}),
        &[],
        candidates,
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
    .await?
    .exchanges)
}

#[tokio::test]
async fn management_openapi_http_vectors_unbounded_patterns_need_candidates_or_a_reason() {
    let document = unbounded_pattern(json!({}));
    // RED: nothing derives the 15/16 digit boundary from this pattern, so the
    // run must refuse rather than pass without probing it.
    let text = message(run_with_candidates(&judge(capped_at_15), &document, &[]).await);
    assert!(
        text.contains("/n")
            && text.contains("no derivable length bounds")
            && text.contains("x-omg-pattern-boundaries"),
        "{text}"
    );
    assert!(!text.contains("15"), "{text}");
    // GREEN: candidates at the boundary (judged by the SCHEMA) make it pass...
    run_with_candidates(&judge(capped_at_15), &document, &candidate_values())
        .await
        .unwrap();
    // ...and a handler whose cap is one digit off now fails.
    let text =
        message(run_with_candidates(&judge(capped_at_16), &document, &candidate_values()).await);
    assert!(
        text.contains("supplied candidate")
            && text.contains("/n")
            && text.contains("schema refuses but handler accepts"),
        "{text}"
    );
    // A reviewed reason is the other way out.
    let reviewed = unbounded_pattern(json!({"x-omg-pattern-boundaries":
        "The cap is exercised by the handler's own unit tests."}));
    run_with_candidates(&judge(capped_at_15), &reviewed, &[])
        .await
        .unwrap();
    // A schema that leaves the pattern unbounded while the handler caps it is stricter.
    let open = body_only(
        json!({"type":"object","required":["n"],"additionalProperties":false,
        "properties":{"n":{"type":"string","pattern":"^(0|[1-9][0-9]*)$"}}}),
    );
    let mut with_refusal = candidate_values();
    with_refusal.push(("/n".to_owned(), json!("007")));
    let text = message(run_with_candidates(&judge(capped_at_15), &open, &with_refusal).await);
    assert!(
        text.contains("supplied candidate") && text.contains("schema accepts but handler refuses"),
        "{text}"
    );
    // A maxLength, or one plain character class, already derives the bounds.
    run_with_candidates(
        &judge(capped_at_15),
        &unbounded_pattern(json!({"maxLength":15})),
        &[],
    )
    .await
    .unwrap();
}

#[test]
fn management_openapi_http_vectors_candidates_are_checked() {
    let document = unbounded_pattern(json!({}));
    let plan = |candidates: &[(&str, Value)]| {
        let candidates: Vec<(String, Value)> = candidates
            .iter()
            .map(|(p, v)| ((*p).to_owned(), v.clone()))
            .collect();
        super::http_vectors::body_plan(
            &document,
            &document["paths"][TEMPLATE]["post"],
            &json!({"n":"15"}),
            &[],
            &candidates,
        )
    };
    let text = format!("{:#}", plan(&[]).err().unwrap());
    assert!(text.contains("no derivable length bounds"), "{text}");
    // One-sided candidates prove nothing about the boundary.
    let text = format!(
        "{:#}",
        plan(&[("/n", json!("0")), ("/n", json!("7"))])
            .err()
            .unwrap()
    );
    assert!(
        text.contains("accepts") && text.contains("refuses"),
        "{text}"
    );
    // A candidate must name a walked location.
    let text = format!(
        "{:#}",
        plan(&[("/zzz", json!("1")), ("/n", json!("0")), ("/n", json!("x"))])
            .err()
            .unwrap()
    );
    assert!(text.contains("/zzz"), "{text}");
    // Both sides present: the verdict of each comes from the schema.
    let ok = plan(&[("/n", json!("0")), ("/n", json!("1".repeat(16)))]).unwrap();
    let verdict = |digits: usize| {
        ok.cases
            .iter()
            .find(|c| {
                c.kind == "supplied candidate"
                    && c.body["n"].as_str().is_some_and(|n| n.len() == digits)
            })
            .map(|c| c.accept)
    };
    assert_eq!((verdict(1), verdict(16)), (Some(true), Some(false)));
}

#[test]
fn management_openapi_http_vectors_unbounded_query_patterns_need_candidates_too() {
    let document = |extra: Value| {
        let mut schema = json!({"type":"string","pattern":"^(0|[1-9][0-9]{0,14})$"});
        for (k, v) in extra.as_object().unwrap() {
            schema[k] = v.clone();
        }
        json!({"openapi":"3.1.0","paths":{TEMPLATE:{"get":{"parameters":[
            parameter("n", true, schema)],"responses":{"200":{"description":"ok"}}}}},
            "components":{"schemas":{}}})
    };
    let plan = |document: &Value, candidates: &[(&str, &str)]| {
        let candidates: Vec<(String, String)> = candidates
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect();
        super::http_vectors::query_plan(
            document,
            &document["paths"][TEMPLATE]["get"],
            &[("n".to_owned(), "15".to_owned())],
            &[],
            &candidates,
        )
    };
    let open = document(json!({}));
    let text = format!("{:#}", plan(&open, &[]).err().unwrap());
    assert!(
        text.contains("no derivable length bounds") && text.contains("n"),
        "{text}"
    );
    let sixteen = "1".repeat(16);
    let good = plan(&open, &[("n", "0"), ("n", sixteen.as_str())]).unwrap();
    assert!(good.cases.iter().any(|c| c.kind == "supplied candidate"
        && matches!(&c.edit, Edit::Set(r) if r.len() == 16)
        && !c.accept));
    assert!(plan(&open, &[("n", "0"), ("n", "7")]).is_err());
    assert!(plan(&open, &[("zzz", "0"), ("n", "0"), ("n", sixteen.as_str())]).is_err());
    plan(
        &document(json!({"x-omg-pattern-boundaries":"Covered by the handler's unit tests."})),
        &[],
    )
    .unwrap();
}

#[test]
fn management_openapi_http_vectors_the_byte_oracle_covers_every_element_and_map_value() {
    let accepts = |schema: Value, instance: Value| {
        let document = body_only(schema.clone());
        super::http_vectors::accepts(&document, &schema, &instance).unwrap()
    };
    let limited = || json!({"type":"string","x-omg-max-bytes":4});
    let two_byte = "\u{e9}\u{e9}\u{e9}"; // 3 characters, 6 bytes
    // Homogeneous `items`: the SECOND element is checked as well as the first.
    let items = json!({"type":"array","items":limited()});
    assert!(accepts(items.clone(), json!(["ab", "ab"])));
    assert!(!accepts(items.clone(), json!(["ab", two_byte])));
    assert!(!accepts(items.clone(), json!([two_byte, "ab"])));
    // The tail after a positional prefix, and positional elements themselves.
    let tuple =
        json!({"type":"array","prefixItems":[{"type":"string"},limited()],"items":limited()});
    assert!(accepts(tuple.clone(), json!([two_byte, "ab", "ab"])));
    assert!(!accepts(tuple.clone(), json!(["ab", two_byte, "ab"])));
    assert!(!accepts(tuple, json!(["ab", "ab", "ab", two_byte])));
    // Schema-valued map values, declared by pattern or by additionalProperties.
    let map = json!({"type":"object","additionalProperties":limited()});
    assert!(accepts(map.clone(), json!({"k":"ab"})));
    assert!(!accepts(map, json!({"k":two_byte})));
    let patterned = json!({"type":"object","patternProperties":{"^x-":limited()},
        "additionalProperties":false});
    assert!(accepts(patterned.clone(), json!({"x-a":"ab"})));
    assert!(!accepts(patterned, json!({"x-a":two_byte})));
    // Nested through properties, as before.
    let nested = json!({"type":"object","properties":{"xs":{"type":"array","items":limited()}}});
    assert!(!accepts(nested, json!({"xs":["ab", two_byte]})));
}

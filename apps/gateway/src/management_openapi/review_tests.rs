//! Independent-review regressions. All tests here are deliberately DB-free.
use super::*;

pub(in crate::management::openapi) fn ceiling(personal: bool) -> Value {
    json!({"id":uuid::Uuid::nil(),"rule":null,"builtin":false,"kind":"admission_ceiling","state":"firing","severity":"warning","level":1,"summary":"An API key is near its admission ceiling","details":{"scope":"key","personal":personal,"workspace_name":if personal { Value::Null } else { json!("Team") },"workspace_kind":if personal { "personal" } else { "team" },"window_minutes":5,"admissions":60000,"admissions_per_second":"200.0","by_rate":true,"by_lock_wait":false,"threshold_requests_per_second":200,"lock_wait_p95_at_least_ms":null,"threshold_lock_wait_ms":250},"workspace":null,"connection":null,"fired_at":"2026-10-28T00:00:00Z","resolved_at":null,"resolution":null,"email":null,"cleared_email":null,"read":false})
}

#[test]
fn management_openapi_review_middleware_inventory_is_shared_by_all_protected_routes() {
    use crate::route_policy::{Access, ROUTES};
    let document = artifact();
    for route in ROUTES.iter().filter(|r| r.path.starts_with("/api/v1/")) {
        for method in route.methods.split_whitespace() {
            let op = &document["paths"][route.path][method.to_ascii_lowercase()];
            assert!(
                op["responses"].get("413").is_some(),
                "missing serving body-limit refusal: {method} {}",
                route.path
            );
            let statuses: &[&str] = match route.access {
                Access::Anonymous => &[],
                Access::BrowserLogout => &["401", "403", "500"],
                _ => &["401", "403", "429", "500", "503"],
            };
            for status in statuses {
                assert!(
                    op["responses"].get(status).is_some(),
                    "missing middleware {status}: {method} {}",
                    route.path
                );
            }
        }
    }
    assert!(
        document["paths"]["/api/v1/auth/logout"]["post"]["responses"]
            .get("429")
            .is_none(),
        "logout uses its own session/CSRF handler, not management bearer throttles"
    );
}

#[test]
fn management_openapi_review_personal_ceiling_never_discloses_identity() {
    let schema = json!({"$ref":"#/components/schemas/IdentityAdminCeilingNotification"});
    let good = ceiling(true);
    validate_value(artifact(), &schema, &good).unwrap();
    let mut absent_name = good.clone();
    absent_name["details"]
        .as_object_mut()
        .unwrap()
        .remove("workspace_name");
    validate_value(artifact(), &schema, &absent_name).unwrap();
    for field in ["workspace", "workspace_name"] {
        let mut leak = good.clone();
        if field == "workspace" {
            leak["workspace"] =
                json!({"id":uuid::Uuid::nil(),"name":"Private owner workspace","kind":"personal"});
        } else {
            leak["details"][field] = json!("Private owner workspace");
        }
        assert!(
            validate_value(artifact(), &schema, &leak).is_err(),
            "accepted personal identity leak in {field}"
        );
    }
}

#[test]
fn management_openapi_review_completed_components_and_inline_schemas_are_not_vacuous() {
    for value in [
        json!(true),
        json!({}),
        json!({"anyOf":[{"type":"string"},{}]}),
        json!({"$ref":"#/components/schemas/IdentityNotification/properties/details"}),
        json!({"$comment":"Stored alert_events.details is an arbitrary producer-specific JSON object; monetary and identity refinements remain enforced.","type":"object"}),
        json!({"type":"object"}),
        json!({"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}}),
    ] {
        let mut document = artifact().clone();
        document["components"]["schemas"]["get_api_v1_auth_configResponse"] = value.clone();
        assert!(
            require_complete(&document, &document["paths"]["/api/v1/auth/config"]["get"]).is_err(),
            "accepted vacuous completed component: {value}"
        );
        let mut document = artifact().clone();
        document["paths"]["/api/v1/auth/config"]["get"]["responses"]["200"]["content"]["application/json"]
            ["schema"] = value.clone();
        assert!(
            require_complete(&document, &document["paths"]["/api/v1/auth/config"]["get"]).is_err(),
            "accepted vacuous inline schema: {value}"
        );
    }
}

#[test]
fn management_openapi_review_unused_completed_contracts_are_checked() {
    for value in [
        json!(true),
        json!({}),
        json!({"anyOf":[{"type":"string"},{}]}),
        json!({"type":"object"}),
    ] {
        let mut document = artifact().clone();
        document["components"]["schemas"]["UnusedComplete"] = value.clone();
        assert!(crate::management::openapi::quality::completed(&document).is_err());
        let mut document = artifact().clone();
        document["components"]["responses"]["UnusedComplete"] = json!({"description":"mutation control","content":{"application/json":{"schema":value}}});
        assert!(crate::management::openapi::quality::completed(&document).is_err());
    }
}

#[test]
fn management_openapi_review_logo_rejects_mislabeled_sensitive_json() {
    let mut h = HeaderMap::new();
    for (name, value) in [
        ("etag", format!("\"{}\"", "a".repeat(64))),
        ("cache-control", "public, max-age=300".into()),
        ("x-content-type-options", "nosniff".into()),
        ("content-security-policy", "default-src 'none'".into()),
        ("cross-origin-resource-policy", "same-origin".into()),
        ("content-type", "image/png".into()),
    ] {
        h.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    assert!(
        validate(
            artifact(),
            "GET",
            "/api/v1/branding/logo",
            StatusCode::OK,
            &h,
            b"{\"plaintext_inference_token\":\"must-not-leak\"}"
        )
        .is_err()
    );
}

#[test]
fn management_openapi_review_304_only_registry_is_rejected() {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("etag", format!("\"{}\"", "a".repeat(64))),
        ("cache-control", "public, max-age=300".into()),
        ("x-content-type-options", "nosniff".into()),
        ("content-security-policy", "default-src 'none'".into()),
        ("cross-origin-resource-policy", "same-origin".into()),
    ] {
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    let captured = Captured {
        status: StatusCode::NOT_MODIFIED,
        headers,
        bytes: axum::body::Bytes::new(),
        method: "GET".into(),
        template: Some("/api/v1/branding/logo".into()),
        query: vec![],
    };
    let mut fixtures = PositiveFixtures::default();
    fixtures.observe(&captured);
    assert!(
        std::panic::catch_unwind(|| fixtures.finish(&["branding"])).is_err(),
        "304 without representation incorrectly completed the area"
    );
}

/// CodeQL `rust/cleartext-logging`: contract failures reach panics and CI logs,
/// so they may name only the method, TEMPLATE, status, error kind and JSON
/// pointers, never a concrete path, header value, body byte or instance value.
const CANARY_SECRET: &str = "CANARY-secret-token-8e2f41";
const CANARY_ID: &str = "7c1f9a52-3b1e-4e07-9f0f-0d2a5c4b9e11";
const CANARY_EMAIL: &str = "canary.person@canary-example.invalid";

fn assert_clean(label: &str, error: &anyhow::Error) {
    for rendering in [
        format!("{error}"),
        format!("{error:#}"),
        format!("{error:?}"),
    ] {
        for canary in [CANARY_SECRET, CANARY_ID, CANARY_EMAIL] {
            assert!(
                !rendering.contains(canary),
                "{label}: a failure message leaked a captured value"
            );
        }
    }
}

fn json_headers(extra: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (name, value) in [
        ("content-type", "application/json"),
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
    ]
    .iter()
    .chain(extra)
    {
        h.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    h
}

#[test]
fn management_openapi_review_failure_messages_never_echo_captured_values() {
    let item = format!("/api/v1/me/access-requests/{CANARY_ID}");
    let template = "/api/v1/me/access-requests/{id}";
    let secret_values = json!({"id":CANARY_SECRET,"workspace_id":CANARY_EMAIL,"status":CANARY_SECRET,"reason":CANARY_SECRET});
    let bodies: Vec<(&str, Vec<u8>)> = vec![
        ("wrong types", serde_json::to_vec(&secret_values).unwrap()),
        (
            "unexpected secret property",
            serde_json::to_vec(&json!({"omg_session":CANARY_SECRET,CANARY_EMAIL:CANARY_ID}))
                .unwrap(),
        ),
        (
            "wrong root",
            serde_json::to_vec(&json!([CANARY_SECRET, CANARY_ID])).unwrap(),
        ),
        (
            "truncated json",
            format!("{{\"id\":\"{CANARY_SECRET}").into_bytes(),
        ),
        (
            "not json",
            format!("{CANARY_SECRET} {CANARY_EMAIL}").into_bytes(),
        ),
        (
            "not utf8",
            [CANARY_SECRET.as_bytes(), &[0xff, 0xfe]].concat(),
        ),
    ];
    for (label, bytes) in bodies {
        let error =
            check("GET", &item, StatusCode::OK, &json_headers(&[]), &bytes).expect_err(label);
        assert_clean(label, &error);
        let text = format!("{error:#}");
        assert!(
            text.contains(template),
            "{label}: names the template: {text}"
        );
        assert!(text.contains("200"), "{label}: names the status: {text}");
    }
    // A schema failure is identified by kind and pointers only.
    let error = check(
        "GET",
        &item,
        StatusCode::OK,
        &json_headers(&[]),
        &serde_json::to_vec(&secret_values).unwrap(),
    )
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("kind ")
            && text.contains("at instance ")
            && text.contains("(schema /components/"),
        "{text}"
    );
    // Undocumented statuses, response header values and media types.
    let error = check(
        "GET",
        &item,
        StatusCode::IM_A_TEAPOT,
        &json_headers(&[]),
        b"{}",
    )
    .unwrap_err();
    assert_clean("status", &error);
    let export = format!("/api/v1/me/export?from={CANARY_SECRET}&to={CANARY_EMAIL}");
    let error = check(
        "GET",
        &export,
        StatusCode::OK,
        &json_headers(&[
            ("x-omg-export-sha256", CANARY_SECRET),
            ("content-disposition", CANARY_EMAIL),
        ]),
        b"{}",
    )
    .unwrap_err();
    assert_clean("header values and query values", &error);
    let mut odd = json_headers(&[]);
    odd.insert(
        "content-type",
        format!("application/x-{CANARY_SECRET}").parse().unwrap(),
    );
    let error = check("GET", &item, StatusCode::OK, &odd, b"{}").unwrap_err();
    assert_clean("media type", &error);
    // Direct schema validation, including `oneOf` with more than one match,
    // whose own Display embeds the whole instance.
    let document = json!({"components":{"schemas":{
        "Any":{"oneOf":[{"type":"object"},{"type":"object","required":["a"]}]},
        "Strict":{"type":"object","required":["a"],"additionalProperties":false,"properties":{
            "a":{"enum":["fine"]},"b":{"type":"string","pattern":"^x$","maxLength":3},"c":{"type":"integer"}}}}}});
    for (schema, value) in [
        (
            json!({"$ref":"#/components/schemas/Any"}),
            json!({"a":CANARY_SECRET}),
        ),
        (
            json!({"$ref":"#/components/schemas/Strict"}),
            json!({"a":CANARY_SECRET,"b":CANARY_EMAIL,"c":CANARY_ID,"d":CANARY_SECRET}),
        ),
        (json!({"const":"fine"}), json!(CANARY_SECRET)),
        (
            json!({"type":"string","format":"uuid"}),
            json!(CANARY_SECRET),
        ),
    ] {
        let error = validate_value(&document, &schema, &value).unwrap_err();
        assert_clean(&schema.to_string(), &error);
        assert!(format!("{error}").contains("kind "), "{error}");
    }
}

#[cfg(feature = "integration-tests")]
#[tokio::test]
async fn management_openapi_review_capture_panics_never_echo_captured_values() {
    // The real capture path panics on a contract failure: its payload is what
    // CI prints. It must not carry the path's id or any response byte.
    let item = format!("/api/v1/me/access-requests/{CANARY_ID}");
    let response = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .header("referrer-policy", "no-referrer")
        .body(axum::body::Body::from(
            serde_json::to_vec(&json!({"id":CANARY_SECRET,"owner":CANARY_EMAIL})).unwrap(),
        ))
        .unwrap();
    // Captured deliberately has no Debug: it would print the body.
    let panic = match tokio::spawn(async move { capture("GET", &item, response).await }).await {
        Err(error) => error.into_panic(),
        Ok(_) => panic!("a contract failure must panic"),
    };
    let message = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .expect("string panic payload");
    for canary in [CANARY_SECRET, CANARY_ID, CANARY_EMAIL] {
        assert!(!message.contains(canary), "a panic leaked a captured value");
    }
    assert!(
        message.contains("/api/v1/me/access-requests/{id}"),
        "the panic does not name the operation template"
    );
}

/// Slice 6 patterns stay in the portable ECMA-262 subset that every client
/// generator and the Rust validator read alike: no Unicode property escapes
/// (`\p{..}`, `\P{..}`), no braced code points, no named groups, no flags. The
/// control set is the explicit class that equals `char::is_control`.
#[test]
fn management_openapi_slice6_patterns_are_portable() {
    fn walk(value: &Value, found: &mut Vec<String>, at: &str) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if key == "pattern"
                        && let Some(pattern) = child.as_str()
                    {
                        let banned = ["\\p", "\\P", "\\u{", "\\k<", "(?", "\\N{"];
                        if banned.iter().any(|b| pattern.contains(b)) {
                            found.push(at.to_owned());
                        }
                    }
                    walk(child, found, &format!("{at}/{key}"));
                }
            }
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    walk(child, found, &format!("{at}/{i}"));
                }
            }
            _ => {}
        }
    }
    let mut found = Vec::new();
    for (name, text) in [
        (
            "operations",
            include_str!(
                "../../../../docs/openapi/management/activity-credentials-operations.json"
            ),
        ),
        (
            "schemas",
            include_str!("../../../../docs/openapi/management/activity-credentials-schemas.json"),
        ),
    ] {
        walk(&serde_json::from_str(text).unwrap(), &mut found, name);
    }
    assert!(found.is_empty(), "non-portable slice 6 patterns: {found:?}");
    // The explicit class is exactly `char::is_control`.
    let class = json!({"type":"string","pattern":"^[\\u0000-\\u001F\\u007F-\\u009F]$"});
    for c in (0..=0x2FFu32).filter_map(char::from_u32) {
        let matched = validate_value(artifact(), &class, &json!(c.to_string())).is_ok();
        assert_eq!(matched, c.is_control(), "code point {}", c as u32);
    }
}

/// A known budget window is exact: no unresolved usage and no unknown hold
/// (`read_budget_report` reports a zero unknown count only when every counter
/// is zero).
#[test]
fn management_openapi_slice6_known_budget_window_is_exact() {
    let schema = json!({"$ref":"#/components/schemas/S6KnownBudgetWindow"});
    let known = json!({"layer":"key","period":"month","amount_microusd":"100","monthly_budget_microusd":"100","budget_period":"month","window_start":"2026-10-01T00:00:00Z","window_end":"2026-11-01T00:00:00Z","usage_visible":true,"used_microusd":"7","unresolved_usage":false,"exhausted":false,"settled_microusd":"5","pending_held_microusd":"2","unknown_held_microusd":"0","unknown_cost_requests":"0","remaining_microusd":"93","headroom_status":"known"});
    assert!(validate_value(artifact(), &schema, &known).is_ok());
    for (field, value) in [
        ("unresolved_usage", json!(true)),
        ("unknown_held_microusd", json!("1")),
        ("unknown_cost_requests", json!("1")),
        ("remaining_microusd", Value::Null),
    ] {
        let mut window = known.clone();
        window[field] = value;
        assert!(
            validate_value(artifact(), &schema, &window).is_err(),
            "a known window accepted a contradictory {field}"
        );
    }
}

//! Independent-review regressions. All tests here are deliberately DB-free.
use super::*;

pub(super) fn ceiling(personal: bool) -> Value {
    json!({"id":uuid::Uuid::nil(),"rule":null,"builtin":false,"kind":"admission_ceiling","state":"firing","severity":"warning","level":1,"summary":"An API key is near its admission ceiling","details":{"personal":personal,"workspace_name":null},"workspace":null,"connection":null,"fired_at":"2026-10-28T00:00:00Z","resolved_at":null,"resolution":null,"email":null,"cleared_email":null,"read":false})
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
    };
    let mut fixtures = PositiveFixtures::default();
    fixtures.observe(&captured);
    assert!(
        std::panic::catch_unwind(|| fixtures.finish(&["branding"])).is_err(),
        "304 without representation incorrectly completed the area"
    );
}

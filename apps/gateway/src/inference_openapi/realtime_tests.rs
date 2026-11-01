//! Upgrade metadata and PR3 closure controls (#82 PR3c).
use super::{generator, responses::validate_documented, tests::generated};
use axum::http::{HeaderMap, StatusCode};
use serde_json::json;

#[test]
fn inference_openapi_realtime_upgrade_headers() {
    let doc = generated();
    let mut headers = HeaderMap::new();
    for (key, value) in [
        ("upgrade", "websocket"),
        ("connection", "upgrade"),
        ("sec-websocket-accept", "dGhlIHNhbXBsZSBub25jZQ=="),
        ("sec-websocket-protocol", "realtime"),
        ("cache-control", "no-store"),
        ("x-request-id", "01900000-0000-7000-8000-000000000000"),
    ] {
        headers.insert(key, value.parse().unwrap());
    }
    assert!(
        validate_documented(
            &doc,
            "GET",
            "/v1/realtime",
            StatusCode::SWITCHING_PROTOCOLS,
            &headers,
            b""
        )
        .is_ok(),
        "documented upgrade rejected"
    );
    for key in ["upgrade", "connection", "sec-websocket-accept"] {
        let mut bad = headers.clone();
        bad.remove(key);
        assert!(
            validate_documented(
                &doc,
                "GET",
                "/v1/realtime",
                StatusCode::SWITCHING_PROTOCOLS,
                &bad,
                b""
            )
            .is_err(),
            "missing upgrade header accepted"
        );
    }
}

#[test]
fn inference_openapi_realtime_never_offers_x_api_key() {
    let doc = generated();
    assert_eq!(
        doc["paths"]["/v1/realtime"]["get"]["x-omg-websocket"]["auth"],
        json!(["Bearer", "openai-insecure-api-key.<key>"])
    );
    let mut bad = doc.clone();
    bad["paths"]["/v1/realtime"]["get"]["security"] = json!([{"anthropicApiKey":[]}]);
    assert!(
        generator::verify(&bad).is_err(),
        "Realtime x-api-key offered"
    );
}

#[test]
fn inference_openapi_zero_placeholders_once_pr2_lands() {
    let doc = generated();
    let slots = doc["components"]["schemas"].as_object().unwrap();
    let remaining: Vec<_> = slots
        .iter()
        .filter(|(_, s)| s["x-omg-schema-status"] == "incomplete")
        .map(|(name, _)| name.as_str())
        .collect();
    let expected = [
        "post_v1_chat_completionsRequest",
        "post_v1_chat_completionsResponse",
        "post_v1_messagesRequest",
        "post_v1_messagesResponse",
        "post_v1_responsesRequest",
        "post_v1_responsesResponse",
    ];
    assert!(
        remaining.iter().all(|s| expected.contains(s)),
        "placeholder outside PR2"
    );
    assert_eq!(
        generator::placeholder_count(&doc),
        6,
        "falling baseline: PR2 must remove these six slots to reach zero"
    );
}

#[cfg(feature = "integration-tests")]
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn inference_openapi_realtime_wire_fixtures(pool: sqlx::PgPool) {
    let mut fixtures = super::responses::Fixtures::default();
    crate::protocols::realtime::inference_openapi_handshake_fixtures(pool, &mut fixtures).await;
    fixtures.finish(super::responses::REALTIME_AREAS);
}

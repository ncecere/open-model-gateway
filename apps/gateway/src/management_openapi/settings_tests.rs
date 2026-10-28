//! Slice 5 boundary controls independent of the database fixture.
use super::responses::{artifact, validate_value};
use serde_json::{Value, json};

#[test]
fn management_openapi_settings_saved_configuration_request_vectors() {
    for (method, path) in [
        ("POST", "/api/v1/platform/settings/email/test"),
        ("POST", "/api/v1/platform/settings/storage/test"),
        ("POST", "/api/v1/platform/settings/audit-export/test"),
        ("POST", "/api/v1/platform/alerts/webhooks/{id}/test"),
        ("DELETE", "/api/v1/platform/alerts/webhooks/{id}"),
    ] {
        let operation = &artifact()["paths"][path][method.to_ascii_lowercase()];
        assert_eq!(operation["requestBody"]["required"], false);
        let schema = &operation["requestBody"]["content"]["application/json"]["schema"];
        for bytes in [
            "",
            "{}",
            " {} ",
            "null",
            "[]",
            "true",
            "1",
            "\"text\"",
            "{",
            " ",
            "{\"environment_variables\":{}}",
        ] {
            let actual = crate::management::settings::no_configuration(bytes.as_bytes()).is_ok();
            let documented = bytes.is_empty()
                || serde_json::from_str::<Value>(bytes)
                    .is_ok_and(|v| validate_value(artifact(), schema, &v).is_ok());
            assert_eq!(documented, actual, "{method} {path}: {bytes:?}");
        }
    }
}

fn event(personal: bool) -> Value {
    json!({"id":uuid::Uuid::nil(),"rule":null,"builtin":false,"kind":"admission_ceiling","state":"firing","severity":"warning","level":1,"summary":"Admission ceiling","details":{"scope":"workspace","personal":personal,"workspace_name":null,"workspace_kind":if personal{"personal"}else{"team"},"window_minutes":5,"admissions":60000,"admissions_per_second":"200.0","by_rate":true,"by_lock_wait":false,"threshold_requests_per_second":200,"lock_wait_p95_at_least_ms":null,"threshold_lock_wait_ms":null},"workspace":null,"connection":null,"fired_at":"2026-10-28T00:00:00Z","resolved_at":null,"resolution":null,"email":null,"cleared_email":null})
}
#[test]
fn management_openapi_settings_alert_identity_and_integer_delivery_controls() {
    let schema = json!({"$ref":"#/components/schemas/SettingsEvents"});
    let personal = event(true);
    validate_value(
        artifact(),
        &schema,
        &json!({"data":[personal],"has_more":false}),
    )
    .unwrap();
    for field in ["workspace", "workspace_name"] {
        let mut leak = event(true);
        if field == "workspace" {
            leak[field] =
                json!({"id":uuid::Uuid::nil(),"name":"Foreign personal","kind":"personal"});
        } else {
            leak["details"][field] = json!("Foreign personal");
        }
        assert!(
            validate_value(
                artifact(),
                &schema,
                &json!({"data":[leak],"has_more":false})
            )
            .is_err()
        );
    }
    for (field, forbidden) in [
        ("workspace_id", json!(uuid::Uuid::nil())),
        ("owner_email", json!("owner@private.invalid")),
        ("api_key_id", json!(uuid::Uuid::nil())),
        (
            "workspace",
            json!({"id":uuid::Uuid::nil(),"kind":"personal","name":"Private"}),
        ),
    ] {
        let mut leak = event(true);
        leak["details"][field] = forbidden;
        assert!(
            validate_value(
                artifact(),
                &schema,
                &json!({"data":[leak],"has_more":false})
            )
            .is_err(),
            "accepted ceiling identity {field}"
        );
    }
    let mut redacted = event(false);
    redacted["details"]
        .as_object_mut()
        .unwrap()
        .remove("workspace_name");
    validate_value(
        artifact(),
        &json!({"$ref":"#/components/schemas/SettingsRedactedCeilingEvent"}),
        &redacted,
    )
    .unwrap();
    redacted["details"]["workspace_name"] = json!("Hidden shared workspace");
    assert!(
        validate_value(
            artifact(),
            &json!({"$ref":"#/components/schemas/SettingsRedactedCeilingEvent"}),
            &redacted
        )
        .is_err()
    );
    let mut normal = event(false);
    normal["kind"] = json!("budget_threshold");
    normal["email"] = json!({"status":"sent","recipients":2,"sent":2,"failed":0,"error":null,"attempt_count":1,"retrying":false,"next_attempt_at":null,"waiting":0,"exhausted":0,"suppression_reason":null});
    validate_value(
        artifact(),
        &schema,
        &json!({"data":[normal],"has_more":false}),
    )
    .unwrap();
    for field in [
        "recipients",
        "sent",
        "failed",
        "attempt_count",
        "waiting",
        "exhausted",
    ] {
        let mut wrong = normal.clone();
        wrong["email"][field] = json!("2");
        assert!(
            validate_value(
                artifact(),
                &schema,
                &json!({"data":[wrong],"has_more":false})
            )
            .is_err()
        );
    }
}
#[test]
fn management_openapi_settings_webhook_redaction_is_not_optional_secrets() {
    let redacted = json!({"id":uuid::Uuid::nil(),"label":"Receiver","enabled":false,"created_at":"2026-10-28T00:00:00Z","updated_at":"2026-10-28T00:00:00Z","signing_configured":true,"overlap_active":false,"previous_secret_until":null,"waiting":0,"exhausted":0,"last_success_at":null,"last_error":null});
    let schema = json!({"$ref":"#/components/schemas/SettingsWebhookRedacted"});
    validate_value(artifact(), &schema, &redacted).unwrap();
    for (field, value) in [
        ("url", json!("https://receiver.example.test/ingest")),
        ("current_secret_ref", json!("env:CURRENT")),
        ("previous_secret_ref", Value::Null),
    ] {
        let mut leak = redacted.clone();
        leak[field] = value;
        assert!(validate_value(artifact(), &schema, &leak).is_err());
    }
    let mut privileged = redacted;
    privileged["url"] = json!("https://receiver.example.test/ingest");
    privileged["current_secret_ref"] = json!("env:CURRENT");
    privileged["previous_secret_ref"] = Value::Null;
    let schema = json!({"$ref":"#/components/schemas/SettingsWebhookPrivileged"});
    validate_value(artifact(), &schema, &privileged).unwrap();
    privileged["current_secret_ref"] = json!("plaintext-signing-secret-must-never-appear");
    assert!(validate_value(artifact(), &schema, &privileged).is_err());
}

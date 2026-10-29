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

/// A producer-shaped admission-ceiling incident as a Platform Admin sees it:
/// a personal workspace has no workspace reference and no name; a Team has both.
fn event(personal: bool) -> Value {
    json!({"id":uuid::Uuid::nil(),"rule":{"id":uuid::Uuid::nil(),"name":"Ceiling","scope":"installation","deleted":false},"builtin":false,"kind":"admission_ceiling","state":"firing","severity":"warning","level":1,"summary":"Admission ceiling","details":{"scope":"workspace","personal":personal,"workspace_name":if personal{Value::Null}else{json!("Team")},"workspace_kind":if personal{"personal"}else{"team"},"window_minutes":5,"admissions":60000,"admissions_per_second":"200.0","by_rate":true,"by_lock_wait":false,"threshold_requests_per_second":200,"lock_wait_p95_at_least_ms":null,"threshold_lock_wait_ms":null},"workspace":if personal{Value::Null}else{json!({"id":uuid::Uuid::nil(),"name":"Team","kind":"team"})},"connection":null,"fired_at":"2026-10-28T00:00:00Z","resolved_at":null,"resolution":null,"email":null,"cleared_email":null})
}
fn page(events: Vec<Value>) -> Value {
    json!({"data":events,"has_more":false})
}
#[test]
fn management_openapi_settings_alert_identity_and_integer_delivery_controls() {
    let schema = json!({"$ref":"#/components/schemas/SettingsPlatformEvents"});
    validate_value(artifact(), &schema, &page(vec![event(true), event(false)])).unwrap();
    for field in ["workspace", "workspace_name"] {
        let mut leak = event(true);
        if field == "workspace" {
            leak[field] =
                json!({"id":uuid::Uuid::nil(),"name":"Foreign personal","kind":"personal"});
        } else {
            leak["details"][field] = json!("Foreign personal");
        }
        assert!(validate_value(artifact(), &schema, &page(vec![leak])).is_err());
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
            validate_value(artifact(), &schema, &page(vec![leak])).is_err(),
            "accepted ceiling identity {field}"
        );
    }
    // `personal` is tied to `workspace_kind`: a Team/Project incident never
    // claims to be personal, and a personal one never claims to be shared.
    for (personal, kind) in [(false, "personal"), (true, "team"), (true, "project")] {
        let mut tied = event(personal);
        tied["details"]["workspace_kind"] = json!(kind);
        assert!(
            validate_value(artifact(), &schema, &page(vec![tied])).is_err(),
            "accepted personal={personal} with workspace_kind={kind}"
        );
    }
    let redacted_schema = json!({"$ref":"#/components/schemas/SettingsRedactedCeilingEvent"});
    let mut redacted = event(false);
    redacted["workspace"] = Value::Null;
    redacted["details"]
        .as_object_mut()
        .unwrap()
        .remove("workspace_name");
    validate_value(artifact(), &redacted_schema, &redacted).unwrap();
    validate_value(artifact(), &schema, &page(vec![redacted.clone()])).unwrap();
    redacted["details"]["workspace_name"] = json!("Hidden shared workspace");
    assert!(validate_value(artifact(), &redacted_schema, &redacted).is_err());
    let mut normal = event(false);
    normal["kind"] = json!("budget_threshold");
    normal["email"] = json!({"status":"sent","recipients":2,"sent":2,"failed":0,"error":null,"attempt_count":1,"retrying":false,"next_attempt_at":null,"waiting":0,"exhausted":0,"suppression_reason":null});
    validate_value(artifact(), &schema, &page(vec![normal.clone()])).unwrap();
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
        assert!(validate_value(artifact(), &schema, &page(vec![wrong])).is_err());
    }
}
/// Workspace history is ordinary-only: it never returns an installation-rule
/// incident, so an admission-ceiling incident (in any audience projection) is
/// refused by its schema, while an ordinary workspace-rule incident is accepted.
#[test]
fn management_openapi_settings_workspace_events_never_carry_ceiling_incidents() {
    let schema = json!({"$ref":"#/components/schemas/SettingsWorkspaceEvents"});
    let team = json!({"id":uuid::Uuid::nil(),"name":"Team","kind":"team"});
    let mut ordinary = event(false);
    ordinary["kind"] = json!("budget_threshold");
    ordinary["rule"] =
        json!({"id":uuid::Uuid::nil(),"name":"Budget","scope":"workspace","deleted":false});
    ordinary["workspace"] = team;
    validate_value(artifact(), &schema, &page(vec![ordinary.clone()])).unwrap();
    for ceiling in [event(true), event(false), {
        let mut redacted = event(false);
        redacted["workspace"] = Value::Null;
        redacted["details"]
            .as_object_mut()
            .unwrap()
            .remove("workspace_name");
        redacted
    }] {
        assert!(validate_value(artifact(), &schema, &page(vec![ceiling])).is_err());
    }
    // A workspace rule's incident cannot carry an installation rule reference
    // or a connection, and the personal built-in alert needs a personal reference.
    let mut foreign = ordinary.clone();
    foreign["rule"]["scope"] = json!("installation");
    assert!(validate_value(artifact(), &schema, &page(vec![foreign])).is_err());
    let mut builtin = ordinary;
    builtin["builtin"] = json!(true);
    builtin["rule"] = Value::Null;
    assert!(validate_value(artifact(), &schema, &page(vec![builtin.clone()])).is_err());
    builtin["workspace"] = json!({"id":uuid::Uuid::nil(),"name":"Personal","kind":"personal"});
    validate_value(artifact(), &schema, &page(vec![builtin])).unwrap();
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

#[test]
fn management_openapi_settings_webhook_url_and_created_projection() {
    let created = json!({"id":uuid::Uuid::nil(),"label":"Receiver","enabled":false,"created_at":"2026-10-28T00:00:00Z","updated_at":"2026-10-28T00:00:00Z","signing_configured":true,"overlap_active":false,"previous_secret_until":null,"waiting":0,"exhausted":0,"last_success_at":null,"last_error":null,"url":"https://receiver.example.test/ingest","current_secret_ref":"env:CURRENT","previous_secret_ref":null});
    let schema = json!({"$ref":"#/components/schemas/SettingsWebhookCreated"});
    validate_value(artifact(), &schema, &created).unwrap();
    // A fresh destination is disabled and has no history or rotation.
    for (field, value) in [
        ("enabled", json!(true)),
        ("waiting", json!(1)),
        ("last_error", json!("rejected")),
        ("previous_secret_ref", json!("env:PREVIOUS")),
    ] {
        let mut wrong = created.clone();
        wrong[field] = value;
        assert!(
            validate_value(artifact(), &schema, &wrong).is_err(),
            "{field}"
        );
    }
    // https, or loopback http (development only); never userinfo, query or fragment.
    let schema = json!({"$ref":"#/components/schemas/SettingsWebhookPrivileged"});
    for (url, valid) in [
        ("https://receiver.example.test/ingest", true),
        ("https://receiver.example.test:8443/", true),
        ("http://127.0.0.1:8080/hook", true),
        ("http://localhost/hook", true),
        ("http://receiver.example.test/ingest", false),
        ("https://user:secret@receiver.example.test/ingest", false),
        ("https://receiver.example.test/ingest?token=1", false),
        ("https://receiver.example.test/ingest#frag", false),
        ("ftp://receiver.example.test/ingest", false),
        ("https://", false),
    ] {
        let mut candidate = created.clone();
        candidate["url"] = json!(url);
        assert_eq!(
            validate_value(artifact(), &schema, &candidate).is_ok(),
            valid,
            "{url}"
        );
    }
}

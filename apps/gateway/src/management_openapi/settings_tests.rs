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

/// The fired email every incident has (the worker's sent shape).
fn delivery() -> Value {
    json!({"status":"sent","recipients":2,"sent":2,"failed":0,"error":null,"attempt_count":2,"retrying":false,"next_attempt_at":null,"waiting":0,"exhausted":0,"suppression_reason":null})
}
/// `alerts::budget_condition` details of a monthly budget.
fn budget_details() -> Value {
    json!({"layer":"type","period":"month","threshold_percent":80,"used_percent":"80","used_microusd":"8000000","budget_microusd":"10000000","unknown_cost_requests":0,"window_start":"2026-10-01T00:00:00Z","window_end":"2026-11-01T00:00:00Z"})
}
/// A producer-shaped admission-ceiling incident as a Platform Admin sees it:
/// a personal workspace has no workspace reference and no name; a Team has both.
fn event(personal: bool) -> Value {
    json!({"id":uuid::Uuid::nil(),"rule":{"id":uuid::Uuid::nil(),"name":"Ceiling","scope":"installation","deleted":false},"builtin":false,"kind":"admission_ceiling","state":"firing","severity":"warning","level":1,"summary":"Admission ceiling","details":{"scope":"workspace","personal":personal,"workspace_name":if personal{Value::Null}else{json!("Team")},"workspace_kind":if personal{"personal"}else{"team"},"window_minutes":5,"admissions":60000,"admissions_per_second":"200.0","by_rate":true,"by_lock_wait":false,"threshold_requests_per_second":200,"lock_wait_p95_at_least_ms":null,"threshold_lock_wait_ms":null},"workspace":if personal{Value::Null}else{json!({"id":uuid::Uuid::nil(),"name":"Team","kind":"team"})},"connection":null,"fired_at":"2026-10-28T00:00:00Z","resolved_at":null,"resolution":null,"email":delivery(),"cleared_email":null})
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
    normal["details"] = budget_details();
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
    ordinary["details"] = budget_details();
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

// ---------------------------------------------------------------------------
// Incident history: closed per-kind details and the fired email
// ---------------------------------------------------------------------------

fn incident(kind: &str, rule: Value, workspace: Value, connection: Value, details: Value) -> Value {
    let mut e = event(false);
    e["kind"] = json!(kind);
    e["rule"] = rule;
    e["workspace"] = workspace;
    e["connection"] = connection;
    e["details"] = details;
    e
}
fn installation() -> Value {
    json!({"id":uuid::Uuid::nil(),"name":"Rule","scope":"installation","deleted":false})
}
fn team() -> Value {
    json!({"id":uuid::Uuid::nil(),"name":"Team","kind":"team"})
}
fn connection() -> Value {
    json!({"id":uuid::Uuid::nil(),"name":"Connection","provider":"openai"})
}
fn spike(baseline: &str, ratio: Value) -> Value {
    json!({"current_microusd":"9000000","baseline_total_microusd":baseline,"baseline_hourly_microusd":"1000","ratio":ratio,"factor_percent":300,"min_spend_microusd":"1000000","unknown_cost_requests":0})
}
fn aged(level: i32, severity: &str, hours: i32) -> Value {
    let mut e = incident(
        "aged_unknown_hold",
        Value::Null,
        team(),
        Value::Null,
        json!({"threshold_hours":hours}),
    );
    e["level"] = json!(level);
    e["severity"] = json!(severity);
    e["summary"] = json!(format!(
        "Holds with unknown cost are over {hours} hours old"
    ));
    e
}
fn accepted(schema: &str, e: &Value) -> bool {
    let schema = json!({"$ref":format!("#/components/schemas/{schema}")});
    validate_value(artifact(), &schema, &page(vec![e.clone()])).is_ok()
}

#[test]
fn management_openapi_settings_every_incident_has_its_fired_email() {
    let valid = incident(
        "budget_threshold",
        installation(),
        team(),
        Value::Null,
        budget_details(),
    );
    assert!(accepted("SettingsPlatformEvents", &valid));
    let mut missing = valid.clone();
    missing["email"] = Value::Null;
    assert!(!accepted("SettingsPlatformEvents", &missing));
    // Only a cleared incident has a cleared email.
    let mut cleared = valid.clone();
    cleared["state"] = json!("resolved");
    cleared["resolved_at"] = json!("2026-10-28T01:00:00Z");
    cleared["resolution"] = json!("cleared");
    cleared["cleared_email"] = delivery();
    assert!(accepted("SettingsPlatformEvents", &cleared));
    for resolution in ["superseded", "rule_disabled"] {
        let mut wrong = cleared.clone();
        wrong["resolution"] = json!(resolution);
        assert!(!accepted("SettingsPlatformEvents", &wrong), "{resolution}");
        wrong["cleared_email"] = Value::Null;
        assert!(accepted("SettingsPlatformEvents", &wrong), "{resolution}");
    }
    let mut firing = valid.clone();
    firing["cleared_email"] = delivery();
    assert!(!accepted("SettingsPlatformEvents", &firing));
    // A delivery carries stored values only.
    for (field, value, ok) in [
        ("status", json!("queued"), false),
        ("status", json!("report_pending"), false),
        ("status", json!("partial"), true),
        ("error", json!("host_not_allowed"), true),
        ("error", json!("interrupted"), true),
        ("error", json!("boom"), false),
        (
            "suppression_reason",
            json!("workspace_generation_limit"),
            true,
        ),
        ("suppression_reason", json!("other"), false),
    ] {
        let mut e = valid.clone();
        e["email"][field] = value;
        assert_eq!(accepted("SettingsPlatformEvents", &e), ok, "{field}");
    }
}

#[test]
fn management_openapi_settings_incident_details_are_closed_per_kind() {
    let kinds = [
        incident(
            "budget_threshold",
            installation(),
            team(),
            Value::Null,
            budget_details(),
        ),
        incident(
            "spend_spike",
            installation(),
            Value::Null,
            Value::Null,
            spike("168000", json!("9.0")),
        ),
        incident(
            "provider_failing",
            installation(),
            Value::Null,
            connection(),
            json!({"consecutive_failures":5,"failed":7,"finished":9,"window_minutes":15,"threshold_consecutive":5,"threshold_percent":null}),
        ),
        incident(
            "error_rate",
            installation(),
            Value::Null,
            Value::Null,
            json!({"failed":30,"finished":100,"observed_percent":30,"threshold_percent":10,"min_requests":20,"window_minutes":15}),
        ),
    ];
    for valid in &kinds {
        assert!(
            accepted("SettingsPlatformEvents", valid),
            "{}",
            valid["kind"]
        );
        // No identity can be added to the details of any kind.
        for field in ["api_key_id", "owner_email", "workspace_id", "user"] {
            let mut leak = valid.clone();
            leak["details"][field] = json!("must-not-appear");
            assert!(
                !accepted("SettingsPlatformEvents", &leak),
                "{} {field}",
                valid["kind"]
            );
        }
        // Details of another kind do not fit.
        let mut swapped = valid.clone();
        swapped["details"] = if valid["kind"] == "error_rate" {
            budget_details()
        } else {
            json!({"failed":30,"finished":100,"observed_percent":30,"threshold_percent":10,"min_requests":20,"window_minutes":15})
        };
        assert!(
            !accepted("SettingsPlatformEvents", &swapped),
            "{}",
            valid["kind"]
        );
    }
    // Which incidents name a workspace or a connection (alerts.rs producers).
    let mut e = kinds[1].clone();
    e["workspace"] = team();
    assert!(
        !accepted("SettingsPlatformEvents", &e),
        "an installation spike has no workspace"
    );
    let mut e = kinds[3].clone();
    e["workspace"] = team();
    assert!(
        !accepted("SettingsPlatformEvents", &e),
        "an installation error rate has no workspace"
    );
    let mut e = kinds[2].clone();
    e["connection"] = Value::Null;
    assert!(
        !accepted("SettingsPlatformEvents", &e),
        "a provider incident names its connection"
    );
    let mut e = kinds[0].clone();
    e["workspace"] = Value::Null;
    assert!(
        !accepted("SettingsPlatformEvents", &e),
        "a budget incident names its workspace"
    );
    // Windows and ratios follow their producers.
    let mut lifetime = kinds[0].clone();
    lifetime["details"]["period"] = json!("lifetime");
    assert!(
        !accepted("SettingsPlatformEvents", &lifetime),
        "a lifetime budget has no window"
    );
    lifetime["details"]["window_start"] = Value::Null;
    lifetime["details"]["window_end"] = Value::Null;
    assert!(accepted("SettingsPlatformEvents", &lifetime));
    let mut windowless = kinds[0].clone();
    windowless["details"]["window_end"] = Value::Null;
    assert!(
        !accepted("SettingsPlatformEvents", &windowless),
        "a monthly budget has its window"
    );
    let none = incident(
        "spend_spike",
        installation(),
        Value::Null,
        Value::Null,
        spike("0", Value::Null),
    );
    assert!(accepted("SettingsPlatformEvents", &none));
    let mut ratio = none.clone();
    ratio["details"]["ratio"] = json!("1.0");
    assert!(
        !accepted("SettingsPlatformEvents", &ratio),
        "no ratio without a baseline"
    );
    let mut missing = kinds[1].clone();
    missing["details"]["ratio"] = Value::Null;
    assert!(
        !accepted("SettingsPlatformEvents", &missing),
        "a baseline has its ratio"
    );
    // Built-in incidents write their own details, never another kind's.
    let scim = json!({"resource":"user"});
    let mut builtin = incident(
        "scim_last_admin",
        Value::Null,
        Value::Null,
        Value::Null,
        scim.clone(),
    );
    builtin["builtin"] = json!(true);
    assert!(accepted("SettingsPlatformEvents", &builtin));
    builtin["details"] = json!({"resource":"contract"});
    assert!(!accepted("SettingsPlatformEvents", &builtin));
    builtin["kind"] = json!("partitions_missing");
    builtin["details"] = scim;
    assert!(!accepted("SettingsPlatformEvents", &builtin));
}

/// Constraint `alert_events_aged_shape`: level 1 is a 24-hour warning, level 2 a
/// 168-hour critical; a personal workspace is never named.
#[test]
fn management_openapi_settings_aged_hold_follows_its_table_constraint() {
    assert!(accepted("SettingsPlatformEvents", &aged(1, "warning", 24)));
    assert!(accepted(
        "SettingsPlatformEvents",
        &aged(2, "critical", 168)
    ));
    for (level, severity, hours) in [
        (1, "critical", 24),
        (1, "warning", 168),
        (2, "warning", 168),
        (2, "critical", 24),
        (3, "warning", 24),
    ] {
        assert!(
            !accepted("SettingsPlatformEvents", &aged(level, severity, hours)),
            "{level} {severity} {hours}"
        );
    }
    let mut wrong_summary = aged(1, "warning", 24);
    wrong_summary["summary"] = json!("Holds with unknown cost are over 168 hours old");
    assert!(!accepted("SettingsPlatformEvents", &wrong_summary));
    let mut personal = aged(1, "warning", 24);
    personal["workspace"] = json!({"id":uuid::Uuid::nil(),"name":"Personal","kind":"personal"});
    assert!(!accepted("SettingsPlatformEvents", &personal));
    assert!(!accepted("SettingsWorkspaceEvents", &personal));
    assert!(accepted("SettingsWorkspaceEvents", &aged(1, "warning", 24)));
    let mut unnamed = aged(2, "critical", 168);
    unnamed["workspace"] = Value::Null;
    assert!(accepted("SettingsPlatformEvents", &unnamed));
    assert!(!accepted("SettingsWorkspaceEvents", &unnamed));
}

#[test]
fn management_openapi_settings_key_expiry_reminders_name_no_key() {
    let mut e = incident(
        "key_expiring",
        Value::Null,
        team(),
        Value::Null,
        json!({"expires_at":"2026-10-29T00:00:00Z","threshold_days":1}),
    );
    assert!(accepted("SettingsWorkspaceEvents", &e));
    assert!(!accepted("SettingsPlatformEvents", &e));
    for field in ["api_key_id", "key_name", "holder"] {
        let mut leak = e.clone();
        leak["details"][field] = json!("must-not-appear");
        assert!(!accepted("SettingsWorkspaceEvents", &leak), "{field}");
    }
    e["details"]["threshold_days"] = json!(3);
    assert!(!accepted("SettingsWorkspaceEvents", &e));
}

#[test]
fn management_openapi_settings_webhook_last_error_is_a_stored_category() {
    let mut hook = json!({"id":uuid::Uuid::nil(),"label":"Receiver","enabled":false,"created_at":"2026-10-28T00:00:00Z","updated_at":"2026-10-28T00:00:00Z","signing_configured":true,"overlap_active":false,"previous_secret_until":null,"waiting":0,"exhausted":0,"last_success_at":null,"last_error":null});
    let schema = json!({"$ref":"#/components/schemas/SettingsWebhookRedacted"});
    for category in [
        "credential",
        "connection",
        "tls",
        "rejected",
        "timeout",
        "interrupted",
        "host_not_allowed",
        "destination_policy",
    ] {
        hook["last_error"] = json!(category);
        assert!(
            validate_value(artifact(), &schema, &hook).is_ok(),
            "{category}"
        );
    }
    hook["last_error"] = json!("custom_failure");
    assert!(validate_value(artifact(), &schema, &hook).is_err());
}

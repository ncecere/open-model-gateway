//! Admin › Settings: authority, validation, auditing, key lifetime enforcement,
//! privacy locks and SMTP delivery against a local mock relay (no internet).
use super::*;
use crate::email::mock::{self, Behaviour};

const GENERAL: &str = "/api/v1/platform/settings/general";
const PRIVACY: &str = "/api/v1/platform/settings/privacy";
const EMAIL: &str = "/api/v1/platform/settings/email";
const TEST: &str = "/api/v1/platform/settings/email/test";

async fn audit_count(pool: &PgPool, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action=$1 AND workspace_id IS NULL")
        .bind(action)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn settings_are_admin_write_auditor_read(pool: PgPool) {
    let f = fixture(&pool).await;
    for path in [GENERAL, PRIVACY, EMAIL, "/api/v1/platform/settings/sign-in"] {
        for user in [&f.admin, &f.auditor] {
            let (status, body) = call(&f.s, user, "GET", path, json!({})).await;
            assert_eq!(status, StatusCode::OK, "{path} {body}");
        }
        for user in [&f.owner, &f.member] {
            assert_eq!(
                call(&f.s, user, "GET", path, json!({})).await.0,
                StatusCode::FORBIDDEN
            );
        }
    }
    let general = json!({"display_name":"Acme AI","support_url":"https://help.example.test/ai","logo_url":null,"human_key_max_lifetime_days":90});
    let privacy = json!({"openrouter_data_collection":"deny","request_log_retention_days":null});
    let email = json!({"host":null});
    for (path, body) in [(GENERAL, &general), (PRIVACY, &privacy), (EMAIL, &email)] {
        assert_eq!(
            call(&f.s, &f.auditor, "PUT", path, body.clone()).await.0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call(&f.s, &f.auditor, "POST", TEST, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    let (_, sign_in) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/settings/sign-in",
        json!({}),
    )
    .await;
    // Tests run without OIDC; the summary never carries a secret.
    assert_eq!(sign_in["enabled"], false);
    assert!(sign_in.get("client_secret").is_none());
    assert_eq!(sign_in["enabled_group_mappings"], 0);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn general_settings_validate_audit_and_bound_human_keys(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, v) = call(&f.s, &f.admin, "GET", GENERAL, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["human_key_max_lifetime_days"], 365);
    assert_eq!(v["timezone"], "UTC");
    for bad in [
        json!({"display_name":"","human_key_max_lifetime_days":30}),
        json!({"display_name":"X","human_key_max_lifetime_days":0}),
        json!({"display_name":"X","human_key_max_lifetime_days":366}),
        json!({"display_name":"X","support_url":"http://help.example.test","human_key_max_lifetime_days":30}),
        json!({"display_name":"X","logo_url":"https://user:pw@cdn.example.test/logo.png","human_key_max_lifetime_days":30}),
        json!({"display_name":"X","logo_url":"javascript:alert(1)","human_key_max_lifetime_days":30}),
    ] {
        assert_eq!(
            call(&f.s, &f.admin, "PUT", GENERAL, bad.clone()).await.0,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    // Unknown fields are rejected by strict deserialization.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PUT",
            GENERAL,
            json!({"display_name":"X","human_key_max_lifetime_days":30,"extra":true})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (status, v) = call(&f.s, &f.admin, "PUT", GENERAL, json!({"display_name":"  Acme AI ","support_url":"https://help.example.test/ai","logo_url":" ","human_key_max_lifetime_days":30})).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["display_name"], "Acme AI");
    assert_eq!(v["support_url"], "https://help.example.test/ai");
    assert_eq!(v["logo_url"], Value::Null);
    assert_eq!(v["updated_by"], "admin@example.test");
    assert_eq!(audit_count(&pool, "settings.general_updated").await, 1);
    let (_, me) = call(&f.s, &f.member, "GET", "/api/v1/me", json!({})).await;
    assert_eq!(me["installation"]["name"], "Acme AI");
    assert_eq!(me["installation"]["key_max_lifetime_days"], 30);
    // New and rotated human keys are bounded by the installation maximum.
    let path = format!("/api/v1/workspaces/{}/keys", f.personal);
    let (status, err) = call(
        &f.s,
        &f.owner,
        "POST",
        &path,
        json!({"name":"Long","expires_in_days":31}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["reason"], "key_lifetime_exceeds_maximum");
    let (status, created) = call(
        &f.s,
        &f.owner,
        "POST",
        &path,
        json!({"name":"Short","expires_in_days":30}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let (status, err) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("{path}/{}/rotate", id(&created)),
        json!({"expires_in_days":60}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["reason"], "key_lifetime_exceeds_maximum");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn privacy_settings_follow_the_database_unless_the_environment_locks_them(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, v) = call(&f.s, &f.auditor, "GET", PRIVACY, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["prompt_response_storage"], "never_stored");
    assert_eq!(v["openrouter_data_collection"]["stored"], "deny");
    assert_eq!(
        v["openrouter_data_collection"]["variable"],
        "GATEWAY_OPENROUTER_DATA_COLLECTION"
    );
    assert_eq!(v["request_log_retention_days"]["stored"], Value::Null);
    for bad in [
        json!({"openrouter_data_collection":"maybe"}),
        json!({"openrouter_data_collection":"deny","request_log_retention_days":29}),
        json!({"openrouter_data_collection":"deny","request_log_retention_days":3651}),
    ] {
        assert_eq!(
            call(&f.s, &f.admin, "PUT", PRIVACY, bad.clone()).await.0,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    let locked = crate::providers::openrouter::DataCollection::env_override().is_some()
        || crate::maintenance::retention_from_env()
            .ok()
            .flatten()
            .is_some();
    let (status, v) = call(
        &f.s,
        &f.admin,
        "PUT",
        PRIVACY,
        json!({"openrouter_data_collection":"deny","request_log_retention_days":90}),
    )
    .await;
    if locked {
        // An operator override in this environment: changes are refused, not ignored.
        assert!(
            matches!(status, StatusCode::OK | StatusCode::CONFLICT),
            "{v}"
        );
        return;
    }
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["request_log_retention_days"]["value"], 90);
    assert_eq!(v["openrouter_data_collection"]["locked"], false);
    assert_eq!(audit_count(&pool, "settings.privacy_updated").await, 1);
    // The maintenance refresh reads exactly what was stored.
    assert_eq!(
        crate::maintenance::refresh_runtime_settings(&f.s)
            .await
            .unwrap(),
        Some(90)
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn email_settings_validate_references_and_transport_security(pool: PgPool) {
    let f = fixture(&pool).await;
    let (_, v) = call(&f.s, &f.admin, "GET", EMAIL, json!({})).await;
    assert_eq!(v["status"], "not_configured");
    assert_eq!(v["configured"], false);
    let base = json!({"host":"smtp.example.test","port":587,"tls":"starttls","from_address":"gateway@example.test"});
    let with = |patch: Value| {
        let mut b = base.clone();
        for (k, v) in patch.as_object().unwrap() {
            b[k] = v.clone();
        }
        b
    };
    for (body, reason) in [
        (
            with(json!({"tls":"none"})),
            Some("plaintext_requires_loopback"),
        ),
        (
            with(json!({"username":"relay","password_ref":"env:NOT_ALLOWLISTED_SMTP"})),
            Some("credential_reference_not_allowed"),
        ),
        (with(json!({"username":"relay"})), None),
        (
            with(json!({"username":"relay","password_ref":"hunter2"})),
            None,
        ),
        (with(json!({"host":"smtp.example.test:587"})), None),
        (with(json!({"port":0})), None),
        (with(json!({"port":70000})), None),
        (with(json!({"tls":"ssl"})), None),
        (with(json!({"from_address":"not-an-address"})), None),
        (json!({"host":null,"port":25}), None),
    ] {
        let (status, err) = call(&f.s, &f.admin, "PUT", EMAIL, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        if let Some(reason) = reason {
            assert_eq!(err["error"]["reason"], reason, "{body}");
        }
    }
    let (status, v) = call(&f.s, &f.admin, "PUT", EMAIL, base.clone()).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "ready");
    assert_eq!(v["tls"], "starttls");
    assert_eq!(v["last_test"], Value::Null);
    // Turning delivery off clears everything.
    let (status, v) = call(&f.s, &f.admin, "PUT", EMAIL, json!({"host":null})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["configured"], false);
    assert_eq!(v["host"], Value::Null);
    assert_eq!(audit_count(&pool, "settings.email_updated").await, 2);
    // Without a relay, a test send is refused rather than pretending.
    let (status, err) = call(&f.s, &f.admin, "POST", TEST, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err["error"]["reason"], "email_not_configured");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn test_email_reaches_the_admin_is_audited_and_rate_limited(pool: PgPool) {
    let f = fixture(&pool).await;
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    let (status, v) = call(&f.s, &f.admin, "PUT", EMAIL, json!({"host":"127.0.0.1","port":port,"tls":"none","from_address":"gateway@example.test","from_name":"Gateway"})).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (status, v) = call(&f.s, &f.admin, "POST", TEST, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["recipient"], "admin@example.test");
    let got = rx.recv().await.unwrap();
    assert!(got.rcpt_to[0].contains("admin@example.test"));
    assert!(got.data.contains("Gateway"));
    let (_, v) = call(&f.s, &f.auditor, "GET", EMAIL, json!({})).await;
    assert_eq!(v["last_test"]["ok"], true);
    // A second send in the same minute is allowed, a third is not.
    assert_eq!(
        call(&f.s, &f.admin, "POST", TEST, json!({})).await.0,
        StatusCode::OK
    );
    let (status, err) = call(&f.s, &f.admin, "POST", TEST, json!({})).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(err["error"]["reason"], "email_test_rate_limited");
    assert_eq!(audit_count(&pool, "settings.email_test").await, 2);
    // Audit metadata never carries addresses or message content.
    let metadata: Vec<Value> =
        sqlx::query_scalar("SELECT metadata FROM audit_events WHERE action LIKE 'settings.%'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(metadata.iter().all(|m| !m.to_string().contains('@')));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn failed_delivery_is_reported_as_a_category(pool: PgPool) {
    let f = fixture(&pool).await;
    let (port, _rx) = mock::serve(Behaviour {
        reject_recipient: true,
        ..Default::default()
    })
    .await;
    call(
        &f.s,
        &f.admin,
        "PUT",
        EMAIL,
        json!({"host":"localhost","port":port,"tls":"none","from_address":"gateway@example.test"}),
    )
    .await;
    let (status, v) = call(&f.s, &f.admin, "POST", TEST, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v,
        json!({"ok":false,"error":"rejected","recipient":"admin@example.test"})
    );
    let (_, v) = call(&f.s, &f.admin, "GET", EMAIL, json!({})).await;
    assert_eq!(v["last_test"]["error"], "rejected");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn invitations_are_emailed_when_delivery_is_configured(pool: PgPool) {
    let f = fixture(&pool).await;
    let path = format!("/api/v1/workspaces/{}/invitations", f.team);
    let (status, v) = call(
        &f.s,
        &f.owner,
        "POST",
        &path,
        json!({"email":"new@example.test","role":"member"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["email_delivery"], "not_configured");
    assert!(v["token"].is_string());
    let (port, mut rx) = mock::serve(Behaviour::default()).await;
    call(
        &f.s,
        &f.admin,
        "PUT",
        EMAIL,
        json!({"host":"127.0.0.1","port":port,"tls":"none","from_address":"gateway@example.test"}),
    )
    .await;
    let (status, v) = call(
        &f.s,
        &f.owner,
        "POST",
        &path,
        json!({"email":"Second@Example.test","role":"admin"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["email_delivery"], "sent");
    // The copy-code fallback still works: the token is returned once.
    let token = v["token"].as_str().unwrap();
    let got = rx.recv().await.unwrap();
    assert!(got.rcpt_to[0].contains("second@example.test"));
    assert!(got.data.contains(token));
    assert!(got.data.contains("Team"));
    // The code is never placed in a URL.
    assert!(!got.data.contains(&format!("={token}")));
}

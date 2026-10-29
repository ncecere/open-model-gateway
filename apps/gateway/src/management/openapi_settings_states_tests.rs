//! Slice 5 settings fixtures that drive every documented branch of the settings
//! responses through the real serving stack: nullable and union alternatives
//! (logo set/cleared, email configured/unconfigured, storage backends and probe
//! outcomes, sign-in variants, audit export states) are reached by changing the
//! real configuration or stored state, never by editing a response.
use super::*;
use crate::filestore::{FileStoreConfig, FileStoreRuntime};
use base64::Engine;

/// Who performs a write: a browser session or an admin token whose owner has no
/// email address (so `updated_by`, which is the writer's email, is null).
pub(super) enum Actor<'a> {
    Session(&'a Browser),
    Token(&'a str),
}
async fn act(
    g: &Gateway,
    actor: &Actor<'_>,
    method: &str,
    path: &str,
    media: &str,
    bytes: Vec<u8>,
) -> Captured {
    match actor {
        Actor::Session(browser) => g.raw(Some(browser), method, path, media, bytes).await,
        Actor::Token(token) => {
            capture_http(
                method,
                path,
                g.client
                    .request(method.parse().unwrap(), format!("{}{path}", g.base))
                    .header("origin", ORIGIN)
                    .header("content-type", media)
                    .bearer_auth(token)
                    .body(bytes)
                    .send()
                    .await
                    .unwrap(),
            )
            .await
        }
    }
}
async fn json_act(
    g: &Gateway,
    observed: &mut PositiveFixtures,
    actor: &Actor<'_>,
    method: &str,
    path: &str,
    input: Value,
) -> Value {
    let bytes = if input.is_null() {
        Vec::new()
    } else {
        serde_json::to_vec(&input).unwrap()
    };
    let r = act(g, actor, method, path, "application/json", bytes).await;
    positive(observed, &r, method, path)
}

/// An admin whose user record has no email address, with a token that may write
/// settings. The browser session path needs an email claim; a token does not.
pub(super) async fn admin_without_email(pool: &PgPool) -> String {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,NULL)")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'admin','manual')",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    let mut tx = crate::db::begin(pool).await.unwrap();
    crate::governance::locks::exclusive(&mut tx, [crate::governance::locks::Scope::User(id)])
        .await
        .unwrap();
    let token = crate::admin_tokens::create_admin_token(
        &mut tx,
        id,
        "Contract writer without email",
        &["platform:read".into(), "settings:write".into()],
        30,
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    token.token
}

/// General settings and the logo: `support_url`, the deprecated `logo_url`, the
/// stored logo and the writer's email each seen both set and unset, per operation.
pub(super) async fn general_and_logo(
    g: &Gateway,
    observed: &mut PositiveFixtures,
    admin: &Browser,
    auditor: &Browser,
    nomail: &str,
) {
    let general = format!("{SETTINGS}/general");
    let logo = format!("{general}/logo");
    for actor in [Actor::Session(admin), Actor::Token(nomail)] {
        for support in [None, Some("https://support.example.test")] {
            for logo_url in [None, Some("https://cdn.example.test/logo.png")] {
                let body = json!({"display_name":"Contract Gateway","support_url":support,"logo_url":logo_url,"human_key_max_lifetime_days":30});
                // Start from no stored logo so removal is exercised below.
                json_act(g, observed, &actor, "DELETE", &logo, Value::Null).await;
                let saved = json_act(g, observed, &actor, "PUT", &general, body.clone()).await;
                assert!(saved["logo"].is_null());
                let read = good(g, observed, auditor, "GET", &general, &general, Value::Null).await;
                assert_eq!(read["support_url"], saved["support_url"]);
                // No logo stored: a removal changes nothing but is still a success.
                let removed = json_act(g, observed, &actor, "DELETE", &logo, Value::Null).await;
                assert_eq!(removed["logo_url"], saved["logo_url"]);
                // An upload replaces the deprecated URL with the stored logo.
                let r = act(
                    g,
                    &actor,
                    "PUT",
                    &logo,
                    "image/png",
                    branding::fixtures::png(64, 64),
                )
                .await;
                let uploaded = positive(observed, &r, "PUT", &logo);
                assert!(uploaded["logo_url"].is_null() && uploaded["logo"].is_object());
                let with_logo = json_act(g, observed, &actor, "PUT", &general, body).await;
                assert!(with_logo["logo"].is_object());
                let read = good(g, observed, auditor, "GET", &general, &general, Value::Null).await;
                assert!(read["logo"].is_object());
                let cleared = json_act(g, observed, &actor, "DELETE", &logo, Value::Null).await;
                assert!(cleared["logo"].is_null());
            }
        }
    }
}

/// Request-log retention stored and unset.
pub(super) async fn privacy(g: &Gateway, observed: &mut PositiveFixtures, admin: &Browser) {
    if crate::maintenance::retention_from_env().unwrap().is_some() {
        return;
    }
    let path = format!("{SETTINGS}/privacy");
    for days in [json!(90), Value::Null] {
        let current = value(&g.send(admin, "GET", &path, Value::Null).await);
        let body = json!({"openrouter_data_collection":current["openrouter_data_collection"]["stored"],"request_log_retention_days":days});
        let saved = good(g, observed, admin, "PUT", &path, &path, body).await;
        assert_eq!(saved["request_log_retention_days"]["stored"], days);
        let read = good(g, observed, admin, "GET", &path, &path, Value::Null).await;
        assert_eq!(read["request_log_retention_days"]["value"], days);
    }
}

/// Email: unconfigured, relay without credentials, relay with a credential
/// reference, and the last test outcome both successful and failed.
pub(super) async fn email(
    g: &Gateway,
    observed: &mut PositiveFixtures,
    admin: &Browser,
    tester: &Browser,
    mail_port: u16,
) {
    let path = format!("{SETTINGS}/email");
    let test = format!("{path}/test");
    // Delivery off.
    let off = good(g, observed, admin, "PUT", &path, &path, json!({"host":null,"port":null,"tls":null,"username":null,"password_ref":null,"from_address":null,"from_name":null})).await;
    assert_eq!(off["configured"], false);
    good(g, observed, admin, "GET", &path, &path, Value::Null).await;
    // A relay without credentials or a sender name.
    let relay = json!({"host":"127.0.0.1","port":mail_port,"tls":"none","from_address":"gateway@example.test"});
    let saved = good(g, observed, admin, "PUT", &path, &path, relay.clone()).await;
    assert!(saved["last_test"].is_null() && saved["username"].is_null());
    let read = good(g, observed, admin, "GET", &path, &path, Value::Null).await;
    assert!(read["last_test"].is_null() && read["from_name"].is_null());
    // A relay with a credential reference and a sender name; the variable is not
    // set in this environment, so the credential is unavailable and a test fails
    // with its category, never a relay reply.
    let creds = json!({"host":"127.0.0.1","port":mail_port,"tls":"none","from_address":"gateway@example.test","from_name":"Contract Gateway","username":"smtp-user","password_ref":"env:CONTRACT_SMTP_PASSWORD"});
    let saved = good(g, observed, admin, "PUT", &path, &path, creds).await;
    assert_eq!(saved["password_ref_allowed"], true);
    assert_eq!(saved["status"], "credential_unavailable");
    let failed = good(g, observed, tester, "POST", &test, &test, json!({})).await;
    assert_eq!(failed["ok"], false);
    assert_eq!(failed["error"], "credential");
    let read = good(g, observed, admin, "GET", &path, &path, Value::Null).await;
    assert_eq!(read["last_test"]["ok"], false);
    assert_eq!(read["last_test"]["error"], "credential");
    // The same relay without credentials delivers, and the read shows the pass.
    good(g, observed, admin, "PUT", &path, &path, relay).await;
    let sent = good(g, observed, tester, "POST", &test, &test, json!({})).await;
    assert_eq!(sent["ok"], true);
    let read = good(g, observed, admin, "GET", &path, &path, Value::Null).await;
    assert_eq!(read["last_test"]["ok"], true);
    assert!(read["last_test"]["error"].is_null());
}

fn key_b64() -> String {
    base64::prelude::BASE64_STANDARD.encode(crate::entropy::bytes::<32>().unwrap())
}
/// A file store runtime built from real environment-shaped configuration.
pub(super) async fn runtime(kind: &str, local_dir: &std::path::Path) -> FileStoreRuntime {
    let keys = format!("main:{},old:{}", key_b64(), key_b64());
    let dir = local_dir.to_string_lossy().into_owned();
    let kind = kind.to_owned();
    let config = FileStoreConfig::from_lookup(&move |name| match (kind.as_str(), name) {
        (_, "GATEWAY_FILE_ENCRYPTION_KEYS_ENV") => Some("CONTRACT_FILE_KEYS".into()),
        (_, "CONTRACT_FILE_KEYS") => Some(keys.clone()),
        ("local", "GATEWAY_FILE_STORE") => Some("local".into()),
        ("local", "GATEWAY_FILE_STORE_DIR") => Some(dir.clone()),
        ("s3", "GATEWAY_FILE_STORE") | ("s3-dead", "GATEWAY_FILE_STORE") => Some("s3".into()),
        ("s3" | "s3-dead", "GATEWAY_S3_BUCKET") => Some("contract-bucket".into()),
        ("s3" | "s3-dead", "GATEWAY_S3_AUTH") => Some("static".into()),
        ("s3" | "s3-dead", "GATEWAY_S3_ACCESS_KEY_ID_ENV") => Some("CONTRACT_S3_KEY".into()),
        ("s3" | "s3-dead", "GATEWAY_S3_SECRET_ACCESS_KEY_ENV") => Some("CONTRACT_S3_SECRET".into()),
        ("s3" | "s3-dead", "CONTRACT_S3_KEY") => Some("AKIACONTRACTKEY".into()),
        ("s3" | "s3-dead", "CONTRACT_S3_SECRET") => Some("contract-secret-value".into()),
        // Nothing listens on port 1: the probe fails fast with a category.
        ("s3-dead", "GATEWAY_S3_ENDPOINT") => Some("http://127.0.0.1:1".into()),
        ("s3-dead", "GATEWAY_S3_ENDPOINT_ALLOWLIST") => Some("http://127.0.0.1:1".into()),
        _ => None,
    })
    .unwrap();
    FileStoreRuntime::build(config).await.unwrap()
}

async fn forget_probe(pool: &PgPool) {
    sqlx::query("UPDATE installation_settings SET file_store_last_check_at=NULL,file_store_last_check_ok=NULL,file_store_last_check_error=NULL,file_store_last_check_target=NULL WHERE singleton")
        .execute(pool)
        .await
        .unwrap();
}

/// Storage: every backend (off, memory, local, S3 with and without an explicit
/// endpoint), encryption on and off, and the recorded probe absent, passing and
/// failing, read and saved through each.
pub(super) async fn storage(
    pool: &PgPool,
    store: &Store,
    observed: &mut PositiveFixtures,
    admin: &Browser,
    auditor: &Browser,
    testers: [&Browser; 3],
) {
    let path = format!("{SETTINGS}/storage");
    let test = format!("{path}/test");
    let dir = tempfile::tempdir().unwrap();
    let input = json!({"batch":{"enabled":false,"retention_days":30},"video":{"enabled":false,"retention_days":30},"user_files":{"enabled":false,"retention_days":30},"export":{"retention_days":7}});
    let mut probes = testers.into_iter();
    for (label, rt, probe) in [
        ("off", FileStoreRuntime::off(), false),
        ("memory", FileStoreRuntime::memory(), true),
        (
            "local",
            runtime("local", &dir.path().join("files")).await,
            true,
        ),
        ("s3", runtime("s3", dir.path()).await, false),
        ("s3-dead", runtime("s3-dead", dir.path()).await, true),
    ] {
        let gateway = Gateway::start(store.clone(), pool, rt).await;
        forget_probe(pool).await;
        let read = good(
            &gateway,
            observed,
            auditor,
            "GET",
            &path,
            &path,
            Value::Null,
        )
        .await;
        assert!(read["health"].is_null(), "{label}");
        let saved = good(
            &gateway,
            observed,
            admin,
            "PUT",
            &path,
            &path,
            input.clone(),
        )
        .await;
        assert!(saved["health"].is_null(), "{label}");
        if !probe {
            continue;
        }
        let tester = probes.next().unwrap_or(admin);
        let result = good(&gateway, observed, tester, "POST", &test, &test, json!({})).await;
        assert_eq!(result["ok"], label != "s3-dead", "{label}");
        let read = good(
            &gateway,
            observed,
            auditor,
            "GET",
            &path,
            &path,
            Value::Null,
        )
        .await;
        assert_eq!(read["health"]["ok"], label != "s3-dead", "{label}");
        let saved = good(
            &gateway,
            observed,
            admin,
            "PUT",
            &path,
            &path,
            input.clone(),
        )
        .await;
        assert_eq!(
            saved["health"]["error"].is_null(),
            label != "s3-dead",
            "{label}"
        );
    }
}

/// Audit export: never configured on this replica, configured but disabled with
/// nothing queued, queued and delivering, and failing and retrying.
/// Three gateways, three actors and the exporter are distinct fixtures of one
/// scenario; a parameter struct would only rename them.
#[allow(clippy::too_many_arguments)]
pub(super) async fn audit_export(
    pool: &PgPool,
    configured: &Gateway,
    unconfigured: &Gateway,
    broken: &Gateway,
    observed: &mut PositiveFixtures,
    admin: &Browser,
    auditor: &Browser,
    tester: &Browser,
    exporter: crate::audit_export::Exporter,
) {
    let path = format!("{SETTINGS}/audit-export");
    let test = format!("{path}/test");
    // A replica without a destination reports neither a transport nor a host.
    crate::audit_export::sync_state(
        &Store::new(pool.clone()),
        &crate::audit_export::Setting::Off,
    )
    .await
    .unwrap();
    // Disabling writes its own audit marker; nothing stays queued.
    sqlx::query("DELETE FROM audit_export_queue")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE audit_export_state SET attempts=0,next_attempt_at='-infinity',last_success_at=NULL,last_error=NULL,last_error_at=NULL,last_heartbeat_at=NULL")
        .execute(pool)
        .await
        .unwrap();
    let idle = good(
        unconfigured,
        observed,
        auditor,
        "GET",
        &path,
        &path,
        Value::Null,
    )
    .await;
    assert!(idle["transport"].is_null() && idle["destination_host"].is_null());
    assert_eq!(idle["configured"], false);
    let idle = good(
        configured,
        observed,
        admin,
        "GET",
        &path,
        &path,
        Value::Null,
    )
    .await;
    assert_eq!(idle["transport"], "webhook");
    assert!(idle["oldest_queued_at"].is_null() && idle["enabled_at"].is_null());
    // Enabled: audited activity is queued and a delivery has succeeded.
    crate::audit_export::sync_state(
        &Store::new(pool.clone()),
        &crate::audit_export::Setting::On(exporter),
    )
    .await
    .unwrap();
    let general = format!("{SETTINGS}/general");
    let body = json!({"display_name":"Contract Gateway","support_url":null,"logo_url":null,"human_key_max_lifetime_days":30});
    good(configured, observed, admin, "PUT", &general, &general, body).await;
    sqlx::query("UPDATE audit_export_state SET last_success_at=now(),last_heartbeat_at=now()")
        .execute(pool)
        .await
        .unwrap();
    let queued = good(
        configured,
        observed,
        auditor,
        "GET",
        &path,
        &path,
        Value::Null,
    )
    .await;
    assert!(queued["queued"].as_i64().unwrap() > 0);
    assert!(queued["oldest_queued_at"].is_string() && queued["last_success_at"].is_string());
    // Failing: the last error and the next retry are reported.
    sqlx::query("UPDATE audit_export_state SET attempts=2,next_attempt_at=now()+interval '1 minute',last_error='connection',last_error_at=now()")
        .execute(pool)
        .await
        .unwrap();
    let failing = good(
        configured,
        observed,
        auditor,
        "GET",
        &path,
        &path,
        Value::Null,
    )
    .await;
    assert_eq!(failing["retrying"], true);
    assert_eq!(failing["last_error"], "connection");
    assert!(failing["next_attempt_at"].is_string());
    // A test delivery to a receiver that is not there fails with a category.
    let failed = good(broken, observed, tester, "POST", &test, &test, json!({})).await;
    assert_eq!(failed["ok"], false);
    assert!(failed["error"].is_string());
}

/// Sign-in: no provider, a provider without SCIM, and a provider with SCIM that
/// has and has not synchronised.
pub(super) async fn sign_in(
    pool: &PgPool,
    plain: &Gateway,
    scim: &Gateway,
    none: &Gateway,
    observed: &mut PositiveFixtures,
    auditor: &Browser,
) {
    let path = format!("{SETTINGS}/sign-in");
    let b = good(none, observed, auditor, "GET", &path, &path, Value::Null).await;
    assert_eq!(b["enabled"], false);
    let b = good(plain, observed, auditor, "GET", &path, &path, Value::Null).await;
    assert_eq!(b["scim"]["enabled"], false);
    sqlx::query("UPDATE scim_state SET last_write_at=NULL WHERE singleton")
        .execute(pool)
        .await
        .unwrap();
    let b = good(scim, observed, auditor, "GET", &path, &path, Value::Null).await;
    assert!(b["scim"]["last_sync_at"].is_null());
    sqlx::query("UPDATE scim_state SET last_write_at=now() WHERE singleton")
        .execute(pool)
        .await
        .unwrap();
    let b = good(scim, observed, auditor, "GET", &path, &path, Value::Null).await;
    assert!(b["scim"]["last_sync_at"].is_string());
}

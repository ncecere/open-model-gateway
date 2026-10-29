//! Slice 5 HTTP boundary vectors: for every documented query parameter and JSON
//! request body of the settings and alert operations, the SCHEMA's verdict on each
//! boundary input must equal the real HANDLER's verdict over HTTP. Valid bodies carry
//! every optional field so each is walked; contexts that a request consumes (a created
//! rule, a created destination, a rate-limited test) are fresh per vector set.
use super::*;
use openapi::http_vectors::{
    Probe, binary_body_vectors_http, body_vectors_http, query_vectors_fixed,
};

const WORKSPACES: &str = "/api/v1/workspaces";
const HOOK_URL: &str = "https://receiver.example.test/ingest";
const WEBHOOKS: &str = "/api/v1/platform/alerts/webhooks";

async fn fresh_admin(pool: &PgPool, name: &str) -> Browser {
    let principal = user(pool, name, "admin").await;
    browser(pool, &principal).await
}
async fn json(g: &Gateway, actor: &Browser, method: &str, path: &str, body: Value) -> Probe {
    Probe::from(g.send(actor, method, path, body).await)
}
fn id_of(r: &Captured) -> Uuid {
    id(&value(r))
}

/// Soft-delete the rules a vector set created (the same update the DELETE handler runs).
async fn forget_rules(pool: &PgPool, since: chrono::DateTime<chrono::Utc>) {
    sqlx::query("UPDATE alert_rules SET deleted_at=now(),enabled=false,updated_at=now() WHERE created_at>=$1 AND deleted_at IS NULL")
        .bind(since)
        .execute(pool)
        .await
        .unwrap();
}
async fn live_webhooks(pool: &PgPool) -> Vec<Uuid> {
    sqlx::query_scalar("SELECT id FROM alert_webhook_destinations WHERE deleted_at IS NULL")
        .fetch_all(pool)
        .await
        .unwrap()
}
/// Delete every destination a vector set created, through the API.
async fn forget_webhooks(g: &Gateway, pool: &PgPool, admin: &Browser, keep: &[Uuid]) {
    for id in live_webhooks(pool)
        .await
        .into_iter()
        .filter(|id| !keep.contains(id))
    {
        let r = g
            .send(admin, "DELETE", &format!("{WEBHOOKS}/{id}"), json!({}))
            .await;
        assert!(r.status.is_success());
    }
}

/// Configuration bodies: general, privacy, email (relay / credentials / off) and storage.
pub(super) async fn settings(
    g: &Gateway,
    pool: &PgPool,
    observed: &mut PositiveFixtures,
    mail_port: u16,
) {
    let admin = fresh_admin(pool, "vector-settings").await;
    let general = format!("{SETTINGS}/general");
    body_vectors_http(
        observed,
        "PUT",
        &general,
        json!({"display_name":"Contract Gateway","support_url":"https://support.example.test","logo_url":"https://cdn.example.test/logo.png","human_key_max_lifetime_days":30}),
        async |body| json(g, &admin, "PUT", &general, body).await,
    )
    .await;
    let privacy = format!("{SETTINGS}/privacy");
    body_vectors_http(
        observed,
        "PUT",
        &privacy,
        json!({"openrouter_data_collection":"deny","request_log_retention_days":90}),
        async |body| json(g, &admin, "PUT", &privacy, body).await,
    )
    .await;
    let storage = format!("{SETTINGS}/storage");
    body_vectors_http(
        observed,
        "PUT",
        &storage,
        json!({"batch":{"enabled":true,"retention_days":30},"video":{"enabled":false,"retention_days":30},"user_files":{"enabled":false,"retention_days":30},"export":{"retention_days":7}}),
        async |body| json(g, &admin, "PUT", &storage, body).await,
    )
    .await;
    let email = format!("{SETTINGS}/email");
    for valid in [
        json!({"host":"127.0.0.1","port":mail_port,"tls":"none","username":"smtp-user","password_ref":"env:CONTRACT_SMTP_PASSWORD","from_address":"gateway@example.test","from_name":"Contract Gateway"}),
        json!({"host":"127.0.0.1","port":mail_port,"tls":"none","username":null,"password_ref":null,"from_address":"gateway@example.test","from_name":null}),
        json!({"host":null,"port":null,"tls":null,"username":null,"password_ref":null,"from_address":null,"from_name":null}),
    ] {
        body_vectors_http(observed, "PUT", &email, valid, async |body| {
            json(g, &admin, "PUT", &email, body).await
        })
        .await;
    }
}

/// Test actions and a destination delete take no configuration: absent or `{}` only.
pub(super) async fn empty_configuration(
    g: &Gateway,
    pool: &PgPool,
    observed: &mut PositiveFixtures,
    mail_port: u16,
) {
    // A relay that delivers to the local receiver, so the valid `{}` request succeeds.
    let setup = fresh_admin(pool, "vector-empty-setup").await;
    let relay = json!({"host":"127.0.0.1","port":mail_port,"tls":"none","from_address":"gateway@example.test"});
    let saved = g
        .send(&setup, "PUT", &format!("{SETTINGS}/email"), relay)
        .await;
    assert!(saved.status.is_success());
    for part in ["email/test", "storage/test", "audit-export/test"] {
        // The handler's test limiter is per administrator: one fresh one per action.
        let admin = fresh_admin(pool, &format!("vector-{}", part.replace('/', "-"))).await;
        let path = format!("{SETTINGS}/{part}");
        body_vectors_http(observed, "POST", &path, json!({}), async |body| {
            json(g, &admin, "POST", &path, body).await
        })
        .await;
    }
    let keep = live_webhooks(pool).await;
    let admin = fresh_admin(pool, "vector-webhook-test").await;
    let create =
        json!({"label":"Vector empty","url":HOOK_URL,"current_secret_ref":"env:HOOK_CURRENT"});
    let made = g.send(&admin, "POST", WEBHOOKS, create.clone()).await;
    let path = format!("{WEBHOOKS}/{}", id_of(&made));
    let enabled = g
        .send(&admin, "PATCH", &path, json!({"enabled":true}))
        .await;
    assert!(enabled.status.is_success());
    let test = format!("{path}/test");
    body_vectors_http(
        observed,
        "POST",
        "/api/v1/platform/alerts/webhooks/{id}/test",
        json!({}),
        async |body| json(g, &admin, "POST", &test, body).await,
    )
    .await;
    // A delete consumes its destination: every request gets a live one.
    let deleter = fresh_admin(pool, "vector-webhook-delete").await;
    let target = std::cell::Cell::new(String::new());
    let next = async || {
        let made = g.send(&deleter, "POST", WEBHOOKS, create.clone()).await;
        target.set(format!("{WEBHOOKS}/{}", id_of(&made)));
    };
    next().await;
    body_vectors_http(
        observed,
        "DELETE",
        "/api/v1/platform/alerts/webhooks/{id}",
        json!({}),
        async |body| {
            let path = target.take();
            let probe = json(g, &deleter, "DELETE", &path, body).await;
            next().await;
            probe
        },
    )
    .await;
    forget_webhooks(g, pool, &deleter, &keep).await;
}

/// Webhook create (object and positional array) and PATCH (a rotating destination).
pub(super) async fn webhooks(g: &Gateway, pool: &PgPool, observed: &mut PositiveFixtures) {
    let admin = fresh_admin(pool, "vector-hooks").await;
    let keep = live_webhooks(pool).await;
    for valid in [
        json!({"label":"Vector receiver","url":HOOK_URL,"current_secret_ref":"env:HOOK_CURRENT"}),
        json!(["Vector array", HOOK_URL, "env:HOOK_CURRENT"]),
    ] {
        body_vectors_http(observed, "POST", WEBHOOKS, valid, async |body| {
            json(g, &admin, "POST", WEBHOOKS, body).await
        })
        .await;
        forget_webhooks(g, pool, &admin, &keep).await;
    }
    // A destination that already rotates, so omitting either rotation field keeps a
    // consistent stored pair.
    let made = g
        .send(
            &admin,
            "POST",
            WEBHOOKS,
            json!({"label":"Vector patch","url":HOOK_URL,"current_secret_ref":"env:HOOK_CURRENT"}),
        )
        .await;
    let path = format!("{WEBHOOKS}/{}", id_of(&made));
    let until = (chrono::Utc::now() + chrono::Duration::hours(1))
        .format("%Y-%m-%d %H:%M:%SZ")
        .to_string();
    let valid = json!({"label":"Renamed","enabled":true,"current_secret_ref":"env:HOOK_CURRENT","previous_secret_ref":"env:HOOK_PREVIOUS","previous_secret_until":until});
    let rotating = g.send(&admin, "PATCH", &path, valid.clone()).await;
    assert!(rotating.status.is_success());
    body_vectors_http(
        observed,
        "PATCH",
        "/api/v1/platform/alerts/webhooks/{id}",
        valid,
        async |body| json(g, &admin, "PATCH", &path, body).await,
    )
    .await;
    forget_webhooks(g, pool, &admin, &keep).await;
}

/// Every explicit field of a rule body (nulls for the ones foreign to its kind), so
/// the vectors also prove that a foreign field is refused.
fn full(mut body: Value) -> Value {
    for field in [
        "budget_layers",
        "thresholds",
        "spike_factor_percent",
        "min_spend_microusd",
        "window_minutes",
        "error_rate_percent",
        "min_requests",
        "consecutive_failures",
        "provider_connection_id",
        "spend_period",
        "spend_amount_microusd",
        "ceiling_requests_per_second",
        "ceiling_lock_wait_ms",
    ] {
        body.as_object_mut()
            .unwrap()
            .entry(field)
            .or_insert(Value::Null);
    }
    body
}

/// Rule create and update, one valid body per rule kind and shape, in both scopes.
pub(super) async fn rules(
    g: &Gateway,
    pool: &PgPool,
    observed: &mut PositiveFixtures,
    admin: &Browser,
    owner: &Browser,
    team: Uuid,
    connection: Uuid,
) {
    let since: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(pool)
        .await
        .unwrap();
    let scopes = [
        (
            format!("{ALERTS}/rules"),
            "/api/v1/platform/alerts/rules",
            "/api/v1/platform/alerts/rules/{id}",
            admin,
            alerts_states::platform_rules(connection),
        ),
        (
            format!("{WORKSPACES}/{team}/alerts/rules"),
            "/api/v1/workspaces/{ws}/alerts/rules",
            "/api/v1/workspaces/{ws}/alerts/rules/{id}",
            owner,
            alerts_states::workspace_rules(),
        ),
    ];
    for (prefix, post, put, actor, bodies) in &scopes {
        for body in bodies {
            let valid = full(body.clone());
            body_vectors_http(observed, "POST", post, valid.clone(), async |request| {
                json(g, actor, "POST", prefix, request).await
            })
            .await;
            forget_rules(pool, since).await;
            let created = g.send(actor, "POST", prefix, body.clone()).await;
            assert!(created.status.is_success());
            let path = format!("{prefix}/{}", id_of(&created));
            body_vectors_http(observed, "PUT", put, valid, async |request| {
                json(g, actor, "PUT", &path, request).await
            })
            .await;
            forget_rules(pool, since).await;
        }
    }
}

/// Incident history filters, per scope, from a valid request carrying every parameter.
pub(super) async fn events(
    g: &Gateway,
    observed: &mut PositiveFixtures,
    admin: &Browser,
    owner: &Browser,
    team: Uuid,
    platform_rule: Uuid,
    workspace_rule: Uuid,
) {
    let path = format!("{ALERTS}/events");
    let rule = platform_rule.to_string();
    query_vectors_fixed(
        observed,
        "GET",
        "/api/v1/platform/alerts/events",
        &[
            ("status", "firing"),
            ("kind", "budget_threshold"),
            ("rule_id", rule.as_str()),
            ("limit", "5"),
            ("offset", "0"),
        ],
        async |query| {
            g.send(admin, "GET", &format!("{path}?{query}"), Value::Null)
                .await
        },
    )
    .await;
    let path = format!("{WORKSPACES}/{team}/alerts/events");
    let rule = workspace_rule.to_string();
    query_vectors_fixed(
        observed,
        "GET",
        "/api/v1/workspaces/{ws}/alerts/events",
        &[
            ("status", "firing"),
            ("kind", "budget_threshold"),
            ("rule_id", rule.as_str()),
            ("limit", "5"),
            ("offset", "0"),
        ],
        async |query| {
            g.send(owner, "GET", &format!("{path}?{query}"), Value::Null)
                .await
        },
    )
    .await;
}

/// The logo upload: raw bytes under each documented image type.
pub(super) async fn logo(g: &Gateway, pool: &PgPool, observed: &mut PositiveFixtures) {
    let admin = fresh_admin(pool, "vector-logo").await;
    let path = format!("{SETTINGS}/general/logo");
    for (media, bytes) in [
        ("image/png", branding::fixtures::png(64, 64)),
        ("image/jpeg", branding::fixtures::jpeg(64, 64)),
        ("image/webp", branding::fixtures::webp_lossless(64, 64)),
    ] {
        binary_body_vectors_http(
            observed,
            "PUT",
            "/api/v1/platform/settings/general/logo",
            media,
            bytes,
            // Sides outside 16 to 4096 pixels, in the same container.
            vec![
                ("too small", branding::fixtures::png(8, 8)),
                ("too narrow", branding::fixtures::png(15, 64)),
            ],
            async |declared: String, body: Vec<u8>| {
                let r = g.raw(Some(&admin), "PUT", &path, &declared, body).await;
                // Input refusals here are 400, 413 and 415: the status alone decides.
                let accepted = r.status.is_success();
                Probe::decided(r, accepted)
            },
        )
        .await;
    }
}

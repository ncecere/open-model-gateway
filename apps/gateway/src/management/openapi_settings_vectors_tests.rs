//! Slice 5 HTTP boundary vectors: for every documented query parameter and JSON
//! request body of the settings and alert operations, the SCHEMA's verdict on each
//! boundary input must equal the real HANDLER's verdict over HTTP. Valid bodies carry
//! every optional field so each is walked; contexts that a request consumes (a created
//! rule, a created destination, a rate-limited test) are fresh per vector set.
use super::*;
use openapi::http_vectors::{
    Probe, binary_accepts, binary_body_vectors_http, body_vectors_extended, body_vectors_http,
    query_vectors_fixed,
};

const WORKSPACES: &str = "/api/v1/workspaces";
const HOOK_URL: &str = "https://receiver.example.test/ingest";
const WEBHOOKS: &str = "/api/v1/platform/alerts/webhooks";

/// Boundary candidates `(pointer, value)` for the string patterns whose length the
/// schema cannot state (an unbounded whitespace pad around a trimmed value, an
/// alternation). Each pair straddles the boundary: a value the schema accepts and one
/// it refuses, one character apart. Only the pointers a valid body carries as a string
/// are used.
fn carried(valid: &Value, all: Vec<(String, Value)>) -> Vec<(String, Value)> {
    all.into_iter()
        .filter(|(pointer, _)| valid.pointer(pointer).is_some_and(Value::is_string))
        .collect()
}
/// A name of exactly `n` characters (120 bytes is the handler's limit for ASCII).
fn chars(n: usize) -> Value {
    json!("a".repeat(n))
}
fn names(pointer: &'static str) -> Vec<(&'static str, Value)> {
    vec![(pointer, chars(120)), (pointer, chars(121))]
}
fn emails(pointer: &'static str) -> Vec<(&'static str, Value)> {
    vec![(pointer, json!("a@b.c")), (pointer, json!("a@b.c@"))]
}
/// Relay candidates: each pair is one edit apart and straddles the handler's rule.
fn email_candidates() -> Vec<(&'static str, Value)> {
    let mut all = vec![
        ("/host", json!("127.0.0.1")),
        ("/host", json!("127.0.0.1/")),
        ("/tls", json!("none")),
        ("/tls", json!("nonee")),
        ("/username", json!("u".repeat(256))),
        ("/username", json!("u".repeat(257))),
        ("/password_ref", json!("env:CONTRACT_SMTP_PASSWORD")),
        ("/password_ref", json!("env:CONTRACT_SMTP_PASSWORD!")),
    ];
    all.extend(emails("/from_address"));
    all.extend(names("/from_name"));
    all
}
/// Destination candidates, object and positional form: the label limit, and the one
/// approved URL against the same URL with a query (which the pattern refuses).
fn webhook_candidates() -> Vec<(&'static str, Value)> {
    let mut all = Vec::new();
    for (label, url) in [("/label", "/url"), ("/0", "/1")] {
        all.extend(names(label));
        all.push((url, json!(HOOK_URL)));
        all.push((url, json!(format!("{HOOK_URL}?"))));
    }
    all
}
/// The fields of a rule body in the handler's declaration order (serde's positional form).
const RULE_FIELDS: [&str; 19] = [
    "name",
    "kind",
    "enabled",
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
    "notify_workspace_admins",
    "notify_platform_admins",
    "notify_emails",
];
/// Rule candidates: the name limit and the notification address.
fn rule_candidates() -> Vec<(&'static str, Value)> {
    let mut all = names("/name");
    all.extend(emails("/notify_emails/0"));
    all
}
/// The candidates of a body, for the operation's own `body_vectors_extended` run.
async fn extended(
    observed: &mut PositiveFixtures,
    method: &str,
    template: &str,
    valid: Value,
    candidates: Vec<(&'static str, Value)>,
    send: impl AsyncFn(Value) -> Probe,
) {
    let owned = candidates
        .into_iter()
        .map(|(pointer, value)| (pointer.to_owned(), value))
        .collect();
    walked(observed, method, template, valid, owned, send).await;
}
async fn walked(
    observed: &mut PositiveFixtures,
    method: &str,
    template: &str,
    valid: Value,
    candidates: Vec<(String, Value)>,
    send: impl AsyncFn(Value) -> Probe,
) {
    let candidates = carried(&valid, candidates);
    let borrowed: Vec<(&str, Value)> = candidates
        .iter()
        .map(|(pointer, value)| (pointer.as_str(), value.clone()))
        .collect();
    body_vectors_extended(observed, method, template, valid, &[], &borrowed, send).await;
}
/// The same candidates addressed in serde's positional form: `/field/rest` becomes
/// `/<index of field>/rest`.
fn indexed(fields: &[&str], candidates: &[(&'static str, Value)]) -> Vec<(String, Value)> {
    candidates
        .iter()
        .filter_map(|(pointer, value)| {
            let rest = pointer.strip_prefix('/')?;
            let (head, tail) = rest.split_once('/').unwrap_or((rest, ""));
            let index = fields.iter().position(|field| *field == head)?;
            let at = if tail.is_empty() {
                format!("/{index}")
            } else {
                format!("/{index}/{tail}")
            };
            Some((at, value.clone()))
        })
        .collect()
}
/// A valid object body in both wire forms: the object itself and serde's positional
/// array of its fields in declaration order.
async fn both(
    observed: &mut PositiveFixtures,
    method: &str,
    template: &str,
    valid: Value,
    fields: &[&str],
    candidates: Vec<(&'static str, Value)>,
    send: impl AsyncFn(Value) -> Probe,
) {
    let array = json!(fields.iter().map(|f| valid[*f].clone()).collect::<Vec<_>>());
    let positional = indexed(fields, &candidates);
    extended(observed, method, template, valid, candidates, &send).await;
    walked(observed, method, template, array, positional, &send).await;
}

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
    both(
        observed,
        "PUT",
        &general,
        json!({"display_name":"Contract Gateway","support_url":"https://support.example.test","logo_url":"https://cdn.example.test/logo.png","human_key_max_lifetime_days":30}),
        &["display_name","support_url","logo_url","human_key_max_lifetime_days"],
        names("/display_name"),
        async |body| json(g, &admin, "PUT", &general, body).await,
    )
    .await;
    let privacy = format!("{SETTINGS}/privacy");
    both(
        observed,
        "PUT",
        &privacy,
        json!({"openrouter_data_collection":"deny","request_log_retention_days":90}),
        &["openrouter_data_collection", "request_log_retention_days"],
        Vec::new(),
        async |body| json(g, &admin, "PUT", &privacy, body).await,
    )
    .await;
    // Retention left unset: the other alternative of the nullable field.
    both(
        observed,
        "PUT",
        &privacy,
        json!({"openrouter_data_collection":"allow","request_log_retention_days":null}),
        &["openrouter_data_collection", "request_log_retention_days"],
        Vec::new(),
        async |body| json(g, &admin, "PUT", &privacy, body).await,
    )
    .await;
    let storage = format!("{SETTINGS}/storage");
    both(
        observed,
        "PUT",
        &storage,
        json!({"batch":{"enabled":true,"retention_days":30},"video":{"enabled":false,"retention_days":30},"user_files":{"enabled":false,"retention_days":30},"export":{"retention_days":7}}),
        &["batch", "video", "user_files", "export"],
        Vec::new(),
        async |body| json(g, &admin, "PUT", &storage, body).await,
    )
    .await;
    // The nested groups in serde's positional form inside an object, and all positional.
    walked(
        observed,
        "PUT",
        &storage,
        json!({"batch":[true, 30],"video":[false, 30],"user_files":[false, 30],"export":[7]}),
        Vec::new(),
        async |body| json(g, &admin, "PUT", &storage, body).await,
    )
    .await;
    walked(
        observed,
        "PUT",
        &storage,
        json!([[true, 30], [false, 30], [false, 30], [7]]),
        Vec::new(),
        async |body| json(g, &admin, "PUT", &storage, body).await,
    )
    .await;
    let email = format!("{SETTINGS}/email");
    for valid in [
        json!({"host":"127.0.0.1","port":mail_port,"tls":"none","username":"smtp-user","password_ref":"env:CONTRACT_SMTP_PASSWORD","from_address":"gateway@example.test","from_name":"Contract Gateway"}),
        json!({"host":"127.0.0.1","port":mail_port,"tls":"none","username":null,"password_ref":null,"from_address":"gateway@example.test","from_name":null}),
        json!({"host":null,"port":null,"tls":null,"username":null,"password_ref":null,"from_address":null,"from_name":null}),
        // The same two relay shapes with the other sender-name alternative.
        json!({"host":"127.0.0.1","port":mail_port,"tls":"none","username":null,"password_ref":null,"from_address":"gateway@example.test","from_name":"Contract Gateway"}),
        json!({"host":"127.0.0.1","port":mail_port,"tls":"none","username":"smtp-user","password_ref":"env:CONTRACT_SMTP_PASSWORD","from_address":"gateway@example.test","from_name":null}),
    ] {
        both(
            observed,
            "PUT",
            &email,
            valid,
            &[
                "host",
                "port",
                "tls",
                "username",
                "password_ref",
                "from_address",
                "from_name",
            ],
            email_candidates(),
            async |body| json(g, &admin, "PUT", &email, body).await,
        )
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
        extended(
            observed,
            "POST",
            WEBHOOKS,
            valid,
            webhook_candidates(),
            async |body| json(g, &admin, "POST", WEBHOOKS, body).await,
        )
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
    let ended = json!({"label":"Renamed","enabled":true,"current_secret_ref":"env:HOOK_CURRENT","previous_secret_ref":null,"previous_secret_until":null});
    // The rotating destination, then the same destination with the rotation ended
    // (both fields null): the other alternative of each rotation field.
    for valid in [valid, ended] {
        both(
            observed,
            "PATCH",
            "/api/v1/platform/alerts/webhooks/{id}",
            valid,
            &[
                "label",
                "enabled",
                "current_secret_ref",
                "previous_secret_ref",
                "previous_secret_until",
            ],
            names("/label"),
            async |body| json(g, &admin, "PATCH", &path, body).await,
        )
        .await;
    }
    forget_webhooks(g, pool, &admin, &keep).await;
}

/// Every explicit field of a rule body (nulls for the ones foreign to its kind), so
/// the vectors also prove that a foreign field is refused.
fn full(mut body: Value) -> Value {
    let object = body.as_object_mut().unwrap();
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
        object.entry(field).or_insert(Value::Null);
    }
    // The defaulted fields, explicit, so each is walked: an admission ceiling
    // notifies Platform Admins only and lists no addresses.
    let ceiling = object["kind"] == "admission_ceiling";
    object.entry("enabled").or_insert(json!(true));
    object
        .entry("notify_workspace_admins")
        .or_insert(json!(false));
    object
        .entry("notify_platform_admins")
        .or_insert(json!(false));
    object.entry("notify_emails").or_insert(if ceiling {
        json!([])
    } else {
        json!(["ops@example.test"])
    });
    body
}

/// Every installation rule shape the fixtures create, plus the two the optional
/// alternatives need: a provider rule with BOTH an error-rate threshold and a streak,
/// and an admission ceiling with BOTH a rate and a lock-wait threshold.
fn platform_bodies(connection: Uuid) -> Vec<Value> {
    let mut bodies = alerts_states::platform_rules(connection);
    bodies.push(json!({"name":"Kinds provider both","kind":"provider_failing","window_minutes":20,"error_rate_percent":15,"min_requests":10,"consecutive_failures":3}));
    bodies.push(json!({"name":"Kinds ceiling both","kind":"admission_ceiling","window_minutes":5,"ceiling_requests_per_second":150,"ceiling_lock_wait_ms":50,"notify_platform_admins":true}));
    bodies
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
            platform_bodies(connection),
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
            both(
                observed,
                "POST",
                post,
                valid.clone(),
                &RULE_FIELDS,
                rule_candidates(),
                async |request| json(g, actor, "POST", prefix, request).await,
            )
            .await;
            forget_rules(pool, since).await;
            let created = g.send(actor, "POST", prefix, body.clone()).await;
            assert!(created.status.is_success());
            let path = format!("{prefix}/{}", id_of(&created));
            both(
                observed,
                "PUT",
                put,
                valid,
                &RULE_FIELDS,
                rule_candidates(),
                async |request| json(g, actor, "PUT", &path, request).await,
            )
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

/// The handler's verdict on a logo upload from its status alone. A refusal of the
/// INPUT is 400 (malformed or out-of-range image), 413 (over the size limit) or 415
/// (not a PNG, JPEG or WebP image); 2xx accepts. Anything else (authentication,
/// authorization, conflict, throttling, server failure) says nothing about the
/// input, so it fails the vector instead of counting as a refusal.
fn logo_verdict(status: StatusCode) -> bool {
    match status.as_u16() {
        400 | 413 | 415 => false,
        200..=299 => true,
        other => panic!("logo upload: status {other} is not a verdict about the input"),
    }
}

/// A valid PNG of exactly `total` bytes: an ancillary chunk of zeros before IEND.
fn padded_png(total: usize) -> Vec<u8> {
    let mut bytes = branding::fixtures::png(64, 64);
    let pad = total - bytes.len() - 12;
    let mut chunk = u32::try_from(pad).unwrap().to_be_bytes().to_vec();
    chunk.extend_from_slice(b"zzzz");
    chunk.extend(std::iter::repeat_n(0u8, pad));
    chunk.extend_from_slice(&[0; 4]);
    let iend = bytes.len() - 12;
    bytes.splice(iend..iend, chunk);
    bytes
}

/// The independent expectations for the logo gate: (label, declared media type,
/// bytes, accepted). Written from the documented rules (PNG, JPEG and WebP only,
/// 16 to 4096 pixels per side, at most 524288 bytes, no markup, no trailing data,
/// the declared type must be the real one), NOT from the inspector the schema
/// oracle runs, so a mistake shared by the inspector and the oracle cannot hide.
fn logo_table() -> Vec<(&'static str, &'static str, Vec<u8>, bool)> {
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"></svg>"#.to_vec();
    // A valid PNG with one `tEXt` chunk (payload as given) before IEND.
    let with_text = |payload: &[u8]| {
        let mut image = branding::fixtures::png(64, 64);
        let iend = image.len() - 12;
        let mut chunk = u32::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
        chunk.extend_from_slice(b"tEXt");
        chunk.extend_from_slice(payload);
        chunk.extend_from_slice(&[0; 4]);
        image.splice(iend..iend, chunk);
        image
    };
    let text_chunk = with_text(b"Comment\0<html><script>alert(1)</script>");
    let trailing = |mut image: Vec<u8>| {
        image.extend_from_slice(b"<script>alert(1)</script>");
        image
    };
    let jpeg = branding::fixtures::jpeg(64, 64);
    // Each row's verdict is from `branding.rs`: `png_size` (IEND must end the file),
    // `webp_size` (RIFF size + 8 must equal the length), `jpeg_size` (EOI must be the
    // last two bytes) all answer `Invalid` (400) for trailing data; `inspect` refuses
    // any MARKUP needle case-insensitively (`contains_ignore_case`) with `Invalid`
    // (400); `SIDES` is 16..=4096 inclusive, so 4096 x 4096 passes.
    let after = |mut image: Vec<u8>, tail: &[u8]| {
        image.extend_from_slice(tail);
        image
    };
    let mut rows = vec![
        (
            "png with 4 zero bytes after IEND",
            "image/png",
            after(branding::fixtures::png(64, 64), &[0; 4]),
            false,
        ),
        (
            "webp with 2 bytes past the RIFF size",
            "image/webp",
            after(branding::fixtures::webp_lossless(64, 64), &[0; 2]),
            false,
        ),
        (
            "jpeg with a zero byte after EOI",
            "image/jpeg",
            after(jpeg.clone(), &[0]),
            false,
        ),
        (
            "png with an uppercase script tag in a chunk",
            "image/png",
            with_text(b"Comment\0<SCRIPT>"),
            false,
        ),
        (
            "png with a mixed-case iframe tag in a chunk",
            "image/png",
            with_text(b"Comment\0<IfRaMe src=x>"),
            false,
        ),
        (
            "png at 4096 by 4096",
            "image/png",
            branding::fixtures::png(4096, 4096),
            true,
        ),
        ("png", "image/png", branding::fixtures::png(64, 64), true),
        ("jpeg", "image/jpeg", jpeg.clone(), true),
        (
            "webp",
            "image/webp",
            branding::fixtures::webp_lossless(64, 64),
            true,
        ),
        ("svg", "image/svg+xml", svg.clone(), false),
        ("svg declared png", "image/png", svg, false),
        (
            "gif",
            "image/gif",
            b"GIF89a\x40\0\x40\0\0\0\0;".to_vec(),
            false,
        ),
        (
            "gif declared png",
            "image/png",
            b"GIF89a\x40\0\x40\0\0\0\0;".to_vec(),
            false,
        ),
        (
            "png polyglot",
            "image/png",
            trailing(branding::fixtures::png(64, 64)),
            false,
        ),
        ("jpeg polyglot", "image/jpeg", trailing(jpeg.clone()), false),
        (
            "webp polyglot",
            "image/webp",
            trailing(branding::fixtures::webp_lossless(64, 64)),
            false,
        ),
        ("png with html in a chunk", "image/png", text_chunk, false),
        (
            "truncated jpeg",
            "image/jpeg",
            jpeg[..jpeg.len() - 2].to_vec(),
            false,
        ),
        (
            "png declared jpeg",
            "image/jpeg",
            branding::fixtures::png(64, 64),
            false,
        ),
        ("jpeg declared png", "image/png", jpeg, false),
        (
            "unrelated media type",
            "application/octet-stream",
            branding::fixtures::png(64, 64),
            false,
        ),
        ("size at the limit", "image/png", padded_png(524_288), true),
        (
            "size over the limit",
            "image/png",
            padded_png(524_289),
            false,
        ),
    ];
    // Each side at its documented bounds, in every container: 16 and 4096 pass,
    // 15 and 4097 do not.
    for (side, accepted) in [(15u32, false), (16, true), (4096, true), (4097, false)] {
        let png = |w: u32, h: u32| branding::fixtures::png(w, h);
        let jpeg = |w: u32, h: u32| branding::fixtures::jpeg(w as u16, h as u16);
        let webp = |w: u32, h: u32| branding::fixtures::webp_lossless(w, h);
        for (label, media, image) in [
            ("png width", "image/png", png(side, 64)),
            ("png height", "image/png", png(64, side)),
            ("jpeg width", "image/jpeg", jpeg(side, 64)),
            ("jpeg height", "image/jpeg", jpeg(64, side)),
            ("webp width", "image/webp", webp(side, 64)),
            ("webp height", "image/webp", webp(64, side)),
        ] {
            rows.push((label, media, image, accepted));
        }
    }
    rows
}

/// Every row of the independent table against the real handler, before any
/// oracle-based vector. Names the row label and the status only.
async fn logo_gate(g: &Gateway, admin: &Browser, path: &str) {
    for (label, media, bytes, accepted) in logo_table() {
        let r = g.raw(Some(admin), "PUT", path, media, bytes).await;
        assert_eq!(
            logo_verdict(r.status),
            accepted,
            "logo upload row `{label}`: status {}",
            r.status
        );
    }
}

/// The logo upload: the independent table first, then raw bytes under each
/// documented image type.
pub(super) async fn logo(g: &Gateway, pool: &PgPool, observed: &mut PositiveFixtures) {
    let admin = fresh_admin(pool, "vector-logo").await;
    let path = format!("{SETTINGS}/general/logo");
    logo_gate(g, &admin, &path).await;
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
                // Input refusals are 400, 413 and 415; every other status fails.
                let accepted = logo_verdict(r.status);
                Probe::decided(r, accepted)
            },
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// Pinned schema-versus-handler verdicts the derived vectors cannot generate
// ---------------------------------------------------------------------------

/// Send one input and pin BOTH verdicts: the handler's (400/422 refuse, 2xx accept,
/// anything else fails) and the request schema's. A row where they differ is a
/// documented divergence, pinned so a later change to either side is noticed.
/// Messages name the method, the template and the row label only.
#[allow(clippy::too_many_arguments)]
async fn pin(
    g: &Gateway,
    actor: &Browser,
    method: &str,
    template: &str,
    path: &str,
    label: &str,
    body: Value,
    (handler_accepts, schema_accepts): (bool, bool),
) {
    let r = g.send(actor, method, path, body.clone()).await;
    let handler = match r.status.as_u16() {
        400 | 422 => false,
        200..=299 => true,
        other => panic!("{method} {template}: {label}: status {other} is not an input verdict"),
    };
    let document = openapi::responses::artifact();
    let schema = &document["paths"][template][method.to_ascii_lowercase()]["requestBody"]["content"]
        ["application/json"]["schema"];
    let schema = openapi::responses::validate_value(document, schema, &body).is_ok();
    assert_eq!(
        handler, handler_accepts,
        "{method} {template}: {label}: handler verdict"
    );
    assert_eq!(
        schema, schema_accepts,
        "{method} {template}: {label}: schema verdict"
    );
}

/// Settings and rule bodies at the byte, spelling and range rules the derived
/// vectors cannot generate (multibyte text, alternative IP spellings, a float
/// port, more distinct thresholds than the handler allows).
pub(super) async fn pinned(g: &Gateway, pool: &PgPool, mail_port: u16) {
    let admin = fresh_admin(pool, "pinned-settings").await;
    let general = format!("{SETTINGS}/general");
    let general_template = "/api/v1/platform/settings/general";
    let base = json!({"display_name":"Contract Gateway","support_url":null,"logo_url":null,"human_key_max_lifetime_days":30});
    let with = |field: &str, value: Value| {
        let mut body = base.clone();
        body[field] = value;
        body
    };
    // Names are limited in BYTES by the handler, in characters by the schema.
    let rows = [
        (
            "name of 120 ASCII characters",
            with("display_name", json!("a".repeat(120))),
            (true, true),
        ),
        (
            "name of 100 two-byte characters",
            with("display_name", json!("\u{e9}".repeat(100))),
            (false, true),
        ),
        (
            "name of 60 two-byte characters",
            with("display_name", json!("\u{e9}".repeat(60))),
            (true, true),
        ),
        (
            "URL of 2048 ASCII bytes",
            with(
                "support_url",
                json!(format!(
                    "https://support.example.test/{}",
                    "a".repeat(2048 - 29)
                )),
            ),
            (true, true),
        ),
        (
            "URL of 2049 ASCII bytes",
            with(
                "support_url",
                json!(format!(
                    "https://support.example.test/{}",
                    "a".repeat(2049 - 29)
                )),
            ),
            (false, false),
        ),
        (
            "URL of 1100 two-byte characters",
            with(
                "support_url",
                json!(format!(
                    "https://support.example.test/{}",
                    "\u{e9}".repeat(1100)
                )),
            ),
            (false, true),
        ),
    ];
    // The handler trims BEFORE the 2048 cap (settings.rs `https_url`: `text()` then
    // `value.len()`); the schema's maxLength counts the padding. Same for both URLs.
    let padded = format!(" https://support.example.test/{} ", "a".repeat(2048 - 29));
    let mut rows = rows.to_vec();
    rows.extend([
        (
            "support URL of 2048 bytes with surrounding spaces",
            with("support_url", json!(padded.clone())),
            (true, false),
        ),
        (
            "logo URL of 2048 bytes with surrounding spaces",
            with("logo_url", json!(padded)),
            (true, false),
        ),
    ]);
    // The URL is parsed by `reqwest::Url` (url 2.5.8 host.rs, idna with
    // `DnsLength::Ignore`), which accepts an ASCII host label of 64 or more
    // characters; the schema's pattern caps a label at 63, so it refuses it.
    // Pinned as a documented divergence, for both URL fields.
    let long_label = format!("https://{}.example.test", "a".repeat(64));
    // Non-ASCII in the PATH (`https://support.example.test/` is 29 bytes). The
    // handler stores the NORMALISED URL (`url.to_string()`, each two-byte character
    // percent-encoded to six), and the column CHECK limits that form to 2048
    // characters: a value within the 2048-byte cap whose stored form is longer
    // passes the input checks and is refused by the store with 409 (not pinnable
    // here: 409 is undocumented for this operation). So the byte cap is pinned one
    // byte over (1009 two-byte characters and `aa`: 2049 bytes, refused by the
    // handler's byte cap, accepted by the schema, which counts characters), and
    // the accepted side at the largest stored form: 336 two-byte characters and
    // `aaa` store as exactly 2048 characters.
    let multibyte_path = |count: usize, tail: &str| {
        format!(
            "https://support.example.test/{}{tail}",
            "\u{e9}".repeat(count)
        )
    };
    assert_eq!(multibyte_path(1009, "aa").len(), 2049);
    assert_eq!(
        reqwest::Url::parse(&multibyte_path(336, "aaa"))
            .unwrap()
            .as_str()
            .len(),
        2048
    );
    rows.extend([
        (
            "support URL with a 64-character host label",
            with("support_url", json!(long_label.clone())),
            (true, false),
        ),
        (
            "logo URL with a 64-character host label",
            with("logo_url", json!(long_label)),
            (true, false),
        ),
        (
            "support URL with non-ASCII in the path stored as 2048 characters",
            with("support_url", json!(multibyte_path(336, "aaa"))),
            (true, true),
        ),
        (
            "support URL of 2049 bytes with non-ASCII in the path",
            with("support_url", json!(multibyte_path(1009, "aa"))),
            (false, true),
        ),
        (
            "logo URL with non-ASCII in the path stored as 2048 characters",
            with("logo_url", json!(multibyte_path(336, "aaa"))),
            (true, true),
        ),
        (
            "logo URL of 2049 bytes with non-ASCII in the path",
            with("logo_url", json!(multibyte_path(1009, "aa"))),
            (false, true),
        ),
    ]);
    for (label, body, verdicts) in rows {
        pin(
            g,
            &admin,
            "PUT",
            general_template,
            &general,
            label,
            body,
            verdicts,
        )
        .await;
    }
    let email = format!("{SETTINGS}/email");
    let email_template = "/api/v1/platform/settings/email";
    let relay = json!({"host":"127.0.0.1","port":mail_port,"tls":"none","from_address":"gateway@example.test"});
    let credentials = json!({"host":"127.0.0.1","port":mail_port,"tls":"none","username":"smtp-user","password_ref":"env:CONTRACT_SMTP_PASSWORD","from_address":"gateway@example.test"});
    let edit = |base: &Value, field: &str, value: Value| {
        let mut body = base.clone();
        body[field] = value;
        body
    };
    let local = |host: &str| edit(&relay, "host", json!(host));
    let long_address = format!("a@{}.test", vec!["d".repeat(60); 6].join("."));
    assert!(long_address.len() > 320);
    // Hosts over TLS, so only `valid_host` judges them: 63-character labels, 253
    // and 254 characters in all.
    let starttls = edit(&relay, "tls", json!("starttls"));
    let dotted = |last: usize| [63, 63, 63, last].map(|n| "a".repeat(n)).join(".");
    assert_eq!(dotted(61).len(), 253);
    assert_eq!(dotted(62).len(), 254);
    let rows = [
        // The handler trims the TLS mode; the schema models the trimmed spelling.
        (
            "tls with surrounding spaces",
            edit(&relay, "tls", json!(" none ")),
            (true, true),
        ),
        // Plaintext is accepted for any spelling of a loopback address.
        (
            "loopback 127.255.255.255",
            local("127.255.255.255"),
            (true, true),
        ),
        ("loopback IPv6 0::1", local("0::1"), (true, true)),
        (
            "loopback IPv6 full spelling",
            local("0:0:0:0:0:0:0:1"),
            (true, true),
        ),
        ("loopback IPv6 in brackets", local("[::1]"), (true, true)),
        (
            "loopback IPv4 in brackets",
            local("[127.0.0.1]"),
            (true, true),
        ),
        ("loopback with upper case", local("LOCALHOST"), (true, true)),
        // An IPv4-tail spelling of ::1: accepted by the handler, not modelled.
        (
            "loopback IPv6 with an IPv4 tail",
            local("::0.0.0.1"),
            (true, false),
        ),
        // Any run of surrounding brackets is stripped (`trim_matches` in
        // `valid_host` and `is_loopback_host`): the handler accepts these with
        // plaintext, the schema's single balanced pair refuses them.
        (
            "loopback IPv6 in double brackets",
            local("[[::1]]"),
            (true, false),
        ),
        (
            "loopback IPv6 with an opening bracket only",
            local("[::1"),
            (true, false),
        ),
        (
            "loopback IPv6 with a closing bracket only",
            local("::1]"),
            (true, false),
        ),
        // `valid_host` allows 253 bytes (a 254th is refused) and requires a colon
        // string to parse as an address; the schema counts neither.
        (
            "host of 253 characters",
            edit(&starttls, "host", json!(dotted(61))),
            (true, true),
        ),
        (
            "host of 254 characters",
            edit(&starttls, "host", json!(dotted(62))),
            (false, true),
        ),
        (
            "colon string that is not an address",
            edit(&starttls, "host", json!("1:2")),
            (false, true),
        ),
        // Not loopback: a fourth octet out of range is a name, not an address.
        ("octet out of range", local("127.999.0.1"), (false, false)),
        ("remote IPv6", local("2001:db8::1"), (false, false)),
        // A float port is refused by the integer deserializer; the validator
        // treats 25.0 as an integer.
        (
            "port written as 25.0",
            edit(&relay, "port", json!(25.0)),
            (false, true),
        ),
        // Byte limits the schema cannot count.
        (
            "username of 200 two-byte characters",
            edit(&credentials, "username", json!("\u{e9}".repeat(200))),
            (false, true),
        ),
        (
            "username of 256 ASCII bytes",
            edit(&credentials, "username", json!("u".repeat(256))),
            (true, true),
        ),
        (
            "sender address over 320 bytes",
            edit(&relay, "from_address", json!(long_address)),
            (false, true),
        ),
        // An IP-literal domain: `email_valid` and lettre's `check_domain_ascii`
        // accept `[127.0.0.1]`, while the domain class excludes `[`, `]` and `:`.
        (
            "sender address with an IP-literal domain",
            edit(&relay, "from_address", json!("a@[127.0.0.1]")),
            (true, false),
        ),
    ];
    for (label, body, verdicts) in rows {
        pin(
            g,
            &admin,
            "PUT",
            email_template,
            &email,
            label,
            body,
            verdicts,
        )
        .await;
    }
    // Rule bodies: the name limit is in bytes, the distinct thresholds at most five.
    let rules = format!("{ALERTS}/rules");
    let rule_template = "/api/v1/platform/alerts/rules";
    let rule = full(
        json!({"name":"Pinned budget","kind":"budget_threshold","budget_layers":["type"],"thresholds":[80]}),
    );
    let rows = [
        (
            "rule name of 100 two-byte characters",
            edit(&rule, "name", json!("\u{e9}".repeat(100))),
            (false, true),
        ),
        (
            "six distinct thresholds",
            edit(&rule, "thresholds", json!([10, 20, 30, 40, 50, 60])),
            (false, true),
        ),
        (
            "five thresholds with repeats",
            edit(&rule, "thresholds", json!([10, 10, 20, 30, 40, 50, 50])),
            (true, true),
        ),
        // The same IP-literal domain as the sender address above.
        (
            "notify address with an IP-literal domain",
            edit(&rule, "notify_emails", json!(["a@[127.0.0.1]"])),
            (true, false),
        ),
    ];
    let since: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(pool)
        .await
        .unwrap();
    for (label, body, verdicts) in rows {
        pin(
            g,
            &admin,
            "POST",
            rule_template,
            &rules,
            label,
            body,
            verdicts,
        )
        .await;
    }
    forget_rules(pool, since).await;
}

/// The webhook PATCH rotation fields against a destination without and with a
/// rotation: a lone null is a no-op on the first and refused on the second,
/// which only the stored state can tell, so the schema accepts it in both.
pub(super) async fn pinned_webhooks(g: &Gateway, pool: &PgPool) {
    let admin = fresh_admin(pool, "pinned-hooks").await;
    let keep = live_webhooks(pool).await;
    let template = "/api/v1/platform/alerts/webhooks/{id}";
    let made = g
        .send(
            &admin,
            "POST",
            WEBHOOKS,
            json!({"label":"Pinned rotation","url":HOOK_URL,"current_secret_ref":"env:HOOK_CURRENT"}),
        )
        .await;
    let path = format!("{WEBHOOKS}/{}", id_of(&made));
    // No rotation stored.
    for (label, body) in [
        (
            "lone null reference, no rotation",
            json!({"previous_secret_ref":null}),
        ),
        (
            "lone null expiry, no rotation",
            json!({"previous_secret_until":null}),
        ),
        (
            "both null, no rotation",
            json!({"previous_secret_ref":null,"previous_secret_until":null}),
        ),
    ] {
        pin(
            g,
            &admin,
            "PATCH",
            template,
            &path,
            label,
            body,
            (true, true),
        )
        .await;
    }
    // A mismatched pair is refused whatever is stored, and the schema says so.
    let until = (chrono::Utc::now() + chrono::Duration::hours(1))
        .format("%Y-%m-%d %H:%M:%SZ")
        .to_string();
    for (label, body) in [
        (
            "null reference with an expiry",
            json!({"previous_secret_ref":null,"previous_secret_until":until}),
        ),
        (
            "reference with a null expiry",
            json!({"previous_secret_ref":"env:HOOK_PREVIOUS","previous_secret_until":null}),
        ),
    ] {
        pin(
            g,
            &admin,
            "PATCH",
            template,
            &path,
            label,
            body,
            (false, false),
        )
        .await;
    }
    // Rotation stored.
    let rotate = json!({"previous_secret_ref":"env:HOOK_PREVIOUS","previous_secret_until":until});
    let rotating = g.send(&admin, "PATCH", &path, rotate).await;
    assert!(rotating.status.is_success());
    for (label, body) in [
        (
            "lone null reference, rotation stored",
            json!({"previous_secret_ref":null}),
        ),
        (
            "lone null expiry, rotation stored",
            json!({"previous_secret_until":null}),
        ),
    ] {
        pin(
            g,
            &admin,
            "PATCH",
            template,
            &path,
            label,
            body,
            (false, true),
        )
        .await;
    }
    pin(
        g,
        &admin,
        "PATCH",
        template,
        &path,
        "both null, rotation stored",
        json!({"previous_secret_ref":null,"previous_secret_until":null}),
        (true, true),
    )
    .await;
    forget_webhooks(g, pool, &admin, &keep).await;
}

/// Controls for the logo harness pieces, so a vacuous gate cannot pass.
#[cfg(test)]
mod logo_controls {
    use super::*;

    #[test]
    fn verdict_maps_input_statuses_and_nothing_else() {
        for refused in [400, 413, 415] {
            assert!(!logo_verdict(StatusCode::from_u16(refused).unwrap()));
        }
        for accepted in [200, 201, 204] {
            assert!(logo_verdict(StatusCode::from_u16(accepted).unwrap()));
        }
    }
    #[test]
    fn verdict_fails_on_every_other_status() {
        for other in [401u16, 403, 404, 409, 429, 500, 503] {
            let caught =
                std::panic::catch_unwind(|| logo_verdict(StatusCode::from_u16(other).unwrap()));
            assert!(caught.is_err(), "status {other} must fail the vector");
        }
    }
    #[test]
    fn table_is_independent_and_covers_every_documented_rule() {
        let rows = logo_table();
        let accepted = rows.iter().filter(|r| r.3).count();
        assert!(accepted >= 9 && rows.len() - accepted >= 17);
        for label in [
            "svg",
            "gif",
            "png polyglot",
            "truncated jpeg",
            "png declared jpeg",
            "size at the limit",
            "size over the limit",
        ] {
            assert!(rows.iter().any(|r| r.0 == label), "{label}");
        }
        // The limit rows are exactly the documented size and one more.
        let size = |label: &str| rows.iter().find(|r| r.0 == label).unwrap().2.len();
        assert_eq!(size("size at the limit"), 524_288);
        assert_eq!(size("size over the limit"), 524_289);
    }
    /// The schema oracle's image branch must really inspect the bytes: with the
    /// flag it refuses what the documented rules refuse and accepts valid images,
    /// without the flag only the length bounds apply.
    #[test]
    fn binary_oracle_inspects_images_only_when_flagged() {
        let flagged = json!({"type":"string","format":"binary","minLength":1,"maxLength":524_288,"x-omg-validate-image":true});
        let plain = json!({"type":"string","format":"binary","minLength":1,"maxLength":524_288});
        let table = logo_table();
        let row = |label: &str| table.iter().find(|r| r.0 == label).unwrap();
        for label in ["png", "jpeg", "webp"] {
            let (_, media, bytes, _) = row(label);
            assert!(binary_accepts(&flagged, media, bytes), "{label}");
        }
        for label in [
            "svg",
            "gif",
            "png polyglot",
            "png declared jpeg",
            "truncated jpeg",
        ] {
            let (_, media, bytes, _) = row(label);
            assert!(!binary_accepts(&flagged, media, bytes), "{label}");
            // Unflagged, the same bytes are only length-checked.
            assert!(binary_accepts(&plain, media, bytes), "{label}");
        }
        let (_, media, bytes, _) = row("size over the limit");
        assert!(!binary_accepts(&flagged, media, bytes));
        assert!(!binary_accepts(&plain, media, bytes));
    }
}

//! Audit export to a SIEM through a signed HTTPS webhook (#106).
//!
//! - **Operator-controlled destination.** `GATEWAY_AUDIT_WEBHOOK_URL` and
//!   `GATEWAY_AUDIT_WEBHOOK_SECRET_ENV` (the name of the variable holding the
//!   signing secret) come from the environment; Platform Admins can see the
//!   status and send a test, never change or switch off the export.
//! - **What is exported.** The platform audit view's events (no workspace, or
//!   a team or project), queued by a trigger in the transaction that wrote
//!   them (migration 0044). Personal-workspace events never are.
//! - **Delivery.** One leased exporter posts the queue in order, up to
//!   [`BATCH`] events per request, and deletes them only after a 2xx, in a
//!   transaction fenced by its lease term. Failures back off exponentially
//!   (at most [`MAX_BACKOFF`]) and are retried forever. At-least-once: a
//!   receiver dedups on each event's `id`.
//! - **Signature.** `X-OMG-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of
//!   "<t>.<body>">`; `X-OMG-Delivery` is a fresh id per request.

use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::store::Store;

pub const URL_ENV: &str = "GATEWAY_AUDIT_WEBHOOK_URL";
pub const SECRET_ENV_ENV: &str = "GATEWAY_AUDIT_WEBHOOK_SECRET_ENV";
/// The versioned event format.
pub const SCHEMA: &str = "omg.audit.v1";
/// Events per request.
pub const BATCH: i64 = 100;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);
const MIN_SECRET_BYTES: usize = 32;

/// A configured destination. The secret is never logged or returned.
pub struct Exporter {
    url: reqwest::Url,
    secret: zeroize::Zeroizing<Vec<u8>>,
    client: reqwest::Client,
}

impl std::fmt::Debug for Exporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exporter")
            .field("host", &self.host())
            .finish_non_exhaustive()
    }
}

/// This replica's export configuration. Only an explicit `off` turns export
/// off for the installation; a replica without any setting (a rolling deploy,
/// a debug pod) leaves it as it is and doesn't deliver.
pub enum Setting {
    Unset,
    Off,
    On(Exporter),
}

impl Setting {
    pub fn from_env() -> anyhow::Result<Self> {
        let development = std::env::var("GATEWAY_ENV").as_deref() == Ok("development");
        Self::from_lookup(|name| std::env::var(name).ok(), development)
    }

    pub fn from_lookup(
        get: impl Fn(&str) -> Option<String>,
        development: bool,
    ) -> anyhow::Result<Self> {
        let url = get(URL_ENV).filter(|v| !v.trim().is_empty());
        let secret_name = get(SECRET_ENV_ENV).filter(|v| !v.trim().is_empty());
        match url.as_deref().map(str::trim) {
            None => {
                anyhow::ensure!(
                    secret_name.is_none(),
                    "{SECRET_ENV_ENV} is set without {URL_ENV}"
                );
                return Ok(Self::Unset);
            }
            Some("off") => return Ok(Self::Off),
            Some(_) => {}
        }
        let url = url.unwrap_or_default();
        let name =
            secret_name.ok_or_else(|| anyhow::anyhow!("{URL_ENV} needs {SECRET_ENV_ENV}"))?;
        anyhow::ensure!(
            !name.is_empty()
                && name.len() <= 128
                && name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
            "{SECRET_ENV_ENV} must name an upper-case environment variable"
        );
        // A Platform Admin chooses where provider credentials and the SMTP
        // password go: if either allowlist could read the signing secret, an
        // admin could learn it and forge deliveries (#140's rule, here too).
        let listed = |var: &str| {
            get(var).is_some_and(|list| list.split(',').map(str::trim).any(|n| n == name))
        };
        anyhow::ensure!(
            !listed("GATEWAY_SECRET_ENV_ALLOWLIST") && !listed(crate::email::SMTP_ALLOWLIST_VAR),
            "{SECRET_ENV_ENV} names {name}, which a provider or SMTP allowlist can also read; use a variable only the audit export reads"
        );
        anyhow::ensure!(
            !crate::startup::SECRET_NAMES.contains(&name.as_str())
                && !name.starts_with("AWS_")
                && !name.starts_with("GATEWAY_"),
            "{SECRET_ENV_ENV} names a reserved variable ({name})"
        );
        let secret = get(&name)
            .ok_or_else(|| anyhow::anyhow!("{SECRET_ENV_ENV} names a variable that isn't set"))?;
        Ok(
            match Exporter::new(&url, secret.into_bytes(), development)? {
                Some(exporter) => Self::On(exporter),
                None => Self::Unset,
            },
        )
    }

    pub fn exporter(self) -> Option<Exporter> {
        match self {
            Self::On(exporter) => Some(exporter),
            _ => None,
        }
    }
}

impl Exporter {
    /// HTTPS only; plain HTTP only to loopback in development (and tests).
    pub fn new(url: &str, secret: Vec<u8>, development: bool) -> anyhow::Result<Option<Self>> {
        let secret = zeroize::Zeroizing::new(secret);
        anyhow::ensure!(
            secret.len() >= MIN_SECRET_BYTES,
            "the audit webhook secret must be at least {MIN_SECRET_BYTES} bytes"
        );
        let url =
            reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!("{URL_ENV} is not a URL"))?;
        let loopback = url.host_str().is_some_and(|h| {
            h == "localhost"
                || h.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        anyhow::ensure!(
            url.scheme() == "https" || (development && loopback && url.scheme() == "http"),
            "{URL_ENV} must be https:// (http only to loopback in development)"
        );
        anyhow::ensure!(
            url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
            "{URL_ENV} must not contain credentials or a fragment"
        );
        let policy = crate::egress::policy().permit_loopback(development);
        crate::egress::check_url(&policy, crate::egress::Purpose::Audit, &url)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(crate::egress::resolver_with(
                crate::egress::Purpose::Audit,
                policy,
            ))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| anyhow::anyhow!("cannot build the audit export client"))?;
        Ok(Some(Self {
            url,
            secret,
            client,
        }))
    }

    /// The destination host, for status (never the path or query).
    pub fn host(&self) -> &str {
        self.url.host_str().unwrap_or_default()
    }

    /// POST one signed body. `Ok(status)` for any HTTP answer; `Err(category)`
    /// when there was none.
    async fn post(&self, body: Vec<u8>) -> Result<u16, &'static str> {
        let t = chrono::Utc::now().timestamp();
        let response = self
            .client
            .post(self.url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::USER_AGENT,
                concat!("open-model-gateway/", env!("CARGO_PKG_VERSION")),
            )
            .header("x-omg-delivery", Uuid::new_v4().to_string())
            .header("x-omg-signature", sign(&self.secret, t, &body))
            .body(body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    "timeout"
                } else {
                    "connection"
                }
            })?;
        Ok(response.status().as_u16())
    }
}

/// HMAC-SHA256 (RFC 2104) over SHA-256, so no new dependency is needed.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// The `X-OMG-Signature` value: `t=<unix>,v1=<hex HMAC of "<t>.<body>">`.
pub fn sign(secret: &[u8], t: i64, body: &[u8]) -> String {
    let mut message = format!("{t}.").into_bytes();
    message.extend_from_slice(body);
    format!("t={t},v1={}", hex::encode(hmac_sha256(secret, &message)))
}

/// Apply this replica's setting at startup. `On` turns export on (and
/// records the destination host); `Off` turns it off; `Unset` changes
/// nothing. Each real transition is itself an audit event: `disabled` is
/// queued while export is still on and `enabled` once it is, so the SIEM sees
/// where a gap begins and ends. Turning it on starts the queue from now.
pub async fn sync_state(store: &Store, setting: &Setting) -> Result<(), sqlx::Error> {
    let mut tx = crate::db::begin(&store.pool).await?;
    let marker = |action: &'static str| {
        sqlx::query("INSERT INTO audit_events(id,action,resource_type,metadata) VALUES($1,$2,'installation','{}')")
            .bind(Uuid::now_v7())
            .bind(action)
    };
    match setting {
        Setting::Unset => {}
        Setting::On(exporter) => {
            let turned_on = sqlx::query("UPDATE audit_export_state SET enabled=true,enabled_at=now(),destination_host=$1 WHERE NOT enabled")
                .bind(exporter.host())
                .execute(&mut *tx)
                .await?
                .rows_affected()
                > 0;
            if turned_on {
                marker("audit_export.enabled").execute(&mut *tx).await?;
            } else {
                sqlx::query("UPDATE audit_export_state SET destination_host=$1")
                    .bind(exporter.host())
                    .execute(&mut *tx)
                    .await?;
            }
        }
        Setting::Off => {
            let on: bool = sqlx::query_scalar("SELECT enabled FROM audit_export_state FOR UPDATE")
                .fetch_one(&mut *tx)
                .await?;
            if on {
                marker("audit_export.disabled").execute(&mut *tx).await?;
                sqlx::query("UPDATE audit_export_state SET enabled=false,enabled_at=NULL,destination_host=NULL")
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    tx.commit().await
}

/// What one delivery run did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing queued, or backing off.
    Idle,
    /// Accepted by the receiver (this many events).
    Delivered(usize),
    /// Not accepted; retried after the backoff.
    Failed(&'static str),
}

const EVENT: &str = "jsonb_build_object('id',a.id,'occurred_at',a.created_at,'action',a.action,'resource',jsonb_build_object('type',a.resource_type,'id',a.resource_id),'actor',CASE WHEN a.actor_user_id IS NULL THEN NULL ELSE jsonb_build_object('id',a.actor_user_id,'label',(SELECT coalesce(nullif(btrim(u.display_name),''),u.email,'Retained identity') FROM users u WHERE u.id=a.actor_user_id)) END,'workspace',CASE WHEN w.id IS NULL THEN NULL ELSE jsonb_build_object('id',w.id,'name',w.name,'kind',w.kind) END,'metadata',a.metadata)";

fn backoff(attempts: i32) -> f64 {
    2f64.powi(attempts.clamp(0, 16))
        .min(MAX_BACKOFF.as_secs_f64())
}

/// Deliver the oldest queued events once (the lease holder's loop calls it).
pub async fn deliver_once(
    store: &Store,
    exporter: &Exporter,
    fence: Option<&crate::leases::Fence>,
) -> Result<Outcome, sqlx::Error> {
    // 1. Read the batch (in order) unless backing off.
    let mut tx = crate::db::begin(&store.pool).await?;
    crate::leases::fence(&mut tx, fence).await?;
    let (due, attempts, installation): (bool, i32, Uuid) = sqlx::query_as("SELECT s.next_attempt_at<=now(),s.attempts,(SELECT id FROM installation) FROM audit_export_state s")
        .fetch_one(&mut *tx)
        .await?;
    if !due {
        tx.commit().await?;
        return Ok(Outcome::Idle);
    }
    let rows: Vec<(Uuid, Option<Value>)> = sqlx::query_as(&format!("SELECT q.event_id,CASE WHEN a.id IS NULL OR NOT (a.workspace_id IS NULL OR w.kind IN ('team','project')) THEN NULL ELSE {EVENT} END FROM audit_export_queue q LEFT JOIN audit_events a ON a.id=q.event_id AND a.created_at=q.event_created_at LEFT JOIN workspaces w ON w.id=a.workspace_id ORDER BY q.event_created_at,q.event_id LIMIT $1"))
        .bind(BATCH)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    if rows.is_empty() {
        return Ok(Outcome::Idle);
    }
    let ids: Vec<Uuid> = rows.iter().map(|(id, _)| *id).collect();
    let events: Vec<Value> = rows.into_iter().filter_map(|(_, event)| event).collect();
    let missing = ids.len() - events.len();
    if missing > 0 {
        // A partition archived before its events were exported, or a row that
        // isn't platform-visible (re-checked above): counted, logged, skipped.
        crate::metrics::METRICS.audit_export_skipped(missing as u64);
        tracing::warn!(
            missing,
            "queued audit events no longer in the database; skipped"
        );
    }
    // 2. Deliver, outside any transaction.
    let result = if events.is_empty() {
        Ok(200)
    } else {
        let body = serde_json::to_vec(&json!({
            "schema": SCHEMA,
            "installation_id": installation,
            "sent_at": chrono::Utc::now(),
            "events": events,
        }))
        .map_err(|_| sqlx::Error::Protocol("audit export body".into()))?;
        exporter.post(body).await
    };
    // 3. Record the outcome, fenced: a holder whose term ended commits nothing.
    let mut tx = crate::db::begin(&store.pool).await?;
    crate::leases::fence(&mut tx, fence).await?;
    let outcome = match result {
        Ok(status) if (200..300).contains(&status) => {
            sqlx::query("DELETE FROM audit_export_queue WHERE event_id=ANY($1)")
                .bind(&ids)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE audit_export_state SET attempts=0,next_attempt_at='-infinity',last_success_at=now(),last_error=NULL,delivered_total=delivered_total+$1")
                .bind(i64::try_from(events.len()).unwrap_or(i64::MAX))
                .execute(&mut *tx)
                .await?;
            Outcome::Delivered(events.len())
        }
        failure => {
            let category = match failure {
                Ok(status) => status_category(status),
                Err(category) => category,
            };
            sqlx::query("UPDATE audit_export_state SET attempts=attempts+1,next_attempt_at=now()+make_interval(secs=>$1),last_error=$2,last_error_at=now()")
                .bind(backoff(attempts))
                .bind(category)
                .execute(&mut *tx)
                .await?;
            Outcome::Failed(category)
        }
    };
    tx.commit().await?;
    crate::metrics::METRICS.audit_export_delivery(match outcome {
        Outcome::Delivered(_) => "delivered",
        _ => "failed",
    });
    Ok(outcome)
}

fn status_category(status: u16) -> &'static str {
    match status {
        400..=499 => "http_4xx",
        500..=599 => "http_5xx",
        _ => "http_other",
    }
}

/// A test delivery: a signed body with `"test": true` and no events. Doesn't
/// touch the queue or the status.
pub async fn send_test(store: &Store, exporter: &Exporter) -> Result<(), &'static str> {
    let installation: Uuid = sqlx::query_scalar("SELECT id FROM installation")
        .fetch_one(&store.pool)
        .await
        .map_err(|_| "database")?;
    let body = serde_json::to_vec(&json!({
        "schema": SCHEMA,
        "installation_id": installation,
        "sent_at": chrono::Utc::now(),
        "test": true,
        "events": [],
    }))
    .map_err(|_| "body")?;
    match exporter.post(body).await? {
        200..=299 => Ok(()),
        status => Err(status_category(status)),
    }
}

/// Status for Admin › Settings › Audit export. Never the URL or secret.
pub async fn status(store: &Store) -> Result<Value, sqlx::Error> {
    let mut v: Value = sqlx::query_scalar("SELECT jsonb_build_object('enabled',s.enabled,'enabled_at',s.enabled_at,'destination_host',s.destination_host,'queued',(SELECT count(*) FROM audit_export_queue),'oldest_queued_at',(SELECT min(enqueued_at) FROM audit_export_queue),'last_success_at',s.last_success_at,'last_error',s.last_error,'last_error_at',s.last_error_at,'retrying',s.attempts>0,'next_attempt_at',CASE WHEN s.attempts>0 THEN s.next_attempt_at END,'delivered_total',s.delivered_total::text) FROM audit_export_state s")
        .fetch_one(&store.pool)
        .await?;
    v["schema"] = json!(SCHEMA);
    Ok(v)
}

/// The exporter loop for `serve`: only the `audit_export` lease holder
/// delivers; a full batch goes again at once, otherwise every 2 seconds.
pub fn start(store: Store, exporter: std::sync::Arc<Exporter>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            // The backlog gauges come from the delivering replica only.
            if store
                .leases()
                .is_none_or(|leases| leases.held(crate::leases::Lease::AuditExport).is_some())
            {
                record_backlog(&store).await;
            }
            for _ in 0..20 {
                let fence = match store.leases() {
                    Some(leases) => match leases.held(crate::leases::Lease::AuditExport) {
                        Some(fence) => Some(fence),
                        None => break,
                    },
                    None => None,
                };
                match tokio::time::timeout(
                    REQUEST_TIMEOUT * 3,
                    deliver_once(&store, &exporter, fence.as_ref()),
                )
                .await
                {
                    Ok(Ok(Outcome::Delivered(n))) if i64::try_from(n).unwrap_or(0) == BATCH => {
                        continue;
                    }
                    Ok(Ok(Outcome::Failed(category))) => {
                        tracing::warn!(
                            category,
                            host = exporter.host(),
                            "audit export delivery failed; retrying with backoff"
                        );
                        break;
                    }
                    Ok(Err(error)) if crate::leases::is_fenced(&error) => break,
                    Ok(Err(_)) | Err(_) => {
                        tracing::warn!("audit export run incomplete; retrying");
                        break;
                    }
                    Ok(Ok(_)) => break,
                }
            }
        }
    })
}

async fn record_backlog(store: &Store) {
    if let Ok((queued, oldest)) = sqlx::query_as::<_, (i64, Option<f64>)>("SELECT count(*),extract(epoch FROM now()-min(enqueued_at))::float8 FROM audit_export_queue")
        .fetch_one(&store.pool)
        .await
    {
        crate::metrics::METRICS.audit_export_backlog(queued, oldest.unwrap_or(0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test cases 1, 2 and 6 (a key longer than the block).
        assert_eq!(
            hex::encode(hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex::encode(hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
        assert_eq!(
            sign(b"secret", 1_700_000_000, b"{}"),
            format!(
                "t=1700000000,v1={}",
                hex::encode(hmac_sha256(b"secret", b"1700000000.{}"))
            )
        );
    }

    #[test]
    fn configuration_is_operator_only_and_strict() {
        let secret = "s".repeat(32);
        let env = |pairs: Vec<(&'static str, String)>| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.clone())
            }
        };
        assert!(
            matches!(
                Setting::from_lookup(env(vec![]), false).unwrap(),
                Setting::Unset
            ),
            "unset: no export"
        );
        let ok = Setting::from_lookup(
            env(vec![
                (URL_ENV, "https://siem.example.org/ingest?token=x".into()),
                (SECRET_ENV_ENV, "AUDIT_SECRET".into()),
                ("AUDIT_SECRET", secret.clone()),
            ]),
            false,
        )
        .unwrap()
        .exporter()
        .unwrap();
        assert_eq!(ok.host(), "siem.example.org");
        assert!(
            !format!("{ok:?}").contains(&secret),
            "the secret is never printed"
        );
        for (pairs, why) in [
            (
                vec![(URL_ENV, "https://siem.example.org".into())],
                "no secret",
            ),
            (
                vec![(SECRET_ENV_ENV, "AUDIT_SECRET".into())],
                "secret without URL",
            ),
            (
                vec![
                    (URL_ENV, "https://siem.example.org".into()),
                    (SECRET_ENV_ENV, "AUDIT_SECRET".into()),
                ],
                "secret variable unset",
            ),
            (
                vec![
                    (URL_ENV, "https://siem.example.org".into()),
                    (SECRET_ENV_ENV, "AUDIT_SECRET".into()),
                    ("AUDIT_SECRET", "short".into()),
                ],
                "short secret",
            ),
            (
                vec![
                    (URL_ENV, "http://siem.example.org".into()),
                    (SECRET_ENV_ENV, "AUDIT_SECRET".into()),
                    ("AUDIT_SECRET", secret.clone()),
                ],
                "plain http",
            ),
            (
                vec![
                    (URL_ENV, "https://user:pw@siem.example.org".into()),
                    (SECRET_ENV_ENV, "AUDIT_SECRET".into()),
                    ("AUDIT_SECRET", secret.clone()),
                ],
                "credentials",
            ),
            (
                vec![
                    (URL_ENV, "https://169.254.169.254/".into()),
                    (SECRET_ENV_ENV, "AUDIT_SECRET".into()),
                    ("AUDIT_SECRET", secret.clone()),
                ],
                "metadata address",
            ),
            (
                vec![
                    (URL_ENV, "https://siem.example.org".into()),
                    (SECRET_ENV_ENV, "lower".into()),
                    ("lower", secret.clone()),
                ],
                "bad variable name",
            ),
        ] {
            assert!(Setting::from_lookup(env(pairs), false).is_err(), "{why}");
        }
        assert!(
            Exporter::new("http://127.0.0.1:9/", secret.into_bytes(), true)
                .unwrap()
                .is_some(),
            "development loopback"
        );

        // #106 review: the signing secret's variable can't be on an allowlist
        // a Platform Admin can point somewhere, nor a reserved name.
        let base = |name: &'static str, extra: Vec<(&'static str, String)>| {
            let mut pairs = vec![
                (URL_ENV, "https://siem.example.org".to_owned()),
                (SECRET_ENV_ENV, name.to_owned()),
                (name, "s".repeat(32)),
            ];
            pairs.extend(extra);
            pairs
        };
        for (pairs, why) in [
            (
                base(
                    "AUDIT_SECRET",
                    vec![(
                        "GATEWAY_SECRET_ENV_ALLOWLIST",
                        "OPENAI_API_KEY, AUDIT_SECRET".into(),
                    )],
                ),
                "provider allowlist",
            ),
            (
                base(
                    "AUDIT_SECRET",
                    vec![(crate::email::SMTP_ALLOWLIST_VAR, "AUDIT_SECRET".into())],
                ),
                "SMTP allowlist",
            ),
            (base("OPENAI_API_KEY", vec![]), "reserved key"),
            (base("GATEWAY_SCIM_TOKEN", vec![]), "GATEWAY_ prefix"),
            (base("AWS_SECRET_ACCESS_KEY", vec![]), "AWS_ prefix"),
        ] {
            let error = Setting::from_lookup(env(pairs), false)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(error.contains(SECRET_ENV_ENV), "{why}: {error}");
        }
        assert!(
            matches!(
                Setting::from_lookup(env(vec![(URL_ENV, " off ".into())]), false).unwrap(),
                Setting::Off
            ),
            "explicit off"
        );
    }

    #[test]
    fn backoff_doubles_up_to_five_minutes() {
        assert_eq!(backoff(0), 1.0);
        assert_eq!(backoff(3), 8.0);
        assert_eq!(backoff(8), 256.0);
        assert_eq!(backoff(9), 300.0);
        assert_eq!(backoff(1000), 300.0);
    }
}

#[cfg(all(test, feature = "integration-tests"))]
mod database {
    use super::*;
    use axum::{Router, http::HeaderMap, routing::post};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU16, Ordering},
    };

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    type Requests = Arc<Mutex<Vec<(HeaderMap, Vec<u8>)>>>;

    /// A receiver that records every request and answers with `status`.
    struct Receiver {
        url: String,
        seen: Requests,
        status: Arc<AtomicU16>,
    }
    async fn receiver() -> Receiver {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(AtomicU16::new(200));
        let (log, answer) = (seen.clone(), status.clone());
        let app = Router::new().route(
            "/ingest",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let (log, answer) = (log.clone(), answer.clone());
                async move {
                    log.lock().unwrap().push((headers, body.to_vec()));
                    axum::http::StatusCode::from_u16(answer.load(Ordering::SeqCst)).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/ingest", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Receiver { url, seen, status }
    }
    fn exporter(url: &str) -> Exporter {
        Exporter::new(url, SECRET.to_vec(), true).unwrap().unwrap()
    }
    async fn workspace(pool: &sqlx::PgPool, kind: &str) -> Uuid {
        let id = Uuid::new_v4();
        let owner = if kind == "personal" {
            let user = Uuid::new_v4();
            sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
                .bind(user)
                .bind(format!("{user}@example.test"))
                .execute(pool)
                .await
                .unwrap();
            Some(user)
        } else {
            None
        };
        sqlx::query("INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,$2,$3,$4)")
            .bind(id)
            .bind(format!("{kind} space"))
            .bind(kind)
            .bind(owner)
            .execute(pool)
            .await
            .unwrap();
        id
    }
    async fn event(pool: &sqlx::PgPool, workspace: Option<Uuid>, action: &str) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO audit_events(id,workspace_id,action,resource_type,metadata) VALUES($1,$2,$3,'test','{\"count\":1}')")
            .bind(id).bind(workspace).bind(action).execute(pool).await.unwrap();
        id
    }
    async fn queued(pool: &sqlx::PgPool) -> Vec<Uuid> {
        sqlx::query_scalar(
            "SELECT event_id FROM audit_export_queue ORDER BY event_created_at,event_id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }
    /// Turn export on and drop its own `audit_export.enabled` marker, so a
    /// test sees only the events it writes.
    async fn enable(pool: &sqlx::PgPool, store: &Store, receiver: &Receiver) {
        sync_state(store, &Setting::On(exporter(&receiver.url)))
            .await
            .unwrap();
        sqlx::query("DELETE FROM audit_export_queue")
            .execute(pool)
            .await
            .unwrap();
    }
    fn delivered(receiver: &Receiver) -> Vec<Value> {
        receiver
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, body)| serde_json::from_slice::<Value>(body).unwrap())
            .collect()
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn only_shared_events_are_queued_and_only_while_enabled(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let (team, project, personal) = (
            workspace(&pool, "team").await,
            workspace(&pool, "project").await,
            workspace(&pool, "personal").await,
        );
        event(&pool, None, "before.enabled").await;
        assert!(
            queued(&pool).await.is_empty(),
            "off by default: no backfill later either"
        );
        let receiver = receiver().await;
        sync_state(&store, &Setting::On(exporter(&receiver.url)))
            .await
            .unwrap();
        let marker: Uuid =
            sqlx::query_scalar("SELECT id FROM audit_events WHERE action='audit_export.enabled'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let global = event(&pool, None, "installation.updated").await;
        let t = event(&pool, Some(team), "workspace.updated").await;
        let p = event(&pool, Some(project), "workspace.updated").await;
        event(&pool, Some(personal), "key.created").await;
        assert_eq!(
            queued(&pool).await,
            vec![marker, global, t, p],
            "the enabled marker, then never personal-workspace events"
        );
        // A replica without any setting changes nothing.
        sync_state(&store, &Setting::Unset).await.unwrap();
        event(&pool, None, "unset.replica").await;
        assert_eq!(queued(&pool).await.len(), 5, "still exporting");
        // Only an explicit `off` stops it, after queueing its own marker.
        sync_state(&store, &Setting::Off).await.unwrap();
        event(&pool, None, "after.disabled").await;
        let actions: Vec<String> = sqlx::query_scalar("SELECT a.action FROM audit_export_queue q JOIN audit_events a ON a.id=q.event_id ORDER BY q.event_created_at,q.event_id").fetch_all(&pool).await.unwrap();
        assert_eq!(
            actions.last().map(String::as_str),
            Some("audit_export.disabled")
        );
        assert_eq!(
            actions.len(),
            6,
            "disabling stops queueing; queued events wait"
        );
        // Repeating a setting is not another transition.
        sync_state(&store, &Setting::Off).await.unwrap();
        let markers: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE action LIKE 'audit_export.%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(markers, 2);
        let state: (bool, Option<String>) =
            sqlx::query_as("SELECT enabled,destination_host FROM audit_export_state")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, (false, None));
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn batches_are_delivered_in_order_signed_and_then_dequeued(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = receiver().await;
        let exp = exporter(&receiver.url);
        enable(&pool, &store, &receiver).await;
        let team = workspace(&pool, "team").await;
        let mut ids = Vec::new();
        for i in 0..(BATCH + 20) {
            ids.push(
                event(
                    &pool,
                    if i % 2 == 0 { Some(team) } else { None },
                    "test.event",
                )
                .await,
            );
        }
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(100)
        );
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(20)
        );
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Idle
        );
        assert!(queued(&pool).await.is_empty());
        let bodies = delivered(&receiver);
        let sent: Vec<Uuid> = bodies
            .iter()
            .flat_map(|b| {
                b["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["id"].as_str().unwrap().parse().unwrap())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(sent, ids, "every event once, in order");
        assert_eq!(bodies[0]["schema"], SCHEMA);
        let first = &bodies[0]["events"][0];
        assert_eq!(first["workspace"]["kind"], "team");
        assert_eq!(first["metadata"], json!({"count":1}));
        assert_eq!(bodies[0]["events"][1]["workspace"], Value::Null);
        // The signature verifies over "<t>.<body>" with the shared secret.
        for (headers, body) in receiver.seen.lock().unwrap().iter() {
            let signature = headers["x-omg-signature"].to_str().unwrap();
            let t: i64 = signature
                .strip_prefix("t=")
                .unwrap()
                .split(',')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(signature, sign(SECRET, t, body));
            assert!(headers.contains_key("x-omg-delivery"));
        }
        let total: i64 = sqlx::query_scalar("SELECT delivered_total FROM audit_export_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, BATCH + 20);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn an_outage_backs_off_loses_nothing_and_recovers(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = receiver().await;
        let exp = exporter(&receiver.url);
        enable(&pool, &store, &receiver).await;
        let ids = [event(&pool, None, "a").await, event(&pool, None, "b").await];
        receiver.status.store(503, Ordering::SeqCst);
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Failed("http_5xx")
        );
        assert_eq!(queued(&pool).await, ids.to_vec(), "nothing lost");
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Idle,
            "backing off"
        );
        let v = status(&store).await.unwrap();
        assert_eq!(
            (
                v["retrying"].clone(),
                v["last_error"].clone(),
                v["queued"].clone()
            ),
            (json!(true), json!("http_5xx"), json!(2))
        );
        // A refused connection is a failure too, and the backoff grows.
        let down = exporter("http://127.0.0.1:9/ingest");
        sqlx::query("UPDATE audit_export_state SET next_attempt_at=now()-interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            deliver_once(&store, &down, None).await.unwrap(),
            Outcome::Failed("connection")
        );
        let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM audit_export_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(attempts, 2);
        // Receiver back: the same events, once more, then a clean state.
        receiver.status.store(200, Ordering::SeqCst);
        sqlx::query("UPDATE audit_export_state SET next_attempt_at=now()-interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(2)
        );
        assert!(queued(&pool).await.is_empty());
        let v = status(&store).await.unwrap();
        assert_eq!(
            (v["retrying"].clone(), v["last_error"].clone()),
            (json!(false), Value::Null)
        );
        // Both attempts that reached the receiver carried the same event ids (dedup).
        let bodies = delivered(&receiver);
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["events"], bodies[1]["events"]);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn a_stale_lease_term_commits_nothing(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = receiver().await;
        let exp = exporter(&receiver.url);
        enable(&pool, &store, &receiver).await;
        let id = event(&pool, None, "a").await;
        let stale = crate::leases::Fence {
            lease: crate::leases::Lease::AuditExport,
            holder: Uuid::new_v4(),
            epoch: 99,
        };
        let error = deliver_once(&store, &exp, Some(&stale)).await.unwrap_err();
        assert!(crate::leases::is_fenced(&error), "{error}");
        assert_eq!(queued(&pool).await, vec![id]);
        assert!(
            delivered(&receiver).is_empty(),
            "nothing sent without the lease"
        );
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn a_queued_event_no_longer_in_the_database_is_skipped(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = receiver().await;
        let exp = exporter(&receiver.url);
        enable(&pool, &store, &receiver).await;
        let kept = event(&pool, None, "kept").await;
        sqlx::query("INSERT INTO audit_export_queue(event_id,event_created_at) VALUES($1,now()-interval '400 days')").bind(Uuid::new_v4()).execute(&pool).await.unwrap();
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(1)
        );
        assert!(queued(&pool).await.is_empty());
        assert_eq!(delivered(&receiver)[0]["events"][0]["id"], kept.to_string());
    }
}

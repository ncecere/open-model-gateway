//! Signed, bounded webhook delivery. No connection spans HTTP.
use crate::store::Store;
use chrono::{DateTime, Utc};
use sqlx::{Postgres, Transaction};
use std::time::Duration;
use uuid::Uuid;

use super::outbox::{BACKOFF_SECONDS as BACKOFF, LEASE_SECONDS};
const REQUEST: Duration = Duration::from_secs(10);
const STEP: Duration = Duration::from_secs(40); // claim + HTTP + completion, each bounded

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Credential,
    Connection,
    Tls,
    Rejected,
    Timeout,
    HostNotAllowed,
    DestinationPolicy,
    TransientStatus,
}
impl Failure {
    fn retryable(self) -> bool {
        matches!(
            self,
            Self::Connection | Self::Timeout | Self::TransientStatus
        )
    }
    fn category(self) -> &'static str {
        match self {
            Self::Credential => "credential",
            Self::Connection => "connection",
            Self::Tls => "tls",
            Self::Rejected | Self::TransientStatus => "rejected",
            Self::Timeout => "timeout",
            Self::HostNotAllowed => "host_not_allowed",
            Self::DestinationPolicy => "destination_policy",
        }
    }
}
fn http_result(status: u16) -> Result<(), Failure> {
    match status {
        200..=299 => Ok(()),
        408 | 429 | 500..=599 => Err(Failure::TransientStatus),
        _ => Err(Failure::Rejected),
    }
}
fn signature(current: &[u8], previous: Option<&[u8]>, t: i64, body: &[u8]) -> String {
    let mut header = crate::audit_export::sign(current, t, body);
    if let Some(previous) = previous {
        let second = crate::audit_export::sign(previous, t, body);
        header.push_str(
            second
                .strip_prefix(&format!("t={t}"))
                .expect("signature format"),
        );
    }
    header
}
fn request_failure(error: &reqwest::Error) -> Failure {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    let mut connection = false;
    while let Some(e) = source {
        if e.is::<crate::egress::PolicyRefusal>() {
            return Failure::HostNotAllowed;
        }
        if e.is::<rustls::Error>() {
            return Failure::Tls;
        }
        if let Some(e) = e.downcast_ref::<hyper::Error>() {
            connection |= e.is_closed() || e.is_incomplete_message() || e.is_canceled();
        }
        if let Some(e) = e.downcast_ref::<std::io::Error>() {
            connection |= matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::NotConnected
            );
        }
        // io::Error::source can skip its boxed custom error (including a
        // rustls verification/handshake error). Inspect that owner explicitly.
        source = e
            .downcast_ref::<std::io::Error>()
            .and_then(|e| {
                e.get_ref()
                    .map(|inner| inner as &(dyn std::error::Error + 'static))
            })
            .or_else(|| e.source());
    }
    if error.is_timeout() {
        Failure::Timeout
    } else if error.is_connect() || connection {
        Failure::Connection
    } else {
        Failure::Rejected
    }
}

// Never Debug/Serialize: contains management-only URL/reference metadata.
#[derive(sqlx::FromRow)]
struct Destination {
    enabled: bool,
    deleted_at: Option<DateTime<Utc>>,
    url: Option<String>,
    current_secret_ref: Option<String>,
    previous_secret_ref: Option<String>,
    previous_secret_until: Option<DateTime<Utc>>,
    enable_generation: i64,
}
fn active_previous(d: &Destination, now: DateTime<Utc>) -> Option<&str> {
    d.previous_secret_until
        .filter(|until| *until > now)
        .and(d.previous_secret_ref.as_deref())
}
#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    destination_generation: i64,
    payload: Vec<u8>,
    test: bool,
    attempt_count: i32,
    eligible: bool,
    expired: bool,
}
struct Claim {
    row: Row,
    destination: Destination,
    owner: Uuid,
    token: Uuid,
    policy_refused: bool,
    until: tokio::time::Instant,
}

async fn terminal_audit(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    state: &str,
    error: Option<&str>,
) -> sqlx::Result<()> {
    let action = match state {
        "failed" => "alert_webhook.failed",
        "exhausted" => "alert_webhook.exhausted",
        _ => return Ok(()),
    };
    sqlx::query("INSERT INTO audit_events(id,action,resource_type,resource_id,metadata) VALUES($1,$2,'alert_webhook_delivery',$3,jsonb_build_object('reason',$4::text))")
        .bind(Uuid::now_v7()).bind(action).bind(id).bind(error).execute(&mut **tx).await?;
    Ok(())
}

// Lock destination BEFORE queue row, matching management. SHARE serializes
// NO KEY UPDATE without blocking enqueue's FK KEY SHARE. Locks never span I/O.
async fn claim(store: &Store, owner: Uuid) -> sqlx::Result<Option<Option<Claim>>> {
    let mut tx =
        crate::db::begin_with_setup(&store.pool, "SET LOCAL statement_timeout='10s'").await?;
    let candidates: Vec<(Uuid, Uuid)> = sqlx::query_as("SELECT id,destination_id FROM alert_webhook_outbox WHERE (state='waiting' AND next_attempt_at<=clock_timestamp()) OR (state='leased' AND lease_expires_at<=clock_timestamp()) ORDER BY next_attempt_at,id LIMIT 32")
        .fetch_all(&mut *tx).await?;
    for (id, destination) in candidates {
        let Some(d): Option<Destination> = sqlx::query_as("SELECT enabled,deleted_at,url,current_secret_ref,previous_secret_ref,previous_secret_until,enable_generation FROM alert_webhook_destinations d WHERE id=$1 FOR SHARE OF d SKIP LOCKED")
            .bind(destination).fetch_optional(&mut *tx).await? else { continue; };
        let Some(row): Option<Row> = sqlx::query_as("SELECT q.id,q.destination_generation,q.payload,q.test,q.attempt_count,(q.test OR alert_webhook_payload(q.id,q.event_id,q.transition) IS NOT NULL) eligible,(q.created_at+interval '24 hours'<=clock_timestamp()+interval '15 seconds') expired FROM alert_webhook_outbox q WHERE id=$1 AND ((state='waiting' AND next_attempt_at<=clock_timestamp()) OR (state='leased' AND lease_expires_at<=clock_timestamp())) FOR UPDATE OF q SKIP LOCKED")
            .bind(id).fetch_optional(&mut *tx).await? else { continue; };
        let ineligible = !d.enabled
            || d.deleted_at.is_some()
            || d.url.is_none()
            || d.enable_generation != row.destination_generation
            || !row.eligible;
        if ineligible || row.expired || row.attempt_count >= if row.test { 1 } else { 5 } {
            let state = if ineligible {
                "ineligible"
            } else {
                "exhausted"
            };
            sqlx::query("UPDATE alert_webhook_outbox SET state=$2,last_error=CASE WHEN $2='exhausted' THEN 'interrupted' ELSE last_error END,completed_at=clock_timestamp(),lease_owner=NULL,lease_token=NULL,lease_expires_at=NULL WHERE id=$1")
                .bind(id).bind(state).execute(&mut *tx).await?;
            terminal_audit(&mut tx, id, state, Some("interrupted")).await?;
            tx.commit().await?;
            return Ok(Some(None));
        }
        // Operator policy is immutable for this Store. Check it while the
        // destination is locked; resolve bytes only after committing the lease.
        let policy_refused = d
            .url
            .as_deref()
            .is_none_or(|url| store.alert_webhooks.approved_url(url).is_none())
            || d.current_secret_ref
                .as_deref()
                .is_none_or(|r| !store.alert_webhooks.secrets.allows(r))
            || active_previous(&d, Utc::now())
                .is_some_and(|r| !store.alert_webhooks.secrets.allows(r));
        let token = Uuid::new_v4();
        let before = tokio::time::Instant::now();
        let remaining: f64 = sqlx::query_scalar("UPDATE alert_webhook_outbox SET state='leased',attempt_count=attempt_count+1,lease_owner=$2,lease_token=$3,lease_expires_at=least(clock_timestamp()+make_interval(secs=>$4::integer),created_at+interval '24 hours') WHERE id=$1 RETURNING extract(epoch FROM lease_expires_at-clock_timestamp())::float8")
            .bind(id).bind(owner).bind(token).bind(LEASE_SECONDS).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        return Ok(Some(Some(Claim {
            row,
            destination: d,
            owner,
            token,
            policy_refused,
            until: before + Duration::from_secs_f64((remaining - 5.0).max(0.0)),
        })));
    }
    tx.commit().await?;
    Ok(None)
}

async fn send(store: &Store, c: &Claim) -> Result<(), Failure> {
    send_with(store, c, |r| store.alert_webhooks.secrets.signing_bytes(r)).await
}

async fn send_with(
    store: &Store,
    c: &Claim,
    resolve: impl Fn(&str) -> Option<zeroize::Zeroizing<Vec<u8>>>,
) -> Result<(), Failure> {
    if c.policy_refused {
        return Err(Failure::DestinationPolicy);
    }
    let policy = &store.alert_webhooks;
    let d = &c.destination;
    let raw = d.url.as_deref().ok_or(Failure::DestinationPolicy)?;
    let approved = policy.approved_url(raw).ok_or(Failure::DestinationPolicy)?;
    let url = reqwest::Url::parse(&approved).map_err(|_| Failure::DestinationPolicy)?;
    let current_ref = d.current_secret_ref.as_deref().ok_or(Failure::Credential)?;
    if !policy.secrets.allows(current_ref)
        || active_previous(d, Utc::now()).is_some_and(|r| !policy.secrets.allows(r))
    {
        return Err(Failure::DestinationPolicy);
    }
    let current = resolve(current_ref).ok_or(Failure::Credential)?;
    let previous = active_previous(d, Utc::now())
        .map(|r| resolve(r).ok_or(Failure::Credential))
        .transpose()?;
    let mut egress = crate::egress::policy().permit_loopback(policy.development);
    if url.scheme() == "http" {
        egress = egress.only_loopback();
    }
    crate::egress::check_url(&egress, crate::egress::Purpose::AlertWebhook, &url)
        .map_err(|_| Failure::HostNotAllowed)?;
    let client = crate::otlp::export_http_client_with_policy(
        &url,
        crate::egress::Purpose::AlertWebhook,
        egress,
        Duration::from_secs(5),
        REQUEST,
    )
    .map_err(|_| Failure::DestinationPolicy)?;
    let header = signature(
        &current,
        previous.as_ref().map(|p| p.as_slice()),
        Utc::now().timestamp(),
        &c.row.payload,
    );
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("X-OMG-Delivery", c.row.id.to_string())
        .header("X-OMG-Signature", header)
        .body(c.row.payload.clone())
        .send()
        .await
        .map_err(|e| request_failure(&e))?;
    // Drop immediately: never read, buffer, log or parse a receiver body.
    http_result(response.status().as_u16())
}

async fn complete(store: &Store, c: &Claim, result: Result<(), Failure>) -> sqlx::Result<bool> {
    let mut tx =
        crate::db::begin_with_setup(&store.pool, "SET LOCAL statement_timeout='10s'").await?;
    let retry = !c.row.test && result.is_err_and(Failure::retryable);
    let error = result.err().map(Failure::category);
    // Capture one database instant AFTER acquiring the queue lock: deadline
    // decisions cannot disagree at a microsecond boundary, and a lock wait
    // cannot make a pre-lock timestamp authorize an expired completion.
    let state: Option<String> = sqlx::query_scalar("WITH locked AS MATERIALIZED (SELECT id FROM alert_webhook_outbox WHERE id=$1 FOR UPDATE), instant AS MATERIALIZED (SELECT clock_timestamp() at FROM locked) UPDATE alert_webhook_outbox SET state=CASE WHEN $4 AND (attempt_count>=5 OR created_at+interval '24 hours'<=n.at+make_interval(secs=>($6::integer[])[least(attempt_count,4)])) THEN 'exhausted' WHEN $4 THEN 'waiting' ELSE $5 END,last_error=$7,next_attempt_at=CASE WHEN $4 THEN n.at+make_interval(secs=>($6::integer[])[least(attempt_count,4)]) ELSE next_attempt_at END,completed_at=CASE WHEN $4 AND attempt_count<5 AND created_at+interval '24 hours'>n.at+make_interval(secs=>($6::integer[])[least(attempt_count,4)]) THEN NULL ELSE n.at END,lease_owner=NULL,lease_token=NULL,lease_expires_at=NULL FROM instant n WHERE id=$1 AND lease_owner=$2 AND lease_token=$3 AND state='leased' AND lease_expires_at>n.at RETURNING state")
        .bind(c.row.id).bind(c.owner).bind(c.token).bind(retry).bind(if result.is_ok() {"sent"} else {"failed"}).bind(&BACKOFF[..]).bind(error).fetch_optional(&mut *tx).await?;
    if let Some(state) = &state {
        terminal_audit(&mut tx, c.row.id, state, error).await?;
    }
    tx.commit().await?;
    if state.is_some() {
        crate::metrics::METRICS.observe_alert_webhook(match result {
            Ok(()) => "sent",
            Err(e) if e.retryable() => "transient_failure",
            Err(_) => "permanent_failure",
        });
    }
    Ok(state.is_some())
}

/// Row leases fence all replicas, just as #46 does. A crash after acceptance
/// may duplicate; no terminal state can reopen. At most 25 steps / 60 seconds.
pub(crate) async fn deliver(store: &Store) {
    deliver_within(store, Duration::from_secs(60)).await;
}
async fn deliver_within(store: &Store, budget: Duration) {
    let started = tokio::time::Instant::now();
    let owner = Uuid::new_v4();
    let work = async {
        for _ in 0..super::DELIVERIES_PER_TICK {
            if started.elapsed() + STEP > budget {
                break;
            }
            match claim(store, owner).await? {
                None => break,
                Some(None) => continue,
                Some(Some(c)) => {
                    let result = tokio::time::timeout_at(
                        c.until.min(tokio::time::Instant::now() + REQUEST),
                        send(store, &c),
                    )
                    .await
                    .unwrap_or(Err(Failure::Timeout));
                    complete(store, &c, result).await?;
                }
            }
        }
        Ok::<_, sqlx::Error>(())
    };
    if !matches!(
        tokio::time::timeout(budget + Duration::from_secs(10), work).await,
        Ok(Ok(()))
    ) {
        tracing::warn!("alert webhook dispatch incomplete; unfinished leases recover later");
    }
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "webhook_delivery_tests.rs"]
mod delivery_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signature_rotation_and_raw_bytes() {
        let body = b"{\"test\":true}";
        let current = b"01234567890123456789012345678901";
        let previous = b"abcdefghijklmnopqrstuvwxyz012345";
        let header = signature(current, Some(previous), 1700000000, body);
        assert_eq!(
            header,
            format!(
                "{},v1={}",
                crate::audit_export::sign(current, 1700000000, body),
                hex::encode(crate::audit_export::hmac_sha256(
                    previous,
                    b"1700000000.{\"test\":true}"
                ))
            )
        );
        assert_ne!(
            header,
            signature(current, Some(previous), 1700000000, b"{ \"test\":true}")
        );
        assert_eq!(
            signature(current, None, 1700000000, body)
                .matches("v1=")
                .count(),
            1
        );
    }
    #[test]
    fn rotation_overlap_expiry_is_exclusive() {
        let until = Utc::now();
        let d = Destination {
            enabled: true,
            deleted_at: None,
            url: None,
            current_secret_ref: Some("env:CURRENT".into()),
            previous_secret_ref: Some("env:OLD".into()),
            previous_secret_until: Some(until),
            enable_generation: 0,
        };
        assert_eq!(
            active_previous(&d, until - chrono::Duration::microseconds(1)),
            Some("env:OLD")
        );
        assert_eq!(active_previous(&d, until), None);
        assert_eq!(
            active_previous(&d, until + chrono::Duration::microseconds(1)),
            None
        );
    }
    #[test]
    fn status_retry_contract() {
        for code in [200, 201, 204, 299] {
            assert_eq!(http_result(code), Ok(()));
        }
        for code in [408, 429, 500, 503, 599] {
            assert!(http_result(code).unwrap_err().retryable());
        }
        for code in [300, 301, 307, 400, 401, 403, 404, 409] {
            assert!(!http_result(code).unwrap_err().retryable());
        }
    }
}

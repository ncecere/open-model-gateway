//! Durable recipient queue. All database transactions end before SMTP I/O.
//! A lease fences database results, not the relay: a crash after acceptance can
//! duplicate mail. Fingerprints are pseudonymous and must never be formatted.
use std::collections::{HashMap, HashSet};

use sha2::{Digest, Sha256};

use super::*;
use crate::email::{DeliveryError, OutgoingEmail};

#[cfg(all(test, feature = "integration-tests"))]
#[path = "report_delivery_tests.rs"]
mod report_tests;

pub(super) const SEND_DEADLINE: Duration = Duration::from_secs(40);
// 90s lease > 40s whole send deadline. Claims leave >=45s of the 24h window.
const LEASE_SECONDS: i32 = 90;
const BACKOFF_SECONDS: [i32; 4] = [60, 300, 1800, 7200];

// No Debug/Serialize on this type (nor any holder of a recipient fingerprint).
pub(super) struct Claim {
    delivery: Uuid,
    fingerprint: Vec<u8>,
    owner: Uuid,
    token: Uuid,
    settings: SmtpSettings,
    email: OutgoingEmail,
    send_until: tokio::time::Instant,
}

fn fingerprint(delivery: Uuid, email: &str) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(delivery.as_bytes());
    hash.update(b":omg.alert.recipient.v1:");
    hash.update(email.trim().to_lowercase().as_bytes());
    hash.finalize().to_vec()
}

// The address and identity must come from one eligibility statement snapshot.
// Never Debug/Serialize recipients or holders of their plaintext addresses.
#[derive(sqlx::FromRow)]
struct TypedRecipient {
    address: String,
    internal_user_id: Option<Uuid>,
}

/// The existing live recipient rules, including enabled/entitled internal
/// users, plus (for `key_expiring`, #100) the key's own live-authority
/// recipients: never a rule, a listed address or a platform role. Explicit
/// external addresses remain admin-chosen. A listed internal address cannot
/// bypass disablement or loss of platform access. Only built-in personal
/// incidents may notify their owner; never platform/other recipients.
async fn recipients(
    tx: &mut Transaction<'_, Postgres>,
    delivery: Uuid,
    initial: bool,
) -> sqlx::Result<Vec<TypedRecipient>> {
    let key_visible = key_expiry_visible("u.id");
    sqlx::query_as(&format!("SELECT DISTINCT lower(btrim(x.email)) address,(SELECT u.id FROM users u WHERE lower(btrim(u.email))=lower(btrim(x.email)) ORDER BY u.id LIMIT 1) internal_user_id FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id CROSS JOIN LATERAL (
 SELECT unnest(r.notify_emails) email FROM alert_rules r LEFT JOIN workspaces w ON w.id=r.workspace_id WHERE r.id=e.rule_id AND r.enabled AND r.deleted_at IS NULL AND (r.workspace_id IS NULL OR w.disabled_at IS NULL)
 UNION ALL SELECT u.email FROM alert_rules r JOIN effective_workspace_memberships m ON m.workspace_id=r.workspace_id AND m.role IN ('owner','admin') JOIN users u ON u.id=m.user_id WHERE r.id=e.rule_id AND r.enabled AND r.deleted_at IS NULL AND r.notify_workspace_admins
 UNION ALL SELECT u.email FROM alert_rules r JOIN effective_platform_roles p ON p.role='admin' JOIN users u ON u.id=p.user_id LEFT JOIN workspaces w ON w.id=r.workspace_id WHERE r.id=e.rule_id AND r.enabled AND r.deleted_at IS NULL AND r.notify_platform_admins AND (r.workspace_id IS NULL OR w.disabled_at IS NULL)
 UNION ALL SELECT u.email FROM workspaces w JOIN users u ON u.id=w.owner_user_id JOIN effective_platform_roles p ON p.user_id=u.id WHERE w.id=e.workspace_id AND e.builtin='personal_budget' AND w.kind='personal' AND w.disabled_at IS NULL
 UNION ALL SELECT u.email FROM effective_platform_roles p JOIN users u ON u.id=p.user_id WHERE p.role='admin' AND e.builtin=ANY($3)
 UNION ALL SELECT u.email FROM effective_workspace_memberships members JOIN users u ON u.id=members.user_id WHERE e.kind='key_expiring' AND members.workspace_id=e.workspace_id AND ({key_visible}) AND ({KEY_EXPIRY_DELIVERABLE})
 ) x WHERE d.id=$1 AND x.email IS NOT NULL AND NOT EXISTS(SELECT 1 FROM users u WHERE lower(btrim(u.email))=lower(btrim(x.email)) AND (u.disabled_at IS NOT NULL OR u.cleaned_at IS NOT NULL OR NOT EXISTS(SELECT 1 FROM effective_platform_roles p WHERE p.user_id=u.id)))
 AND ($2 OR EXISTS(SELECT 1 FROM alert_recipient_outbox q WHERE q.delivery_id=d.id AND q.fingerprint=sha256(uuid_send(d.id)||convert_to(':omg.alert.recipient.v1:','UTF8')||convert_to(lower(btrim(x.email)),'UTF8')) AND (q.internal_user_id IS NULL OR EXISTS(SELECT 1 FROM users u JOIN effective_platform_roles p ON p.user_id=u.id WHERE u.id=q.internal_user_id AND lower(btrim(u.email))=lower(btrim(x.email)))))) ORDER BY 1 LIMIT $4"))
        .bind(delivery).bind(initial).bind(&INSTALLATION_BUILTINS[..]).bind(MAX_RECIPIENTS)
        .fetch_all(&mut **tx).await
}

/// Caller holds the delivery lock. Serialize aggregates with claims/completions
/// so concurrent successful recipients cannot lose each other's counts.
async fn refresh(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("WITH totals AS (SELECT count(*)::integer recipients,count(*) FILTER(WHERE state='sent')::integer sent,count(*) FILTER(WHERE state IN ('failed','exhausted'))::integer failed,coalesce(sum(attempt_count),0)::integer attempts,count(*) FILTER(WHERE state IN ('waiting','leased'))::integer waiting,count(*) FILTER(WHERE state='exhausted')::integer exhausted,count(*) FILTER(WHERE state='not_configured')::integer not_configured,coalesce(bool_or((state='waiting' AND attempt_count>0) OR (state='leased' AND attempt_count>1)),false) retrying,min(CASE WHEN state='waiting' THEN next_attempt_at WHEN state='leased' THEN lease_expires_at END) due,min(CASE WHEN error='host_not_allowed' THEN 'connection' ELSE error END) FILTER(WHERE state IN ('failed','exhausted','waiting')) error FROM alert_recipient_outbox WHERE delivery_id=$1)
 UPDATE alert_deliveries d SET recipients=t.recipients,sent=t.sent,failed=t.failed,attempt_count=t.attempts,waiting=t.waiting,exhausted=t.exhausted,retrying=t.retrying,next_attempt_at=t.due,error=t.error,status=CASE WHEN t.waiting>0 THEN 'pending' WHEN t.not_configured>0 AND t.sent=0 AND t.failed=0 THEN 'not_configured' WHEN t.sent>0 AND t.failed>0 THEN 'partial' WHEN t.failed>0 THEN 'failed' WHEN t.sent>0 THEN 'sent' ELSE 'no_recipients' END,completed_at=CASE WHEN t.waiting=0 THEN clock_timestamp() END FROM totals t WHERE d.id=$1 AND d.status='pending'")
        .bind(id).execute(&mut **tx).await?;
    Ok(())
}

/// Bounded cleanup, compatible with the old 24h interrupted outcome. Live
/// leases cannot outlast the delivery deadline; terminal successes stay put.
pub(super) async fn cleanup(tx: &mut Transaction<'_, Postgres>) -> sqlx::Result<()> {
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT d.id FROM alert_deliveries d WHERE d.status='pending' AND (d.created_at<=clock_timestamp()-interval '24 hours' OR EXISTS(SELECT 1 FROM alert_recipient_outbox q WHERE q.delivery_id=d.id AND q.state='leased' AND q.lease_expires_at<=clock_timestamp() AND q.attempt_count>=5)) ORDER BY d.created_at,d.id LIMIT 25 FOR UPDATE OF d SKIP LOCKED")
        .fetch_all(&mut **tx).await?;
    for id in ids {
        sqlx::query("UPDATE alert_recipient_outbox q SET state='exhausted',error='interrupted',lease_owner=NULL,lease_token=NULL,lease_expires_at=NULL FROM alert_deliveries d WHERE q.delivery_id=$1 AND d.id=q.delivery_id AND q.state IN ('waiting','leased') AND (d.created_at<=clock_timestamp()-interval '24 hours' OR (q.state='leased' AND q.attempt_count>=5 AND q.lease_expires_at<=clock_timestamp()))")
            .bind(id).execute(&mut **tx).await?;
        let initialized: bool =
            sqlx::query_scalar("SELECT initialized FROM alert_deliveries WHERE id=$1")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
        if initialized {
            refresh(tx, id).await?;
        } else {
            sqlx::query("UPDATE alert_deliveries SET status='failed',error='interrupted',completed_at=clock_timestamp() WHERE id=$1 AND status='pending'")
                .bind(id).execute(&mut **tx).await?;
        }
    }
    Ok(())
}

// Shared verbatim by the lock-free head lookup and the locking claim. Keep
// the cutoff and recipient due predicates identical in both selection steps.
fn due_delivery(now: &str) -> String {
    // A bound database instant, not a volatile clock_timestamp() expression:
    // PostgreSQL can seek past the expired creation-time range in the index.
    format!(
        "d.status='pending' AND d.created_at>{now}::timestamptz-interval '24 hours'+interval '45 seconds' AND (NOT d.initialized OR EXISTS(SELECT 1 FROM alert_recipient_outbox q WHERE q.delivery_id=d.id AND q.attempt_count<5 AND ((q.state='waiting' AND q.next_attempt_at<={now}::timestamptz) OR (q.state='leased' AND q.lease_expires_at<={now}::timestamptz))))"
    )
}

pub(super) fn bucket_candidates_sql() -> String {
    let due = due_delivery("$5");
    format!("WITH RECURSIVE buckets(bucket_id) AS (
 (SELECT bucket_id FROM alert_deliveries WHERE status='pending' ORDER BY bucket_id LIMIT 1)
 UNION ALL
 SELECT next.bucket_id FROM buckets b CROSS JOIN LATERAL
  (SELECT bucket_id FROM alert_deliveries WHERE status='pending' AND bucket_id>b.bucket_id ORDER BY bucket_id LIMIT 1) next
), served AS (SELECT * FROM unnest($1::uuid[],$2::integer[]) AS s(bucket_id,slots)),
recent AS (SELECT * FROM unnest($3::uuid[],$4::bigint[]) AS r(bucket_id,turn))
SELECT b.bucket_id FROM buckets b CROSS JOIN LATERAL
 (SELECT d.created_at FROM alert_deliveries d WHERE d.bucket_id=b.bucket_id AND {due} ORDER BY d.created_at,d.id LIMIT 1) head
LEFT JOIN served s ON s.bucket_id=b.bucket_id
LEFT JOIN recent r ON r.bucket_id=b.bucket_id
ORDER BY coalesce(s.slots,0),r.turn NULLS FIRST,head.created_at,b.bucket_id LIMIT 32")
}

// Scheduling memory only; no authority, credentials or recipient data. The
// evaluator's Store clones share it across ticks, but failover may reset it.
pub(super) const RECENT_BUCKETS: usize = 4096;

fn last_served_order(store: &Store) -> (Vec<Uuid>, Vec<i64>) {
    let history = store
        .alert_dispatch
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut recent: Vec<_> = history.iter().map(|(b, at)| (*b, *at)).collect();
    drop(history);
    recent.sort_by_key(|(bucket, at)| (*at, *bucket));
    // Export monotonic ranks rather than translating process Instants into
    // wall-clock timestamps. SQL NULL means absent/never served.
    let mut previous = None;
    let mut turn = 0_i64;
    recent
        .into_iter()
        .map(|(bucket, at)| {
            if previous != Some(at) {
                turn += 1;
                previous = Some(at);
            }
            (bucket, turn)
        })
        .unzip()
}

fn remember_served(store: &Store, bucket: Uuid) {
    let mut history = store
        .alert_dispatch
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if history.len() >= RECENT_BUCKETS
        && !history.contains_key(&bucket)
        && let Some(oldest) = history
            .iter()
            .min_by_key(|(b, at)| (**at, **b))
            .map(|(b, _)| *b)
    {
        history.remove(&oldest);
    }
    history.insert(bucket, std::time::Instant::now());
}

// One short claim transaction; None means no due work (not a network result).
// The tuple returns (delivery, optional send, bucket), including init-only and
// not-configured steps: every successful step consumes one fair tick slot.
pub(super) async fn claim(
    store: &Store,
    owner: Uuid,
    served: &HashMap<Uuid, u32>,
) -> sqlx::Result<Option<(Uuid, Option<Claim>, Uuid)>> {
    claim_with_hook(store, owner, served, std::future::ready(())).await
}

// The hook permits a deterministic READ COMMITTED interleaving in tests.
pub(super) async fn claim_with_hook(
    store: &Store,
    owner: Uuid,
    served: &HashMap<Uuid, u32>,
    after_eligibility_read: impl std::future::Future<Output = ()>,
) -> sqlx::Result<Option<(Uuid, Option<Claim>, Uuid)>> {
    let mut tx =
        crate::db::begin_with_setup(&store.pool, "SET LOCAL statement_timeout='10s'").await?;
    let (buckets, slots): (Vec<_>, Vec<_>) = served
        .iter()
        .map(|(bucket, n)| (*bucket, *n as i32))
        .unzip();
    let (recent_buckets, turns) = last_served_order(store);
    // Use one database instant for both selection steps. Later lease and live
    // authorization checks retain their existing clock/statement semantics.
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    // Loose/distinct index walk: one seek per pending bucket, never a full
    // backlog scan merely to discover bucket ids. No row locks in step one.
    let candidates: Vec<Uuid> = sqlx::query_scalar(&bucket_candidates_sql())
        .bind(&buckets)
        .bind(&slots)
        .bind(&recent_buckets)
        .bind(&turns)
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;
    let mut row = None;
    let due = due_delivery("$2");
    for bucket in candidates {
        let head: Option<(Uuid, bool)> = sqlx::query_as(&format!("SELECT d.id,d.initialized FROM alert_deliveries d WHERE d.bucket_id=$1 AND {due} ORDER BY d.created_at,d.id LIMIT 1 FOR UPDATE OF d SKIP LOCKED"))
            .bind(bucket).bind(now).fetch_optional(&mut *tx).await?;
        if let Some((id, initialized)) = head {
            row = Some((id, initialized, bucket));
            break;
        }
    }
    let Some((id, initialized, bucket)) = row else {
        return Ok(None);
    };
    let (settings, installation) = relay(&mut tx).await?;
    let Some(settings) = settings else {
        // Do not cancel a live claim: its result must still be recorded.
        sqlx::query("UPDATE alert_recipient_outbox SET state='not_configured',lease_owner=NULL,lease_token=NULL,lease_expires_at=NULL WHERE delivery_id=$1 AND (state='waiting' OR (state='leased' AND lease_expires_at<=clock_timestamp()))")
            .bind(id).execute(&mut *tx).await?;
        if initialized {
            refresh(&mut tx, id).await?;
        } else {
            // Preserve legacy counts without storing pseudonyms for mail we
            // will never attempt.
            let count = recipients(&mut tx, id, true).await?.len() as i32;
            sqlx::query("UPDATE alert_deliveries SET status='not_configured',recipients=$2,retrying=false,waiting=0,next_attempt_at=NULL,completed_at=clock_timestamp() WHERE id=$1").bind(id).bind(count).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        return Ok(Some((id, None, bucket)));
    };
    let addresses = recipients(&mut tx, id, !initialized).await?;
    after_eligibility_read.await;
    if !initialized {
        for to in &addresses {
            sqlx::query(
                "INSERT INTO alert_recipient_outbox(delivery_id,fingerprint,internal_user_id) VALUES($1,$2,$3)",
            )
            .bind(id)
            .bind(fingerprint(id, &to.address))
            .bind(to.internal_user_id)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("UPDATE alert_deliveries SET initialized=true WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    let eligible: Vec<Vec<u8>> = addresses
        .iter()
        .map(|to| fingerprint(id, &to.address))
        .collect();
    // Never cancel a live claim held by another worker. It already authorized
    // its send; the next claim checks live authority again.
    sqlx::query("UPDATE alert_recipient_outbox SET state='ineligible',lease_owner=NULL,lease_token=NULL,lease_expires_at=NULL WHERE delivery_id=$1 AND NOT(fingerprint=ANY($2)) AND (state='waiting' OR (state='leased' AND lease_expires_at<=clock_timestamp()))")
        .bind(id).bind(&eligible).execute(&mut *tx).await?;
    let token = Uuid::new_v4();
    let before_claim = tokio::time::Instant::now();
    let fp: Option<(Vec<u8>, f64)> = sqlx::query_as("WITH candidate AS (SELECT fingerprint FROM alert_recipient_outbox WHERE delivery_id=$1 AND fingerprint=ANY($2) AND attempt_count<5 AND ((state='waiting' AND next_attempt_at<=clock_timestamp()) OR (state='leased' AND lease_expires_at<=clock_timestamp())) ORDER BY next_attempt_at,fingerprint LIMIT 1 FOR UPDATE SKIP LOCKED) UPDATE alert_recipient_outbox q SET state='leased',attempt_count=q.attempt_count+1,lease_owner=$3,lease_token=$4,lease_expires_at=least(clock_timestamp()+make_interval(secs=>$5::integer),d.created_at+interval '24 hours') FROM candidate c,alert_deliveries d WHERE q.delivery_id=$1 AND q.fingerprint=c.fingerprint AND d.id=q.delivery_id RETURNING q.fingerprint,extract(epoch FROM q.lease_expires_at-clock_timestamp())::float8")
        .bind(id).bind(&eligible).bind(owner).bind(token).bind(LEASE_SECONDS).fetch_optional(&mut *tx).await?;
    refresh(&mut tx, id).await?;
    let Some((fp, remaining)) = fp else {
        tx.commit().await?;
        return Ok(Some((id, None, bucket)));
    };
    let pending: Pending = sqlx::query_as("SELECT d.transition,e.summary,e.severity,e.fired_at,e.resolved_at,e.rule_id,r.name rule_name,w.name workspace_name,w.kind workspace_kind,p.name connection_name,e.id event_id,e.builtin,e.kind,e.key_expires_at FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id LEFT JOIN alert_rules r ON r.id=e.rule_id LEFT JOIN workspaces w ON w.id=e.workspace_id LEFT JOIN provider_connections p ON p.id=e.provider_connection_id WHERE d.id=$1")
        .bind(id).fetch_one(&mut *tx).await?;
    let (subject, body) = email_message(pending, installation);
    let to = addresses
        .into_iter()
        .find(|to| fingerprint(id, &to.address) == fp)
        .expect("claimed eligible recipient")
        .address;
    tx.commit().await?;
    Ok(Some((
        id,
        Some(Claim {
            delivery: id,
            fingerprint: fp,
            owner,
            token,
            settings,
            email: OutgoingEmail { to, subject, body },
            send_until: before_claim + Duration::from_secs_f64((remaining - 5.0).max(0.0)),
        }),
        bucket,
    )))
}

pub(super) async fn complete(
    store: &Store,
    claim: &Claim,
    result: Result<(), DeliveryError>,
) -> sqlx::Result<bool> {
    let mut tx =
        crate::db::begin_with_setup(&store.pool, "SET LOCAL statement_timeout='10s'").await?;
    sqlx::query("SELECT id FROM alert_deliveries WHERE id=$1 FOR UPDATE")
        .bind(claim.delivery)
        .execute(&mut *tx)
        .await?;
    let (state, error, retryable) = match result {
        Ok(()) => ("sent", None, false),
        Err(e) => ("failed", Some(e.as_str()), e.retryable()),
    };
    let updated = sqlx::query("UPDATE alert_recipient_outbox q SET state=CASE WHEN $6 AND (q.attempt_count>=5 OR d.created_at+interval '24 hours'<=clock_timestamp()+make_interval(secs=>($7::integer[])[least(q.attempt_count,4)])) THEN 'exhausted' WHEN $6 THEN 'waiting' ELSE $4 END,error=$5,next_attempt_at=CASE WHEN $6 THEN clock_timestamp()+make_interval(secs=>($7::integer[])[least(q.attempt_count,4)]) ELSE q.next_attempt_at END,lease_owner=NULL,lease_token=NULL,lease_expires_at=NULL FROM alert_deliveries d WHERE q.delivery_id=$1 AND q.fingerprint=$2 AND q.lease_token=$3 AND q.lease_owner=$8 AND q.state='leased' AND q.lease_expires_at>clock_timestamp() AND d.id=q.delivery_id AND d.status='pending'")
        .bind(claim.delivery).bind(&claim.fingerprint).bind(claim.token).bind(state).bind(error).bind(retryable).bind(&BACKOFF_SECONDS[..]).bind(claim.owner)
        .execute(&mut *tx).await?.rows_affected() == 1;
    if updated {
        refresh(&mut tx, claim.delivery).await?;
    }
    tx.commit().await?;
    if updated {
        crate::metrics::METRICS.observe_alert_delivery(match result {
            Ok(()) => "sent",
            Err(e) if e.retryable() => "transient_failure",
            Err(_) => "permanent_failure",
        });
    }
    Ok(updated)
}

/// Process at most 25 recipient attempts (and 25 initialization/cleanup
/// actions), regardless of caller limit. Returns distinct deliveries touched,
/// retaining the existing CLI/report convention. No connection spans send().
///
/// Unbounded by wall-clock budget: callers that must themselves stay inside a
/// tick (the evaluator loop) use [`deliver_pending_within`] instead.
/// Upper bound for one tick's cleanup transaction.
pub(crate) const CLEANUP_BOUND: Duration = Duration::from_secs(30);

pub async fn deliver_pending(store: &Store, limit: i64) -> usize {
    // Effectively unlimited: preserves the historical unbounded behaviour
    // for the CLI/report callers and the large existing test suite, while
    // still routing through the one budget-aware implementation.
    deliver_pending_within(store, limit, Duration::from_secs(10 * 365 * 24 * 3600)).await
}

/// Like [`deliver_pending`], but stops claiming further recipients once
/// `elapsed + SEND_DEADLINE` would exceed `budget`. A claimed, leased
/// recipient is never abandoned mid-send (that would burn an attempt and
/// lose its result with no metric): the check runs strictly before the next
/// `claim()`, never after. Callers that must stay inside a fixed window (the
/// evaluator tick) pass a real budget and keep their own outer timeout only
/// as a backstop above `DELIVERIES_PER_TICK * SEND_DEADLINE`.
pub(super) async fn deliver_pending_within(store: &Store, limit: i64, budget: Duration) -> usize {
    let started = tokio::time::Instant::now();
    let mut touched = HashSet::new();
    let mut served = HashMap::new();
    let owner = Uuid::new_v4();
    let outcome = async {
        // Cleanup counts against the budget (`started` is taken before it) and
        // has its own bound, so a degraded database cannot stall the tick.
        tokio::time::timeout(CLEANUP_BOUND, async {
            let mut tx =
                crate::db::begin_with_setup(&store.pool, "SET LOCAL statement_timeout='10s'")
                    .await?;
            cleanup(&mut tx).await?;
            tx.commit().await
        })
        .await
        .map_err(|_| sqlx::Error::PoolTimedOut)??;
        for _ in 0..limit.clamp(1, DELIVERIES_PER_TICK) {
            if started.elapsed() + SEND_DEADLINE > budget {
                break;
            }
            let (id, claim, bucket) = match claim(store, owner, &served).await {
                Ok(Some(claimed)) => claimed,
                Ok(None) => break,
                Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("57014") => {
                    // The aborted claim transaction rolls back. Cleanup already
                    // ran, and the next tick may retry; do not burn more time on
                    // the same expensive statement in this tick.
                    tracing::warn!("alert outbox claim timed out; ending dispatch tick");
                    break;
                }
                Err(error) => return Err(error),
            };
            *served.entry(bucket).or_insert(0) += 1;
            remember_served(store, bucket);
            touched.insert(id);
            if let Some(claim) = claim {
                let now = tokio::time::Instant::now();
                let result = if now >= claim.send_until {
                    Err(DeliveryError::Timeout)
                } else {
                    tokio::time::timeout_at(
                        claim.send_until.min(now + SEND_DEADLINE),
                        claim.settings.send(
                            &store.smtp_secrets,
                            OutgoingEmail {
                                to: claim.email.to.clone(),
                                subject: claim.email.subject.clone(),
                                body: claim.email.body.clone(),
                            },
                        ),
                    )
                    .await
                    .unwrap_or(Err(DeliveryError::Timeout))
                };
                complete(store, &claim, result).await?;
            }
        }
        Ok::<_, sqlx::Error>(())
    }
    .await;
    if outcome.is_err() {
        tracing::warn!("alert outbox unavailable; unfinished leases retry later");
    }
    touched.len()
}

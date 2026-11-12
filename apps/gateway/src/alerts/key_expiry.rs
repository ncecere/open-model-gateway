//! Built-in key-expiry reminders (#100): candidates, retirement, the per-tick
//! evaluation and the live visibility/deliverability predicates.
use super::*;

// ---------- Key-expiry reminders (#100) ----------
//
// Built in, not a rule: no alert_rules row, evaluated once per tick. Events
// carry neither rule_id nor builtin (the shape check is relaxed for this
// kind only, migration 0049): that shape is invisible and undeliverable to
// every pre-0049 read path (see 0049's header and docs/alerts.md), which is
// the chosen rolling-upgrade safety mechanism. Permanent, not open-incident,
// deduplication: (api_key_id, key_expires_at, threshold_days) is never
// recorded twice, including after resolution (alert_events_key_expiry_dedup,
// not partial on resolved_at). At most one reminder is ever open per key at
// a time (the existing partial alert_events_open index, keyed by
// subject_key = the key id, since rule_id/builtin collapse to the same
// sentinel for every row of this kind): the 1-day reminder supersedes an
// open 7-day one silently. Resolution here never means "fixed, worth an
// email": it means superseded by the more urgent threshold, or no longer an
// eligible candidate; neither ever enqueues a delivery row.

/// (key id, its expiry, its workspace, the band it currently matches: 1 or
/// 7 days). A key matches at most one band at any instant, so the first
/// evaluation that ever sees a key already inside one day emits only the
/// 1-day reminder: there is no 7-day row to also backfill. Ordered by
/// expiry so a bounded tick makes progress on the most urgent keys first.
/// Excludes keys already recorded for their current band (permanent dedup)
/// and keys that are not eligible at all: revoked, disabled, expired or
/// expiry-less, in a disabled workspace, a disabled service account, or a
/// human key whose holder lost workspace membership.
const KEY_EXPIRY_CANDIDATES: &str = "WITH eligible AS MATERIALIZED (SELECT k.id,k.expires_at,k.workspace_id,CASE WHEN k.expires_at<=$1+interval '1 day' THEN 1 ELSE 7 END threshold \
 FROM api_keys k JOIN workspaces w ON w.id=k.workspace_id \
 WHERE k.revoked_at IS NULL AND k.disabled_at IS NULL AND k.expires_at IS NOT NULL AND w.disabled_at IS NULL \
 AND k.expires_at>$1 AND k.expires_at<=$1+interval '7 days' \
 AND (k.issued_to_user_id IS NULL OR EXISTS(SELECT 1 FROM effective_workspace_memberships m WHERE m.workspace_id=k.workspace_id AND m.user_id=k.issued_to_user_id)) \
 AND (k.service_account_id IS NULL OR EXISTS(SELECT 1 FROM service_accounts sa WHERE sa.id=k.service_account_id AND sa.disabled_at IS NULL)) \
 AND NOT EXISTS(SELECT 1 FROM alert_events e WHERE e.kind='key_expiring' AND e.api_key_id=k.id AND e.key_expires_at=k.expires_at AND e.threshold_days=CASE WHEN k.expires_at<=$1+interval '1 day' THEN 1 ELSE 7 END) \
 ), allowances AS MATERIALIZED (SELECT w.workspace_id, \
 (SELECT count(*) FROM alert_events e WHERE e.kind='key_expiring' AND e.workspace_id=w.workspace_id AND e.fired_at>$1-interval '24 hours') recent \
 FROM (SELECT DISTINCT workspace_id FROM eligible) w), ranked AS ( \
 SELECT k.*,a.recent,row_number() OVER(PARTITION BY k.workspace_id ORDER BY k.expires_at,k.id) ordinal \
 FROM eligible k JOIN allowances a USING(workspace_id))";

/// Silently resolves (no delivery row, ever) at most [`KEY_EXPIRY_PROGRESS`]
/// open key-expiry incidents, oldest first: `cleared` when the key is no
/// longer an eligible candidate at all, `superseded` when it is still
/// eligible but the 1-day band now applies to an incident fired at the
/// 7-day one.
const KEY_EXPIRY_RETIRE: &str = "WITH decided AS ( \
 SELECT e.id,e.fired_at,CASE \
  WHEN k.id IS NULL OR k.revoked_at IS NOT NULL OR k.disabled_at IS NOT NULL OR w.disabled_at IS NOT NULL \
   OR k.expires_at IS DISTINCT FROM e.key_expires_at OR k.expires_at<=$1 OR k.expires_at>$1+interval '7 days' \
   OR (k.issued_to_user_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM effective_workspace_memberships m WHERE m.workspace_id=k.workspace_id AND m.user_id=k.issued_to_user_id)) \
   OR (k.service_account_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM service_accounts sa WHERE sa.id=k.service_account_id AND sa.disabled_at IS NULL)) THEN 'cleared' \
  WHEN e.threshold_days=7 AND k.expires_at<=$1+interval '1 day' THEN 'superseded' \
  ELSE NULL END resolution \
 FROM alert_events e LEFT JOIN api_keys k ON k.id=e.api_key_id LEFT JOIN workspaces w ON w.id=k.workspace_id \
 WHERE e.kind='key_expiring' AND e.resolved_at IS NULL \
), obsolete AS (SELECT id,resolution FROM decided WHERE resolution IS NOT NULL ORDER BY fired_at,id LIMIT $2) \
 UPDATE alert_events e SET resolved_at=clock_timestamp(),resolution=d.resolution FROM obsolete d WHERE e.id=d.id";

type KeyExpiryCandidate = (Uuid, DateTime<Utc>, Uuid, i32);

/// One bounded tick: retire obsolete open reminders, then create new ones
/// for current candidates. Retiring first means a key that just crossed
/// from the 7-day band into the 1-day band frees its `subject_key` slot in
/// the open-uniqueness index before the new 1-day row tries to claim it.
pub(crate) async fn key_expiring_tick(
    tx: &mut Transaction<'_, Postgres>,
    now: DateTime<Utc>,
) -> Result<Changes, sqlx::Error> {
    let mut changes = Changes::default();
    // Apply each workspace's remaining allowance BEFORE the global LIMIT:
    // even an earlier-expiring backlog cannot fill the tick with skipped rows.
    // Resolved incidents still count, and no dedup row is written for excess keys.
    let capped: Vec<(Uuid, i64, i64)> = sqlx::query_as(&format!("{KEY_EXPIRY_CANDIDATES} SELECT workspace_id,max(recent),count(*) FROM ranked GROUP BY workspace_id HAVING max(recent)+count(*)>$2"))
        .bind(now)
        .bind(KEY_EXPIRY_WORKSPACE_24H)
        .fetch_all(&mut **tx)
        .await?;
    for (workspace_id, count, eligible_candidates) in capped {
        tracing::warn!(%workspace_id, count, eligible_candidates, cap = KEY_EXPIRY_WORKSPACE_24H,
            "workspace key-expiry reminder cap applies; excess candidates deferred");
    }
    let candidates: Vec<KeyExpiryCandidate> = sqlx::query_as(&format!("{KEY_EXPIRY_CANDIDATES} SELECT id,expires_at,workspace_id,threshold FROM ranked WHERE ordinal+recent<=$2 ORDER BY expires_at,id LIMIT $3"))
        .bind(now)
        .bind(KEY_EXPIRY_WORKSPACE_24H)
        .bind(KEY_EXPIRY_PROGRESS)
        .fetch_all(&mut **tx)
        .await?;
    let buckets: Vec<Uuid> = candidates.iter().map(|c| c.2).collect();
    lock_delivery_buckets(tx, &buckets).await?;
    // Retiring still precedes inserts (urgent reminders need the open slot),
    // but all bucket locks now precede these incident writes too.
    changes.resolved += sqlx::query(KEY_EXPIRY_RETIRE)
        .bind(now)
        .bind(KEY_EXPIRY_PROGRESS)
        .execute(&mut **tx)
        .await?
        .rows_affected() as usize;
    for (key_id, expires_at, workspace_id, threshold) in candidates {
        let id = Uuid::new_v4();
        let plural = if threshold == 1 { "" } else { "s" };
        let summary = format!("An API key expires within {threshold} day{plural}");
        let severity = if threshold <= KEY_EXPIRY_URGENT_DAYS {
            "critical"
        } else {
            "warning"
        };
        // Insert incident and fired delivery atomically; ON CONFLICT DO
        // NOTHING absorbs a concurrent/duplicate insert (same tuple, or the
        // open-uniqueness index) without an error. Use the same post-lock
        // database timestamp for the cap and fired_at, not transaction-start
        // now(): a lock wait must not backdate events into an already-full window.
        let inserted = sqlx::query("INSERT INTO alert_events(id,kind,subject_key,level,severity,workspace_id,summary,details,api_key_id,key_expires_at,threshold_days,fired_at) VALUES($1,'key_expiring',$2,$3,$4,$5,$6,$7,$8,$9,$3,$10) ON CONFLICT DO NOTHING")
            .bind(id)
            .bind(key_id.to_string())
            .bind(threshold)
            .bind(severity)
            .bind(workspace_id)
            .bind(&summary)
            .bind(json!({"expires_at": expires_at, "threshold_days": threshold}))
            .bind(key_id)
            .bind(expires_at)
            .bind(now)
            .execute(&mut **tx)
            .await?
            .rows_affected();
        if inserted == 1 {
            sqlx::query(
                "INSERT INTO alert_deliveries(id,event_id,transition) VALUES($1,$2,'fired')",
            )
            .bind(Uuid::new_v4())
            .bind(id)
            .execute(&mut **tx)
            .await?;
            changes.fired += 1;
        }
    }
    Ok(changes)
}

/// Live recipients of one key's expiry reminder, recomputed at send time
/// (never stored): personal workspace -> the owner only; shared human key ->
/// the holder plus the workspace's current owners/admins; service-account
/// key (no human owner) -> the workspace's current owners/admins only.
/// Every recipient needs live platform entitlement; shared recipients also
/// need effective workspace membership. No platform-role-only fallback and
/// no listed addresses.
/// Shared by delivery and all management read paths. The caller is a SQL
/// parameter/expression, never client-provided SQL. `e` is the incident.
/// Authority is live even for resolved history; old attribution cannot grant
/// a former holder access. Personal ownership is checked explicitly.
pub(crate) fn key_expiry_visible(viewer: &str) -> String {
    format!(
        "e.kind='key_expiring' AND EXISTS(SELECT 1 FROM api_keys k JOIN workspaces kw ON kw.id=k.workspace_id WHERE k.id=e.api_key_id AND k.workspace_id=e.workspace_id AND k.revoked_at IS NULL AND k.disabled_at IS NULL AND kw.disabled_at IS NULL AND k.expires_at=e.key_expires_at AND k.expires_at>clock_timestamp() AND (k.issued_to_user_id IS NULL OR EXISTS(SELECT 1 FROM effective_workspace_memberships holder WHERE holder.workspace_id=k.workspace_id AND holder.user_id=k.issued_to_user_id)) AND (k.service_account_id IS NULL OR EXISTS(SELECT 1 FROM service_accounts sa WHERE sa.id=k.service_account_id AND sa.disabled_at IS NULL)) AND EXISTS(SELECT 1 FROM effective_workspace_memberships m WHERE m.workspace_id=k.workspace_id AND m.user_id={viewer} AND ((kw.kind='personal' AND kw.owner_user_id=m.user_id) OR (kw.kind IN ('team','project') AND (k.issued_to_user_id=m.user_id OR m.role IN ('owner','admin'))))))"
    )
}

// Shared with `outbox::recipients`: the outbox's per-claim live-eligibility
// recheck (never a held authority lock spanning SMTP I/O) is what makes a
// key-expiry reminder that becomes obsolete before send (revoked, disabled,
// expired, lost membership, etc.) unsendable; see docs/alerts.md.
pub(super) const KEY_EXPIRY_DELIVERABLE: &str = "e.resolved_at IS NULL AND e.key_expires_at<=clock_timestamp()+interval '7 days' AND e.threshold_days=CASE WHEN e.key_expires_at<=clock_timestamp()+interval '1 day' THEN 1 ELSE 7 END";

//! Fixed, built-in unknown-hold review incidents (#101). No accounting writes.
use super::*;

pub const AGED_HOLD_WARNING_HOURS: i64 = 24;
pub const AGED_HOLD_CRITICAL_HOURS: i64 = 168;
const FIRE_PER_TICK: i64 = 100;
const FIRE_PER_DAY: i64 = 500;
const RETIRE_PER_TICK: i64 = 500;

/// Scope existence probe shared with retirement. `hours` and `now` are
/// server-owned SQL expressions, never request input. Includes enabled
/// workspaces only. The NULL workspace is one anonymous personal aggregate.
/// A reservation's unknown ledger timestamp (partition-scoped) wins over
/// admission: long-running jobs must not alert as soon as they fail.
fn scope_exists(hours: &str, now: &str) -> String {
    format!(
        "EXISTS(SELECT 1 FROM workspaces aw JOIN governance_reservations ar ON ar.workspace_id=aw.id AND ar.state='unknown' AND ar.admitted_at<={now}-({hours})*interval '1 hour' LEFT JOIN monetary_ledger al ON al.execution_id=ar.execution_id AND al.admitted_at=ar.admitted_at AND al.kind='unknown' WHERE aw.disabled_at IS NULL AND ((e.workspace_id IS NOT NULL AND aw.id=e.workspace_id AND aw.kind IN ('team','project')) OR (e.workspace_id IS NULL AND aw.kind='personal')) AND coalesce(al.created_at,ar.admitted_at)<={now}-({hours})*interval '1 hour')"
    )
}

/// Live in-app authority, including resolved history: platform readers see
/// all incidents; shared-workspace owners/admins see only their own. Personal
/// owners and ordinary members receive no incident. Effective views remove
/// disabled, cleaned and unentitled users; old attribution grants nothing.
pub(crate) fn aged_hold_visible(viewer: &str) -> String {
    format!(
        "e.kind='aged_unknown_hold' AND (EXISTS(SELECT 1 FROM effective_platform_roles ap WHERE ap.user_id={viewer} AND ap.role IN ('admin','auditor')) OR EXISTS(SELECT 1 FROM workspaces aw JOIN effective_workspace_memberships am ON am.workspace_id=aw.id WHERE aw.id=e.workspace_id AND aw.kind IN ('team','project') AND aw.disabled_at IS NULL AND am.user_id={viewer} AND am.role IN ('owner','admin')))"
    )
}

type AgedHoldCandidate = (String, Option<Uuid>, DateTime<Utc>);
type AgedHoldRetirement = (Uuid, String);

/// One serialized/fenced job. All time comes from the post-lock database
/// clock supplied by evaluate_once_fenced. The maintained lifetime counts
/// gate candidate probing; retirement is independent so disabled/cleared
/// scopes retire even when no unknown reservations remain anywhere.
pub(crate) async fn aged_hold_tick(
    tx: &mut Transaction<'_, Postgres>,
    now: DateTime<Utc>,
) -> Result<Changes, sqlx::Error> {
    let (_, unknown): (i64, i64) = sqlx::query_as(crate::governance::totals::RESERVATION_COUNTS)
        .fetch_one(&mut **tx)
        .await?;
    let aged = scope_exists("24", "$1");
    let critical = scope_exists("168", "$1");
    // No step-down: a critical incident stays open while ANY 24h hold is
    // present. Silent supersede/clear, exactly like key-expiry retirement.
    // Decide retirements without writing yet: all candidate bucket locks
    // must precede even the incident UPDATEs in this multi-bucket job.
    let retirements: Vec<AgedHoldRetirement> = sqlx::query_as(&format!("WITH decided AS (SELECT e.id,e.fired_at,CASE WHEN NOT ({aged}) THEN 'cleared' WHEN e.level=1 AND ({critical}) THEN 'superseded' END resolution FROM alert_events e WHERE e.kind='aged_unknown_hold' AND e.resolved_at IS NULL) SELECT id,resolution FROM decided WHERE resolution IS NOT NULL ORDER BY fired_at,id LIMIT $2"))
        .bind(now).bind(RETIRE_PER_TICK).fetch_all(&mut **tx).await?;
    let retired_ids: Vec<Uuid> = retirements.iter().map(|r| r.0).collect();
    let allowance = if unknown == 0 {
        0
    } else {
        let recent: i64 = sqlx::query_scalar("SELECT count(*) FROM alert_events WHERE kind='aged_unknown_hold' AND fired_at>$1-interval '24 hours'")
            .bind(now).fetch_one(&mut **tx).await?;
        FIRE_PER_TICK.min((FIRE_PER_DAY - recent).max(0))
    };
    // Probe only workspaces with maintained unknown>0. Each lateral lookup
    // uses workspace/admitted_at and the sound admission cutoff, and prunes
    // both reservation and ledger partitions. No global reservation scan.
    // Aggregating personal candidates erases every workspace identity before
    // constructing an incident. Oldest first, stable subject tie-break.
    // Ignore only the selected retirements for open-incident dedup; their
    // fired_at still counts for cooldown. This preserves escalation and
    // the 500-retirement cap while probing candidates before any writes.
    let candidates: Vec<AgedHoldCandidate> = if allowance > 0 {
        sqlx::query_as("WITH candidates AS (SELECT CASE WHEN w.kind='personal' THEN 'personal' ELSE 'workspace:'||w.id END subject, CASE WHEN w.kind='personal' THEN NULL ELSE w.id END workspace_id, first.unknown_since FROM budget_totals t JOIN workspaces w ON w.id=t.scope_id CROSS JOIN LATERAL (SELECT coalesce(l.created_at,r.admitted_at) unknown_since FROM governance_reservations r LEFT JOIN monetary_ledger l ON l.execution_id=r.execution_id AND l.admitted_at=r.admitted_at AND l.kind='unknown' WHERE r.workspace_id=w.id AND r.state='unknown' AND r.admitted_at<=$1-interval '24 hours' AND coalesce(l.created_at,r.admitted_at)<=$1-interval '24 hours' ORDER BY 1,r.execution_id LIMIT 1) first WHERE t.scope_kind='workspace' AND t.period='lifetime' AND t.period_start='epoch' AND t.unknown>0 AND w.disabled_at IS NULL), scopes AS (SELECT subject,workspace_id,min(unknown_since) unknown_since FROM candidates GROUP BY subject,workspace_id) SELECT subject,workspace_id,unknown_since FROM scopes s WHERE NOT EXISTS(SELECT 1 FROM alert_events e WHERE e.kind='aged_unknown_hold' AND e.subject_key=s.subject AND ((e.resolved_at IS NULL AND NOT(e.id=ANY($3::uuid[]))) OR e.fired_at>$1-interval '24 hours')) ORDER BY unknown_since,subject LIMIT $2")
            .bind(now).bind(allowance).bind(&retired_ids).fetch_all(&mut **tx).await?
    } else {
        Vec::new()
    };
    let buckets: Vec<Uuid> = candidates
        .iter()
        .map(|c| c.1.unwrap_or(Uuid::nil()))
        .collect();
    lock_delivery_buckets(tx, &buckets).await?;
    // Retirement stays silent and still runs when no fires are allowed.
    // It must precede inserts so escalations can claim the open subject.
    let resolutions: Vec<&str> = retirements.iter().map(|r| r.1.as_str()).collect();
    let retired = sqlx::query("UPDATE alert_events e SET resolved_at=clock_timestamp(),resolution=o.resolution FROM unnest($1::uuid[],$2::text[]) o(id,resolution) WHERE e.id=o.id AND e.resolved_at IS NULL")
        .bind(&retired_ids).bind(resolutions).execute(&mut **tx).await?.rows_affected();
    let mut changes = Changes {
        resolved: retired as usize,
        ..Changes::default()
    };
    for (subject, workspace, since) in candidates {
        let critical = since <= now - TimeDelta::hours(AGED_HOLD_CRITICAL_HOURS);
        let hours = if critical {
            AGED_HOLD_CRITICAL_HOURS
        } else {
            AGED_HOLD_WARNING_HOURS
        };
        let id = Uuid::new_v4();
        let inserted = sqlx::query("INSERT INTO alert_events(id,kind,subject_key,level,severity,workspace_id,summary,details,fired_at) VALUES($1,'aged_unknown_hold',$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING")
            .bind(id).bind(subject).bind(if critical { 2 } else { 1 })
            .bind(if critical { "critical" } else { "warning" }).bind(workspace)
            .bind(format!("Holds with unknown cost are over {hours} hours old"))
            .bind(json!({"threshold_hours":hours})).bind(now)
            .execute(&mut **tx).await?.rows_affected();
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

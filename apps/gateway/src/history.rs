//! History parent consistency (migration 0034, scale decision gate D3).
//!
//! Executions and reservations no longer carry foreign keys to their hot
//! parents (workspaces, keys, deployments, price versions, cost centers,
//! batch jobs): those keys locked the parent rows `FOR KEY SHARE` on every
//! insert and created MultiXacts under concurrency. Instead the parents can
//! never be deleted or re-keyed (triggers), and a `BEFORE INSERT` trigger on
//! each history table checks its scoped references with plain reads.
//!
//! [`verify`] re-checks the same relations over stored history: every
//! execution's key belongs to its workspace, its deployment, cost center and
//! batch job exist (the job in the same workspace), every priced
//! reservation's price version belongs to its deployment, and the
//! enforcement itself is intact (check and guard triggers enabled, the
//! retained reservation -> execution and ledger -> reservation keys valid).
//! `open-model-gateway history verify` scans everything (or `--since`); the
//! leased `history_verify` job ([`run_job`], hourly) scans the window since
//! its previous run and raises the built-in `history_orphans` installation
//! incident on any finding. Only a full, clean `history verify` resolves it.
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// One kind of finding with its count and up to [`SAMPLES`] example ids.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Finding {
    pub check: String,
    pub count: i64,
    pub sample_ids: Vec<Uuid>,
}

/// Example ids reported per finding kind.
pub const SAMPLES: usize = 5;

#[derive(Clone, Debug, Serialize)]
pub struct VerifyReport {
    /// Lower bound of the scanned window (`None`: all history).
    pub since: Option<DateTime<Utc>>,
    pub executions_checked: i64,
    pub reservations_checked: i64,
    pub finding_count: i64,
    pub findings: Vec<Finding>,
    pub seconds: f64,
}

impl VerifyReport {
    pub fn consistent(&self) -> bool {
        self.finding_count == 0
    }
}

/// Orphan and scope checks over executions started at or after `$1`.
const EXECUTIONS: &str = "SELECT count(*),
 count(*) FILTER(WHERE k.id IS NULL),(array_agg(e.id ORDER BY e.id) FILTER(WHERE k.id IS NULL))[1:5],
 count(*) FILTER(WHERE w.id IS NULL),(array_agg(e.id ORDER BY e.id) FILTER(WHERE w.id IS NULL))[1:5],
 count(*) FILTER(WHERE d.id IS NULL),(array_agg(e.id ORDER BY e.id) FILTER(WHERE d.id IS NULL))[1:5],
 count(*) FILTER(WHERE e.cost_center_id IS NOT NULL AND c.id IS NULL),
  (array_agg(e.id ORDER BY e.id) FILTER(WHERE e.cost_center_id IS NOT NULL AND c.id IS NULL))[1:5],
 count(*) FILTER(WHERE e.batch_job_id IS NOT NULL AND j.id IS NULL),
  (array_agg(e.id ORDER BY e.id) FILTER(WHERE e.batch_job_id IS NOT NULL AND j.id IS NULL))[1:5]
 FROM inference_executions e
 LEFT JOIN api_keys k ON k.workspace_id=e.workspace_id AND k.id=e.api_key_id
 LEFT JOIN workspaces w ON w.id=e.workspace_id
 LEFT JOIN deployments d ON d.id=e.deployment_id
 LEFT JOIN cost_centers c ON c.id=e.cost_center_id
 LEFT JOIN async_jobs j ON j.workspace_id=e.workspace_id AND j.id=e.batch_job_id
 WHERE e.started_at>=$1::text::timestamptz";

/// Price-version scope of reservations admitted at or after `$1`.
const RESERVATIONS: &str = "SELECT count(*),
 count(*) FILTER(WHERE r.price_id IS NOT NULL AND p.id IS NULL),
  (array_agg(r.execution_id ORDER BY r.execution_id) FILTER(WHERE r.price_id IS NOT NULL AND p.id IS NULL))[1:5]
 FROM governance_reservations r
 LEFT JOIN deployment_prices p ON p.deployment_id=r.deployment_id AND p.id=r.price_id
 WHERE r.admitted_at>=$1::text::timestamptz";

/// The enforcement itself: missing or disabled check/guard triggers, and
/// retained history keys that are missing or not validated.
const ENFORCEMENT: &str = "SELECT x.name FROM (VALUES
 ('inference_executions','inference_executions_parent_check'),
 ('governance_reservations','governance_reservations_parent_check'),
 ('workspaces','workspaces_history_parent'),('workspaces','workspaces_history_parent_truncate'),
 ('api_keys','api_keys_history_parent'),('api_keys','api_keys_history_parent_truncate'),
 ('deployments','deployments_history_parent'),('deployments','deployments_history_parent_truncate'),
 ('cost_centers','cost_centers_history_parent'),('cost_centers','cost_centers_history_parent_truncate'),
 ('async_jobs','async_jobs_history_parent_truncate'),('async_jobs','async_jobs_no_delete'),
 ('async_jobs','async_jobs_guard'),('deployment_prices','deployment_prices_immutable')) x(rel,name)
 WHERE NOT EXISTS(SELECT 1 FROM pg_trigger t WHERE t.tgrelid=to_regclass('public.'||x.rel) AND t.tgname=x.name AND t.tgenabled IN ('O','A'))
 UNION ALL SELECT x.name FROM (VALUES
 ('governance_reservations','governance_reservations_execution_fkey'),
 ('monetary_ledger','monetary_ledger_execution_fkey')) x(rel,name)
 WHERE NOT EXISTS(SELECT 1 FROM pg_constraint k WHERE k.conrelid=to_regclass('public.'||x.rel) AND k.conname=x.name AND k.contype='f' AND k.convalidated)
 UNION ALL SELECT t.tgrelid::regclass::text||'.'||t.tgname FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid
 WHERE c.relispartition AND t.tgname IN ('inference_executions_parent_check','governance_reservations_parent_check') AND t.tgenabled NOT IN ('O','A')
 ORDER BY 1";

type Sample = Option<Vec<Uuid>>;
/// Rows checked, then (count, sample) per execution check.
type ExecutionCounts = (
    i64,
    i64,
    Sample,
    i64,
    Sample,
    i64,
    Sample,
    i64,
    Sample,
    i64,
    Sample,
);

/// Verify history in one snapshot (`REPEATABLE READ`, read-only) from
/// `since` (`None`: everything).
pub async fn verify(
    pool: &PgPool,
    since: Option<DateTime<Utc>>,
) -> Result<VerifyReport, sqlx::Error> {
    let mut tx = crate::db::begin_with_setup(
        pool,
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY",
    )
    .await?;
    let report = verify_in(&mut tx, since).await?;
    tx.commit().await?;
    Ok(report)
}

/// [`verify`] on the store's primary pool (`history verify`).
pub async fn verify_store(
    store: &crate::store::Store,
    since: Option<DateTime<Utc>>,
) -> Result<VerifyReport, sqlx::Error> {
    verify(&store.pool, since).await
}

/// Incident bookkeeping of an operator run: findings open the incident; a
/// full (`since` unset) clean run resolves it.
pub async fn record_cli_result(
    store: &crate::store::Store,
    report: &VerifyReport,
) -> Result<(), sqlx::Error> {
    let mut tx = crate::db::begin(&store.pool).await?;
    reconcile_alert(&mut tx, report, report.since.is_none()).await?;
    tx.commit().await
}

/// [`verify`] inside a caller's transaction.
pub async fn verify_in(
    tx: &mut Transaction<'_, Postgres>,
    since: Option<DateTime<Utc>>,
) -> Result<VerifyReport, sqlx::Error> {
    let started = std::time::Instant::now();
    // -infinity: every partition (no pruning).
    let lower = since.map(|s| s.to_rfc3339());
    let lower = lower.as_deref().unwrap_or("-infinity");
    let e: ExecutionCounts = sqlx::query_as(EXECUTIONS)
        .bind(lower)
        .fetch_one(&mut **tx)
        .await?;
    let r: (i64, i64, Sample) = sqlx::query_as(RESERVATIONS)
        .bind(lower)
        .fetch_one(&mut **tx)
        .await?;
    let broken: Vec<String> = sqlx::query_scalar(ENFORCEMENT).fetch_all(&mut **tx).await?;
    let mut findings = Vec::new();
    let mut add = |check: &str, count: i64, sample: Sample| {
        if count > 0 {
            findings.push(Finding {
                check: check.to_owned(),
                count,
                sample_ids: sample
                    .unwrap_or_default()
                    .into_iter()
                    .take(SAMPLES)
                    .collect(),
            });
        }
    };
    add("execution_key_not_in_workspace", e.1, e.2);
    add("execution_workspace_missing", e.3, e.4);
    add("execution_deployment_missing", e.5, e.6);
    add("execution_cost_center_missing", e.7, e.8);
    add("execution_batch_job_not_in_workspace", e.9, e.10);
    add("reservation_price_not_of_deployment", r.1, r.2);
    for name in broken {
        findings.push(Finding {
            check: format!("enforcement_missing:{name}"),
            count: 1,
            sample_ids: Vec::new(),
        });
    }
    Ok(VerifyReport {
        since,
        executions_checked: e.0,
        reservations_checked: r.0,
        finding_count: findings.iter().map(|f| f.count).sum(),
        findings,
        seconds: started.elapsed().as_secs_f64(),
    })
}

const SUMMARY: &str = "Request history references missing or mismatched parents";

/// Raise the built-in `history_orphans` installation incident for a report
/// with findings (once; Platform Admins are notified), or resolve an open
/// one when `full` (all history) found nothing. Returns (fired, resolved).
pub(crate) async fn reconcile_alert(
    tx: &mut Transaction<'_, Postgres>,
    report: &VerifyReport,
    full: bool,
) -> Result<(bool, bool), sqlx::Error> {
    use crate::alerts::HISTORY_ORPHANS;
    let open: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM alert_events WHERE builtin=$1 AND resolved_at IS NULL FOR UPDATE",
    )
    .bind(HISTORY_ORPHANS)
    .fetch_all(&mut **tx)
    .await?;
    if report.consistent() {
        if !full {
            return Ok((false, false));
        }
        for id in &open {
            crate::alerts::resolve(tx, *id, "cleared").await?;
        }
        return Ok((false, !open.is_empty()));
    }
    if !open.is_empty() {
        return Ok((false, false));
    }
    let id = Uuid::now_v7();
    let details = serde_json::json!({
        "since": report.since,
        "findings": report.findings,
        "action": "run `open-model-gateway history verify` and investigate; never delete history",
    });
    let inserted = sqlx::query("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary,details) VALUES($1,$2,$2,$2,1,'critical',$3,$4) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(HISTORY_ORPHANS)
        .bind(SUMMARY)
        .bind(details)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    if inserted == 1 {
        sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition) VALUES($1,$2,'fired')")
            .bind(Uuid::now_v7())
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    Ok((inserted == 1, false))
}

/// Overlap of consecutive job windows (a row's `started_at` is its
/// admission time, which precedes its commit by at most one transaction).
pub const WINDOW_OVERLAP: Duration = Duration::hours(1);
/// Window of a job run without a recorded previous run.
pub const FIRST_WINDOW: Duration = Duration::hours(25);

/// One run of the leased `history_verify` job: verify the history admitted
/// since the previous completed run (minus [`WINDOW_OVERLAP`]; the last
/// [`FIRST_WINDOW`] when there is none), raise the incident on findings in
/// a fenced transaction, and record the completed run.
pub async fn run_job(
    store: &crate::store::Store,
    fence: Option<&crate::leases::Fence>,
) -> Result<VerifyReport, sqlx::Error> {
    let (now, previous): (DateTime<Utc>, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT clock_timestamp(),(SELECT last_completed_at FROM work_leases WHERE name='history_verify')",
    )
    .fetch_one(&store.pool)
    .await?;
    let since = previous.map_or(now - FIRST_WINDOW, |p| p.min(now) - WINDOW_OVERLAP);
    let report = verify(&store.pool, Some(since)).await?;
    crate::metrics::METRICS.set_history_findings(report.finding_count);
    let mut tx = crate::db::begin(&store.pool).await?;
    crate::leases::fence(&mut tx, fence).await?;
    reconcile_alert(&mut tx, &report, false).await?;
    tx.commit().await?;
    if let Some(f) = fence {
        f.complete(&store.pool).await?;
    }
    if !report.consistent() {
        tracing::error!(
            findings = report.finding_count,
            "request history references missing or mismatched parents"
        );
    }
    Ok(report)
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "history/tests.rs"]
mod tests;

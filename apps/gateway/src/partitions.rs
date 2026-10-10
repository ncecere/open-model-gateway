//! Monthly history partitions (migrations 0030/0031, scale plan P6).
//!
//! `inference_executions`, `governance_reservations`, `monetary_ledger`,
//! `audit_events` and `storage_usage_hours` are range partitioned by UTC
//! month (`history_partitions` lists them). Rows from before the upgrade live
//! in each table's `_p_legacy` partition. There is no default partition: a
//! write outside every partition fails (admission fails closed), so future
//! months are created ahead of time:
//!   * `serve` runs [`run_job`] hourly under the `partitions` work lease. It
//!     calls `omg_ensure_partitions(ahead)` (SECURITY DEFINER: it can only
//!     create canonical month partitions of registered parents; CREATE TABLE
//!     ... LIKE then ATTACH, so the parent is never ACCESS EXCLUSIVE locked)
//!     and raises the built-in `partitions_missing` alert while any table
//!     covers fewer than [`ALERT_BELOW_MONTHS`] future months;
//!   * operators run `open-model-gateway partitions ensure` (the same
//!     function) and `partitions status`.
//!
//! Archival of closed months is a separate operator command
//! ([`crate::archive`]).
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// Future months `serve` keeps (`GATEWAY_PARTITIONS_AHEAD_MONTHS`, 1..12).
pub const DEFAULT_AHEAD: i32 = 3;
/// The alert fires while any table covers fewer future months.
pub const ALERT_BELOW_MONTHS: i64 = 2;
const AHEAD_VAR: &str = "GATEWAY_PARTITIONS_AHEAD_MONTHS";

pub fn ahead_from_env() -> anyhow::Result<i32> {
    match std::env::var(AHEAD_VAR) {
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_AHEAD),
        Ok(v) => {
            let n: i32 = v
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid {AHEAD_VAR}"))?;
            anyhow::ensure!((2..=12).contains(&n), "{AHEAD_VAR} must be 2..12");
            Ok(n)
        }
        Err(_) => anyhow::bail!("invalid {AHEAD_VAR}"),
    }
}

/// One partitioned table's coverage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Coverage {
    pub parent: String,
    /// End of the contiguous partitions starting at the current month.
    pub covered_until: DateTime<Utc>,
    /// Whole future months covered after the current one (-1: the current
    /// month itself has no partition).
    pub months_ahead: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EnsureReport {
    /// `(parent, partition)` created by this run.
    pub created: Vec<(String, String)>,
    pub coverage: Vec<Coverage>,
    /// Tables below [`ALERT_BELOW_MONTHS`].
    pub short: Vec<String>,
    /// Why creating partitions failed, if it did (coverage is still read).
    pub error: Option<String>,
}

const COVERAGE: &str = "SELECT c.parent,c.covered_until,
  ((extract(year FROM c.covered_until AT TIME ZONE 'UTC')*12+extract(month FROM c.covered_until AT TIME ZONE 'UTC'))
   -(extract(year FROM $1::timestamptz AT TIME ZONE 'UTC')*12+extract(month FROM $1::timestamptz AT TIME ZONE 'UTC'))-1)::bigint months_ahead
 FROM omg_partition_coverage($1) c";

/// Coverage of every partitioned history table at `at` (`None`: now).
pub async fn coverage(
    pool: &PgPool,
    at: Option<DateTime<Utc>>,
) -> Result<Vec<Coverage>, sqlx::Error> {
    let at = match at {
        Some(at) => at,
        None => {
            sqlx::query_scalar("SELECT clock_timestamp()")
                .fetch_one(pool)
                .await?
        }
    };
    sqlx::query_as(COVERAGE).bind(at).fetch_all(pool).await
}

/// Create missing month partitions through `ahead` months after the current
/// one (database clock), then read coverage. Creation errors (e.g. a lock
/// timeout) are reported, not raised, so coverage is always checked.
pub async fn ensure(pool: &PgPool, ahead: i32) -> Result<EnsureReport, sqlx::Error> {
    let created = sqlx::query_as::<_, (String, String)>(
        "SELECT parent,partition FROM omg_ensure_partitions($1)",
    )
    .bind(ahead)
    .fetch_all(pool)
    .await;
    report(pool, created, None).await
}

/// [`ensure`] as of `at` (schema owner only: tests and month-boundary drills).
pub async fn ensure_at(
    pool: &PgPool,
    ahead: i32,
    at: DateTime<Utc>,
) -> Result<EnsureReport, sqlx::Error> {
    let created = sqlx::query_as::<_, (String, String)>(
        "SELECT parent,partition FROM omg_ensure_partitions($1,$2)",
    )
    .bind(ahead)
    .bind(at)
    .fetch_all(pool)
    .await;
    report(pool, created, Some(at)).await
}

async fn report(
    pool: &PgPool,
    created: Result<Vec<(String, String)>, sqlx::Error>,
    at: Option<DateTime<Utc>>,
) -> Result<EnsureReport, sqlx::Error> {
    let (created, error) = match created {
        Ok(c) => (c, None),
        Err(e) => (Vec::new(), Some(safe_error(&e))),
    };
    let coverage = coverage(pool, at).await?;
    let short = coverage
        .iter()
        .filter(|c| c.months_ahead < ALERT_BELOW_MONTHS)
        .map(|c| c.parent.clone())
        .collect();
    Ok(EnsureReport {
        created,
        coverage,
        short,
        error,
    })
}

fn safe_error(e: &sqlx::Error) -> String {
    match e {
        sqlx::Error::Database(d) => format!(
            "database error {}",
            d.code().as_deref().unwrap_or("unknown")
        ),
        _ => "database unavailable".into(),
    }
}

const SUMMARY: &str = "History tables are running out of monthly partitions";

/// Raise or clear the built-in `partitions_missing` installation incident
/// (Platform Admins are notified by email like other incidents). Returns
/// (fired, resolved).
pub(crate) async fn reconcile_alert(
    tx: &mut Transaction<'_, Postgres>,
    report: &EnsureReport,
) -> Result<(bool, bool), sqlx::Error> {
    use crate::alerts::PARTITIONS_MISSING;
    let open: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM alert_events WHERE builtin=$1 AND resolved_at IS NULL FOR UPDATE",
    )
    .bind(PARTITIONS_MISSING)
    .fetch_all(&mut **tx)
    .await?;
    if report.short.is_empty() {
        for id in &open {
            crate::alerts::resolve(tx, *id, "cleared").await?;
        }
        return Ok((false, !open.is_empty()));
    }
    if !open.is_empty() {
        return Ok((false, false));
    }
    let critical = report.coverage.iter().any(|c| c.months_ahead < 1);
    let id = Uuid::now_v7();
    let details = serde_json::json!({
        "tables": report.coverage.iter().filter(|c| c.months_ahead < ALERT_BELOW_MONTHS)
            .map(|c| serde_json::json!({"table": c.parent, "covered_until": c.covered_until, "months_ahead": c.months_ahead}))
            .collect::<Vec<_>>(),
        "creation_error": report.error,
        "action": "run `open-model-gateway partitions ensure` with the schema owner",
    });
    let inserted = sqlx::query("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary,details) VALUES($1,$2,$2,$2,1,$3,$4,$5) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(PARTITIONS_MISSING)
        .bind(if critical { "critical" } else { "warning" })
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

/// One run of the leased `partitions` job: ensure, export the coverage gauge,
/// and reconcile the alert in a transaction fenced to the lease term.
pub async fn run_job(
    store: &crate::store::Store,
    ahead: i32,
    fence: Option<&crate::leases::Fence>,
) -> Result<EnsureReport, sqlx::Error> {
    let report = ensure(&store.pool, ahead).await?;
    for c in &report.coverage {
        if let Some(table) = crate::store::PARTITIONED_RELATIONS
            .iter()
            .find(|t| **t == c.parent)
        {
            crate::metrics::METRICS.set_partition_months(table, c.months_ahead);
        }
    }
    let mut tx = crate::db::begin(&store.pool).await?;
    crate::leases::fence(&mut tx, fence).await?;
    reconcile_alert(&mut tx, &report).await?;
    tx.commit().await?;
    if !report.created.is_empty() {
        tracing::info!(
            partitions = report.created.len(),
            "history month partitions created"
        );
    }
    if !report.short.is_empty() {
        tracing::error!(tables = ?report.short, "history tables have fewer than {ALERT_BELOW_MONTHS} future month partitions");
    }
    Ok(report)
}

/// Indexes migration 0030/0031 builds on the existing tables before
/// attaching them as legacy partitions, and the tables they belong to.
/// `partitions prepare` builds them online first (CREATE INDEX
/// CONCURRENTLY) so the upgrade window only validates and swaps.
pub const PREPARED_INDEXES: &[(&str, &str, &str)] = &[
    (
        "inference_executions",
        "inference_executions_p_legacy_pkey",
        "CREATE UNIQUE INDEX CONCURRENTLY inference_executions_p_legacy_pkey ON public.inference_executions(id,started_at)",
    ),
    (
        "inference_executions",
        "inference_executions_p_legacy_root_key",
        "CREATE UNIQUE INDEX CONCURRENTLY inference_executions_p_legacy_root_key ON public.inference_executions(root_request_id,attempt_number,started_at)",
    ),
    (
        "inference_executions",
        "inference_executions_p_legacy_scope_key",
        "CREATE UNIQUE INDEX CONCURRENTLY inference_executions_p_legacy_scope_key ON public.inference_executions(workspace_id,api_key_id,deployment_id,id,started_at)",
    ),
    (
        "governance_reservations",
        "governance_reservations_p_legacy_pkey",
        "CREATE UNIQUE INDEX CONCURRENTLY governance_reservations_p_legacy_pkey ON public.governance_reservations(execution_id,admitted_at)",
    ),
    (
        "governance_reservations",
        "governance_unknown_p_legacy",
        "CREATE INDEX CONCURRENTLY governance_unknown_p_legacy ON public.governance_reservations(admitted_at) WHERE state='unknown'",
    ),
    (
        "audit_events",
        "audit_events_p_legacy_pkey",
        "CREATE UNIQUE INDEX CONCURRENTLY audit_events_p_legacy_pkey ON public.audit_events(id,created_at)",
    ),
    (
        "audit_events",
        "audit_installation_time_p_legacy",
        "CREATE INDEX CONCURRENTLY audit_installation_time_p_legacy ON public.audit_events(created_at,id) WHERE workspace_id IS NULL",
    ),
];

/// Build [`PREPARED_INDEXES`] concurrently on tables that are not yet
/// partitioned (an invalid leftover of an interrupted build is dropped and
/// rebuilt). Reads and writes continue. Returns the indexes built.
pub async fn prepare(pool: &PgPool) -> anyhow::Result<Vec<String>> {
    use sqlx::Connection;
    let mut built = Vec::new();
    // One session without a statement timeout; every DDL statement alone
    // (CONCURRENTLY cannot run inside a transaction or a multi-statement query).
    let mut conn = pool.acquire().await?.detach();
    sqlx::raw_sql("SET statement_timeout=0")
        .execute(&mut conn)
        .await?;
    for (table, index, ddl) in PREPARED_INDEXES {
        let kind: Option<String> = sqlx::query_scalar("SELECT relkind::text FROM pg_class WHERE relnamespace='public'::regnamespace AND relname=$1")
            .bind(table).fetch_optional(&mut conn).await?;
        if kind.as_deref() != Some("r") {
            continue;
        }
        let valid: Option<bool> = sqlx::query_scalar("SELECT i.indisvalid FROM pg_class c JOIN pg_index i ON i.indexrelid=c.oid WHERE c.relnamespace='public'::regnamespace AND c.relname=$1")
            .bind(index).fetch_optional(&mut conn).await?;
        match valid {
            Some(true) => continue,
            Some(false) => {
                sqlx::raw_sql(&format!("DROP INDEX CONCURRENTLY public.{index}"))
                    .execute(&mut conn)
                    .await?;
            }
            None => {}
        }
        sqlx::raw_sql(ddl).execute(&mut conn).await?;
        built.push((*index).to_owned());
    }
    let _ = conn.close().await;
    Ok(built)
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "partitions/tests.rs"]
pub(crate) mod tests;

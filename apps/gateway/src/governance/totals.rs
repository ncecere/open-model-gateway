//! Maintained budget totals (migration 0015): admission reads one row per
//! (consumption scope, period, period start) instead of scanning history.
//!
//! Rows are written only by database triggers in the same transaction as
//! every reservation/execution write, so totals cannot drift from a code path
//! that forgets them. `verify` compares them with a full scan.
use super::BudgetPeriod;
use chrono::{DateTime, Utc};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

/// Consumption scope of a budget layer. Installation, type-default/override
/// and local budgets read the installation or workspace scope; key budgets the
/// key lineage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Installation,
    Workspace(Uuid),
    KeyLineage(Uuid),
}
impl Scope {
    pub(crate) fn of(workspace: Option<Uuid>, lineage: Option<Uuid>) -> Self {
        match (workspace, lineage) {
            (None, _) => Self::Installation,
            (Some(w), None) => Self::Workspace(w),
            (Some(_), Some(l)) => Self::KeyLineage(l),
        }
    }
    fn key(self) -> (&'static str, Uuid) {
        match self {
            Self::Installation => ("installation", Uuid::nil()),
            Self::Workspace(w) => ("workspace", w),
            Self::KeyLineage(l) => ("key", l),
        }
    }
}
impl BudgetPeriod {
    /// Start of the bucket holding admissions at `at` (the epoch for lifetime).
    pub fn bucket_start(self, at: DateTime<Utc>) -> DateTime<Utc> {
        self.window(at).0
    }
}

/// Consumption of one budget window: settled actual plus active holds, and
/// whether unresolved unbounded/unpriced usage makes it a lower bound.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Consumption {
    pub used_microusd: i128,
    pub unresolved: bool,
}

/// Reads every requested window in one indexed query (missing rows are zero),
/// in request order.
pub(crate) async fn read(
    tx: &mut Transaction<'_, Postgres>,
    windows: &[(Scope, BudgetPeriod)],
    at: DateTime<Utc>,
) -> Result<Vec<Consumption>, sqlx::Error> {
    if windows.is_empty() {
        return Ok(Vec::new());
    }
    let mut kinds = Vec::with_capacity(windows.len());
    let mut ids = Vec::with_capacity(windows.len());
    let mut periods = Vec::with_capacity(windows.len());
    let mut starts = Vec::with_capacity(windows.len());
    for (scope, period) in windows {
        let (kind, id) = scope.key();
        kinds.push(kind);
        ids.push(id);
        periods.push(period.as_str());
        starts.push(period.bucket_start(at));
    }
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT coalesce(t.settled_microusd+t.held_microusd,0)::text,coalesce(t.unresolved+t.unreserved_executions,0)>0
         FROM unnest($1::text[],$2::uuid[],$3::text[],$4::timestamptz[]) WITH ORDINALITY q(kind,id,period,start,i)
         LEFT JOIN budget_totals t ON t.scope_kind=q.kind AND t.scope_id=q.id AND t.period=q.period AND t.period_start=q.start
         ORDER BY q.i",
    )
    .bind(&kinds)
    .bind(&ids)
    .bind(&periods)
    .bind(&starts)
    .fetch_all(&mut **tx)
    .await?;
    rows.into_iter()
        .map(|(used, unresolved)| {
            Ok(Consumption {
                used_microusd: used.parse().map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                unresolved,
            })
        })
        .collect()
}

/// Budget consumption of one workspace (or one key lineage inside it) in the
/// current window of `period` at `at`. Used by policy reports; admission
/// reads the same rows.
pub(crate) async fn budget_consumption(
    tx: &mut Transaction<'_, Postgres>,
    workspace: Uuid,
    lineage: Option<Uuid>,
    period: BudgetPeriod,
    at: DateTime<Utc>,
) -> Result<(String, bool), sqlx::Error> {
    let c = read(tx, &[(Scope::of(Some(workspace), lineage), period)], at).await?;
    let c = c.first().copied().unwrap_or_default();
    Ok((c.used_microusd.to_string(), c.unresolved))
}

/// Exact expected totals from a full scan (the rules of migration 0015's
/// backfill and of the former admission scan).
const EXPECTED: &str = r#"SELECT s.kind scope_kind,s.id scope_id,p.period,
 CASE WHEN p.period='lifetime' THEN 'epoch'::timestamptz ELSE date_trunc(p.period,c.at,'UTC') END period_start,
 sum(c.settled) settled_microusd,sum(c.held) held_microusd,sum(c.reservations) reservations,sum(c.pending) pending,
 sum(c.unknown) unknown,sum(c.unresolved) unresolved,sum(c.unreserved) unreserved_executions
FROM (
 SELECT r.workspace_id,r.api_key_id,r.admitted_at at,
  CASE WHEN r.state='settled' THEN coalesce(r.actual_microusd,0) ELSE 0 END::numeric settled,
  CASE WHEN r.state='settled' THEN 0 ELSE coalesce(r.held_microusd,0) END::numeric held,
  1::bigint reservations,(r.state='pending')::int::bigint pending,(r.state='unknown')::int::bigint unknown,
  (r.state<>'settled' AND (r.unbounded_cost OR r.held_microusd IS NULL))::int::bigint unresolved,
  0::bigint unreserved
 FROM governance_reservations r
 UNION ALL
 SELECT e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,1 FROM inference_executions e
 WHERE NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=e.id)
) c JOIN api_keys k ON k.id=c.api_key_id
CROSS JOIN LATERAL (VALUES('installation','00000000-0000-0000-0000-000000000000'::uuid),('workspace',c.workspace_id),('key',k.governance_key_id)) s(kind,id)
CROSS JOIN (VALUES('day'),('week'),('month'),('lifetime')) p(period)
GROUP BY 1,2,3,4"#;

/// One bucket whose maintained totals differ from the scan.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub struct Mismatch {
    pub scope_kind: String,
    pub scope_id: Uuid,
    pub period: String,
    pub period_start: DateTime<Utc>,
    /// `settled,held,reservations,pending,unknown,unresolved,unreserved` as maintained.
    pub maintained: String,
    /// The same quantities from the full scan.
    pub scanned: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct VerifyReport {
    /// Buckets compared (union of maintained and scanned).
    pub buckets: i64,
    pub mismatch_count: i64,
    /// At most 20 examples.
    pub mismatches: Vec<Mismatch>,
}
impl VerifyReport {
    pub fn consistent(&self) -> bool {
        self.mismatch_count == 0
    }
}

/// Compares `budget_totals` with a full scan in one read-only snapshot. It
/// takes no installation lock: the triggers commit totals atomically with the
/// rows they summarize, so one snapshot sees both consistently.
pub async fn verify(store: &crate::store::Store) -> Result<VerifyReport, sqlx::Error> {
    let mut tx = store.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let report = verify_in(&mut tx).await?;
    tx.rollback().await?;
    Ok(report)
}

pub(crate) async fn verify_in(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<VerifyReport, sqlx::Error> {
    let compared = format!(
        r#"WITH e AS ({EXPECTED}),
        j AS (SELECT coalesce(t.scope_kind,e.scope_kind) scope_kind,coalesce(t.scope_id,e.scope_id) scope_id,
          coalesce(t.period,e.period) period,coalesce(t.period_start,e.period_start) period_start,
          concat_ws(',',coalesce(t.settled_microusd,0),coalesce(t.held_microusd,0),coalesce(t.reservations,0),coalesce(t.pending,0),coalesce(t.unknown,0),coalesce(t.unresolved,0),coalesce(t.unreserved_executions,0)) maintained,
          concat_ws(',',coalesce(e.settled_microusd,0),coalesce(e.held_microusd,0),coalesce(e.reservations,0),coalesce(e.pending,0),coalesce(e.unknown,0),coalesce(e.unresolved,0),coalesce(e.unreserved_executions,0)) scanned
          FROM budget_totals t FULL JOIN e ON e.scope_kind=t.scope_kind AND e.scope_id=t.scope_id AND e.period=t.period AND e.period_start=t.period_start)"#
    );
    let (buckets, mismatch_count): (i64, i64) = sqlx::query_as(&format!(
        "{compared} SELECT count(*),count(*) FILTER(WHERE maintained<>scanned) FROM j"
    ))
    .fetch_one(&mut **tx)
    .await?;
    let mismatches = if mismatch_count > 0 {
        sqlx::query_as(&format!(
            "{compared} SELECT scope_kind,scope_id,period,period_start,maintained,scanned FROM j WHERE maintained<>scanned ORDER BY 1,2,3,4 LIMIT 20"
        ))
        .fetch_all(&mut **tx)
        .await?
    } else {
        Vec::new()
    };
    Ok(VerifyReport {
        buckets,
        mismatch_count,
        mismatches,
    })
}

/// The former admission-time scan of one scope over `[start, end)`, kept as
/// the test oracle for the maintained totals.
#[cfg(test)]
pub(crate) async fn scan_consumption(
    tx: &mut Transaction<'_, Postgres>,
    workspace: Option<Uuid>,
    lineage: Option<Uuid>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<(String, bool), sqlx::Error> {
    sqlx::query_as(r#"WITH accounting AS (
      SELECT workspace_id,api_key_id,admitted_at budget_at,state,held_microusd,actual_microusd,unbounded_cost FROM governance_reservations
      UNION ALL SELECT e.workspace_id,e.api_key_id,e.started_at,'unknown',NULL::bigint,NULL::bigint,true FROM inference_executions e WHERE NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=e.id))
      SELECT coalesce(sum(CASE WHEN state='settled' THEN actual_microusd ELSE held_microusd END),0)::text,
        count(*) FILTER(WHERE state<>'settled' AND (unbounded_cost OR held_microusd IS NULL))>0
      FROM accounting WHERE ($1::uuid IS NULL OR workspace_id=$1) AND ($2::uuid IS NULL OR api_key_id IN(SELECT id FROM api_keys WHERE workspace_id=$1 AND governance_key_id=$2)) AND budget_at>=$3 AND budget_at<$4"#)
        .bind(workspace).bind(lineage).bind(start).bind(end).fetch_one(&mut **tx).await
}

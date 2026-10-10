//! Maintained rate and in-flight counters (migration 0024): admission reads
//! one row per (scope, current minute) and per scope instead of scanning the
//! current minute's reservations and every live lease.
//!
//! Rows are written only by database triggers in the same transaction as
//! every reservation/execution/async-job write. Lease expiry is applied at
//! read time: pending reservations whose lease expired at or before the
//! admission instant (the `governance_leases` partial index, bounded by
//! reconciliation lag) are subtracted, exactly as the former scan ignored
//! them. `verify_in` compares the counters with a full scan.
use super::totals::Consumption;
use chrono::{DateTime, Utc};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

/// Minutes admission may still read; older minute rows can be pruned (the
/// 0024 guard refuses removing newer ones).
pub const RETAINED_MINUTES: i32 = 10;

/// Rate consumption of one scope at an admission instant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counters {
    /// Interactive admissions (and unreserved executions) this minute.
    pub requests: i64,
    /// Of those, without a token reservation (tokens/minute cannot be enforced).
    pub unreserved: i64,
    /// Token consumption of this minute's interactive reservations.
    pub tokens: i128,
    /// Live pending reservations not accepted as async jobs.
    pub inflight: i64,
    /// Live pending async-job reservations whose job is still active.
    pub jobs: i64,
}

/// Limits of one policy layer as applied to an admission (jobs are exempt
/// from per-minute limits; interactive work never checks the job limit).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Limits {
    pub requests_per_minute: Option<i64>,
    pub tokens_per_minute: Option<i64>,
    pub concurrent_requests: Option<i64>,
    pub concurrent_jobs: Option<i64>,
}

impl Counters {
    /// (rate limits ok, job limit ok) for one more admission reserving
    /// `reserved` tokens: the former scan's predicates (`count + 1 <= limit`).
    pub(crate) fn admits(&self, limits: Limits, reserved: Option<i64>) -> (bool, bool) {
        let requests = limits
            .requests_per_minute
            .is_none_or(|limit| i128::from(self.requests) < i128::from(limit));
        // A layer with a tokens limit requires a finite reservation (checked
        // before); a missing one could never fit.
        let tokens = limits.tokens_per_minute.is_none_or(|limit| {
            self.unreserved == 0
                && reserved.is_some_and(|r| self.tokens + i128::from(r) <= i128::from(limit))
        });
        let concurrent = limits
            .concurrent_requests
            .is_none_or(|limit| i128::from(self.inflight) < i128::from(limit));
        let jobs = limits
            .concurrent_jobs
            .is_none_or(|limit| i128::from(self.jobs) < i128::from(limit));
        (requests && tokens && concurrent, jobs)
    }
}

/// Everything admission's limit check reads, from one statement.
pub(super) struct LimitState {
    /// Applicable policy layers (none for gateway-run batch lines).
    pub(super) policies: Vec<super::Policy>,
    /// Applicable budgets in `BUDGETS` order, each with its maintained totals.
    pub(super) budgets: Vec<(super::Budget, Consumption)>,
    /// Rate counters of the workspace (`None`) and the key lineage (`Some`),
    /// read only when a workspace or key policy layer exists.
    pub(super) counters: Vec<(Option<Uuid>, Counters)>,
}

/// Reads the policy layers, budgets with their current-window totals (0015)
/// and the workspace and key-lineage counters at `at` in one round trip.
/// Pending reservations whose lease expired at or before `at` are subtracted
/// from the in-flight counters.
pub(super) async fn read_limits(
    tx: &mut Transaction<'_, Postgres>,
    workspace: Uuid,
    lineage: Uuid,
    at: DateTime<Utc>,
    policies: bool,
) -> Result<LimitState, sqlx::Error> {
    read_state(tx, workspace, lineage, at, policies, false).await
}

/// The workspace and key-lineage counters at `at` (whether or not a policy
/// applies), read by the same statement as admission.
#[cfg(all(test, feature = "integration-tests"))]
pub(crate) async fn read_counters(
    tx: &mut Transaction<'_, Postgres>,
    workspace: Uuid,
    lineage: Uuid,
    at: DateTime<Utc>,
) -> Result<(Counters, Counters), sqlx::Error> {
    let state = read_state(tx, workspace, lineage, at, false, true).await?;
    let get = |key: Option<Uuid>| {
        state
            .counters
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, c)| *c)
            .unwrap_or_default()
    };
    Ok((get(None), get(Some(lineage))))
}

async fn read_state(
    tx: &mut Transaction<'_, Postgres>,
    workspace: Uuid,
    lineage: Uuid,
    at: DateTime<Utc>,
    policies: bool,
    counters: bool,
) -> Result<LimitState, sqlx::Error> {
    type Row = (
        i32,
        i64,
        Option<Uuid>,
        Option<Uuid>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(&format!(
        r#"WITH pol AS MATERIALIZED (SELECT row_number() OVER () i,p.* FROM ({policies_sql}) p WHERE $4),
        bud AS MATERIALIZED (SELECT row_number() OVER () i,b.* FROM ({budgets_sql}) b),
        expired AS MATERIALIZED (SELECT k.governance_key_id lineage,c.inflight,c.jobs
          FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id
          JOIN api_keys k ON k.id=r.api_key_id LEFT JOIN async_jobs j ON j.execution_id=r.execution_id
          CROSS JOIN LATERAL rate_contribution(r,e.workload_kind,e.batch_job_id,j.id IS NOT NULL,j.state,j.cancel_requested_at,1) c
          WHERE r.state='pending' AND r.lease_expires_at<=$3 AND r.workspace_id=$1)
        SELECT * FROM (
        SELECT 1 g,i,workspace_id,api_key_id,requests_per_minute::bigint a,tokens_per_minute::bigint b,concurrent_requests::bigint c,concurrent_jobs::bigint d,NULL::text period,NULL::text v FROM pol
        UNION ALL
        SELECT 2,b.i,b.workspace_id,b.api_key_id,b.amount_microusd::bigint,(coalesce(t.unresolved+t.unreserved_executions,0)>0)::int::bigint,NULL,NULL,b.period,coalesce(t.settled_microusd+t.held_microusd,0)::text
         FROM bud b LEFT JOIN budget_totals t ON t.scope_kind=CASE WHEN b.api_key_id IS NULL THEN 'workspace' ELSE 'key' END
         AND t.scope_id=coalesce(b.api_key_id,b.workspace_id) AND t.period=b.period
         AND t.period_start=CASE WHEN b.period='lifetime' THEN 'epoch'::timestamptz ELSE date_trunc(b.period,$3::timestamptz,'UTC') END
        UNION ALL
        SELECT 3,s.i,$1,CASE WHEN s.kind='key' THEN $2::uuid END,coalesce(m.requests,0),coalesce(m.unreserved,0),coalesce(f.requests,0)-x.inflight,coalesce(f.jobs,0)-x.jobs,NULL,coalesce(m.tokens,0)::text
         FROM (VALUES ('workspace',$1::uuid,1::bigint),('key',$2::uuid,2::bigint)) s(kind,id,i)
         LEFT JOIN rate_minute_counters m ON m.minute_start=date_trunc('minute',$3::timestamptz,'UTC') AND m.scope_kind=s.kind AND m.scope_id=s.id
         LEFT JOIN inflight_counters f ON f.scope_kind=s.kind AND f.scope_id=s.id
         CROSS JOIN LATERAL (SELECT coalesce(sum(d.inflight),0)::bigint inflight,coalesce(sum(d.jobs),0)::bigint jobs FROM expired d WHERE s.kind='workspace' OR d.lineage=s.id) x
         WHERE $5 OR EXISTS(SELECT 1 FROM pol)
        ) q ORDER BY g,i"#,
        policies_sql = super::POLICIES,
        budgets_sql = super::BUDGETS,
    ))
    .bind(workspace)
    .bind(lineage)
    .bind(at)
    .bind(policies)
    .bind(counters)
    .fetch_all(&mut **tx)
    .await?;
    let decode = |e: std::num::ParseIntError| sqlx::Error::Decode(Box::new(e));
    let mut state = LimitState {
        policies: Vec::new(),
        budgets: Vec::new(),
        counters: Vec::new(),
    };
    for (g, _, _workspace_id, api_key_id, a, b, c, d, period, v) in rows {
        match g {
            1 => state.policies.push(super::Policy {
                api_key_id,
                requests_per_minute: a,
                tokens_per_minute: b,
                concurrent_requests: c,
                concurrent_jobs: d,
            }),
            2 => state.budgets.push((
                super::Budget {
                    api_key_id,
                    period: period.unwrap_or_default(),
                    amount_microusd: a.unwrap_or_default(),
                },
                Consumption {
                    used_microusd: v.unwrap_or_default().parse().map_err(decode)?,
                    unresolved: b.unwrap_or_default() > 0,
                },
            )),
            _ => state.counters.push((
                api_key_id,
                Counters {
                    requests: a.unwrap_or_default(),
                    unreserved: b.unwrap_or_default(),
                    tokens: v.unwrap_or_default().parse().map_err(decode)?,
                    inflight: c.unwrap_or_default(),
                    jobs: d.unwrap_or_default(),
                },
            )),
        }
    }
    Ok(state)
}

/// Deletes up to `limit` minute rows that admission no longer reads (older
/// than [`RETAINED_MINUTES`]). Concurrent pruners skip each other's rows.
pub async fn prune(store: &crate::store::Store, limit: i64) -> Result<u64, sqlx::Error> {
    prune_fenced(store, limit, None).await
}

/// [`prune`] in a transaction fenced to a `maintenance` lease term.
pub async fn prune_fenced(
    store: &crate::store::Store,
    limit: i64,
    fence: Option<&crate::leases::Fence>,
) -> Result<u64, sqlx::Error> {
    let mut tx = crate::db::begin(&store.pool).await?;
    crate::leases::fence(&mut tx, fence).await?;
    let pruned = sqlx::query("DELETE FROM rate_minute_counters WHERE (minute_start,scope_kind,scope_id) IN (SELECT minute_start,scope_kind,scope_id FROM rate_minute_counters WHERE minute_start<date_trunc('minute',clock_timestamp(),'UTC')-make_interval(mins=>$1) ORDER BY 1,2,3 LIMIT $2 FOR UPDATE SKIP LOCKED)")
        .bind(RETAINED_MINUTES)
        .bind(limit)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(pruned)
}

/// The former scan's classification of every reservation and unreserved
/// execution, attributed to each counted scope (workspace, key lineage).
const CLASSIFIED: &str = r#"a AS (
  SELECT r.workspace_id,r.api_key_id,r.minute_start,r.state,r.reserved_tokens,
   rate_reserved_tokens(r.reserved_tokens,r.input_tokens,r.output_tokens,r.billing_usage) used_tokens,
   e.workload_kind IN('videos','batches') OR e.batch_job_id IS NOT NULL AS job,
   j.id IS NOT NULL OR e.batch_job_id IS NOT NULL AS accepted,
   coalesce(j.state IN('completed','failed','cancelled','expired') OR j.cancel_requested_at IS NOT NULL,e.batch_job_id IS NOT NULL) AS job_done,
   false AS execution_only
  FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id LEFT JOIN async_jobs j ON j.execution_id=r.execution_id
  WHERE r.state='pending' OR r.minute_start>=$1
  UNION ALL SELECT e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),'unknown',NULL,0,false,false,false,true
  FROM inference_executions e WHERE e.started_at>=$1 AND NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=e.id)),
s AS (SELECT a.*,sc.kind,sc.id FROM a JOIN api_keys k ON k.id=a.api_key_id
  CROSS JOIN LATERAL (VALUES('workspace',a.workspace_id),('key',k.governance_key_id)) sc(kind,id))"#;

/// One counter row that differs from the scan.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub struct Mismatch {
    /// `minute` or `inflight`.
    pub counter: String,
    pub minute_start: Option<DateTime<Utc>>,
    pub scope_kind: String,
    pub scope_id: Uuid,
    /// `requests,unreserved,tokens` (minute) or `requests,jobs` (in-flight).
    pub maintained: String,
    pub scanned: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RateReport {
    pub buckets: i64,
    pub mismatch_count: i64,
    pub mismatches: Vec<Mismatch>,
}

/// Compares the in-flight counters and the minute counters of the last five
/// minutes (and later) with a full scan. Older minutes are not read by
/// admission and may be pruned, so they are not compared.
pub(crate) async fn verify_in(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<RateReport, sqlx::Error> {
    let horizon: DateTime<Utc> = sqlx::query_scalar(
        "SELECT date_trunc('minute',clock_timestamp(),'UTC')-interval '5 minutes'",
    )
    .fetch_one(&mut **tx)
    .await?;
    let compared = format!(
        r#"WITH {CLASSIFIED},
        em AS (SELECT minute_start,kind,id,count(*) FILTER(WHERE NOT job) requests,
          count(*) FILTER(WHERE NOT job AND reserved_tokens IS NULL) unreserved,
          coalesce(sum(used_tokens) FILTER(WHERE NOT job AND NOT execution_only),0) tokens
          FROM s WHERE minute_start>=$1 GROUP BY 1,2,3),
        ef AS (SELECT kind,id,count(*) FILTER(WHERE NOT accepted) requests,
          count(*) FILTER(WHERE job AND NOT job_done) jobs FROM s WHERE state='pending' AND NOT execution_only GROUP BY 1,2),
        j AS (
          SELECT 'minute' counter,coalesce(t.minute_start,em.minute_start) minute_start,coalesce(t.scope_kind,em.kind) scope_kind,coalesce(t.scope_id,em.id) scope_id,
           concat_ws(',',coalesce(t.requests,0),coalesce(t.unreserved,0),coalesce(t.tokens,0)) maintained,
           concat_ws(',',coalesce(em.requests,0),coalesce(em.unreserved,0),coalesce(em.tokens,0)) scanned
          FROM (SELECT * FROM rate_minute_counters WHERE minute_start>=$1) t
          FULL JOIN em ON em.minute_start=t.minute_start AND em.kind=t.scope_kind AND em.id=t.scope_id
          UNION ALL
          SELECT 'inflight',NULL::timestamptz,coalesce(t.scope_kind,ef.kind),coalesce(t.scope_id,ef.id),
           concat_ws(',',coalesce(t.requests,0),coalesce(t.jobs,0)),
           concat_ws(',',coalesce(ef.requests,0),coalesce(ef.jobs,0))
          FROM inflight_counters t FULL JOIN ef ON ef.kind=t.scope_kind AND ef.id=t.scope_id)"#
    );
    let (buckets, mismatch_count): (i64, i64) = sqlx::query_as(&format!(
        "{compared} SELECT count(*),count(*) FILTER(WHERE maintained<>scanned) FROM j"
    ))
    .bind(horizon)
    .fetch_one(&mut **tx)
    .await?;
    let mismatches = if mismatch_count > 0 {
        sqlx::query_as(&format!(
            "{compared} SELECT counter,minute_start,scope_kind,scope_id,maintained,scanned FROM j WHERE maintained<>scanned ORDER BY 1,2,3,4 LIMIT 20"
        ))
        .bind(horizon)
        .fetch_all(&mut **tx)
        .await?
    } else {
        Vec::new()
    };
    Ok(RateReport {
        buckets,
        mismatch_count,
        mismatches,
    })
}

/// The former admission-time scan (`RATE_ACCOUNTING` before 0024), kept
/// verbatim as the test oracle for the maintained counters. `$1` workspace,
/// `$2` key lineage, `$3` admission time, `$4` requests/min, `$5` tokens/min,
/// `$6` requests at once, `$7` reserved tokens, `$8` jobs at once. Returns
/// (rate limits ok, job limit ok). Admission no longer runs it: the
/// installation policy layer, its last user, was removed in 0026.
#[cfg(test)]
pub(crate) const SCAN: &str = r#"WITH accounting AS (
  SELECT r.workspace_id,r.api_key_id,r.minute_start,r.state,r.lease_expires_at,r.reserved_tokens,r.input_tokens,r.output_tokens,r.billing_usage,e.workload_kind IN('videos','batches') OR e.batch_job_id IS NOT NULL AS job,j.id IS NOT NULL OR e.batch_job_id IS NOT NULL AS accepted,coalesce(j.state IN('completed','failed','cancelled','expired') OR j.cancel_requested_at IS NOT NULL,e.batch_job_id IS NOT NULL) AS job_done FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id LEFT JOIN async_jobs j ON j.execution_id=r.execution_id WHERE r.minute_start=date_trunc('minute',$3::timestamptz,'UTC') OR (r.state='pending' AND r.lease_expires_at>$3)
  UNION ALL SELECT e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),'unknown',NULL::timestamptz,NULL::bigint,e.input_tokens,e.output_tokens,e.billing_usage,false,false,false FROM inference_executions e WHERE e.started_at>=date_trunc('minute',$3::timestamptz,'UTC') AND e.started_at<date_trunc('minute',$3::timestamptz,'UTC')+interval '1 minute' AND NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=e.id)
  ) SELECT
  ($4::bigint IS NULL OR count(*) FILTER(WHERE NOT job AND minute_start=date_trunc('minute',$3::timestamptz,'UTC'))::numeric+1<=$4)
  AND ($5::bigint IS NULL OR (count(*) FILTER(WHERE NOT job AND minute_start=date_trunc('minute',$3::timestamptz,'UTC') AND reserved_tokens IS NULL)=0 AND coalesce(sum(greatest(coalesce(reserved_tokens,0)::numeric,coalesce(input_tokens,0)::numeric+coalesce(output_tokens,0)::numeric,coalesce((billing_usage->>'total_input_tokens')::numeric,0)+coalesce(output_tokens,0)::numeric,coalesce((billing_usage->>'uncached_input_tokens')::numeric,0)+coalesce((billing_usage->>'cache_read_input_tokens')::numeric,0)+greatest(coalesce((billing_usage->>'cache_write_input_tokens')::numeric,0),coalesce((billing_usage->>'cache_write_default_input_tokens')::numeric,0)+coalesce((billing_usage->>'cache_write_5m_input_tokens')::numeric,0)+coalesce((billing_usage->>'cache_write_1h_input_tokens')::numeric,0))+coalesce(output_tokens,0)::numeric)) FILTER(WHERE NOT job AND minute_start=date_trunc('minute',$3::timestamptz,'UTC')),0)+$7::bigint<=$5))
  AND ($6::bigint IS NULL OR count(*) FILTER(WHERE state='pending' AND lease_expires_at>$3 AND NOT accepted)::numeric+1<=$6),
  ($8::bigint IS NULL OR count(*) FILTER(WHERE job AND state='pending' AND lease_expires_at>$3 AND NOT job_done)::numeric+1<=$8)
  FROM accounting WHERE ($1::uuid IS NULL OR workspace_id=$1) AND ($2::uuid IS NULL OR api_key_id IN(SELECT id FROM api_keys WHERE workspace_id=$1 AND governance_key_id=$2))"#;

/// The quantities behind [`SCAN`]'s predicates for one scope at `at`
/// (requests, unreserved, tokens of reservations, live in-flight, live jobs),
/// kept as a second oracle that pins the components, not just the verdicts.
#[cfg(all(test, feature = "integration-tests"))]
pub(crate) async fn scan_counters(
    tx: &mut Transaction<'_, Postgres>,
    workspace: Option<Uuid>,
    lineage: Option<Uuid>,
    at: DateTime<Utc>,
) -> Result<Counters, sqlx::Error> {
    let row: (i64, i64, String, i64, i64) = sqlx::query_as(r#"WITH accounting AS (
      SELECT r.workspace_id,r.api_key_id,r.minute_start,r.state,r.lease_expires_at,r.reserved_tokens,r.input_tokens,r.output_tokens,r.billing_usage,e.workload_kind IN('videos','batches') OR e.batch_job_id IS NOT NULL AS job,j.id IS NOT NULL OR e.batch_job_id IS NOT NULL AS accepted,coalesce(j.state IN('completed','failed','cancelled','expired') OR j.cancel_requested_at IS NOT NULL,e.batch_job_id IS NOT NULL) AS job_done,false AS execution_only FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id LEFT JOIN async_jobs j ON j.execution_id=r.execution_id WHERE r.minute_start=date_trunc('minute',$3::timestamptz,'UTC') OR (r.state='pending' AND r.lease_expires_at>$3)
      UNION ALL SELECT e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),'unknown',NULL::timestamptz,NULL::bigint,e.input_tokens,e.output_tokens,e.billing_usage,false,false,false,true FROM inference_executions e WHERE e.started_at>=date_trunc('minute',$3::timestamptz,'UTC') AND e.started_at<date_trunc('minute',$3::timestamptz,'UTC')+interval '1 minute' AND NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=e.id)
      ) SELECT count(*) FILTER(WHERE NOT job AND minute_start=date_trunc('minute',$3::timestamptz,'UTC')),
      count(*) FILTER(WHERE NOT job AND minute_start=date_trunc('minute',$3::timestamptz,'UTC') AND reserved_tokens IS NULL),
      coalesce(sum(greatest(coalesce(reserved_tokens,0)::numeric,coalesce(input_tokens,0)::numeric+coalesce(output_tokens,0)::numeric,coalesce((billing_usage->>'total_input_tokens')::numeric,0)+coalesce(output_tokens,0)::numeric,coalesce((billing_usage->>'uncached_input_tokens')::numeric,0)+coalesce((billing_usage->>'cache_read_input_tokens')::numeric,0)+greatest(coalesce((billing_usage->>'cache_write_input_tokens')::numeric,0),coalesce((billing_usage->>'cache_write_default_input_tokens')::numeric,0)+coalesce((billing_usage->>'cache_write_5m_input_tokens')::numeric,0)+coalesce((billing_usage->>'cache_write_1h_input_tokens')::numeric,0))+coalesce(output_tokens,0)::numeric)) FILTER(WHERE NOT job AND NOT execution_only AND minute_start=date_trunc('minute',$3::timestamptz,'UTC')),0)::text,
      count(*) FILTER(WHERE state='pending' AND lease_expires_at>$3 AND NOT accepted),
      count(*) FILTER(WHERE job AND state='pending' AND lease_expires_at>$3 AND NOT job_done)
      FROM accounting WHERE ($1::uuid IS NULL OR workspace_id=$1) AND ($2::uuid IS NULL OR api_key_id IN(SELECT id FROM api_keys WHERE workspace_id=$1 AND governance_key_id=$2))"#)
        .bind(workspace).bind(lineage).bind(at).fetch_one(&mut **tx).await?;
    Ok(Counters {
        requests: row.0,
        unreserved: row.1,
        tokens: row.2.parse().expect("integer tokens"),
        inflight: row.3,
        jobs: row.4,
    })
}

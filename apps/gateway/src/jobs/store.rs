//! Job and job-file rows (migration 0016). Every client lookup is scoped by
//! the principal's workspace. Metadata only.
use chrono::{DateTime, Utc};
use sqlx::FromRow;
use uuid::Uuid;

use super::types::*;
use crate::{inference::types::Deployment, store::Store};

type Result<T> = std::result::Result<T, sqlx::Error>;

pub(crate) const JOB_COLUMNS: &str = "id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,state,upstream_status,progress,error_code,created_at,completed_at,expires_at,cancel_requested_at,deleted_at,poll_deadline_at,settled_at,video_seconds,video_size,batch_endpoint,request_total,request_completed,request_failed,batch_mode,user_id,input_file_id,work_file_id,output_file_id,error_file_id,price_tier,retry_limit,submit_started_at,in_progress_at,finalizing_at,last_progress_at,completion_window_hours";

#[derive(Clone, Debug, FromRow)]
pub struct JobRow {
    pub id: Uuid,
    pub kind: String,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub deployment_id: Uuid,
    pub execution_id: Uuid,
    pub public_model: String,
    pub provider: String,
    /// `None` for gateway-run batches and native batches not yet submitted.
    pub upstream_id: Option<String>,
    pub state: String,
    pub upstream_status: Option<String>,
    pub progress: Option<i16>,
    pub error_code: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub cancel_requested_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub poll_deadline_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
    pub video_seconds: Option<i32>,
    pub video_size: Option<String>,
    pub batch_endpoint: Option<String>,
    pub request_total: Option<i32>,
    pub request_completed: Option<i32>,
    pub request_failed: Option<i32>,
    /// `native`, `gateway`, or `None` for a 0016 passthrough batch.
    pub batch_mode: Option<String>,
    pub user_id: Option<Uuid>,
    pub input_file_id: Option<Uuid>,
    pub work_file_id: Option<Uuid>,
    pub output_file_id: Option<Uuid>,
    pub error_file_id: Option<Uuid>,
    /// Native batches: `batch` when the batch price list applied.
    pub price_tier: Option<String>,
    pub retry_limit: i16,
    pub submit_started_at: Option<DateTime<Utc>>,
    pub in_progress_at: Option<DateTime<Utc>>,
    pub finalizing_at: Option<DateTime<Utc>>,
    pub last_progress_at: Option<DateTime<Utc>>,
    /// Batches (0022): the completion window in hours (`None`: 24).
    pub completion_window_hours: Option<i16>,
}
impl JobRow {
    /// A batch's completion window (24 h unless created with a longer one).
    pub fn completion_window(&self) -> chrono::TimeDelta {
        chrono::TimeDelta::hours(i64::from(self.completion_window_hours.unwrap_or(24)))
    }
    pub fn mode(&self) -> Option<BatchMode> {
        match self.batch_mode.as_deref() {
            Some("native") => Some(BatchMode::Native),
            Some("gateway") => Some(BatchMode::Gateway),
            _ => None,
        }
    }
    pub fn endpoint(&self) -> Option<BatchEndpoint> {
        self.batch_endpoint
            .as_deref()
            .and_then(BatchEndpoint::parse)
    }
    /// The principal the batch runs as (live authorization is rechecked at
    /// every admission).
    pub fn principal(&self) -> crate::auth::Principal {
        crate::auth::Principal {
            key_id: self.api_key_id,
            workspace_id: self.workspace_id,
            user_id: self.user_id,
        }
    }
    pub fn job_state(&self) -> JobState {
        // The column is CHECK-constrained to the same set.
        JobState::parse(&self.state).unwrap_or(JobState::Failed)
    }
    pub fn job_kind(&self) -> Option<JobKind> {
        JobKind::parse(&self.kind)
    }
    pub fn upstream(&self) -> Option<UpstreamId> {
        self.upstream_id.as_deref().and_then(UpstreamId::parse)
    }
}

/// How a batch executes (0021).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchMode {
    /// On the provider's batch API (one upstream attempt).
    Native,
    /// Line by line through the gateway's inference engine.
    Gateway,
}
impl BatchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Gateway => "gateway",
        }
    }
}

pub(crate) struct NewJob<'a> {
    pub id: Uuid,
    pub kind: JobKind,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub deployment_id: Uuid,
    pub execution_id: Uuid,
    pub public_model: &'a str,
    pub provider: &'a str,
    pub upstream_id: &'a UpstreamId,
    pub poll_deadline_at: DateTime<Utc>,
    pub video_seconds: Option<i32>,
    pub video_size: Option<&'a str>,
    pub batch_endpoint: Option<&'a str>,
}
pub(crate) async fn insert_job(store: &Store, job: &NewJob<'_>) -> Result<JobRow> {
    sqlx::query_as(&format!("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,poll_deadline_at,video_seconds,video_size,batch_endpoint) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) RETURNING {JOB_COLUMNS}"))
        .bind(job.id).bind(job.kind.as_str()).bind(job.workspace_id).bind(job.api_key_id).bind(job.deployment_id).bind(job.execution_id).bind(job.public_model).bind(job.provider).bind(job.upstream_id.as_str()).bind(job.poll_deadline_at).bind(job.video_seconds).bind(job.video_size).bind(job.batch_endpoint)
        .fetch_one(&store.pool).await
}
/// A new engine batch (0021): created before any upstream work.
pub(crate) struct NewBatch<'a> {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub user_id: Option<Uuid>,
    pub deployment_id: Uuid,
    pub execution_id: Uuid,
    pub public_model: &'a str,
    pub provider: &'a str,
    pub poll_deadline_at: DateTime<Utc>,
    pub endpoint: BatchEndpoint,
    pub mode: BatchMode,
    pub input_file_id: Uuid,
    pub work_file_id: Uuid,
    pub price_tier: Option<&'a str>,
    pub retry_limit: i16,
    pub requests: i32,
    pub window_hours: i16,
}
pub(crate) async fn insert_batch(store: &Store, b: &NewBatch<'_>) -> Result<JobRow> {
    sqlx::query_as(&format!("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,poll_deadline_at,batch_endpoint,batch_mode,user_id,input_file_id,work_file_id,price_tier,retry_limit,request_total,request_completed,request_failed,upstream_status,completion_window_hours) VALUES($1,'batch',$2,$3,$4,$5,$6,$7,NULL,$8,$9,$10,$11,$12,$13,$14,$15,$16,0,0,'validating',$17) RETURNING {JOB_COLUMNS}"))
        .bind(b.id).bind(b.workspace_id).bind(b.api_key_id).bind(b.deployment_id).bind(b.execution_id).bind(b.public_model).bind(b.provider).bind(b.poll_deadline_at).bind(b.endpoint.as_str()).bind(b.mode.as_str()).bind(b.user_id).bind(b.input_file_id).bind(b.work_file_id).bind(b.price_tier).bind(b.retry_limit).bind(b.requests).bind(b.window_hours)
        .fetch_one(&store.pool).await
}
/// A job of `kind` owned by `workspace` (anything else is not found).
pub(crate) async fn job(
    store: &Store,
    workspace: Uuid,
    kind: JobKind,
    id: Uuid,
) -> Result<Option<JobRow>> {
    sqlx::query_as(&format!(
        "SELECT {JOB_COLUMNS} FROM async_jobs WHERE id=$1 AND workspace_id=$2 AND kind=$3"
    ))
    .bind(id)
    .bind(workspace)
    .bind(kind.as_str())
    .fetch_optional(&store.pool)
    .await
}
pub(crate) async fn reload(store: &Store, id: Uuid) -> Result<JobRow> {
    sqlx::query_as(&format!("SELECT {JOB_COLUMNS} FROM async_jobs WHERE id=$1"))
        .bind(id)
        .fetch_one(&store.pool)
        .await
}
/// Cursor page of the workspace's jobs of one kind (newest first by default).
pub(crate) async fn list(
    store: &Store,
    workspace: Uuid,
    kind: JobKind,
    after: Option<Uuid>,
    limit: i64,
    ascending: bool,
) -> Result<Vec<JobRow>> {
    let (cmp, order) = if ascending {
        (">", "ASC")
    } else {
        ("<", "DESC")
    };
    sqlx::query_as(&format!("SELECT {JOB_COLUMNS} FROM async_jobs j WHERE workspace_id=$1 AND kind=$2 AND deleted_at IS NULL AND ($3::uuid IS NULL OR (created_at,id){cmp}(SELECT created_at,id FROM async_jobs WHERE id=$3 AND workspace_id=$1 AND kind=$2)) ORDER BY created_at {order},id {order} LIMIT $4"))
        .bind(workspace).bind(kind.as_str()).bind(after).bind(limit)
        .fetch_all(&store.pool).await
}

/// One provider observation, applied only along the forward-only machine.
pub(crate) struct Observation<'a> {
    pub state: JobState,
    pub upstream_status: &'a str,
    pub progress: Option<u8>,
    pub completed_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub error: Option<&'a ErrorCode>,
    pub counts: Option<RequestCounts>,
}
fn at(unix: Option<i64>) -> Option<DateTime<Utc>> {
    unix.and_then(|s| DateTime::from_timestamp(s, 0))
}
/// Apply `obs` to `job` (regressions are ignored). Returns the current row.
pub(crate) async fn observe(
    store: &Store,
    job: &JobRow,
    obs: &Observation<'_>,
    poll_interval_seconds: f64,
) -> Result<JobRow> {
    let current = job.job_state();
    let next = if current.may_become(obs.state) {
        obs.state
    } else {
        current
    };
    // A terminal row keeps its recorded terminal facts.
    let changed: Option<JobRow> = sqlx::query_as(&format!("UPDATE async_jobs SET state=$3,upstream_status=CASE WHEN state IN('queued','in_progress') THEN $4 ELSE upstream_status END,progress=coalesce($5,progress),completed_at=CASE WHEN $3 IN('completed','failed','cancelled','expired') THEN coalesce(completed_at,$6,clock_timestamp()) ELSE completed_at END,expires_at=coalesce($7,expires_at),error_code=coalesce(error_code,$8),request_total=coalesce($9,request_total),request_completed=coalesce($10,request_completed),request_failed=coalesce($11,request_failed),last_polled_at=clock_timestamp(),poll_failures=0,next_poll_at=clock_timestamp()+make_interval(secs=>$12) WHERE id=$1 AND state=$2 RETURNING {JOB_COLUMNS}"))
        .bind(job.id).bind(current.as_str()).bind(next.as_str()).bind(obs.upstream_status).bind(obs.progress.map(i16::from)).bind(at(obs.completed_at)).bind(at(obs.expires_at)).bind(obs.error.map(|e| e.as_str())).bind(obs.counts.map(|c| c.total as i32)).bind(obs.counts.map(|c| c.completed as i32)).bind(obs.counts.map(|c| c.failed as i32)).bind(poll_interval_seconds)
        .fetch_optional(&store.pool).await?;
    match changed {
        Some(row) => Ok(row),
        // A concurrent observer moved the row first.
        None => reload(store, job.id).await,
    }
}
/// A successful poll without a state change (resets the backoff).
pub(crate) async fn poll_ok(store: &Store, id: Uuid, interval_seconds: f64) -> Result<()> {
    sqlx::query("UPDATE async_jobs SET poll_failures=0,last_polled_at=clock_timestamp(),next_poll_at=clock_timestamp()+make_interval(secs=>$2) WHERE id=$1")
        .bind(id)
        .bind(interval_seconds)
        .execute(&store.pool)
        .await?;
    Ok(())
}
/// Poll failure: exponential backoff (interval × 2^failures, at most 32×).
pub(crate) async fn poll_failed(store: &Store, id: Uuid, interval_seconds: f64) -> Result<()> {
    sqlx::query("UPDATE async_jobs SET poll_failures=poll_failures+1,last_polled_at=clock_timestamp(),next_poll_at=clock_timestamp()+make_interval(secs=>$2*power(2,least(poll_failures,5))) WHERE id=$1")
        .bind(id).bind(interval_seconds).execute(&store.pool).await?;
    Ok(())
}
pub(crate) async fn mark_settled(store: &Store, id: Uuid) -> Result<()> {
    sqlx::query("UPDATE async_jobs SET settled_at=clock_timestamp() WHERE id=$1 AND settled_at IS NULL AND state IN('completed','failed','cancelled','expired')")
        .bind(id)
        .execute(&store.pool)
        .await?;
    Ok(())
}
pub(crate) async fn mark_cancel_requested(store: &Store, id: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE async_jobs SET cancel_requested_at=coalesce(cancel_requested_at,clock_timestamp()) WHERE id=$1",
    )
    .bind(id)
    .execute(&store.pool)
    .await?;
    Ok(())
}
pub(crate) async fn mark_deleted(store: &Store, id: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE async_jobs SET deleted_at=coalesce(deleted_at,clock_timestamp()) WHERE id=$1",
    )
    .bind(id)
    .execute(&store.pool)
    .await?;
    Ok(())
}
/// Claim up to `limit` due, unsettled provider jobs within their poll
/// deadline, pushing their next poll forward so concurrent pollers skip
/// them. Gateway-run batches belong to the batch runner, never the poller.
pub(crate) async fn claim_due(
    store: &Store,
    limit: i64,
    interval_seconds: f64,
) -> Result<Vec<JobRow>> {
    sqlx::query_as(&format!("UPDATE async_jobs SET next_poll_at=clock_timestamp()+make_interval(secs=>$2) WHERE id IN(SELECT id FROM async_jobs WHERE settled_at IS NULL AND batch_mode IS DISTINCT FROM 'gateway' AND next_poll_at<=clock_timestamp() AND poll_deadline_at>clock_timestamp() ORDER BY next_poll_at,id LIMIT $1 FOR UPDATE SKIP LOCKED) RETURNING {JOB_COLUMNS}"))
        .bind(limit).bind(interval_seconds).fetch_all(&store.pool).await
}
pub(crate) async fn reservation_pending(store: &Store, execution: Uuid) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT coalesce((SELECT state='pending' FROM governance_reservations WHERE execution_id=$1),false)",
    )
    .bind(execution)
    .fetch_one(&store.pool)
    .await
}
/// The job's pinned deployment while it and its connection are enabled.
pub(crate) async fn target(store: &Store, deployment: Uuid) -> Result<Option<Deployment>> {
    sqlx::query_as("SELECT d.id,p.provider,d.upstream_model,p.credential_ref,p.endpoint,p.region,m.supported_protocols FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id JOIN models m ON m.id=d.model_id WHERE d.id=$1 AND d.enabled AND p.enabled")
        .bind(deployment)
        .fetch_optional(&store.pool)
        .await
}

// ----------------------------------------------------------- Legacy files ----

/// Gateway file ids of a 0016 passthrough batch: (input, output, error).
/// Their contents lived at the provider and are no longer served.
pub(crate) async fn legacy_files(
    store: &Store,
    job: Uuid,
) -> Result<(Option<Uuid>, Option<Uuid>, Option<Uuid>)> {
    let rows: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id,purpose FROM async_job_files WHERE job_id=$1")
            .bind(job)
            .fetch_all(&store.pool)
            .await?;
    let pick = |p: &str| rows.iter().find(|r| r.1 == p).map(|r| r.0);
    Ok((pick("batch"), pick("batch_output"), pick("batch_error")))
}

// ---------------------------------------------------------- Engine batches ----

/// Record a native batch's upstream id (once).
pub(crate) async fn set_upstream(store: &Store, id: Uuid, upstream: &UpstreamId) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE async_jobs SET upstream_id=$2 WHERE id=$1 AND upstream_id IS NULL")
            .bind(id)
            .bind(upstream.as_str())
            .execute(&store.pool)
            .await?
            .rows_affected()
            == 1,
    )
}
/// Claim a native batch's submission: exclusive with an early cancel.
pub(crate) async fn claim_submit(store: &Store, id: Uuid) -> Result<bool> {
    Ok(sqlx::query("UPDATE async_jobs SET submit_started_at=clock_timestamp() WHERE id=$1 AND submit_started_at IS NULL AND upstream_id IS NULL AND cancel_requested_at IS NULL AND state='queued'")
        .bind(id).execute(&store.pool).await?.rows_affected() == 1)
}
/// Cancel a native batch whose submission never started (nothing upstream).
pub(crate) async fn cancel_unsubmitted(store: &Store, id: Uuid) -> Result<bool> {
    Ok(sqlx::query("UPDATE async_jobs SET cancel_requested_at=coalesce(cancel_requested_at,clock_timestamp()),state='cancelled',upstream_status='cancelled',completed_at=clock_timestamp() WHERE id=$1 AND submit_started_at IS NULL AND upstream_id IS NULL AND state='queued'")
        .bind(id).execute(&store.pool).await?.rows_affected() == 1)
}
/// Display status (`validating`, `in_progress`, `finalizing`, `cancelling`)
/// of an unfinished batch.
pub(crate) async fn set_status(store: &Store, id: Uuid, status: &str) -> Result<()> {
    sqlx::query("UPDATE async_jobs SET upstream_status=$2,finalizing_at=CASE WHEN $2='finalizing' THEN coalesce(finalizing_at,clock_timestamp()) ELSE finalizing_at END WHERE id=$1 AND state IN('queued','in_progress')")
        .bind(id)
        .bind(status)
        .execute(&store.pool)
        .await?;
    Ok(())
}
pub(crate) async fn mark_running(store: &Store, id: Uuid) -> Result<()> {
    sqlx::query("UPDATE async_jobs SET state='in_progress',upstream_status=CASE WHEN cancel_requested_at IS NULL THEN 'in_progress' ELSE upstream_status END,in_progress_at=coalesce(in_progress_at,clock_timestamp()),last_progress_at=coalesce(last_progress_at,clock_timestamp()) WHERE id=$1 AND state='queued'")
        .bind(id)
        .execute(&store.pool)
        .await?;
    Ok(())
}
/// Lines finished since the last call.
pub(crate) async fn progress(store: &Store, id: Uuid, completed: i32, failed: i32) -> Result<()> {
    sqlx::query("UPDATE async_jobs SET request_completed=coalesce(request_completed,0)+$2,request_failed=coalesce(request_failed,0)+$3,last_progress_at=clock_timestamp() WHERE id=$1")
        .bind(id)
        .bind(completed)
        .bind(failed)
        .execute(&store.pool)
        .await?;
    Ok(())
}
/// Terminal state of an engine batch with its result files.
pub(crate) struct Finished<'a> {
    pub state: JobState,
    pub error_code: Option<&'a str>,
    pub output_file: Option<Uuid>,
    pub error_file: Option<Uuid>,
    pub counts: Option<RequestCounts>,
}
pub(crate) async fn finish_batch(store: &Store, id: Uuid, f: &Finished<'_>) -> Result<JobRow> {
    sqlx::query("UPDATE async_jobs SET state=$2,upstream_status=$2,error_code=coalesce(error_code,$3),output_file_id=coalesce(output_file_id,$4),error_file_id=coalesce(error_file_id,$5),request_total=coalesce($6,request_total),request_completed=coalesce($7,request_completed),request_failed=coalesce($8,request_failed),completed_at=coalesce(completed_at,clock_timestamp()) WHERE id=$1 AND state IN('queued','in_progress')")
        .bind(id).bind(f.state.as_str()).bind(f.error_code).bind(f.output_file).bind(f.error_file)
        .bind(f.counts.map(|c| c.total as i32)).bind(f.counts.map(|c| c.completed as i32)).bind(f.counts.map(|c| c.failed as i32))
        .execute(&store.pool).await?;
    reload(store, id).await
}
/// Observed provider progress (native): counts and the stall clock. The
/// gateway counted the lines at admission, so a provider total never replaces
/// it (OpenAI reports `total: 0` while it is still validating).
pub(crate) async fn observe_counts(store: &Store, id: Uuid, counts: RequestCounts) -> Result<()> {
    sqlx::query("UPDATE async_jobs SET last_progress_at=CASE WHEN request_completed IS DISTINCT FROM $3 OR request_failed IS DISTINCT FROM $4 THEN clock_timestamp() ELSE coalesce(last_progress_at,clock_timestamp()) END,request_total=coalesce(request_total,$2),request_completed=$3,request_failed=$4 WHERE id=$1 AND state IN('queued','in_progress')")
        .bind(id).bind(counts.total as i32).bind(counts.completed as i32).bind(counts.failed as i32)
        .execute(&store.pool).await?;
    Ok(())
}

/// Claim up to `limit` runnable gateway-run batches for this runner (a lease
/// that must be renewed; a crashed runner's batches are resumed elsewhere).
pub(crate) async fn claim_gateway(
    store: &Store,
    runner: Uuid,
    lease_seconds: f64,
    limit: i64,
) -> Result<Vec<JobRow>> {
    sqlx::query_as(&format!("UPDATE async_jobs SET runner_id=$1,runner_lease_until=clock_timestamp()+make_interval(secs=>$2) WHERE id IN(SELECT id FROM async_jobs WHERE batch_mode='gateway' AND settled_at IS NULL AND (runner_lease_until IS NULL OR runner_lease_until<clock_timestamp()) ORDER BY created_at,id LIMIT $3 FOR UPDATE SKIP LOCKED) RETURNING {JOB_COLUMNS}"))
        .bind(runner).bind(lease_seconds).bind(limit).fetch_all(&store.pool).await
}
/// Renew this runner's lease. `None` when another runner owns the batch or
/// it is settled; otherwise whether cancel was requested.
pub(crate) async fn renew_lease(
    store: &Store,
    id: Uuid,
    runner: Uuid,
    lease_seconds: f64,
) -> Result<Option<bool>> {
    sqlx::query_scalar("UPDATE async_jobs SET runner_lease_until=clock_timestamp()+make_interval(secs=>$3) WHERE id=$1 AND runner_id=$2 AND settled_at IS NULL RETURNING cancel_requested_at IS NOT NULL")
        .bind(id).bind(runner).bind(lease_seconds).fetch_optional(&store.pool).await
}
pub(crate) async fn release_runner(store: &Store, id: Uuid, runner: Uuid) -> Result<()> {
    sqlx::query("UPDATE async_jobs SET runner_lease_until=NULL WHERE id=$1 AND runner_id=$2")
        .bind(id)
        .bind(runner)
        .execute(&store.pool)
        .await?;
    Ok(())
}

/// A line a crashed runner left `running` is `interrupted`: never executed
/// again (its attempt's hold is reconciled as unknown). Returns how many.
pub(crate) async fn interrupt_running(store: &Store, job: Uuid) -> Result<i32> {
    let n = sqlx::query("UPDATE batch_lines SET state='interrupted',error_code='interrupted',finished_at=clock_timestamp() WHERE job_id=$1 AND state='running'")
        .bind(job)
        .execute(&store.pool)
        .await?
        .rows_affected();
    Ok(i32::try_from(n).unwrap_or(i32::MAX))
}
/// `(line_no, state, segment)` of every claimed line of a batch.
pub(crate) async fn line_states(
    store: &Store,
    job: Uuid,
) -> Result<Vec<(i32, String, Option<i32>)>> {
    sqlx::query_as("SELECT line_no,state,segment FROM batch_lines WHERE job_id=$1")
        .bind(job)
        .fetch_all(&store.pool)
        .await
}
/// Claim a line's first attempt. Inserting the row is the claim, so a line
/// can never run twice (even across runners).
pub(crate) async fn claim_line(
    store: &Store,
    job: Uuid,
    workspace: Uuid,
    line_no: i32,
    execution: Uuid,
) -> Result<bool> {
    Ok(sqlx::query("INSERT INTO batch_lines(job_id,workspace_id,line_no,state,execution_id) VALUES($1,$2,$3,'running',$4) ON CONFLICT DO NOTHING")
        .bind(job).bind(workspace).bind(line_no).bind(execution).execute(&store.pool).await?.rows_affected() == 1)
}
/// Claim an explicit retry (a new attempt with its own execution).
pub(crate) async fn retry_line(
    store: &Store,
    job: Uuid,
    line_no: i32,
    attempts: i16,
    execution: Uuid,
) -> Result<bool> {
    Ok(sqlx::query("UPDATE batch_lines SET state='running',attempts=attempts+1,execution_id=$4,status_code=NULL,error_code=NULL,finished_at=NULL WHERE job_id=$1 AND line_no=$2 AND state='failed' AND attempts=$3 AND segment IS NULL")
        .bind(job).bind(line_no).bind(attempts).bind(execution).execute(&store.pool).await?.rows_affected() == 1)
}
pub(crate) async fn finish_line(
    store: &Store,
    job: Uuid,
    line_no: i32,
    succeeded: bool,
    status_code: Option<u16>,
    error_code: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE batch_lines SET state=$3,status_code=$4,error_code=$5,finished_at=clock_timestamp() WHERE job_id=$1 AND line_no=$2 AND state='running'")
        .bind(job)
        .bind(line_no)
        .bind(if succeeded { "succeeded" } else { "failed" })
        .bind(status_code.map(|s| s as i16))
        .bind(error_code)
        .execute(&store.pool)
        .await?;
    Ok(())
}
/// Record a stored result segment and the lines it holds.
pub(crate) async fn insert_segment(
    store: &Store,
    job: Uuid,
    workspace: Uuid,
    seq: i32,
    file: Uuid,
    lines: &[i32],
) -> Result<()> {
    let mut tx = store.pool.begin().await?;
    sqlx::query(
        "INSERT INTO batch_segments(job_id,workspace_id,seq,file_id,lines) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(job)
    .bind(workspace)
    .bind(seq)
    .bind(file)
    .bind(lines.len() as i32)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE batch_lines SET segment=$2 WHERE job_id=$1 AND line_no=ANY($3) AND segment IS NULL AND state<>'running'")
        .bind(job).bind(seq).bind(lines).execute(&mut *tx).await?;
    tx.commit().await
}
/// `(seq, file id)` of a batch's segments, in order.
pub(crate) async fn segments(store: &Store, job: Uuid) -> Result<Vec<(i32, Uuid)>> {
    sqlx::query_as("SELECT seq,file_id FROM batch_segments WHERE job_id=$1 ORDER BY seq")
        .bind(job)
        .fetch_all(&store.pool)
        .await
}

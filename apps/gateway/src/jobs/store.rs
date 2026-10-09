//! Job and job-file rows (migration 0016). Every client lookup is scoped by
//! the principal's workspace. Metadata only.
use chrono::{DateTime, Utc};
use sqlx::FromRow;
use uuid::Uuid;

use super::types::*;
use crate::{inference::types::Deployment, store::Store};

type Result<T> = std::result::Result<T, sqlx::Error>;

pub(crate) const JOB_COLUMNS: &str = "id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,state,upstream_status,progress,error_code,created_at,completed_at,expires_at,cancel_requested_at,deleted_at,poll_deadline_at,settled_at,video_seconds,video_size,batch_endpoint,request_total,request_completed,request_failed";

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
    pub upstream_id: String,
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
}
impl JobRow {
    pub fn job_state(&self) -> JobState {
        // The column is CHECK-constrained to the same set.
        JobState::parse(&self.state).unwrap_or(JobState::Failed)
    }
    pub fn job_kind(&self) -> Option<JobKind> {
        JobKind::parse(&self.kind)
    }
    pub fn upstream(&self) -> Option<UpstreamId> {
        UpstreamId::parse(&self.upstream_id)
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
/// Claim up to `limit` due, unsettled jobs within their poll deadline,
/// pushing their next poll forward so concurrent pollers skip them.
pub(crate) async fn claim_due(
    store: &Store,
    limit: i64,
    interval_seconds: f64,
) -> Result<Vec<JobRow>> {
    sqlx::query_as(&format!("UPDATE async_jobs SET next_poll_at=clock_timestamp()+make_interval(secs=>$2) WHERE id IN(SELECT id FROM async_jobs WHERE settled_at IS NULL AND next_poll_at<=clock_timestamp() AND poll_deadline_at>clock_timestamp() ORDER BY next_poll_at,id LIMIT $1 FOR UPDATE SKIP LOCKED) RETURNING {JOB_COLUMNS}"))
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

// ------------------------------------------------------------------ Files ----

pub(crate) const FILE_COLUMNS: &str = "id,workspace_id,api_key_id,deployment_id,public_model,upstream_id,purpose,bytes,endpoint,request_count,output_token_sum,max_line_output,job_id,claimed_by_execution_id,created_at";
#[derive(Clone, Debug, FromRow)]
pub struct FileRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub deployment_id: Uuid,
    pub public_model: String,
    pub upstream_id: String,
    pub purpose: String,
    pub bytes: Option<i64>,
    pub endpoint: Option<String>,
    pub request_count: Option<i32>,
    pub output_token_sum: Option<i64>,
    pub max_line_output: Option<i32>,
    pub job_id: Option<Uuid>,
    pub claimed_by_execution_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}
pub(crate) struct NewInputFile<'a> {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub deployment_id: Uuid,
    pub public_model: &'a str,
    pub upstream_id: &'a UpstreamId,
    pub bytes: u64,
    pub request_count: u32,
    pub output_token_sum: u64,
    pub max_line_output: u32,
}
pub(crate) async fn insert_input_file(store: &Store, f: &NewInputFile<'_>) -> Result<FileRow> {
    sqlx::query_as(&format!("INSERT INTO async_job_files(id,workspace_id,api_key_id,deployment_id,public_model,upstream_id,purpose,bytes,endpoint,request_count,output_token_sum,max_line_output) VALUES($1,$2,$3,$4,$5,$6,'batch',$7,$8,$9,$10,$11) RETURNING {FILE_COLUMNS}"))
        .bind(f.id).bind(f.workspace_id).bind(f.api_key_id).bind(f.deployment_id).bind(f.public_model).bind(f.upstream_id.as_str()).bind(f.bytes.min(i64::MAX as u64) as i64).bind(BATCH_ENDPOINT).bind(f.request_count as i32).bind(f.output_token_sum.min(i64::MAX as u64) as i64).bind(f.max_line_output as i32)
        .fetch_one(&store.pool).await
}
/// Record a batch's provider output/error file under a gateway id (once).
pub(crate) async fn output_file(
    store: &Store,
    job: &JobRow,
    purpose: &str,
    upstream: &UpstreamId,
) -> Result<()> {
    sqlx::query("INSERT INTO async_job_files(id,workspace_id,api_key_id,deployment_id,public_model,upstream_id,purpose,job_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING")
        .bind(Uuid::new_v4()).bind(job.workspace_id).bind(job.api_key_id).bind(job.deployment_id).bind(&job.public_model).bind(upstream.as_str()).bind(purpose).bind(job.id)
        .execute(&store.pool).await?;
    Ok(())
}
pub(crate) async fn file(store: &Store, workspace: Uuid, id: Uuid) -> Result<Option<FileRow>> {
    sqlx::query_as(&format!(
        "SELECT {FILE_COLUMNS} FROM async_job_files WHERE id=$1 AND workspace_id=$2"
    ))
    .bind(id)
    .bind(workspace)
    .fetch_optional(&store.pool)
    .await
}
/// Gateway file ids of a batch: (input, output, error).
pub(crate) async fn job_files(
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
/// Claim an unused input file for one batch attempt.
pub(crate) async fn claim_file(
    store: &Store,
    workspace: Uuid,
    id: Uuid,
    execution: Uuid,
) -> Result<bool> {
    Ok(sqlx::query("UPDATE async_job_files SET claimed_by_execution_id=$3 WHERE id=$1 AND workspace_id=$2 AND purpose='batch' AND job_id IS NULL AND claimed_by_execution_id IS NULL")
        .bind(id).bind(workspace).bind(execution).execute(&store.pool).await?.rows_affected() == 1)
}
/// Release a claim when nothing was sent upstream (admission refused).
pub(crate) async fn release_file(store: &Store, id: Uuid, execution: Uuid) -> Result<()> {
    sqlx::query("UPDATE async_job_files SET claimed_by_execution_id=NULL WHERE id=$1 AND claimed_by_execution_id=$2 AND job_id IS NULL")
        .bind(id).bind(execution).execute(&store.pool).await?;
    Ok(())
}
pub(crate) async fn attach_file(store: &Store, id: Uuid, job: Uuid) -> Result<()> {
    sqlx::query("UPDATE async_job_files SET job_id=$2 WHERE id=$1 AND job_id IS NULL")
        .bind(id)
        .bind(job)
        .execute(&store.pool)
        .await?;
    Ok(())
}

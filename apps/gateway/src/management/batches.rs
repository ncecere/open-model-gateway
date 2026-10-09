//! Jobs (batches) in the dashboard (docs/batches.md#monitoring): the same
//! batches as `GET /v1/batches`, with progress, mode, the price list applied
//! and cost so far.
//!
//! Visibility follows request details: workspace admins (and the personal
//! owner) see every batch of the workspace; other members see the batches
//! they created. Platform Admins/Auditors see Team/Project batches across the
//! installation and only totals for personal workspaces. Rows are metadata:
//! never lines, custom ids, outputs or metadata values.
use super::*;
use crate::{
    filestore::{FileStoreRuntime, files::public_id},
    inference::Engine,
    jobs::{
        BatchMode, JobError, JobRow, Jobs,
        types::{JobKind, client_id, parse_client_id},
    },
};

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/workspaces/{ws}/batches", get(list))
        .route("/api/v1/workspaces/{ws}/batches/{id}", get(detail))
        .route("/api/v1/workspaces/{ws}/batches/{id}/cancel", post(cancel))
        .route("/api/v1/platform/batches", get(platform_list))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListQuery {
    /// `active`, `finished` or `all` (default).
    status: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}
impl ListQuery {
    fn parse(&self) -> Result<(Option<bool>, i64, i64), ApiError> {
        let active = match self.status.as_deref() {
            None | Some("all") => None,
            Some("active") => Some(true),
            Some("finished") => Some(false),
            Some(_) => return Err(invalid()),
        };
        let limit = self.limit.unwrap_or(50);
        let offset = self.offset.unwrap_or(0);
        if !(1..=200).contains(&limit) || !(0..=100_000).contains(&offset) {
            return Err(invalid());
        }
        Ok((active, limit, offset))
    }
}

/// One batch row with its cost so far (settled actual + active holds of its
/// attempts; `unknown` when any attempt's cost is unresolved).
const ROW: &str = "jsonb_build_object(
 'job_id',j.id,'workspace_id',j.workspace_id,'workspace_name',w.name,'workspace_kind',w.kind,
 'state',j.state,'upstream_status',j.upstream_status,'mode',coalesce(j.batch_mode,'native'),
 'endpoint',j.batch_endpoint,'model',j.public_model,'provider',j.provider,'price_tier',j.price_tier,
 'total',j.request_total,'completed',coalesce(j.request_completed,0),'failed',coalesce(j.request_failed,0),
 'error_code',j.error_code,'created_at',j.created_at,'in_progress_at',j.in_progress_at,
 'finalizing_at',j.finalizing_at,'completed_at',j.completed_at,'cancel_requested_at',j.cancel_requested_at,
 'last_progress_at',j.last_progress_at,'input_file',j.input_file_id,'output_file',j.output_file_id,
 'error_file',j.error_file_id,'user_id',j.user_id,
 'settled_microusd',(c.settled)::text,'held_microusd',(c.held)::text,'cost_unknown',c.unknown,'key_name',k.name,
 'running_lines',(SELECT count(*) FROM batch_lines l WHERE l.job_id=j.id AND l.state='running'),
 'waiting_reason',(SELECT b.reason FROM batch_route_waits b WHERE b.job_id=j.id AND b.reason IS NOT NULL AND j.settled_at IS NULL AND b.updated_at>clock_timestamp()-interval '10 seconds' ORDER BY b.waiting_lines DESC,b.deployment_id LIMIT 1))
 FROM async_jobs j JOIN workspaces w ON w.id=j.workspace_id LEFT JOIN api_keys k ON k.id=j.api_key_id
 CROSS JOIN LATERAL (SELECT coalesce(sum(r.actual_microusd) FILTER(WHERE r.state='settled'),0) settled,
  coalesce(sum(r.held_microusd) FILTER(WHERE r.state<>'settled'),0) held, coalesce(bool_or(r.state='unknown'),false) unknown
  FROM governance_reservations r WHERE r.execution_id IN (SELECT e.id FROM inference_executions e WHERE e.batch_job_id=j.id)
   OR (r.execution_id=j.execution_id AND j.batch_mode IS DISTINCT FROM 'gateway')) c";

fn render(mut row: Value, me: Uuid) -> Value {
    let id = |key: &str| {
        row.get(key)
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
    };
    let job = id("job_id");
    let files = ["input_file", "output_file", "error_file"].map(|k| id(k).map(public_id));
    let mine = id("user_id") == Some(me);
    if let Some(obj) = row.as_object_mut() {
        obj.remove("job_id");
        obj.remove("user_id");
        obj.insert(
            "id".into(),
            job.map(|j| client_id(JobKind::Batch.prefix(), j))
                .map_or(Value::Null, Value::String),
        );
        for (key, file) in ["input_file_id", "output_file_id", "error_file_id"]
            .into_iter()
            .zip(files)
        {
            obj.insert(key.into(), file.map_or(Value::Null, Value::String));
        }
        for key in ["input_file", "output_file", "error_file"] {
            obj.remove(key);
        }
        obj.insert("mine".into(), json!(mine));
        // Native batches show which price list applied; gateway-run lines
        // always use standard prices.
        let tier = obj
            .get("price_tier")
            .and_then(Value::as_str)
            .map(str::to_owned);
        obj.insert(
            "batch_price".into(),
            match tier.as_deref() {
                Some("batch") => json!(true),
                Some(_) => json!(false),
                None => Value::Null,
            },
        );
    }
    row
}

async fn list(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(q): Query<ListQuery>,
) -> ApiResult {
    let (active, limit, offset) = q.parse()?;
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    let mut rows: Vec<Value> = sqlx::query_scalar(&format!("SELECT {ROW} WHERE j.workspace_id=$1 AND j.kind='batch' AND ($2 OR j.user_id=$3) AND ($4::boolean IS NULL OR (j.settled_at IS NULL)=$4) ORDER BY j.created_at DESC,j.id DESC LIMIT $5 OFFSET $6"))
        .bind(ws).bind(a.view_all_activity).bind(u.user_id).bind(active).bind(limit + 1).bind(offset)
        .fetch_all(&mut *tx).await?;
    tx.commit().await?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let data: Vec<Value> = rows.into_iter().map(|r| render(r, u.user_id)).collect();
    Ok(Json(json!({
        "data": data,
        "has_more": more,
        "scope": if a.view_all_activity { "workspace" } else { "own" },
    })))
}

/// A batch this caller may see (404 otherwise).
async fn visible(
    s: &Store,
    u: &BrowserPrincipal,
    ws: Uuid,
    id: &str,
) -> Result<(Value, Uuid, resources::WorkspaceAccess), ApiError> {
    let job = parse_client_id(JobKind::Batch.prefix(), id).ok_or_else(missing)?;
    let (mut tx, a) = resources::workspace_tx(s, u, ws).await?;
    resources::detail_access(&a)?;
    let row: Value = sqlx::query_scalar(&format!(
        "SELECT {ROW} WHERE j.id=$1 AND j.workspace_id=$2 AND j.kind='batch' AND ($3 OR j.user_id=$4)"
    ))
    .bind(job)
    .bind(ws)
    .bind(a.view_all_activity)
    .bind(u.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    tx.commit().await?;
    Ok((row, job, a))
}

async fn detail(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, String)>,
) -> ApiResult {
    let (row, job, _) = visible(&s, &u, ws, &id).await?;
    // Line outcomes by code (gateway-run): counts only.
    let outcomes: Vec<(String, Option<String>, i64)> = sqlx::query_as("SELECT state,error_code,count(*) FROM batch_lines WHERE job_id=$1 GROUP BY 1,2 ORDER BY 3 DESC LIMIT 20")
        .bind(job)
        .fetch_all(&s.pool)
        .await?;
    let outcomes: Vec<Value> = outcomes
        .into_iter()
        .map(|(state, code, n)| json!({"state": state, "code": code, "lines": n}))
        .collect();
    // Capacity-aware scheduling (0022): why lines wait, per model route,
    // with the batch's place in each route's queue.
    let scheduling = crate::jobs::schedule::batch_waits(&s.pool, job).await?;
    Ok(Json(
        json!({"batch": render(row, u.user_id), "outcomes": outcomes, "scheduling": scheduling}),
    ))
}

async fn cancel(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Extension(engine): Extension<Engine>,
    ext: Option<Extension<FileStoreRuntime>>,
    Path((ws, id)): Path<(Uuid, String)>,
) -> ApiResult {
    let (row, job, a) = visible(&s, &u, ws, &id).await?;
    // Admins cancel any batch of the workspace; members only their own.
    if !a.admin && row["user_id"].as_str() != Some(&u.user_id.to_string()) {
        return Err(denied());
    }
    let jobs = Jobs::new(s.clone(), &engine).with_files(ext.map(|Extension(r)| r));
    let current: JobRow = sqlx::query_as(&format!(
        "SELECT {} FROM async_jobs WHERE id=$1 AND workspace_id=$2",
        crate::jobs::job_columns()
    ))
    .bind(job)
    .bind(ws)
    .fetch_one(&s.pool)
    .await?;
    let mode = current.mode();
    match jobs.cancel(current).await {
        Ok(_) => {}
        Err(JobError::Conflict(..)) => {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "This batch has already finished",
            ));
        }
        Err(_) => {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "The batch could not be cancelled right now",
            ));
        }
    }
    let mut tx = resources::installation_tx(&s).await?;
    resources::audit(
        &mut tx,
        &u,
        Some(ws),
        "batch.cancelled",
        "batch",
        Some(job),
        json!({"mode": mode.map_or("native", BatchMode::as_str)}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PlatformQuery {
    status: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// Platform view: Team/Project batches as rows; personal workspaces only as
/// totals (never another owner's batches).
async fn platform_list(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(q): Query<PlatformQuery>,
) -> ApiResult {
    let (active, limit, offset) = ListQuery {
        status: q.status,
        limit: q.limit,
        offset: q.offset,
    }
    .parse()?;
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_read(&mut tx, u.user_id).await?;
    let mut rows: Vec<Value> = sqlx::query_scalar(&format!("SELECT {ROW} WHERE j.kind='batch' AND w.kind IN('team','project') AND ($1::boolean IS NULL OR (j.settled_at IS NULL)=$1) ORDER BY j.created_at DESC,j.id DESC LIMIT $2 OFFSET $3"))
        .bind(active).bind(limit + 1).bind(offset)
        .fetch_all(&mut *tx).await?;
    let personal: Value = sqlx::query_scalar("SELECT jsonb_build_object('active',count(*) FILTER(WHERE j.settled_at IS NULL),'finished',count(*) FILTER(WHERE j.settled_at IS NOT NULL),'failed',count(*) FILTER(WHERE j.state IN('failed','expired')),'lines',coalesce(sum(j.request_total),0)) FROM async_jobs j JOIN workspaces w ON w.id=j.workspace_id WHERE j.kind='batch' AND w.kind='personal'")
        .fetch_one(&mut *tx).await?;
    tx.commit().await?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let data: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let mut r = render(r, u.user_id);
            if let Some(o) = r.as_object_mut() {
                // Platform readers see metadata, not key names.
                o.remove("key_name");
                o.remove("mine");
            }
            r
        })
        .collect();
    Ok(Json(
        json!({"data": data, "has_more": more, "personal": personal}),
    ))
}

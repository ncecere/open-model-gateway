//! Logs: shared filters and visibility for request, generation (upstream
//! attempt) and session views, summary metrics, and the platform (Admin ›
//! Usage & spend › Logs) equivalents. Metadata only, never prompts or bodies.
//!
//! Visibility is the same everywhere:
//! - workspace scope: members see their own human-key activity, shared
//!   administrators (and a personal owner) the whole workspace;
//! - platform scope (Admin/Auditor): Team and Project workspaces only.
//!   Personal workspaces never appear as rows (their totals stay in Usage).
use super::*;
use crate::inference::{client, repository::FinishLabel};
use chrono::{DateTime, TimeDelta, Utc};

pub(super) const DEFAULT_LIMIT: i64 = 50;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct LogQuery {
    pub(super) start_date: Option<String>,
    pub(super) end_date: Option<String>,
    pub(super) model: Option<String>,
    pub(super) key_id: Option<Uuid>,
    pub(super) status: Option<String>,
    pub(super) q: Option<String>,
    pub(super) finish_reason: Option<String>,
    pub(super) streamed: Option<bool>,
    pub(super) session_id: Option<String>,
    /// Platform scope only (a Team/Project id).
    pub(super) workspace_id: Option<Uuid>,
    pub(super) cursor: Option<String>,
    pub(super) limit: Option<i64>,
}
pub(super) struct Filters {
    pub(super) start: DateTime<Utc>,
    pub(super) end: DateTime<Utc>,
    model: Option<String>,
    key: Option<Uuid>,
    status: Option<Vec<String>>,
    q: Option<String>,
    finish: Option<Vec<String>>,
    streamed: Option<bool>,
    session: Option<String>,
}
fn finish_reasons(s: &str) -> Result<Vec<String>, ApiError> {
    let mut out: Vec<String> = Vec::new();
    for v in s.split(',') {
        if !FinishLabel::ALL.contains(&v) {
            return Err(invalid());
        }
        if !out.iter().any(|o| o == v) {
            out.push(v.to_owned());
        }
    }
    Ok(out)
}
impl LogQuery {
    pub(super) fn filters(&self) -> Result<Filters, ApiError> {
        use super::requests::{midnight, strict_date};
        let tomorrow = Utc::now()
            .date_naive()
            .checked_add_signed(TimeDelta::days(1))
            .ok_or_else(invalid)?;
        let end = self
            .end_date
            .as_deref()
            .map(strict_date)
            .transpose()?
            .unwrap_or(tomorrow);
        let start = self
            .start_date
            .as_deref()
            .map(strict_date)
            .transpose()?
            .unwrap_or(end - TimeDelta::days(30));
        if !(1..=93).contains(&(end - start).num_days()) || end > tomorrow {
            return Err(invalid());
        }
        if self
            .model
            .as_deref()
            .is_some_and(|m| m.is_empty() || m.len() > 200 || m.chars().any(char::is_control))
        {
            return Err(invalid());
        }
        let status = self
            .status
            .as_deref()
            .map(super::usage::statuses)
            .transpose()?;
        let finish = self
            .finish_reason
            .as_deref()
            .map(finish_reasons)
            .transpose()?;
        if self
            .session_id
            .as_deref()
            .is_some_and(|s| !client::valid_session_id(s))
        {
            return Err(invalid());
        }
        let q = match self.q.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(q) => {
                let q = q.to_ascii_lowercase();
                if !(4..=36).contains(&q.len())
                    || !q.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
                {
                    return Err(invalid());
                }
                Some(q)
            }
        };
        Ok(Filters {
            start: midnight(start),
            end: midnight(end),
            model: self.model.clone(),
            key: self.key_id,
            status,
            q,
            finish,
            streamed: self.streamed,
            session: self.session_id.clone(),
        })
    }
    pub(super) fn limit(&self) -> Result<i64, ApiError> {
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=100).contains(&limit) {
            return Err(invalid());
        }
        Ok(limit)
    }
    /// Detail and metrics endpoints take filters only (no paging).
    pub(super) fn no_paging(&self) -> Result<(), ApiError> {
        if self.cursor.is_some() || self.limit.is_some() {
            return Err(invalid());
        }
        Ok(())
    }
}

/// Who may see which attempts. `$1` workspace (platform: optional filter),
/// `$2` workspace-wide visibility, `$3` caller.
#[derive(Clone, Copy)]
pub(super) struct LogScope {
    pub(super) platform: bool,
    pub(super) workspace: Option<Uuid>,
    pub(super) all: bool,
    pub(super) user: Uuid,
}
impl LogScope {
    /// Attempt visibility over `e` (executions), `k` (keys), `w` (workspaces).
    pub(super) fn visible(&self) -> &'static str {
        if self.platform {
            // $2/$3 are typed but irrelevant: platform readers see every Team/Project attempt.
            "w.kind IN('team','project') AND ($1::uuid IS NULL OR e.workspace_id=$1) AND $2::boolean IS NOT NULL AND $3::uuid IS NOT NULL"
        } else {
            "e.workspace_id=$1 AND ($2 OR k.issued_to_user_id=$3)"
        }
    }
    /// Workspace restriction of the root preselect over `x`.
    fn preselect_workspace(&self) -> &'static str {
        if self.platform {
            "($1::uuid IS NULL OR x.workspace_id=$1)"
        } else {
            "x.workspace_id=$1"
        }
    }
}
/// Workspace scope: members (own human keys) or workspace-wide visibility.
pub(super) async fn workspace_scope<'a>(
    s: &'a Store,
    u: &BrowserPrincipal,
    ws: Uuid,
    p: &LogQuery,
) -> Result<(Transaction<'a, Postgres>, LogScope), ApiError> {
    if p.workspace_id.is_some() {
        return Err(invalid());
    }
    let (mut tx, a) = resources::workspace_tx(s, u, ws).await?;
    resources::detail_access(&a)?;
    timeout(&mut tx).await?;
    Ok((
        tx,
        LogScope {
            platform: false,
            workspace: Some(ws),
            all: a.view_all_activity,
            user: u.user_id,
        },
    ))
}
/// Platform scope: Admin/Auditor, Team/Project workspaces only.
pub(super) async fn platform_scope<'a>(
    s: &'a Store,
    u: &BrowserPrincipal,
    p: &LogQuery,
) -> Result<(Transaction<'a, Postgres>, LogScope), ApiError> {
    let mut tx = resources::installation_tx(s).await?;
    resources::platform_read(&mut tx, u.user_id).await?;
    timeout(&mut tx).await?;
    Ok((
        tx,
        LogScope {
            platform: true,
            workspace: p.workspace_id,
            all: true,
            user: u.user_id,
        },
    ))
}
async fn timeout(tx: &mut Transaction<'_, Postgres>) -> Result<(), ApiError> {
    sqlx::query("SET LOCAL statement_timeout='10s'")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Tokens per second of a successful generation attempt: output tokens over
/// generation time after the first token (streams) or the whole generation.
const TPS: &str = "CASE WHEN e.workload_kind='generation' AND e.state='succeeded' AND e.output_tokens IS NOT NULL AND e.generation_ms-coalesce(e.time_to_first_token_ms,0)>0 THEN e.generation_ms-coalesce(e.time_to_first_token_ms,0) END";
/// Root requests visible in `scope`: `att` (attempts) then `roots` (one row
/// per root). `{FILTER}` restricts attempts before aggregation.
const ROOTS: &str = "WITH att AS (SELECT e.*,k.name key_name,w.name workspace_name,w.kind workspace_kind,r.state accounting_state,r.actual_microusd,r.held_microusd,(e.billing_usage->>'cache_read_input_tokens')::bigint cached_tokens,{TPS} decode_ms FROM inference_executions e JOIN api_keys k ON k.id=e.api_key_id AND k.workspace_id=e.workspace_id JOIN workspaces w ON w.id=e.workspace_id LEFT JOIN governance_reservations r ON r.execution_id=e.id WHERE {VISIBLE} AND {FILTER}),
roots AS (SELECT root_request_id,(array_agg(workspace_id ORDER BY attempt_number))[1] workspace_id,(array_agg(workspace_name ORDER BY attempt_number))[1] workspace_name,(array_agg(workspace_kind ORDER BY attempt_number))[1] workspace_kind,min(started_at) started_at,CASE WHEN bool_and(completed_at IS NOT NULL) THEN max(completed_at) END completed_at,CASE WHEN bool_and(completed_at IS NOT NULL) THEN floor(extract(epoch FROM max(completed_at)-min(started_at))*1000)::bigint END latency_ms,(array_agg(public_model ORDER BY attempt_number))[1] model,(array_agg(api_key_id ORDER BY attempt_number))[1] key_id,(array_agg(key_name ORDER BY attempt_number))[1] key_name,CASE (array_agg(state ORDER BY attempt_number DESC))[1] WHEN 'started' THEN 'in_progress' ELSE (array_agg(state ORDER BY attempt_number DESC))[1] END status,count(*) attempts,CASE WHEN count(*) FILTER(WHERE input_tokens IS NULL)=0 THEN sum(input_tokens) END input_tokens,CASE WHEN count(*) FILTER(WHERE output_tokens IS NULL)=0 THEN sum(output_tokens) END output_tokens,CASE WHEN count(*) FILTER(WHERE cached_tokens IS NULL)=0 THEN sum(cached_tokens) END cached_tokens,CASE WHEN count(*) FILTER(WHERE reasoning_tokens IS NULL)=0 THEN sum(reasoning_tokens) END reasoning_tokens,CASE WHEN count(*) FILTER(WHERE actual_microusd IS NULL)=0 THEN sum(actual_microusd) END cost_microusd,coalesce(sum(actual_microusd),0) known_cost_microusd,coalesce(sum(held_microusd) FILTER(WHERE actual_microusd IS NULL AND accounting_state IN('pending','unknown')),0) held_microusd,(array_agg(cost_center_id ORDER BY attempt_number))[1] cost_center_id,(array_agg(cost_center_name ORDER BY attempt_number))[1] cost_center_name,(array_agg(cost_center_code ORDER BY attempt_number))[1] cost_center_code,(array_agg(workload_kind ORDER BY attempt_number))[1] workload_kind,bool_or(streamed) streamed,(array_agg(finish_reason ORDER BY attempt_number DESC))[1] finish_reason,(array_agg(time_to_first_token_ms ORDER BY attempt_number DESC))[1] ttft_ms,(array_agg(generation_ms ORDER BY attempt_number DESC))[1] generation_ms,(array_agg(decode_ms ORDER BY attempt_number DESC))[1] decode_ms,(array_agg(output_tokens ORDER BY attempt_number DESC))[1] final_output_tokens,(array_agg(upstream_model ORDER BY attempt_number DESC))[1] upstream_model,(array_agg(reported_upstream_model ORDER BY attempt_number DESC))[1] reported_upstream_model,(array_agg(provider ORDER BY attempt_number DESC))[1] provider,(array_agg(client_session_id ORDER BY attempt_number))[1] session_id,(array_agg(client_app ORDER BY attempt_number))[1] app,array_agg(id) execution_ids FROM att GROUP BY root_request_id)";
/// One root row (requests list, detail summary).
pub(super) const ROW: &str = "jsonb_build_object('root_request_id',root_request_id,'workspace',jsonb_build_object('id',workspace_id,'name',workspace_name,'kind',workspace_kind),'started_at',started_at,'completed_at',completed_at,'model',model,'upstream_model',upstream_model,'reported_upstream_model',reported_upstream_model,'provider',provider,'key',jsonb_build_object('id',key_id,'name',key_name),'status',status,'finish_reason',finish_reason,'attempts',attempts,'input_tokens',input_tokens::text,'output_tokens',output_tokens::text,'cached_input_tokens',cached_tokens::text,'reasoning_tokens',reasoning_tokens::text,'cost_microusd',cost_microusd::text,'held_microusd',held_microusd::text,'latency_ms',latency_ms,'time_to_first_token_ms',ttft_ms,'generation_ms',generation_ms,'tokens_per_second',CASE WHEN decode_ms>0 THEN round(final_output_tokens*1000.0/decode_ms,2)::text END,'cost_center',CASE WHEN cost_center_id IS NULL THEN NULL ELSE jsonb_build_object('id',cost_center_id,'name',cost_center_name,'code',cost_center_code) END,'workload_kind',workload_kind,'streamed',streamed,'session_id',session_id,'app',app)";
/// Filters on roots: $4 start, $5 end, $6 model, $7 key, $8 statuses,
/// $9 id prefix, $10 finish reasons (final attempt), $11 streamed, $12 session.
pub(super) const FILTERED: &str = "started_at>=$4 AND started_at<$5 AND ($6::text IS NULL OR model=$6) AND ($7::uuid IS NULL OR key_id=$7) AND ($8::text[] IS NULL OR status=ANY($8)) AND ($9::text IS NULL OR root_request_id::text LIKE $9||'%' OR EXISTS(SELECT 1 FROM unnest(execution_ids) x WHERE x::text LIKE $9||'%')) AND ($10::text[] IS NULL OR finish_reason=ANY($10)) AND ($11::boolean IS NULL OR streamed=$11) AND ($12::text IS NULL OR session_id=$12)";

/// Root requests whose first attempt started in the period, with the
/// root-invariant filters (model, key, streamed, session) pushed down.
pub(super) fn listed_roots(scope: &LogScope, sessions_only: bool) -> String {
    let filter = format!(
        "e.root_request_id IN(SELECT x.root_request_id FROM inference_executions x WHERE {} AND x.attempt_number=1 AND x.started_at>=$4 AND x.started_at<$5 AND ($6::text IS NULL OR x.public_model=$6) AND ($7::uuid IS NULL OR x.api_key_id=$7) AND ($11::boolean IS NULL OR x.streamed=$11) AND ($12::text IS NULL OR x.client_session_id=$12){})",
        scope.preselect_workspace(),
        if sessions_only {
            " AND x.client_session_id IS NOT NULL"
        } else {
            ""
        }
    );
    roots(scope, &filter)
}
pub(super) fn roots(scope: &LogScope, filter: &str) -> String {
    ROOTS
        .replace("{TPS}", TPS)
        .replace("{VISIBLE}", scope.visible())
        .replace("{FILTER}", filter)
}
pub(super) type PgQueryAs<'q, O> =
    sqlx::query::QueryAs<'q, Postgres, O, sqlx::postgres::PgArguments>;
/// Binds `$1..$12` (scope and filters).
pub(super) fn bind_filters<'q, O>(
    q: PgQueryAs<'q, O>,
    scope: &LogScope,
    f: &'q Filters,
) -> PgQueryAs<'q, O>
where
    O: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
{
    q.bind(scope.workspace)
        .bind(scope.all)
        .bind(scope.user)
        .bind(f.start)
        .bind(f.end)
        .bind(f.model.as_deref())
        .bind(f.key)
        .bind(f.status.as_deref())
        .bind(f.q.as_deref())
        .bind(f.finish.as_deref())
        .bind(f.streamed)
        .bind(f.session.as_deref())
}
pub(super) fn cursor_parts(c: &str) -> Result<(DateTime<Utc>, Uuid), ApiError> {
    let (micros, id) = c.split_once('_').ok_or_else(invalid)?;
    let micros: i64 = micros.parse().map_err(|_| invalid())?;
    Ok((
        DateTime::from_timestamp_micros(micros).ok_or_else(invalid)?,
        Uuid::parse_str(id).map_err(|_| invalid())?,
    ))
}
fn page(rows: Vec<(Value, DateTime<Utc>, Uuid)>, limit: i64) -> Json<Value> {
    let next = (rows.len() > limit as usize).then(|| {
        let (_, t, id) = &rows[limit as usize - 1];
        format!("{}_{id}", t.timestamp_micros())
    });
    let data: Vec<Value> = rows.into_iter().take(limit as usize).map(|r| r.0).collect();
    Json(json!({"data":data,"next_cursor":next}))
}

// ----- Requests (root requests) -----

pub(super) async fn list_requests(
    mut tx: Transaction<'_, Postgres>,
    scope: LogScope,
    p: &LogQuery,
) -> ApiResult {
    let f = p.filters()?;
    let limit = p.limit()?;
    let cursor = p.cursor.as_deref().map(cursor_parts).transpose()?;
    // Roots are selected by their first attempt's start inside the range.
    let sql = format!(
        "{} SELECT {ROW},started_at,root_request_id FROM roots WHERE {FILTERED} AND ($13::timestamptz IS NULL OR (started_at,root_request_id)<($13,$14)) ORDER BY started_at DESC,root_request_id DESC LIMIT $15",
        listed_roots(&scope, false)
    );
    let rows: Vec<(Value, DateTime<Utc>, Uuid)> = bind_filters(sqlx::query_as(&sql), &scope, &f)
        .bind(cursor.map(|c| c.0))
        .bind(cursor.map(|c| c.1))
        .bind(limit + 1)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(page(rows, limit))
}

// ----- Generations (one row per upstream attempt) -----

const GENERATION: &str = "jsonb_build_object('execution_id',e.id,'root_request_id',e.root_request_id,'attempt_number',e.attempt_number,'workspace',jsonb_build_object('id',w.id,'name',w.name,'kind',w.kind),'started_at',e.started_at,'completed_at',e.completed_at,'model',e.public_model,'upstream_model',coalesce(e.upstream_model,d.upstream_model),'reported_upstream_model',e.reported_upstream_model,'connection',jsonb_build_object('id',p.id,'name',p.name,'provider',p.provider),'key',jsonb_build_object('id',k.id,'name',k.name),'status',CASE e.state WHEN 'started' THEN 'in_progress' ELSE e.state END,'error_code',e.error_code,'finish_reason',e.finish_reason,'streamed',e.streamed,'workload_kind',e.workload_kind,'input_tokens',e.input_tokens::text,'output_tokens',e.output_tokens::text,'cached_input_tokens',e.billing_usage->>'cache_read_input_tokens','reasoning_tokens',e.reasoning_tokens::text,'cost_microusd',r.actual_microusd::text,'held_microusd',CASE WHEN r.actual_microusd IS NOT NULL THEN '0' WHEN r.state IN('pending','unknown') THEN r.held_microusd::text END,'latency_ms',e.elapsed_ms,'time_to_first_token_ms',e.time_to_first_token_ms,'generation_ms',e.generation_ms,'tokens_per_second',CASE WHEN ({TPS})>0 THEN round(e.output_tokens*1000.0/({TPS}),2)::text END,'session_id',e.client_session_id,'app',e.client_app)";

pub(super) async fn list_generations(
    mut tx: Transaction<'_, Postgres>,
    scope: LogScope,
    p: &LogQuery,
) -> ApiResult {
    let f = p.filters()?;
    let limit = p.limit()?;
    let cursor = p.cursor.as_deref().map(cursor_parts).transpose()?;
    let sql = format!(
        "SELECT {},e.started_at,e.id FROM inference_executions e JOIN api_keys k ON k.id=e.api_key_id AND k.workspace_id=e.workspace_id JOIN workspaces w ON w.id=e.workspace_id JOIN deployments d ON d.id=e.deployment_id JOIN provider_connections p ON p.id=d.provider_connection_id LEFT JOIN governance_reservations r ON r.execution_id=e.id WHERE {} AND e.started_at>=$4 AND e.started_at<$5 AND ($6::text IS NULL OR e.public_model=$6) AND ($7::uuid IS NULL OR e.api_key_id=$7) AND ($8::text[] IS NULL OR (CASE e.state WHEN 'started' THEN 'in_progress' ELSE e.state END)=ANY($8)) AND ($9::text IS NULL OR e.id::text LIKE $9||'%' OR e.root_request_id::text LIKE $9||'%') AND ($10::text[] IS NULL OR e.finish_reason=ANY($10)) AND ($11::boolean IS NULL OR e.streamed=$11) AND ($12::text IS NULL OR e.client_session_id=$12) AND ($13::timestamptz IS NULL OR (e.started_at,e.id)<($13,$14)) ORDER BY e.started_at DESC,e.id DESC LIMIT $15",
        GENERATION.replace("{TPS}", TPS),
        scope.visible()
    );
    let rows: Vec<(Value, DateTime<Utc>, Uuid)> = bind_filters(sqlx::query_as(&sql), &scope, &f)
        .bind(cursor.map(|c| c.0))
        .bind(cursor.map(|c| c.1))
        .bind(limit + 1)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(page(rows, limit))
}

// ----- Sessions (root requests grouped by client session id) -----

const SESSIONS: &str = "{ROOTS}, s AS (SELECT workspace_id,(array_agg(workspace_name))[1] workspace_name,(array_agg(workspace_kind))[1] workspace_kind,session_id,count(*) requests,sum(attempts) attempts,count(*) FILTER(WHERE status='failed') failed,count(*) FILTER(WHERE status='in_progress') in_progress,CASE WHEN count(*) FILTER(WHERE input_tokens IS NULL)=0 THEN sum(input_tokens) END input_tokens,CASE WHEN count(*) FILTER(WHERE output_tokens IS NULL)=0 THEN sum(output_tokens) END output_tokens,CASE WHEN count(*) FILTER(WHERE cost_microusd IS NULL)=0 THEN sum(cost_microusd) END cost_microusd,sum(known_cost_microusd) known_cost_microusd,sum(held_microusd) held_microusd,count(*) FILTER(WHERE cost_microusd IS NULL) unresolved_requests,min(started_at) first_at,max(started_at) last_at,(array_agg(DISTINCT model ORDER BY model))[1:10] models,count(DISTINCT model) model_count,(array_agg(model ORDER BY started_at DESC))[1] last_model,max(app) app,count(DISTINCT key_id) keys FROM roots WHERE {FILTERED} AND session_id IS NOT NULL GROUP BY workspace_id,session_id)";
const SESSION_ROW: &str = "jsonb_build_object('session_id',session_id,'workspace',jsonb_build_object('id',workspace_id,'name',workspace_name,'kind',workspace_kind),'requests',requests::text,'attempts',attempts::text,'failed_requests',failed::text,'in_progress_requests',in_progress::text,'input_tokens',input_tokens::text,'output_tokens',output_tokens::text,'cost_microusd',cost_microusd::text,'known_cost_microusd',known_cost_microusd::text,'held_microusd',held_microusd::text,'unresolved_requests',unresolved_requests::text,'first_at',first_at,'last_at',last_at,'models',to_jsonb(models),'model_count',model_count,'last_model',last_model,'app',app,'keys',keys)";
fn sessions_sql(scope: &LogScope) -> String {
    SESSIONS
        .replace("{ROOTS}", &listed_roots(scope, true))
        .replace("{FILTERED}", FILTERED)
}
fn session_cursor(c: &str) -> Result<(DateTime<Utc>, Uuid, String), ApiError> {
    let mut parts = c.splitn(3, '_');
    let (Some(micros), Some(ws), Some(session)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(invalid());
    };
    let micros: i64 = micros.parse().map_err(|_| invalid())?;
    if session.len() % 2 != 0 || session.len() > 4 * 128 * 2 {
        return Err(invalid());
    }
    let bytes = (0..session.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(session.get(i..i + 2).ok_or_else(invalid)?, 16)
                .map_err(|_| invalid())
        })
        .collect::<Result<Vec<u8>, ApiError>>()?;
    let session = String::from_utf8(bytes).map_err(|_| invalid())?;
    if !client::valid_session_id(&session) {
        return Err(invalid());
    }
    Ok((
        DateTime::from_timestamp_micros(micros).ok_or_else(invalid)?,
        Uuid::parse_str(ws).map_err(|_| invalid())?,
        session,
    ))
}
pub(super) async fn list_sessions(
    mut tx: Transaction<'_, Postgres>,
    scope: LogScope,
    p: &LogQuery,
) -> ApiResult {
    let f = p.filters()?;
    let limit = p.limit()?;
    let cursor = p.cursor.as_deref().map(session_cursor).transpose()?;
    let sql = format!(
        "{} SELECT {SESSION_ROW},last_at,workspace_id,session_id FROM s WHERE ($13::timestamptz IS NULL OR (last_at,workspace_id,session_id COLLATE \"C\")<($13,$14,$15::text COLLATE \"C\")) ORDER BY last_at DESC,workspace_id DESC,session_id COLLATE \"C\" DESC LIMIT $16",
        sessions_sql(&scope)
    );
    let rows: Vec<(Value, DateTime<Utc>, Uuid, String)> =
        bind_filters(sqlx::query_as(&sql), &scope, &f)
            .bind(cursor.as_ref().map(|c| c.0))
            .bind(cursor.as_ref().map(|c| c.1))
            .bind(cursor.as_ref().map(|c| c.2.clone()))
            .bind(limit + 1)
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    let next = (rows.len() > limit as usize).then(|| {
        let (_, t, ws, session) = &rows[limit as usize - 1];
        let hex: String = session.bytes().map(|b| format!("{b:02x}")).collect();
        format!("{}_{ws}_{hex}", t.timestamp_micros())
    });
    let data: Vec<Value> = rows.into_iter().take(limit as usize).map(|r| r.0).collect();
    Ok(Json(json!({"data":data,"next_cursor":next})))
}
/// One session of one workspace within the filters (404 when nothing is visible).
pub(super) async fn session_detail(
    mut tx: Transaction<'_, Postgres>,
    mut scope: LogScope,
    ws: Uuid,
    session: &str,
    p: &LogQuery,
) -> ApiResult {
    p.no_paging()?;
    if p.session_id.is_some() || !client::valid_session_id(session) {
        return Err(if p.session_id.is_some() {
            invalid()
        } else {
            missing()
        });
    }
    let mut f = p.filters()?;
    f.session = Some(session.to_owned());
    scope.workspace = Some(ws);
    let sql = format!("{} SELECT {SESSION_ROW} FROM s", sessions_sql(&scope));
    let row: Option<(Value,)> = bind_filters(sqlx::query_as(&sql), &scope, &f)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(row.ok_or_else(missing)?.0))
}

// ----- Metrics (summary of the filtered root requests) -----

pub(super) async fn metrics(
    mut tx: Transaction<'_, Postgres>,
    scope: LogScope,
    p: &LogQuery,
) -> ApiResult {
    p.no_paging()?;
    let f = p.filters()?;
    let sql = format!(
        "{}, f AS (SELECT * FROM roots WHERE {FILTERED}) SELECT jsonb_build_object('requests',count(*)::text,'completed',count(*) FILTER(WHERE status<>'in_progress')::text,'failed',count(*) FILTER(WHERE status='failed')::text,'error_rate',CASE WHEN count(*) FILTER(WHERE status<>'in_progress')>0 THEN round(count(*) FILTER(WHERE status='failed')::numeric/count(*) FILTER(WHERE status<>'in_progress'),4)::text END,'latency_p50_ms',percentile_disc(0.5) WITHIN GROUP (ORDER BY latency_ms),'latency_p95_ms',percentile_disc(0.95) WITHIN GROUP (ORDER BY latency_ms),'avg_time_to_first_token_ms',round(avg(ttft_ms))::bigint,'ttft_requests',count(ttft_ms)::text,'tokens_per_second',CASE WHEN coalesce(sum(decode_ms) FILTER(WHERE decode_ms>0),0)>0 THEN round(sum(final_output_tokens) FILTER(WHERE decode_ms>0)*1000.0/sum(decode_ms) FILTER(WHERE decode_ms>0),2)::text END,'input_tokens',coalesce(sum(input_tokens),0)::text,'output_tokens',coalesce(sum(output_tokens),0)::text,'unknown_token_requests',count(*) FILTER(WHERE input_tokens IS NULL OR output_tokens IS NULL)::text,'known_cost_microusd',coalesce(sum(known_cost_microusd),0)::text,'held_microusd',coalesce(sum(held_microusd),0)::text,'unresolved_requests',count(*) FILTER(WHERE cost_microusd IS NULL)::text) FROM f",
        listed_roots(&scope, false)
    );
    let (v,): (Value,) = bind_filters(sqlx::query_as(&sql), &scope, &f)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(v))
}

// ----- Handlers -----

pub(super) async fn workspace_generations(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<LogQuery>,
) -> ApiResult {
    let (tx, scope) = workspace_scope(&s, &u, ws, &p).await?;
    list_generations(tx, scope, &p).await
}
pub(super) async fn workspace_sessions(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<LogQuery>,
) -> ApiResult {
    let (tx, scope) = workspace_scope(&s, &u, ws, &p).await?;
    list_sessions(tx, scope, &p).await
}
pub(super) async fn workspace_session(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, session)): Path<(Uuid, String)>,
    Query(p): Query<LogQuery>,
) -> ApiResult {
    let (tx, scope) = workspace_scope(&s, &u, ws, &p).await?;
    session_detail(tx, scope, ws, &session, &p).await
}
pub(super) async fn workspace_metrics(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<LogQuery>,
) -> ApiResult {
    let (tx, scope) = workspace_scope(&s, &u, ws, &p).await?;
    metrics(tx, scope, &p).await
}
pub(super) async fn platform_requests(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<LogQuery>,
) -> ApiResult {
    let (tx, scope) = platform_scope(&s, &u, &p).await?;
    list_requests(tx, scope, &p).await
}
pub(super) async fn platform_request(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(root): Path<String>,
    Query(p): Query<LogQuery>,
) -> ApiResult {
    let (tx, scope) = platform_scope(&s, &u, &p).await?;
    super::requests::detail(tx, scope, &root, &p).await
}
pub(super) async fn platform_generations(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<LogQuery>,
) -> ApiResult {
    let (tx, scope) = platform_scope(&s, &u, &p).await?;
    list_generations(tx, scope, &p).await
}
pub(super) async fn platform_sessions(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<LogQuery>,
) -> ApiResult {
    let (tx, scope) = platform_scope(&s, &u, &p).await?;
    list_sessions(tx, scope, &p).await
}
pub(super) async fn platform_session(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, session)): Path<(Uuid, String)>,
    Query(p): Query<LogQuery>,
) -> ApiResult {
    // The path names the workspace; a conflicting workspace_id filter is invalid.
    if p.workspace_id.is_some_and(|w| w != ws) {
        return Err(invalid());
    }
    let (tx, scope) = platform_scope(&s, &u, &p).await?;
    session_detail(tx, scope, ws, &session, &p).await
}
pub(super) async fn platform_metrics(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<LogQuery>,
) -> ApiResult {
    let (tx, scope) = platform_scope(&s, &u, &p).await?;
    metrics(tx, scope, &p).await
}
pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/workspaces/{ws}/requests",
            get(super::requests::requests),
        )
        .route(
            "/api/v1/workspaces/{ws}/requests/{root}",
            get(super::requests::request_detail),
        )
        .route(
            "/api/v1/workspaces/{ws}/generations",
            get(workspace_generations),
        )
        .route("/api/v1/workspaces/{ws}/sessions", get(workspace_sessions))
        .route(
            "/api/v1/workspaces/{ws}/sessions/{session}",
            get(workspace_session),
        )
        .route(
            "/api/v1/workspaces/{ws}/logs/metrics",
            get(workspace_metrics),
        )
        .route("/api/v1/platform/logs/requests", get(platform_requests))
        .route(
            "/api/v1/platform/logs/requests/{root}",
            get(platform_request),
        )
        .route(
            "/api/v1/platform/logs/generations",
            get(platform_generations),
        )
        .route("/api/v1/platform/logs/sessions", get(platform_sessions))
        .route(
            "/api/v1/platform/logs/sessions/{ws}/{session}",
            get(platform_session),
        )
        .route("/api/v1/platform/logs/metrics", get(platform_metrics))
}

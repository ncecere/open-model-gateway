//! Request logs: root requests and their attempt timelines. Metadata only,
//! never prompts or bodies. Members see their own human-key requests, shared
//! administrators the whole workspace, personal workspaces only their owner;
//! the platform view (Admin/Auditor) sees Team/Project requests only. Shared
//! filters, visibility and the other log views are in `logs`.
use super::logs::{self, FILTERED, LogQuery, LogScope, ROW, bind_filters};
use super::*;
use chrono::{DateTime, NaiveDate, Utc};

pub(super) fn strict_date(s: &str) -> Result<NaiveDate, ApiError> {
    if s.len() != 10 {
        return Err(invalid());
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| invalid())
}
pub(super) fn midnight(d: NaiveDate) -> DateTime<Utc> {
    d.and_time(chrono::NaiveTime::MIN).and_utc()
}
/// Data policy applied to a connection, from current server configuration
/// (not a historical snapshot). Only OpenRouter has a configurable setting.
pub(crate) fn data_policy(provider: &str) -> Value {
    if provider == "openrouter" {
        json!({"data_collection":crate::providers::openrouter::DataCollection::configured().as_str(),"basis":"current_configuration"})
    } else {
        json!({"data_collection":"unknown","basis":"not_configured"})
    }
}
pub(super) async fn requests(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<LogQuery>,
) -> ApiResult {
    let (tx, scope) = logs::workspace_scope(&s, &u, ws, &p).await?;
    logs::list_requests(tx, scope, &p).await
}
pub(super) async fn request_detail(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, root)): Path<(Uuid, String)>,
    Query(p): Query<LogQuery>,
) -> ApiResult {
    let (tx, scope) = logs::workspace_scope(&s, &u, ws, &p).await?;
    detail(tx, scope, &root, &p).await
}
/// One root request with its attempt timeline and previous/next neighbours
/// within the list filters (prev = newer, next = older).
pub(super) async fn detail(
    mut tx: Transaction<'_, Postgres>,
    scope: LogScope,
    root: &str,
    p: &LogQuery,
) -> ApiResult {
    p.no_paging()?;
    // A short or malformed id is "not found", never a parser message
    // (resolve short ids through the list's `q` prefix search instead).
    let root = Uuid::parse_str(root).map_err(|_| missing())?;
    let f = p.filters()?;
    // Visibility of the root itself ignores list filters.
    let sql = format!(
        "{} SELECT {ROW},started_at,workspace_id FROM roots",
        logs::roots(&scope, "e.root_request_id=$4")
    );
    let (mut summary, started, ws): (Value, DateTime<Utc>, Uuid) = sqlx::query_as(&sql)
        .bind(scope.workspace)
        .bind(scope.all)
        .bind(scope.user)
        .bind(root)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    let attempts: Vec<(Value, String)> = sqlx::query_as(&format!("SELECT jsonb_build_object('attempt_number',e.attempt_number,'execution_id',e.id,'state',e.state,'error_code',e.error_code,'finish_reason',e.finish_reason,'started_at',e.started_at,'completed_at',e.completed_at,'latency_ms',e.elapsed_ms,'time_to_first_token_ms',e.time_to_first_token_ms,'generation_ms',e.generation_ms,'streamed',e.streamed,'workload_kind',e.workload_kind,'deployment',jsonb_build_object('id',d.id,'upstream_model',coalesce(e.upstream_model,d.upstream_model)),'connection',jsonb_build_object('id',p.id,'name',p.name,'provider',p.provider),'input_tokens',e.input_tokens::text,'output_tokens',e.output_tokens::text,'cached_input_tokens',e.billing_usage->>'cache_read_input_tokens','reasoning_tokens',e.reasoning_tokens::text,'billing_usage',e.billing_usage,'meter_usage',e.meter_usage,'cost_microusd',r.actual_microusd::text,'held_microusd',CASE WHEN r.actual_microusd IS NOT NULL THEN '0' WHEN r.state IN('pending','unknown') THEN r.held_microusd::text END,'accounting_state',coalesce(r.state,'missing'),'unresolved_reason',CASE WHEN r.actual_microusd IS NOT NULL THEN NULL WHEN r.execution_id IS NULL THEN 'missing_reservation' WHEN r.price_id IS NULL THEN 'unpriced' WHEN r.unbounded_cost THEN 'unbounded_cost_or_unknown_rate' WHEN r.state='pending' THEN 'in_progress' ELSE 'incomplete_usage' END,'price_id',r.price_id,'pricing_version',dp.pricing_version,'failover_reason',CASE WHEN e.attempt_number>1 THEN coalesce(lag(e.error_code) OVER w,lag(e.state) OVER w) END,'details_redacted_at',e.details_redacted_at),p.provider FROM inference_executions e JOIN api_keys k ON k.id=e.api_key_id AND k.workspace_id=e.workspace_id JOIN workspaces w ON w.id=e.workspace_id JOIN deployments d ON d.id=e.deployment_id JOIN provider_connections p ON p.id=d.provider_connection_id LEFT JOIN governance_reservations r ON r.execution_id=e.id LEFT JOIN deployment_prices dp ON dp.id=r.price_id WHERE {} AND e.root_request_id=$4 WINDOW w AS (ORDER BY e.attempt_number) ORDER BY e.attempt_number", scope.visible()))
        .bind(scope.workspace).bind(scope.all).bind(scope.user).bind(root).fetch_all(&mut *tx).await?;
    let attempts: Vec<Value> = attempts
        .into_iter()
        .map(|(mut v, provider)| {
            v["data_policy"] = data_policy(&provider);
            v
        })
        .collect();
    // Neighbours within the current list filters.
    let all = format!(
        "{} SELECT root_request_id FROM roots WHERE {FILTERED}",
        logs::listed_roots(&scope, false)
    );
    let prev: Option<(Uuid,)> = bind_filters(
        sqlx::query_as(&format!("{all} AND (started_at,root_request_id)>($13,$14) ORDER BY started_at,root_request_id LIMIT 1")),
        &scope,
        &f,
    )
    .bind(started)
    .bind(root)
    .fetch_optional(&mut *tx)
    .await?;
    let next: Option<(Uuid,)> = bind_filters(
        sqlx::query_as(&format!("{all} AND (started_at,root_request_id)<($13,$14) ORDER BY started_at DESC,root_request_id DESC LIMIT 1")),
        &scope,
        &f,
    )
    .bind(started)
    .bind(root)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    summary["workspace_id"] = json!(ws);
    if let Some(o) = summary.as_object_mut() {
        o.insert("attempt_count".into(), o["attempts"].clone());
        o.insert("attempts".into(), Value::Array(attempts));
        o.insert("prev_id".into(), json!(prev.map(|p| p.0)));
        o.insert("next_id".into(), json!(next.map(|n| n.0)));
    }
    Ok(Json(summary))
}

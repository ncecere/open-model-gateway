//! Request logs: root requests and their attempt timelines. Metadata only,
//! never prompts or bodies. Members see their own human-key requests, shared
//! administrators the whole workspace, personal workspaces only their owner.
use super::*;
use chrono::{DateTime, NaiveDate, TimeDelta, Utc};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct RequestQuery {
    start_date: Option<String>,
    end_date: Option<String>,
    model: Option<String>,
    key_id: Option<Uuid>,
    status: Option<String>,
    q: Option<String>,
    cursor: Option<String>,
    limit: Option<i64>,
}
struct Filters {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    model: Option<String>,
    key: Option<Uuid>,
    status: Option<Vec<String>>,
    q: Option<String>,
}
pub(super) fn strict_date(s: &str) -> Result<NaiveDate, ApiError> {
    if s.len() != 10 {
        return Err(invalid());
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| invalid())
}
pub(super) fn midnight(d: NaiveDate) -> DateTime<Utc> {
    d.and_time(chrono::NaiveTime::MIN).and_utc()
}
impl RequestQuery {
    fn filters(&self) -> Result<Filters, ApiError> {
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
        })
    }
}
/// Root-request summaries visible to the caller ($1 workspace, $2 all, $3 user),
/// with optional root id ($4). Every predicate precedes aggregation.
const ROOTS: &str = "WITH att AS (SELECT e.*,k.name key_name,r.state accounting_state,r.actual_microusd,r.held_microusd FROM inference_executions e JOIN api_keys k ON k.id=e.api_key_id AND k.workspace_id=e.workspace_id LEFT JOIN governance_reservations r ON r.execution_id=e.id WHERE e.workspace_id=$1 AND ($2 OR k.issued_to_user_id=$3) AND {ROOT_FILTER}),
roots AS (SELECT root_request_id,min(started_at) started_at,CASE WHEN bool_and(completed_at IS NOT NULL) THEN max(completed_at) END completed_at,(array_agg(public_model ORDER BY attempt_number))[1] model,(array_agg(api_key_id ORDER BY attempt_number))[1] key_id,(array_agg(key_name ORDER BY attempt_number))[1] key_name,CASE (array_agg(state ORDER BY attempt_number DESC))[1] WHEN 'started' THEN 'in_progress' ELSE (array_agg(state ORDER BY attempt_number DESC))[1] END status,count(*) attempts,CASE WHEN count(*) FILTER(WHERE input_tokens IS NULL)=0 THEN sum(input_tokens)::text END input_tokens,CASE WHEN count(*) FILTER(WHERE output_tokens IS NULL)=0 THEN sum(output_tokens)::text END output_tokens,CASE WHEN count(*) FILTER(WHERE actual_microusd IS NULL)=0 THEN sum(actual_microusd)::text END cost_microusd,coalesce(sum(held_microusd) FILTER(WHERE actual_microusd IS NULL AND accounting_state IN('pending','unknown')),0)::text held_microusd,(array_agg(cost_center_id ORDER BY attempt_number))[1] cost_center_id,(array_agg(cost_center_name ORDER BY attempt_number))[1] cost_center_name,(array_agg(cost_center_code ORDER BY attempt_number))[1] cost_center_code,(array_agg(workload_kind ORDER BY attempt_number))[1] workload_kind,bool_or(streamed) streamed,array_agg(id) execution_ids FROM att GROUP BY root_request_id)";
const ROW: &str = "jsonb_build_object('root_request_id',root_request_id,'started_at',started_at,'completed_at',completed_at,'model',model,'key',jsonb_build_object('id',key_id,'name',key_name),'status',status,'attempts',attempts,'input_tokens',input_tokens,'output_tokens',output_tokens,'cost_microusd',cost_microusd,'held_microusd',held_microusd,'latency_ms',CASE WHEN completed_at IS NOT NULL THEN floor(extract(epoch FROM completed_at-started_at)*1000)::bigint END,'cost_center',CASE WHEN cost_center_id IS NULL THEN NULL ELSE jsonb_build_object('id',cost_center_id,'name',cost_center_name,'code',cost_center_code) END,'workload_kind',workload_kind,'streamed',streamed)";
/// Filters on roots: $4 start, $5 end, $6 model, $7 key, $8 statuses (any of), $9 id prefix.
const FILTERED: &str = "started_at>=$4 AND started_at<$5 AND ($6::text IS NULL OR model=$6) AND ($7::uuid IS NULL OR key_id=$7) AND ($8::text[] IS NULL OR status=ANY($8)) AND ($9::text IS NULL OR root_request_id::text LIKE $9||'%' OR EXISTS(SELECT 1 FROM unnest(execution_ids) x WHERE x::text LIKE $9||'%'))";
fn roots(filter: &str) -> String {
    ROOTS.replace("{ROOT_FILTER}", filter)
}
fn bind_filters<'q, O>(
    q: sqlx::query::QueryAs<'q, Postgres, O, sqlx::postgres::PgArguments>,
    ws: Uuid,
    all: bool,
    user: Uuid,
    f: &'q Filters,
) -> sqlx::query::QueryAs<'q, Postgres, O, sqlx::postgres::PgArguments>
where
    O: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
{
    q.bind(ws)
        .bind(all)
        .bind(user)
        .bind(f.start)
        .bind(f.end)
        .bind(f.model.as_deref())
        .bind(f.key)
        .bind(f.status.as_deref())
        .bind(f.q.as_deref())
}
fn cursor_parts(c: &str) -> Result<(DateTime<Utc>, Uuid), ApiError> {
    let (micros, id) = c.split_once('_').ok_or_else(invalid)?;
    let micros: i64 = micros.parse().map_err(|_| invalid())?;
    Ok((
        DateTime::from_timestamp_micros(micros).ok_or_else(invalid)?,
        Uuid::parse_str(id).map_err(|_| invalid())?,
    ))
}
pub(super) async fn requests(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<RequestQuery>,
) -> ApiResult {
    let f = p.filters()?;
    let limit = p.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(invalid());
    }
    let cursor = p.cursor.as_deref().map(cursor_parts).transpose()?;
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    sqlx::query("SET LOCAL statement_timeout='10s'")
        .execute(&mut *tx)
        .await?;
    // Roots are selected by their first attempt's start inside the range.
    let sql = format!(
        "{} SELECT {ROW},started_at,root_request_id FROM roots WHERE {FILTERED} AND ($10::timestamptz IS NULL OR (started_at,root_request_id)<($10,$11)) ORDER BY started_at DESC,root_request_id DESC LIMIT $12",
        roots(
            "e.root_request_id IN(SELECT x.root_request_id FROM inference_executions x WHERE x.workspace_id=$1 AND x.attempt_number=1 AND x.started_at>=$4 AND x.started_at<$5)"
        )
    );
    let rows: Vec<(Value, DateTime<Utc>, Uuid)> =
        bind_filters(sqlx::query_as(&sql), ws, a.view_all_activity, u.user_id, &f)
            .bind(cursor.map(|c| c.0))
            .bind(cursor.map(|c| c.1))
            .bind(limit + 1)
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    let next = (rows.len() > limit as usize).then(|| {
        let (_, t, id) = &rows[limit as usize - 1];
        format!("{}_{id}", t.timestamp_micros())
    });
    let data: Vec<Value> = rows.into_iter().take(limit as usize).map(|r| r.0).collect();
    Ok(Json(json!({"data":data,"next_cursor":next})))
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
pub(super) async fn request_detail(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, root)): Path<(Uuid, String)>,
    Query(p): Query<RequestQuery>,
) -> ApiResult {
    if p.cursor.is_some() || p.limit.is_some() {
        return Err(invalid());
    }
    // A short or malformed id is "not found", never a parser message
    // (resolve short ids through the list's `q` prefix search instead).
    let root = Uuid::parse_str(&root).map_err(|_| missing())?;
    let f = p.filters()?;
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    sqlx::query("SET LOCAL statement_timeout='10s'")
        .execute(&mut *tx)
        .await?;
    // Visibility of the root itself ignores list filters.
    let sql = format!(
        "{} SELECT {ROW},started_at FROM roots",
        roots("e.root_request_id=$4")
    );
    let (mut summary, started): (Value, DateTime<Utc>) = sqlx::query_as(&sql)
        .bind(ws)
        .bind(a.view_all_activity)
        .bind(u.user_id)
        .bind(root)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    let attempts: Vec<(Value, String)> = sqlx::query_as("SELECT jsonb_build_object('attempt_number',e.attempt_number,'execution_id',e.id,'state',e.state,'error_code',e.error_code,'started_at',e.started_at,'completed_at',e.completed_at,'latency_ms',e.elapsed_ms,'streamed',e.streamed,'workload_kind',e.workload_kind,'deployment',jsonb_build_object('id',d.id,'upstream_model',d.upstream_model),'connection',jsonb_build_object('id',p.id,'name',p.name,'provider',p.provider),'input_tokens',e.input_tokens::text,'output_tokens',e.output_tokens::text,'billing_usage',e.billing_usage,'meter_usage',e.meter_usage,'cost_microusd',r.actual_microusd::text,'held_microusd',CASE WHEN r.actual_microusd IS NOT NULL THEN '0' WHEN r.state IN('pending','unknown') THEN r.held_microusd::text END,'accounting_state',coalesce(r.state,'missing'),'unresolved_reason',CASE WHEN r.actual_microusd IS NOT NULL THEN NULL WHEN r.execution_id IS NULL THEN 'missing_reservation' WHEN r.price_id IS NULL THEN 'unpriced' WHEN r.unbounded_cost THEN 'unbounded_cost_or_unknown_rate' WHEN r.state='pending' THEN 'in_progress' ELSE 'incomplete_usage' END,'price_id',r.price_id,'pricing_version',dp.pricing_version,'failover_reason',CASE WHEN e.attempt_number>1 THEN coalesce(lag(e.error_code) OVER w,lag(e.state) OVER w) END,'details_redacted_at',e.details_redacted_at),p.provider FROM inference_executions e JOIN api_keys k ON k.id=e.api_key_id AND k.workspace_id=e.workspace_id JOIN deployments d ON d.id=e.deployment_id JOIN provider_connections p ON p.id=d.provider_connection_id LEFT JOIN governance_reservations r ON r.execution_id=e.id LEFT JOIN deployment_prices dp ON dp.id=r.price_id WHERE e.workspace_id=$1 AND e.root_request_id=$2 AND ($3 OR k.issued_to_user_id=$4) WINDOW w AS (ORDER BY e.attempt_number) ORDER BY e.attempt_number").bind(ws).bind(root).bind(a.view_all_activity).bind(u.user_id).fetch_all(&mut *tx).await?;
    let attempts: Vec<Value> = attempts
        .into_iter()
        .map(|(mut v, provider)| {
            v["data_policy"] = data_policy(&provider);
            v
        })
        .collect();
    // Neighbours within the current list filters: prev = newer, next = older.
    let all = format!(
        "{} SELECT root_request_id FROM roots WHERE {FILTERED}",
        roots(
            "e.root_request_id IN(SELECT x.root_request_id FROM inference_executions x WHERE x.workspace_id=$1 AND x.attempt_number=1 AND x.started_at>=$4 AND x.started_at<$5)"
        )
    );
    let prev: Option<(Uuid,)> = bind_filters(
        sqlx::query_as(&format!("{all} AND (started_at,root_request_id)>($10,$11) ORDER BY started_at,root_request_id LIMIT 1")),
        ws,
        a.view_all_activity,
        u.user_id,
        &f,
    )
    .bind(started)
    .bind(root)
    .fetch_optional(&mut *tx)
    .await?;
    let next: Option<(Uuid,)> = bind_filters(
        sqlx::query_as(&format!("{all} AND (started_at,root_request_id)<($10,$11) ORDER BY started_at DESC,root_request_id DESC LIMIT 1")),
        ws,
        a.view_all_activity,
        u.user_id,
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

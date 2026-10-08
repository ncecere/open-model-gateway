//! Usage analytics (overview tiles, top lists and an explore pivot). Same
//! authorization as cost reports: privacy predicates precede aggregation.
//! Money is integer micro-USD; counts, ratios and rates are decimal strings;
//! unknown is null.
use super::requests::strict_date;
use super::*;
use crate::governance::BudgetPeriod;
use chrono::{DateTime, NaiveDate, TimeDelta, Utc};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OverviewQuery {
    start_date: String,
    end_date: String,
    workspace_id: Option<Uuid>,
    model_id: Option<Uuid>,
    key_id: Option<Uuid>,
    member_user_id: Option<Uuid>,
    status: Option<String>,
    cost_center_id: Option<String>,
    service_account_id: Option<Uuid>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExploreQuery {
    start_date: String,
    end_date: String,
    workspace_id: Option<Uuid>,
    metric: String,
    group_by: String,
    then_by: Option<String>,
    top: Option<i64>,
    model_id: Option<Uuid>,
    key_id: Option<Uuid>,
    member_user_id: Option<Uuid>,
    status: Option<String>,
    cost_center_id: Option<String>,
    service_account_id: Option<Uuid>,
}
/// Optional narrowing filters. They only narrow rows the caller may already
/// aggregate; they never widen visibility.
#[derive(Default)]
struct Filters {
    model: Option<Uuid>,
    key: Option<Uuid>,
    member: Option<Uuid>,
    status: Option<Vec<String>>,
    cost_center: Option<Uuid>,
    unallocated: bool,
    service_account: Option<Uuid>,
}
/// Attempt status values (display form: `started` is `in_progress`).
pub(super) const STATUSES: [&str; 5] = [
    "succeeded",
    "failed",
    "cancelled",
    "indeterminate",
    "in_progress",
];
/// Strict comma-separated status list: known values, no empty items, deduplicated.
pub(super) fn statuses(s: &str) -> Result<Vec<String>, ApiError> {
    let mut out: Vec<String> = Vec::new();
    for v in s.split(',') {
        if !STATUSES.contains(&v) {
            return Err(invalid());
        }
        if !out.iter().any(|o| o == v) {
            out.push(v.to_owned());
        }
    }
    Ok(out)
}
/// `cost_center_id`: a UUID or `unallocated`.
pub(super) fn cost_center(s: Option<&str>) -> Result<(Option<Uuid>, bool), ApiError> {
    match s {
        None => Ok((None, false)),
        Some("unallocated") => Ok((None, true)),
        Some(v) => Ok((Some(Uuid::parse_str(v).map_err(|_| invalid())?), false)),
    }
}
fn filters(
    model: Option<Uuid>,
    key: Option<Uuid>,
    member: Option<Uuid>,
    status: Option<&str>,
    cc: Option<&str>,
    service_account: Option<Uuid>,
) -> Result<Filters, ApiError> {
    let (cost_center, unallocated) = cost_center(cc)?;
    Ok(Filters {
        model,
        key,
        member,
        status: status.map(statuses).transpose()?,
        cost_center,
        unallocated,
        service_account,
    })
}
fn period(start: &str, end: &str) -> Result<(NaiveDate, NaiveDate, NaiveDate), ApiError> {
    let (start, end) = (strict_date(start)?, strict_date(end)?);
    let days = (end - start).num_days();
    if !(1..=93).contains(&days)
        || end
            > Utc::now()
                .date_naive()
                .checked_add_signed(TimeDelta::days(1))
                .ok_or_else(invalid)?
    {
        return Err(invalid());
    }
    let prior = start
        .checked_sub_signed(TimeDelta::days(days))
        .ok_or_else(invalid)?;
    Ok((prior, start, end))
}
/// Scope: workspace (optionally own human keys only) or platform (optional workspace filter).
struct Scope {
    workspace: Option<Uuid>,
    own: Option<Uuid>,
    platform: bool,
    members: bool,
}
/// Attempts in `[$3,$4)` visible to the caller ($1 workspace, $2 own user),
/// narrowed by $5 model, $6 key (never a personal key at platform scope, $12),
/// $7 member (human keys), $8 statuses, $9/$10 cost center/unallocated and
/// $11 service account. Every predicate precedes aggregation.
const BASE: &str = "WITH rows AS (SELECT e.id,e.root_request_id,e.started_at,e.public_model,d.model_id,e.provider,e.workspace_id,w.name workspace_name,w.kind workspace_kind,e.api_key_id,k.name key_name,k.issued_to_user_id,CASE WHEN u.display_name IS NULL THEN u.email ELSE u.display_name||' · '||u.email END member_email,k.service_account_id,e.cost_center_id,e.cost_center_name,e.input_tokens,e.output_tokens,e.billing_usage,r.state accounting_state,r.actual_microusd,r.held_microusd FROM inference_executions e JOIN api_keys k ON k.id=e.api_key_id AND k.workspace_id=e.workspace_id JOIN workspaces w ON w.id=e.workspace_id JOIN deployments d ON d.id=e.deployment_id LEFT JOIN users u ON u.id=k.issued_to_user_id LEFT JOIN governance_reservations r ON r.execution_id=e.id WHERE ($1::uuid IS NULL OR e.workspace_id=$1) AND ($2::uuid IS NULL OR k.issued_to_user_id=$2) AND e.started_at>=($3::date::timestamp AT TIME ZONE 'UTC') AND e.started_at<($4::date::timestamp AT TIME ZONE 'UTC') AND ($5::uuid IS NULL OR d.model_id=$5) AND ($6::uuid IS NULL OR (e.api_key_id=$6 AND NOT ($12 AND w.kind='personal'))) AND ($7::uuid IS NULL OR (k.issued_to_user_id=$7 AND k.service_account_id IS NULL)) AND ($8::text[] IS NULL OR (CASE e.state WHEN 'started' THEN 'in_progress' ELSE e.state END)=ANY($8)) AND ($9::uuid IS NULL OR e.cost_center_id=$9) AND (NOT $10 OR e.cost_center_id IS NULL) AND ($11::uuid IS NULL OR k.service_account_id=$11))";
type Scalar<'q> = sqlx::query::QueryScalar<'q, Postgres, Value, sqlx::postgres::PgArguments>;
/// Binds the `BASE` parameters $1..$12.
fn bind_base<'q>(
    q: Scalar<'q>,
    sc: &Scope,
    start: NaiveDate,
    end: NaiveDate,
    f: &'q Filters,
) -> Scalar<'q> {
    q.bind(sc.workspace)
        .bind(sc.own)
        .bind(start)
        .bind(end)
        .bind(f.model)
        .bind(f.key)
        .bind(f.member)
        .bind(f.status.as_deref())
        .bind(f.cost_center)
        .bind(f.unallocated)
        .bind(f.service_account)
        .bind(sc.platform)
}
/// Numeric aggregate of a metric over grouped `rows` (null when unknown).
fn metric(name: &str) -> Option<&'static str> {
    Some(match name {
        "spend" => "coalesce(sum(actual_microusd),0)::numeric",
        "requests" => "count(DISTINCT root_request_id)::numeric",
        "attempts" => "count(id)::numeric",
        "tokens" => "coalesce(sum(coalesce(input_tokens,0)+coalesce(output_tokens,0)),0)::numeric",
        "input_tokens" => "coalesce(sum(input_tokens),0)::numeric",
        "output_tokens" => "coalesce(sum(output_tokens),0)::numeric",
        "unknown_token_attempts" => {
            "count(id) FILTER(WHERE input_tokens IS NULL OR output_tokens IS NULL)::numeric"
        }
        "held_microusd" => {
            "coalesce(sum(held_microusd) FILTER(WHERE actual_microusd IS NULL AND accounting_state IN('pending','unknown')),0)::numeric"
        }
        "unresolved_attempts" => "count(id) FILTER(WHERE actual_microusd IS NULL)::numeric",
        "cache_hit_rate" => {
            "(CASE WHEN coalesce(sum((billing_usage->>'total_input_tokens')::numeric) FILTER(WHERE billing_usage->>'total_input_tokens' IS NOT NULL AND billing_usage->>'cache_read_input_tokens' IS NOT NULL),0)>0 THEN trim_scale(round(sum((billing_usage->>'cache_read_input_tokens')::numeric) FILTER(WHERE billing_usage->>'total_input_tokens' IS NOT NULL AND billing_usage->>'cache_read_input_tokens' IS NOT NULL)/sum((billing_usage->>'total_input_tokens')::numeric) FILTER(WHERE billing_usage->>'total_input_tokens' IS NOT NULL AND billing_usage->>'cache_read_input_tokens' IS NOT NULL),4)) END)"
        }
        "blended_microusd_per_million" => {
            "(CASE WHEN coalesce(sum(input_tokens+output_tokens) FILTER(WHERE actual_microusd IS NOT NULL),0)>0 THEN trim_scale(round(sum(actual_microusd) FILTER(WHERE actual_microusd IS NOT NULL AND input_tokens IS NOT NULL AND output_tokens IS NOT NULL)*1000000/sum(input_tokens+output_tokens) FILTER(WHERE actual_microusd IS NOT NULL),4)) END)"
        }
        _ => return None,
    })
}
const TILES: [(&str, &[&str]); 5] = [
    ("spend", &["held_microusd", "unresolved_attempts"]),
    ("requests", &["attempts"]),
    (
        "tokens",
        &["input_tokens", "output_tokens", "unknown_token_attempts"],
    ),
    ("cache_hit_rate", &[]),
    ("blended_microusd_per_million", &[]),
];
/// `(id, name)` SQL of a grouping dimension. Platform scope collapses personal
/// workspace keys into one row; service-account keys collapse for members.
fn dimension(name: &str, platform: bool) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "model" => ("public_model", "public_model"),
        "provider" => ("provider", "provider"),
        "workspace" => ("workspace_id::text", "workspace_name"),
        "cost_center" => (
            "cost_center_id::text",
            "coalesce(cost_center_name,'Unallocated')",
        ),
        "key" if platform => (
            "CASE WHEN workspace_kind='personal' THEN NULL ELSE api_key_id::text END",
            "CASE WHEN workspace_kind='personal' THEN 'Personal workspace keys' ELSE key_name END",
        ),
        "key" => ("api_key_id::text", "key_name"),
        "member" => (
            "CASE WHEN service_account_id IS NULL THEN issued_to_user_id::text END",
            "CASE WHEN service_account_id IS NULL THEN member_email ELSE 'Service accounts' END",
        ),
        "day" => (
            "((started_at AT TIME ZONE 'UTC')::date)::text",
            "((started_at AT TIME ZONE 'UTC')::date)::text",
        ),
        _ => return None,
    })
}
fn share(value: &str, total: &str) -> String {
    format!("CASE WHEN {total}>0 THEN trim_scale(round(({value})/{total},4))::text END")
}
async fn scope<'a>(
    s: &'a Store,
    u: &BrowserPrincipal,
    ws: Option<Uuid>,
    filter: Option<Uuid>,
    f: &Filters,
) -> Result<(Transaction<'a, Postgres>, Scope), ApiError> {
    let mut tx = resources::installation_tx(s).await?;
    sqlx::query("SET LOCAL statement_timeout='10s'")
        .execute(&mut *tx)
        .await?;
    let scope = match ws {
        Some(ws) => {
            if filter.is_some_and(|f| f != ws) {
                return Err(invalid());
            }
            let a = resources::workspace_access(&mut tx, u, ws).await?;
            Scope {
                workspace: Some(ws),
                own: (!a.view_all_activity).then_some(u.user_id),
                platform: false,
                members: a.view_all_activity,
            }
        }
        None => {
            resources::platform_read(&mut tx, u.user_id).await?;
            Scope {
                workspace: filter,
                own: None,
                platform: true,
                members: true,
            }
        }
    };
    // A per-member filter is a member breakdown: workspace-wide visibility only.
    if f.member.is_some() && !scope.members {
        return Err(member_visibility());
    }
    Ok((tx, scope))
}
async fn deadline<T>(
    future: impl std::future::Future<Output = Result<T, ApiError>>,
) -> Result<T, ApiError> {
    tokio::time::timeout(std::time::Duration::from_secs(10), future)
        .await
        .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "Report deadline exceeded"))?
}
/// Installation-wide budget windows with settled, held and total use
/// (settled actual plus active holds by admission time, the admission rule).
/// Platform readers only; unaffected by report filters.
pub(super) async fn installation_budgets(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<Value, ApiError> {
    let limits = governance::installation_limits(tx).await?;
    let (now, created): (DateTime<Utc>, DateTime<Utc>) =
        sqlx::query_as("SELECT clock_timestamp(),created_at FROM installation WHERE singleton")
            .fetch_one(&mut **tx)
            .await?;
    let mut out = Vec::new();
    for (period, amount) in &limits.budgets {
        let (start, end) = period.window(now);
        let (settled, held, unresolved): (String, String, bool) = sqlx::query_as("SELECT coalesce(sum(r.actual_microusd) FILTER(WHERE r.state='settled'),0)::text,coalesce(sum(r.held_microusd) FILTER(WHERE r.state<>'settled'),0)::text,count(*) FILTER(WHERE r.state<>'settled' AND (r.unbounded_cost OR r.held_microusd IS NULL))>0 OR EXISTS(SELECT 1 FROM inference_executions e WHERE e.started_at>=$1 AND e.started_at<$2 AND NOT EXISTS(SELECT 1 FROM governance_reservations x WHERE x.execution_id=e.id)) FROM governance_reservations r WHERE r.admitted_at>=$1 AND r.admitted_at<$2").bind(start).bind(end).fetch_one(&mut **tx).await?;
        let parse = |v: &str| v.parse::<i128>().map_err(|_| invalid());
        let used = parse(&settled)? + parse(&held)?;
        let lifetime = *period == BudgetPeriod::Lifetime;
        out.push(json!({"period":period.as_str(),"amount_microusd":amount.to_string(),"used_microusd":used.to_string(),"settled_microusd":settled,"held_microusd":held,"unresolved_usage":unresolved,"exhausted":used>=i128::from(*amount),"window_start":if lifetime {created} else {start},"window_end":if lifetime {None} else {Some(end)}}));
    }
    Ok(Value::Array(out))
}
async fn overview(s: Store, u: BrowserPrincipal, ws: Option<Uuid>, p: OverviewQuery) -> ApiResult {
    deadline(async {
        let (prior, start, end) = period(&p.start_date, &p.end_date)?;
        let f = filters(
            p.model_id,
            p.key_id,
            p.member_user_id,
            p.status.as_deref(),
            p.cost_center_id.as_deref(),
            p.service_account_id,
        )?;
        let (mut tx, sc) = scope(&s, &u, ws, p.workspace_id, &f).await?;
        let all_metrics: Vec<&str> = TILES
            .iter()
            .flat_map(|(m, extra)| std::iter::once(*m).chain(extra.iter().copied()))
            .collect();
        let agg = all_metrics
            .iter()
            .map(|m| format!("{} AS {m}", metric(m).unwrap_or("NULL")))
            .collect::<Vec<_>>()
            .join(",");
        let tile = |m: &str, extra: &[&str]| {
            let mut fields = format!(
                "'value',c.{m}::text,'previous',p.{m}::text,'delta',(c.{m}-p.{m})::text,'change_ratio',CASE WHEN p.{m}>0 THEN trim_scale(round((c.{m}-p.{m})/p.{m},4))::text END,'daily',(SELECT coalesce(jsonb_agg(jsonb_build_object('date',d,'value',v) ORDER BY d),'[]'::jsonb) FROM (SELECT day::date::text d,{m}::text v FROM daily) x)"
            );
            for e in extra {
                fields.push_str(&format!(",'{e}',c.{e}::text"));
            }
            format!("'{m}',jsonb_build_object({fields})")
        };
        let tiles = TILES
            .iter()
            .map(|(m, extra)| tile(m, extra))
            .collect::<Vec<_>>()
            .join(",");
        // Top rows add the blended configured-rate cost per million tokens;
        // model rows add `model_id` when the alias maps to one model.
        let top = |dim: &str| -> Result<String, ApiError> {
            let (id, name) = dimension(dim, sc.platform).ok_or_else(invalid)?;
            let spend = metric("spend").ok_or_else(invalid)?;
            let (model_field, model_col) = if dim == "model" {
                (
                    ",'model_id',model_id",
                    ",CASE WHEN count(DISTINCT model_id)=1 THEN min(model_id::text) END model_id",
                )
            } else {
                ("", "")
            };
            Ok(format!(
                "(SELECT coalesce(jsonb_agg(jsonb_build_object('id',id,'name',name,'spend_microusd',spend::text,'requests',requests::text,'tokens',tokens::text,'share',{},'blended_microusd_per_million',blended::text{model_field}) ORDER BY spend DESC,name,id),'[]'::jsonb) FROM (SELECT {id} id,{name} name,{spend} spend,{} requests,{} tokens,{} blended{model_col} FROM rows WHERE started_at>=($13::date::timestamp AT TIME ZONE 'UTC') GROUP BY 1,2 ORDER BY 3 DESC,2,1 LIMIT 10) t)",
                share("spend", "(SELECT spend FROM cur)"),
                metric("requests").ok_or_else(invalid)?,
                metric("tokens").ok_or_else(invalid)?,
                metric("blended_microusd_per_million").ok_or_else(invalid)?,
            ))
        };
        let members = if sc.members {
            top("member")?
        } else {
            "NULL".into()
        };
        let sql = format!(
            "{BASE}, cur AS (SELECT {agg} FROM rows WHERE started_at>=($13::date::timestamp AT TIME ZONE 'UTC')), prev AS (SELECT {agg} FROM rows WHERE started_at<($13::date::timestamp AT TIME ZONE 'UTC')), daily AS (SELECT day,{agg} FROM generate_series($13::date::timestamp,($4::date-1)::timestamp,interval '1 day') day LEFT JOIN rows ON (rows.started_at AT TIME ZONE 'UTC')::date=day::date GROUP BY day) SELECT jsonb_build_object('scope','{}','period',jsonb_build_object('start_date',$13::date::text,'end_date',$4::date::text,'timezone','UTC'),'previous_period',jsonb_build_object('start_date',$3::date::text,'end_date',$13::date::text),'observed_at',statement_timestamp(),'basis','configured_rate_estimate','currency','USD','tiles',jsonb_build_object({tiles}),'top',jsonb_build_object('models',{},'keys',{},'members',{members})) FROM cur c,prev p",
            if sc.platform { "platform" } else { "workspace" },
            top("model")?,
            top("key")?,
        );
        let mut v: Value = bind_base(sqlx::query_scalar(&sql), &sc, prior, end, &f)
            .bind(start)
            .fetch_one(&mut *tx)
            .await?;
        v["installation_budgets"] = if sc.platform {
            installation_budgets(&mut tx).await?
        } else {
            Value::Null
        };
        tx.commit().await?;
        Ok(Json(v))
    })
    .await
}
pub(super) async fn workspace_overview(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<OverviewQuery>,
) -> ApiResult {
    overview(s, u, Some(ws), p).await
}
pub(super) async fn platform_overview(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<OverviewQuery>,
) -> ApiResult {
    overview(s, u, None, p).await
}
async fn explore(s: Store, u: BrowserPrincipal, ws: Option<Uuid>, p: ExploreQuery) -> ApiResult {
    deadline(async {
        let (_, start, end) = period(&p.start_date, &p.end_date)?;
        if !matches!(
            p.metric.as_str(),
            "spend" | "requests" | "tokens" | "cache_hit_rate"
        ) || p.then_by.as_deref() == Some(p.group_by.as_str())
        {
            return Err(invalid());
        }
        let top = p.top.unwrap_or(10);
        if !(1..=25).contains(&top) {
            return Err(invalid());
        }
        let f = filters(
            p.model_id,
            p.key_id,
            p.member_user_id,
            p.status.as_deref(),
            p.cost_center_id.as_deref(),
            p.service_account_id,
        )?;
        let (mut tx, sc) = scope(&s, &u, ws, p.workspace_id, &f).await?;
        let (gid, gname) = dimension(&p.group_by, sc.platform).ok_or_else(invalid)?;
        let then = p
            .then_by
            .as_deref()
            .map(|t| dimension(t, sc.platform).ok_or_else(invalid))
            .transpose()?;
        if !sc.members && (p.group_by == "member" || p.then_by.as_deref() == Some("member")) {
            return Err(member_visibility());
        }
        let m = metric(&p.metric).ok_or_else(invalid)?;
        let held = metric("held_microusd").ok_or_else(invalid)?;
        let unresolved = metric("unresolved_attempts").ok_or_else(invalid)?;
        let additive = p.metric != "cache_hit_rate";
        // Days are a time axis: all days in range, chronological.
        let day = p.group_by == "day";
        let (order, limit) = if day {
            ("gname", 93)
        } else {
            ("value DESC NULLS LAST,gname,gid", top)
        };
        let share_sql = |v: &str| {
            if additive {
                share(v, "(SELECT value FROM total)")
            } else {
                "NULL".into()
            }
        };
        let then_sql = match then {
            Some((tid, tname)) => format!(
                "(SELECT coalesce(jsonb_agg(jsonb_build_object('group',jsonb_build_object('id',gid,'name',gname),'then',jsonb_build_object('id',tid,'name',tname),'value',value::text,'share',{},'held_microusd',held::text,'unresolved_attempts',unresolved::text) ORDER BY gpos,rn),'[]'::jsonb) FROM (SELECT g.pos gpos,g.gname,x.*,row_number() OVER (PARTITION BY x.gid ORDER BY x.value DESC NULLS LAST,x.tname,x.tid) rn FROM (SELECT {gid} gid,{tid} tid,{tname} tname,{m} value,{held} held,{unresolved} unresolved FROM rows GROUP BY 1,2,3) x JOIN g ON g.gid IS NOT DISTINCT FROM x.gid) y WHERE rn<=5)",
                share_sql("value")
            ),
            None => format!(
                "(SELECT coalesce(jsonb_agg(jsonb_build_object('group',jsonb_build_object('id',gid,'name',gname),'then',NULL,'value',value::text,'share',{},'held_microusd',held::text,'unresolved_attempts',unresolved::text) ORDER BY pos),'[]'::jsonb) FROM g)",
                share_sql("value")
            ),
        };
        let other = if additive && !day {
            format!(
                "CASE WHEN (SELECT count(*) FROM groups)>{limit} THEN jsonb_build_object('value',((SELECT value FROM total)-(SELECT coalesce(sum(value),0) FROM g))::text,'share',{}) END",
                share_sql("(SELECT value FROM total)-(SELECT coalesce(sum(value),0) FROM g)")
            )
        } else {
            "NULL".into()
        };
        // Series values are decimal strings (null for an undefined ratio).
        let series = if day {
            "'[]'::jsonb".to_owned()
        } else {
            format!(
                "(SELECT coalesce(jsonb_agg(jsonb_build_object('date',d,'values',vals) ORDER BY d),'[]'::jsonb) FROM (SELECT day::date::text d,(SELECT coalesce(jsonb_agg(jsonb_build_object('id',g.gid,'value',(coalesce(s.value,{zero}))::text) ORDER BY g.pos),'[]'::jsonb) FROM g LEFT JOIN (SELECT {gid} gid,{m} value FROM rows WHERE (started_at AT TIME ZONE 'UTC')::date=day::date GROUP BY 1) s ON s.gid IS NOT DISTINCT FROM g.gid) vals FROM generate_series($3::date::timestamp,($4::date-1)::timestamp,interval '1 day') day) z)",
                zero = if additive { "0" } else { "NULL" },
            )
        };
        let sql = format!(
            "{BASE}, groups AS (SELECT {gid} gid,{gname} gname,{m} value,{held} held,{unresolved} unresolved FROM rows GROUP BY 1,2), g AS (SELECT row_number() OVER (ORDER BY {order}) pos,* FROM groups ORDER BY {order} LIMIT {limit}), total AS (SELECT {m} value,{held} held,{unresolved} unresolved FROM rows) SELECT jsonb_build_object('scope','{}','metric',$13::text,'group_by',$14::text,'then_by',$15::text,'period',jsonb_build_object('start_date',$3::date::text,'end_date',$4::date::text,'timezone','UTC'),'basis','configured_rate_estimate','currency','USD','total',(SELECT jsonb_build_object('value',value::text,'held_microusd',held::text,'unresolved_attempts',unresolved::text) FROM total),'rows',{then_sql},'other',{other},'truncated',(SELECT count(*) FROM groups)>{limit},'series',{series})",
            if sc.platform { "platform" } else { "workspace" },
        );
        let v: Value = bind_base(sqlx::query_scalar(&sql), &sc, start, end, &f)
            .bind(&p.metric)
            .bind(&p.group_by)
            .bind(p.then_by.as_deref())
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Json(v))
    })
    .await
}
pub(super) async fn workspace_explore(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<ExploreQuery>,
) -> ApiResult {
    explore(s, u, Some(ws), p).await
}
pub(super) async fn platform_explore(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<ExploreQuery>,
) -> ApiResult {
    explore(s, u, None, p).await
}

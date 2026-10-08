use super::*;
use axum::{extract::RawQuery, http::header};
use chrono::{Datelike, NaiveDate, TimeDelta, Utc};
use std::collections::HashMap;
#[derive(Default)]
struct Filters {
    start: NaiveDate,
    end: NaiveDate,
    compare: bool,
    workspace: Option<Uuid>,
    model: Option<String>,
    provider: Option<String>,
    cost_center: Option<Uuid>,
    unallocated: bool,
    actor: Option<Uuid>,
    account: Option<Uuid>,
    status: Option<String>,
    /// Exact API key (credential) id; never a personal key at platform scope.
    key: Option<Uuid>,
    /// Attempt states (`in_progress` = started), comma list.
    states: Option<Vec<String>>,
    limit: Option<i64>,
    offset: i64,
}
fn decode(s: &str) -> Result<String, ApiError> {
    let s = s.replace('+', " ");
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'%'
            && (i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit())
        {
            return Err(invalid());
        }
    }
    percent_encoding::percent_decode_str(&s)
        .decode_utf8()
        .map(|s| s.into_owned())
        .map_err(|_| invalid())
}
impl Filters {
    fn parse(raw: Option<String>, details: bool, csv: bool) -> Result<Self, ApiError> {
        let mut params = HashMap::new();
        if let Some(raw) = raw {
            if raw.len() > 4096 {
                return Err(invalid());
            }
            for pair in raw.split('&') {
                if pair.is_empty() {
                    return Err(invalid());
                }
                let (k, v) = pair.split_once('=').ok_or_else(invalid)?;
                let k = decode(k)?;
                let v = decode(v)?;
                if params.insert(k, v).is_some() {
                    return Err(invalid());
                }
            }
        }
        let date = |s: &str| -> Result<NaiveDate, ApiError> {
            if s.len() != 10 {
                return Err(invalid());
            }
            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|_| invalid())
                .and_then(|d| {
                    if (1..=9999).contains(&d.year()) {
                        Ok(d)
                    } else {
                        Err(invalid())
                    }
                })
        };
        let start = date(&params.remove("start_date").ok_or_else(invalid)?)?;
        let end = date(&params.remove("end_date").ok_or_else(invalid)?)?;
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
        let compare = match params.remove("compare").as_deref() {
            None | Some("none") => false,
            Some("previous_period") => true,
            _ => return Err(invalid()),
        };
        let uuid = |s: Option<String>| {
            s.map(|s| Uuid::parse_str(&s).map_err(|_| invalid()))
                .transpose()
        };
        let workspace = uuid(params.remove("workspace_id"))?;
        let actor = uuid(params.remove("actor_user_id"))?;
        let account = uuid(params.remove("service_account_id"))?;
        let key = uuid(params.remove("key_id"))?;
        let states = params
            .remove("status")
            .as_deref()
            .map(crate::management::usage::statuses)
            .transpose()?;
        let cc = params.remove("cost_center_id");
        let unallocated = cc.as_deref() == Some("unallocated");
        let cost_center = if unallocated { None } else { uuid(cc)? };
        let model = params.remove("model");
        let provider = params.remove("provider");
        if model
            .iter()
            .chain(provider.iter())
            .any(|s| s.is_empty() || s.len() > 200 || s.chars().any(char::is_control))
        {
            return Err(invalid());
        }
        let status = params.remove("accounting_status");
        if status
            .as_deref()
            .is_some_and(|s| !matches!(s, "pending" | "unknown" | "settled" | "missing"))
        {
            return Err(invalid());
        }
        let limit = params
            .remove("limit")
            .map(|s| s.parse::<i64>().map_err(|_| invalid()))
            .transpose()?;
        let has_offset = params.contains_key("offset");
        let offset = params
            .remove("offset")
            .map(|s| s.parse::<i64>().map_err(|_| invalid()))
            .transpose()?
            .unwrap_or(0);
        if !params.is_empty()
            || (!details && (limit.is_some() || has_offset))
            || !(0..=100000).contains(&offset)
            || limit.is_some_and(|n| !(1..=if csv { 1000 } else { 200 }).contains(&n))
        {
            return Err(invalid());
        }
        let f = Self {
            start,
            end,
            compare,
            workspace,
            model,
            provider,
            cost_center,
            unallocated,
            actor,
            account,
            status,
            key,
            states,
            limit,
            offset,
        };
        if f.compare {
            f.prior_start()?;
        }
        Ok(f)
    }
    fn prior_start(&self) -> Result<NaiveDate, ApiError> {
        let d = self
            .start
            .checked_sub_signed(TimeDelta::days((self.end - self.start).num_days()))
            .ok_or_else(invalid)?;
        if d.year() < 1 {
            return Err(invalid());
        }
        Ok(d)
    }
    fn workspace(&mut self, ws: Uuid) -> Result<(), ApiError> {
        if self.workspace.is_some_and(|v| v != ws) {
            return Err(invalid());
        }
        self.workspace = Some(ws);
        Ok(())
    }
}
// Every filter and the own-human-key predicate precedes aggregation and dimension discovery.
const BASE: &str = "WITH all_rows AS (SELECT e.*,k.issued_to_user_id,k.service_account_id,w.name workspace_name,r.execution_id reservation_id,r.price_id,r.state accounting_state,r.actual_microusd,r.held_microusd,r.unbounded_cost,r.lease_expires_at,r.cost_components,p.pricing_version,p.price_lines FROM inference_executions e JOIN api_keys k ON k.id=e.api_key_id AND k.workspace_id=e.workspace_id JOIN workspaces w ON w.id=e.workspace_id LEFT JOIN governance_reservations r ON r.execution_id=e.id LEFT JOIN deployment_prices p ON p.id=r.price_id WHERE ($1::uuid IS NULL OR e.workspace_id=$1) AND ($2::uuid IS NULL OR k.issued_to_user_id=$2) AND e.started_at >= ($12::date::timestamp AT TIME ZONE 'UTC') AND e.started_at < ($4::date::timestamp AT TIME ZONE 'UTC') AND ($5::text IS NULL OR e.public_model=$5) AND ($6::text IS NULL OR e.provider=$6) AND ($7::uuid IS NULL OR e.cost_center_id=$7) AND (NOT $8 OR e.cost_center_id IS NULL) AND ($9::uuid IS NULL OR k.issued_to_user_id=$9) AND ($10::uuid IS NULL OR k.service_account_id=$10) AND ($11::text IS NULL OR coalesce(r.state,'missing')=$11) AND ($13::uuid IS NULL OR (e.api_key_id=$13 AND NOT ($15 AND w.kind='personal'))) AND ($14::text[] IS NULL OR (CASE e.state WHEN 'started' THEN 'in_progress' ELSE e.state END)=ANY($14))), rows AS (SELECT * FROM all_rows WHERE started_at>=($3::date::timestamp AT TIME ZONE 'UTC'))";
const TOTALS: &str = "jsonb_build_object('known_cost_microusd',coalesce(sum(actual_microusd),0)::text,'held_microusd',coalesce(sum(held_microusd) FILTER(WHERE actual_microusd IS NULL AND accounting_state IN('pending','unknown')),0)::text,'attempts',count(id)::text,'root_requests',count(DISTINCT root_request_id)::text,'unresolved_attempts',count(id) FILTER(WHERE actual_microusd IS NULL)::text)";
fn bind<'q>(
    sql: &'q str,
    f: &Filters,
    own: Option<Uuid>,
    prior: NaiveDate,
    platform: bool,
) -> Result<sqlx::query::QueryScalar<'q, Postgres, Value, sqlx::postgres::PgArguments>, ApiError> {
    Ok(sqlx::query_scalar(sql)
        .bind(f.workspace)
        .bind(own)
        .bind(f.start)
        .bind(f.end)
        .bind(f.model.clone())
        .bind(f.provider.clone())
        .bind(f.cost_center)
        .bind(f.unallocated)
        .bind(f.actor)
        .bind(f.account)
        .bind(f.status.clone())
        .bind(prior)
        .bind(f.key)
        .bind(f.states.clone())
        .bind(platform))
}
/// Workload kinds whose attempts can produce a meter (SQL list).
fn meter_workloads(key: &str) -> &'static str {
    match key {
        "output_images" => "'images'",
        "input_characters" | "output_audio_seconds_ms" => "'audio_speech'",
        "input_audio_seconds_ms" => "'audio_transcriptions'",
        "search_units" => "'rerank'",
        _ => "'images','audio_transcriptions','audio_speech','rerank','systemone'",
    }
}
fn breakdown(id: &str, name: &str) -> String {
    format!(
        "(SELECT coalesce(jsonb_agg(v ORDER BY v->>'name',v->>'id'),'[]'::jsonb) FROM (SELECT jsonb_build_object('id',{id},'name',{name},'totals',{TOTALS}) v FROM rows GROUP BY {id},{name} ORDER BY {name},{id} LIMIT 100) b)"
    )
}
fn cost_centers() -> String {
    format!(
        "(SELECT coalesce(jsonb_agg(v ORDER BY v->>'name',v->>'id'),'[]'::jsonb) FROM (SELECT jsonb_build_object('id',cost_center_id::text,'name',coalesce(min(cost_center_name),'Unallocated'),'totals',{TOTALS}) v FROM rows GROUP BY cost_center_id ORDER BY min(cost_center_name),cost_center_id LIMIT 100) b)"
    )
}
async fn report(
    tx: &mut Transaction<'_, Postgres>,
    f: &Filters,
    own: Option<Uuid>,
    platform: bool,
) -> Result<Value, ApiError> {
    let components = CostComponents::default();
    let component_fields = serde_json::to_value(components)
        .map_err(|_| invalid())?
        .as_object()
        .ok_or_else(invalid)?
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let meter_component_fields =
        serde_json::to_value(crate::billing::MeterCostComponents::default())
            .map_err(|_| invalid())?
            .as_object()
            .ok_or_else(invalid)?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
    // Token components come from v2 and v3 settlements; meter components only from v3.
    let mut component_sql=component_fields.iter().map(|key|format!("'{key}',coalesce(sum((cost_components->>'{key}')::numeric) FILTER(WHERE accounting_state='settled' AND pricing_version IN(2,3)),0)::text")).chain(meter_component_fields.iter().map(|key|format!("'{key}',coalesce(sum((cost_components->>'{key}')::numeric) FILTER(WHERE accounting_state='settled' AND pricing_version=3),0)::text"))).collect::<Vec<_>>().join(",");
    component_sql.push_str(
        ",'legacy_microusd',coalesce(sum(actual_microusd) FILTER(WHERE pricing_version=1),0)::text",
    );
    // Observed meter totals; null when nothing was observed (unknown, not zero).
    let meter_sql = crate::billing::MeterUsage::KEYS
        .iter()
        .map(|key| format!("'{key}',sum((meter_usage->>'{key}')::numeric)::text"))
        .collect::<Vec<_>>()
        .join(",");
    // Attempts whose workload can produce each meter (unless the pinned v3
    // price marks it not applicable), and those among them where the meter
    // was not observed. A settled failure (a free pre-processing rejection)
    // used nothing. Decimal strings; the meter total is a lower bound
    // whenever its unknown count is nonzero.
    let relevant = |key: &str| {
        format!(
            "workload_kind IN({}) AND NOT coalesce(price_lines @> '[{{\"meter\":\"{key}\",\"not_applicable\":true}}]'::jsonb,false)",
            meter_workloads(key)
        )
    };
    let meter_relevant_sql = crate::billing::MeterUsage::KEYS
        .iter()
        .map(|key| format!("'{key}',count(*) FILTER(WHERE {})::text", relevant(key)))
        .collect::<Vec<_>>()
        .join(",");
    let meter_unknown_sql = crate::billing::MeterUsage::KEYS
        .iter()
        .map(|key| {
            format!(
                "'{key}',count(*) FILTER(WHERE {} AND (meter_usage->>'{key}') IS NULL AND NOT (state='failed' AND accounting_state='settled'))::text",
                relevant(key)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let billing_fields = [
        "total_input_tokens",
        "uncached_input_tokens",
        "cache_read_input_tokens",
        "cache_write_input_tokens",
        "cache_write_default_input_tokens",
        "cache_write_5m_input_tokens",
        "cache_write_1h_input_tokens",
    ];
    let billing_sql = billing_fields
        .iter()
        .map(|key| format!("'{key}',sum((billing_usage->>'{key}')::numeric)::text"))
        .collect::<Vec<_>>()
        .join(",");
    let comparison = if f.compare {
        format!(
            "jsonb_build_object('period',jsonb_build_object('start_date',$12::date::text,'end_date',$3::date::text,'timezone','UTC','includes_partial_day',false),'totals',(SELECT {TOTALS} FROM all_rows WHERE started_at<($3::date::timestamp AT TIME ZONE 'UTC')))"
        )
    } else {
        "NULL".into()
    };
    let service = if platform {
        "'[]'::jsonb".into()
    } else {
        breakdown(
            "service_account_id::text",
            "coalesce(service_account_id::text,'Human keys')",
        )
    };
    let sql = format!(
        "{BASE} SELECT jsonb_build_object('period',jsonb_build_object('start_date',$3::date::text,'end_date',$4::date::text,'timezone','UTC','includes_partial_day',$4::date>(statement_timestamp() AT TIME ZONE 'UTC')::date),'scope','{}','observed_at',statement_timestamp(),'basis','configured_rate_estimate','currency','USD','totals',(SELECT {TOTALS} FROM rows),'health',(SELECT jsonb_build_object('pending_attempts',count(*) FILTER(WHERE accounting_state='pending')::text,'unknown_attempts',count(*) FILTER(WHERE accounting_state='unknown')::text,'missing_reservation_attempts',count(*) FILTER(WHERE reservation_id IS NULL)::text,'unpriced_attempts',count(*) FILTER(WHERE price_id IS NULL)::text,'aged_hold_attempts',count(*) FILTER(WHERE accounting_state IN('pending','unknown') AND lease_expires_at<=statement_timestamp())::text,'unbounded_attempts',count(*) FILTER(WHERE coalesce(unbounded_cost,reservation_id IS NULL))::text) FROM rows),'daily',(SELECT coalesce(jsonb_agg(v ORDER BY v->>'date'),'[]'::jsonb) FROM (SELECT jsonb_build_object('date',day::date::text,'totals',{TOTALS}) v FROM generate_series($3::date::timestamp,($4::date-1)::timestamp,interval '1 day') AS calendar(day) LEFT JOIN rows ON (rows.started_at AT TIME ZONE 'UTC')::date=day::date GROUP BY day) d),'breakdowns',jsonb_build_object('models',{},'providers',{},'workspaces',{},'cost_centers',{},'service_accounts',{}),'comparison',{},'coverage',(SELECT jsonb_build_object('priced_attempts',count(*) FILTER(WHERE price_id IS NOT NULL)::text,'settled_attempts',count(*) FILTER(WHERE accounting_state='settled')::text,'total_attempts',count(*)::text,'complete_billing_attempts',count(*) FILTER(WHERE billing_usage IS NOT NULL AND (SELECT count(*)=7 AND bool_and(j.value<>'null'::jsonb) FROM jsonb_each(billing_usage) j))::text,'legacy_pricing_attempts',count(*) FILTER(WHERE pricing_version=1)::text,'incomplete_billing_attempts',count(*) FILTER(WHERE billing_usage IS NULL OR NOT (SELECT count(*)=7 AND bool_and(j.value<>'null'::jsonb) FROM jsonb_each(billing_usage) j))::text) FROM rows),'cost_components',(SELECT jsonb_build_object({component_sql}) FROM rows),'billing_usage',(SELECT jsonb_build_object({billing_sql}) FROM rows),'meter_usage',(SELECT jsonb_build_object({meter_sql}) FROM rows),'meter_relevant_attempts',(SELECT jsonb_build_object({meter_relevant_sql}) FROM rows),'meter_unknown_attempts',(SELECT jsonb_build_object({meter_unknown_sql}) FROM rows),'provider_reported_cost_microusd',(SELECT sum(provider_cost_microusd)::text FROM rows),'breakdowns_truncated',(SELECT count(DISTINCT public_model)>100 OR count(DISTINCT provider)>100 OR count(DISTINCT workspace_id)>100 OR count(DISTINCT coalesce(cost_center_id::text,'unallocated'))>100 OR {} FROM rows))",
        if platform { "platform" } else { "workspace" },
        breakdown("public_model", "public_model"),
        breakdown("provider", "provider"),
        breakdown("workspace_id::text", "workspace_name"),
        cost_centers(),
        service,
        comparison,
        if platform {
            "false"
        } else {
            "count(DISTINCT coalesce(service_account_id::text,'human'))>100"
        }
    );
    bind(
        &sql,
        f,
        own,
        if f.compare { f.prior_start()? } else { f.start },
        platform,
    )?
    .fetch_one(&mut **tx)
    .await
    .map_err(Into::into)
}
async fn report_tx(s: &Store) -> Result<Transaction<'_, Postgres>, ApiError> {
    let mut tx = s.pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='10s'")
        .execute(&mut *tx)
        .await?;
    resources::catalog_lock(&mut tx, false).await?;
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM installation WHERE singleton FOR NO KEY UPDATE")
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    Ok(tx)
}
async fn workspace_report_tx<'a>(
    s: &'a Store,
    u: &BrowserPrincipal,
    ws: Uuid,
) -> Result<(Transaction<'a, Postgres>, resources::WorkspaceAccess), ApiError> {
    let mut tx = report_tx(s).await?;
    let access = resources::workspace_access(&mut tx, u, ws).await?;
    Ok((tx, access))
}
async fn deadline<T>(
    future: impl std::future::Future<Output = Result<T, ApiError>>,
) -> Result<T, ApiError> {
    tokio::time::timeout(std::time::Duration::from_secs(10), future)
        .await
        .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "Report deadline exceeded"))?
}
pub(super) async fn workspace_report(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    RawQuery(raw): RawQuery,
) -> ApiResult {
    deadline(async {
        let mut f = Filters::parse(raw, false, false)?;
        f.workspace(ws)?;
        let (mut tx, a) = workspace_report_tx(&s, &u, ws).await?;
        let value = report(
            &mut tx,
            &f,
            if a.view_all_activity {
                None
            } else {
                Some(u.user_id)
            },
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(Json(value))
    })
    .await
}
pub(super) async fn platform_report(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    RawQuery(raw): RawQuery,
) -> ApiResult {
    deadline(async {
        let f = Filters::parse(raw, false, false)?;
        let mut tx = report_tx(&s).await?;
        resources::platform_read(&mut tx, u.user_id).await?;
        let value = report(&mut tx, &f, None, true).await?;
        tx.commit().await?;
        Ok(Json(value))
    })
    .await
}
pub(super) async fn cost_summary(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
) -> ApiResult {
    deadline(async {let now=Utc::now().date_naive();let f=Filters{workspace:Some(ws),start:now.with_day(1).ok_or_else(invalid)?,end:now.checked_add_signed(TimeDelta::days(1)).ok_or_else(invalid)?,..Default::default()};let(mut tx,a)=workspace_report_tx(&s,&u,ws).await?;let value=report(&mut tx,&f,if a.view_all_activity{None}else{Some(u.user_id)},false).await?;tx.commit().await?;Ok(Json(json!({"currency":"USD","known_cost_microusd":value["totals"]["known_cost_microusd"],"held_microusd":value["totals"]["held_microusd"],"unknown_cost_requests":value["totals"]["unresolved_attempts"],"requests":value["totals"]["attempts"]})))}).await
}
async fn cost_rows(
    tx: &mut Transaction<'_, Postgres>,
    f: &Filters,
    own: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Value>, ApiError> {
    let sql = format!(
        "{BASE} SELECT jsonb_build_object('id',id,'root_request_id',root_request_id,'attempt_number',attempt_number,'public_model',public_model,'provider',provider,'state',state,'workload_kind',workload_kind,'started_at',started_at,'input_tokens',input_tokens::text,'output_tokens',output_tokens::text,'billing_usage',billing_usage,'meter_usage',meter_usage,'output_image_variant',output_image_variant,'provider_cost_microusd',provider_cost_microusd::text,'cost_components',cost_components,'price_id',price_id,'pricing_version',pricing_version,'cost_microusd',actual_microusd::text,'reserved_microusd',held_microusd::text,'active_held_microusd',CASE WHEN actual_microusd IS NOT NULL THEN '0' ELSE held_microusd::text END,'unresolved_reason',CASE WHEN actual_microusd IS NOT NULL THEN NULL WHEN reservation_id IS NULL THEN 'missing_reservation' WHEN price_id IS NULL THEN 'unpriced' WHEN unbounded_cost THEN 'unbounded_cost_or_unknown_rate' WHEN pricing_version=1 THEN 'incomplete_raw_usage' WHEN pricing_version=3 THEN 'incomplete_or_unpriced_meter_usage' WHEN billing_usage IS NULL THEN 'missing_billing_usage' ELSE 'incomplete_usage_or_cache_allocation' END,'cost_status',coalesce(accounting_state,'unknown'),'unbounded_cost',coalesce(unbounded_cost,true),'cost_center_id',cost_center_id,'cost_center_name',cost_center_name,'cost_center_code',cost_center_code,'details_redacted_at',details_redacted_at) FROM rows ORDER BY started_at DESC,id LIMIT $16 OFFSET $17"
    );
    Ok(bind(&sql, f, own, f.start, false)?
        .bind(limit)
        .bind(f.offset)
        .fetch_all(&mut **tx)
        .await?)
}
pub(super) async fn costs(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    RawQuery(raw): RawQuery,
) -> ApiResult {
    deadline(async {
        let mut f = Filters::parse(raw, true, false)?;
        f.workspace(ws)?;
        let (mut tx, a) = workspace_report_tx(&s, &u, ws).await?;
        let limit = f.limit.unwrap_or(100);
        let mut data = cost_rows(
            &mut tx,
            &f,
            if a.view_all_activity {
                None
            } else {
                Some(u.user_id)
            },
            limit + 1,
        )
        .await?;
        let more = data.len() > limit as usize;
        data.truncate(limit as usize);
        tx.commit().await?;
        Ok(Json(json!({"data":data,"has_more":more})))
    })
    .await
}
fn csv_cell(value: &str) -> String {
    let trimmed = value.trim_start_matches(|c: char| c.is_whitespace() || c.is_control());
    format!(
        "\"{}{}\"",
        if trimmed.starts_with(['=', '+', '-', '@']) || value.starts_with(['\t', '\r', '\n']) {
            "'"
        } else {
            ""
        },
        value.replace('"', "\"\"")
    )
}
pub(super) async fn usage_export(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    deadline(async {
        let mut f = Filters::parse(raw, true, true)?;
        f.workspace(ws)?;
        let (mut tx, a) = workspace_report_tx(&s, &u, ws).await?;
        let limit = f.limit.unwrap_or(1000);
        let data = cost_rows(
            &mut tx,
            &f,
            if a.view_all_activity {
                None
            } else {
                Some(u.user_id)
            },
            limit,
        )
        .await?;
        tx.commit().await?;
        let fields = [
            "id",
            "root_request_id",
            "attempt_number",
            "public_model",
            "provider",
            "state",
            "workload_kind",
            "started_at",
            "input_tokens",
            "output_tokens",
            "billing_usage",
            "cost_components",
            "price_id",
            "pricing_version",
            "cost_microusd",
            "reserved_microusd",
            "active_held_microusd",
            "unresolved_reason",
            "cost_status",
            "unbounded_cost",
            "cost_center_id",
            "cost_center_name",
            "cost_center_code",
            // Appended in Phase 1; earlier columns keep their positions.
            "meter_usage",
            "output_image_variant",
            "provider_cost_microusd",
        ];
        let mut csv = format!("{}\r\n", fields.join(","));
        for row in &data {
            csv.push_str(
                &fields
                    .iter()
                    .map(|field| {
                        csv_cell(&match &row[*field] {
                            Value::Null => String::new(),
                            Value::String(v) => v.clone(),
                            v => v.to_string(),
                        })
                    })
                    .collect::<Vec<_>>()
                    .join(","),
            );
            csv.push_str("\r\n");
        }
        Ok((
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_owned()),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"usage.csv\"".to_owned(),
                ),
                (header::CACHE_CONTROL, "no-store".to_owned()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
                (
                    header::HeaderName::from_static("x-export-limit"),
                    limit.to_string(),
                ),
                (
                    header::HeaderName::from_static("x-export-offset"),
                    f.offset.to_string(),
                ),
                (
                    header::HeaderName::from_static("x-export-rows"),
                    data.len().to_string(),
                ),
            ],
            csv,
        )
            .into_response())
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reconcile {
    input_tokens: String,
    output_tokens: String,
    #[serde(deserialize_with = "nullable")]
    billing_usage: Option<crate::billing::BillingUsage>,
    evidence: String,
    /// Optional meter evidence; when present all six counters are explicit.
    #[serde(default)]
    meter_usage: Option<crate::billing::MeterUsage>,
    #[serde(default)]
    output_image_variant: Option<crate::billing::MeterVariant>,
    #[serde(default)]
    provider_cost_microusd: Option<String>,
}
pub(super) async fn reconcile(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, execution)): Path<(Uuid, Uuid)>,
    Json(p): Json<Reconcile>,
) -> ApiResult {
    let (mut tx, _) = resources::workspace_tx(&s, &u, ws).await?;
    resources::platform_write(&mut tx, u.user_id).await?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM inference_executions WHERE workspace_id=$1 AND id=$2",
    )
    .bind(ws)
    .bind(execution)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    tx.commit().await?;
    let usage = Usage {
        input_tokens: Some(money(&p.input_tokens, false)? as u64),
        output_tokens: Some(money(&p.output_tokens, false)? as u64),
        billing: p.billing_usage,
        meters: p.meter_usage,
        output_image_variant: p.output_image_variant,
        provider_cost_microusd: p
            .provider_cost_microusd
            .as_deref()
            .map(|v| money(v, false))
            .transpose()?,
    };
    crate::governance::resolve_usage(&s, ws, execution, usage, &p.evidence, u.user_id)
        .await
        .map_err(|e| match e {
            InferenceError::InvalidRequest => invalid(),
            InferenceError::Configuration => ApiError(
                StatusCode::CONFLICT,
                "Execution has no complete pinned valuation",
            ),
            _ => ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Reconciliation unavailable",
            ),
        })?;
    Ok(ok())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_dates_and_filters() {
        for query in [
            "start_date=2025-01-01&end_date=2025-04-05",
            "start_date=2025-02-29&end_date=2025-03-01",
            "start_date=2025-01-01&end_date=2025-01-02&provider=a&provider=b",
            "start_date=2025-01-01&end_date=2025-01-02&typo=x",
            "start_date=2025-01-01&end_date=2025-01-02&provider=%GG",
        ] {
            assert!(Filters::parse(Some(query.into()), false, false).is_err());
        }
        let f = Filters::parse(
            Some("start_date=2025-01-01&end_date=2025-04-04&compare=previous_period".into()),
            false,
            false,
        )
        .unwrap();
        assert_eq!((f.end - f.start).num_days(), 93);
        assert_eq!(f.prior_start().unwrap().to_string(), "2024-09-30");
    }
    #[test]
    fn csv_formula_safety() {
        assert_eq!(csv_cell(" =SUM(A1)"), "\"' =SUM(A1)\"");
        assert_eq!(csv_cell("a\"b"), "\"a\"\"b\"");
    }
}

use super::*;
use crate::billing::v3::{MaxUnits, PriceLines, display_lines, display_summary};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Price {
    /// Required for v1/v2; forbidden for v3, whose token rates are price lines.
    #[serde(default)]
    input_microusd_per_million: Option<String>,
    #[serde(default)]
    output_microusd_per_million: Option<String>,
    input_token_limit: i64,
    output_token_limit: i64,
    #[serde(default = "legacy")]
    pricing_version: i16,
    #[serde(default)]
    cache_pricing: Option<CachePricing>,
    #[serde(default)]
    price_lines: Option<PriceLines>,
    #[serde(default)]
    max_units: Option<MaxUnits>,
    /// Batch price list (0021, v3 only): the rates native batches are charged
    /// (a provider's published batch prices; never derived). Shares the token
    /// ceilings and `max_units`.
    #[serde(default)]
    batch_price_lines: Option<PriceLines>,
}
fn legacy() -> i16 {
    1
}
pub(super) async fn prices(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Query(page): Query<Page>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    exists(&mut tx, "deployments", id).await?;
    let (limit, offset) = page.bounds()?;
    let mut data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'deployment_id',deployment_id,'input_microusd_per_million',input_microusd_per_million::text,'output_microusd_per_million',output_microusd_per_million::text,'input_token_limit',input_token_limit,'output_token_limit',output_token_limit,'pricing_version',pricing_version,'cache_pricing',cache_pricing,'price_lines',price_lines,'max_units',max_units,'batch_price_lines',batch_price_lines,'created_at',created_at) FROM deployment_prices WHERE deployment_id=$1 ORDER BY created_at DESC,id DESC LIMIT $2 OFFSET $3").bind(id).bind(limit+1).bind(offset).fetch_all(&mut *tx).await?;
    let more = data.len() > limit as usize;
    data.truncate(limit as usize);
    for row in &mut data {
        add_display(row);
    }
    tx.commit().await?;
    Ok(Json(json!({"data":data,"has_more":more})))
}
async fn exists(tx: &mut Transaction<'_, Postgres>, table: &str, id: Uuid) -> Result<(), ApiError> {
    sqlx::query_scalar::<_, Uuid>(&format!("SELECT id FROM {table} WHERE id=$1 FOR SHARE"))
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)?;
    Ok(())
}
/// Exact human display for v3 lines, computed from integers (never floats).
/// `display_lines[i]` describes `price_lines[i]`; v1/v2 rows get null. A
/// batch price list gets `batch_display_lines`/`batch_display_summary`.
pub(crate) fn add_display(row: &mut Value) {
    let display = |key: &str| {
        let lines = row
            .get(key)
            .cloned()
            .and_then(|v| serde_json::from_value::<PriceLines>(v).ok());
        match lines {
            Some(l) => (json!(display_lines(&l)), json!(display_summary(&l))),
            None => (Value::Null, Value::Null),
        }
    };
    let (lines_display, summary) = display("price_lines");
    let (batch_display, batch_summary) = display("batch_price_lines");
    if let Some(obj) = row.as_object_mut() {
        obj.insert("display_lines".into(), lines_display);
        obj.insert("display_summary".into(), summary);
        obj.insert("batch_display_lines".into(), batch_display);
        obj.insert("batch_display_summary".into(), batch_summary);
    }
}
/// A batch list covers exactly the standard list's meters (same meter,
/// variant and tier keys, same not-applicable meters), so switching lists
/// never turns a priced meter into a missing one.
fn same_meters(standard: &PriceLines, batch: &PriceLines) -> bool {
    standard.1 == batch.1 && same_valued_meters(standard, batch)
}
fn same_valued_meters(standard: &PriceLines, batch: &PriceLines) -> bool {
    let keys = |l: &PriceLines| {
        let mut k: Vec<String> = serde_json::to_value(l)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|line| {
                format!(
                    "{}|{}|{}|{}",
                    line["meter"],
                    line["variant"],
                    line["min_prompt_tokens"],
                    line["not_applicable"]
                )
            })
            .collect();
        k.sort();
        k
    };
    keys(standard) == keys(batch)
}
/// A price version whose exact integer bounds have been checked; only this can be inserted.
pub(crate) struct ValidPrice {
    price: Price,
    input: Option<i64>,
    output: Option<i64>,
}
pub(super) async fn create_price(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(deployment): Path<Uuid>,
    Json(p): Json<Price>,
) -> DetailedResult {
    let p = validate_price(p)?;
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    let id = insert_price(&mut tx, &u, deployment, p).await?;
    tx.commit().await?;
    Ok(identifier(id))
}
pub(crate) fn validate_price(p: Price) -> Result<ValidPrice, ApiError> {
    // V3 prices whose input-family token meters are all not applicable (speech,
    // transcription) may use an input ceiling of 0; every other price keeps a
    // positive input ceiling.
    let zero_input_ok = p.pricing_version == 3
        && p.input_token_limit == 0
        && p.price_lines
            .as_ref()
            .is_some_and(PriceLines::input_tokens_inapplicable);
    if !(positive_limit(p.input_token_limit) || zero_input_ok)
        || !(0..=i64::from(i32::MAX)).contains(&p.output_token_limit)
    {
        return Err(invalid());
    }
    if p.pricing_version == 3 {
        let (Some(lines), None, None, None) = (
            &p.price_lines,
            &p.cache_pricing,
            &p.input_microusd_per_million,
            &p.output_microusd_per_million,
        ) else {
            return Err(invalid());
        };
        // Even an unbounded (unknown) bound must not overflow when computed.
        crate::billing::v3::bound(
            lines,
            p.max_units.as_ref().unwrap_or(&MaxUnits::default()),
            p.input_token_limit as u64,
            p.output_token_limit as u64,
        )
        .map_err(|_| invalid())?;
        // The batch list is validated the same way and must price the same
        // meters (it replaces the standard list for native batches).
        if let Some(batch) = &p.batch_price_lines {
            crate::billing::v3::bound(
                batch,
                p.max_units.as_ref().unwrap_or(&MaxUnits::default()),
                p.input_token_limit as u64,
                p.output_token_limit as u64,
            )
            .map_err(|_| invalid())?;
            if !same_meters(lines, batch) {
                return Err(invalid());
            }
        }
        return Ok(ValidPrice {
            price: Price {
                max_units: Some(p.max_units.clone().unwrap_or_default()),
                ..p
            },
            input: None,
            output: None,
        });
    }
    if p.price_lines.is_some() || p.max_units.is_some() || p.batch_price_lines.is_some() {
        return Err(invalid());
    }
    let input = money(
        p.input_microusd_per_million
            .as_deref()
            .ok_or_else(invalid)?,
        false,
    )?;
    let output = money(
        p.output_microusd_per_million
            .as_deref()
            .ok_or_else(invalid)?,
        false,
    )?;
    match (p.pricing_version, &p.cache_pricing) {
        (1, None) => {
            crate::billing::charge(p.input_token_limit as u64, input)
                .and_then(|a| {
                    crate::billing::charge(p.output_token_limit as u64, output).and_then(|b| {
                        a.checked_add(b)
                            .ok_or(crate::billing::BillingError::Overflow)
                    })
                })
                .map_err(|_| invalid())?;
        }
        (2, Some(r)) => {
            crate::billing::bound_v2(
                input,
                output,
                r,
                p.input_token_limit as u64,
                p.output_token_limit as u64,
            )
            .map_err(|_| invalid())?;
        }
        _ => return Err(invalid()),
    };
    Ok(ValidPrice {
        price: p,
        input: Some(input),
        output: Some(output),
    })
}
/// Inserts and audits an immutable price version. The caller holds an exclusive `catalog_tx`.
pub(crate) async fn insert_price(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    deployment: Uuid,
    v: ValidPrice,
) -> Result<Uuid, ManagementError> {
    let ValidPrice {
        price: p,
        input,
        output,
    } = v;
    exists(tx, "deployments", deployment).await?;
    if let Some(lines) = &p.price_lines {
        meters_complete(tx, deployment, lines).await?;
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,cache_pricing,price_lines,max_units,batch_price_lines) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)").bind(id).bind(deployment).bind(input).bind(output).bind(p.input_token_limit).bind(p.output_token_limit).bind(p.pricing_version).bind(p.cache_pricing.map(serde_json::to_value).transpose().map_err(|_|invalid())?).bind(p.price_lines.map(serde_json::to_value).transpose().map_err(|_|invalid())?).bind(p.max_units.map(serde_json::to_value).transpose().map_err(|_|invalid())?).bind(p.batch_price_lines.map(serde_json::to_value).transpose().map_err(|_|invalid())?).execute(&mut **tx).await?;
    resources::audit(
        tx,
        u,
        None,
        "price.created",
        "price",
        Some(id),
        json!({"deployment_id":deployment}),
    )
    .await?;
    Ok(id)
}
/// A v3 price must state every meter its route's workload can use (priced,
/// `not_applicable` or `unknown`): an omitted meter would silently leave every
/// budgeted request unbounded. 400 `price_meters_incomplete` with
/// `missing_meters`.
async fn meters_complete(
    tx: &mut Transaction<'_, Postgres>,
    deployment: Uuid,
    lines: &PriceLines,
) -> Result<(), ManagementError> {
    let protocols: Vec<String> = sqlx::query_scalar("SELECT m.supported_protocols FROM deployments d JOIN models m ON m.id=d.model_id WHERE d.id=$1").bind(deployment).fetch_one(&mut **tx).await?;
    let kind = protocols
        .first()
        .and_then(|p| crate::inference::types::ApiProtocol::parse(p))
        .map_or(crate::inference::types::WorkloadKind::Generation, |p| {
            p.workload()
        });
    let stated = lines.meters();
    let missing: Vec<&str> = crate::governance::price_meters(kind)
        .into_iter()
        .filter(|m| !stated.contains(m))
        .map(|m| m.as_str())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(ManagementError::detailed(
        StatusCode::BAD_REQUEST,
        "price_meters_incomplete",
        format!(
            "A {} price must state every meter the route can use as priced, not_applicable or unknown; missing: {}",
            crate::providers::capabilities::workload_label(kind),
            missing.join(", ")
        ),
        json!({"missing_meters":missing}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelRouting {
    strategy: String,
    max_attempts: i32,
    allow_ambiguous_failover: bool,
    #[serde(deserialize_with = "nullable")]
    required_residency: Option<String>,
}
pub(super) async fn model_routing(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(model): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let p:Value=sqlx::query_scalar("SELECT jsonb_build_object('strategy',coalesce(r.strategy,'priority'),'max_attempts',coalesce(r.max_attempts,1),'allow_ambiguous_failover',coalesce(r.allow_ambiguous_failover,false),'required_residency',r.required_residency) FROM models m LEFT JOIN routing_policies r ON r.model_id=m.id WHERE m.id=$1").bind(model).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    tx.commit().await?;
    Ok(Json(json!({"policy":p})))
}
pub(super) async fn put_model_routing(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(model): Path<Uuid>,
    Json(p): Json<ModelRouting>,
) -> ApiResult {
    if !matches!(p.strategy.as_str(), "priority" | "weighted")
        || !(1..=3).contains(&p.max_attempts)
        || p.required_residency
            .as_deref()
            .is_some_and(|v| v == "unspecified" || !residency_valid(v))
    {
        return Err(invalid());
    }
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    exists(&mut tx, "models", model).await?;
    sqlx::query("INSERT INTO routing_policies(model_id,strategy,max_attempts,allow_ambiguous_failover,required_residency) VALUES($1,$2,$3,$4,$5) ON CONFLICT(model_id) DO UPDATE SET strategy=excluded.strategy,max_attempts=excluded.max_attempts,allow_ambiguous_failover=excluded.allow_ambiguous_failover,required_residency=excluded.required_residency").bind(model).bind(p.strategy).bind(p.max_attempts).bind(p.allow_ambiguous_failover).bind(p.required_residency).execute(&mut *tx).await?;
    resources::audit(
        &mut tx,
        &u,
        None,
        "routing.model_updated",
        "model",
        Some(model),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeploymentRouting {
    priority: i32,
    weight: i32,
    #[serde(deserialize_with = "nullable")]
    residency: Option<String>,
    failure_threshold: i32,
    cooldown_seconds: i32,
}
pub(super) async fn deployment_routing(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let v:Value=sqlx::query_scalar("SELECT jsonb_build_object('routing',jsonb_build_object('priority',coalesce(r.priority,0),'weight',coalesce(r.weight,1),'residency',r.residency,'failure_threshold',coalesce(r.failure_threshold,3),'cooldown_seconds',coalesce(r.cooldown_seconds,30)),'health',jsonb_build_object('consecutive_failures',coalesce(h.consecutive_failures,0),'open_until',h.open_until,'last_observed_at',h.last_observed_at)) FROM deployments d LEFT JOIN deployment_routing r ON r.deployment_id=d.id LEFT JOIN deployment_health h ON h.deployment_id=d.id WHERE d.id=$1").bind(id).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    tx.commit().await?;
    Ok(Json(v))
}
pub(super) async fn put_deployment_routing(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(p): Json<DeploymentRouting>,
) -> ApiResult {
    if !(1..=1000).contains(&p.weight)
        || !(1..=i32::MAX).contains(&p.failure_threshold)
        || !(1..=3600).contains(&p.cooldown_seconds)
        || p.residency
            .as_deref()
            .is_some_and(|r| r == "unspecified" || !residency_valid(r))
    {
        return Err(invalid());
    }
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    exists(&mut tx, "deployments", id).await?;
    sqlx::query("INSERT INTO deployment_routing(deployment_id,priority,weight,residency,failure_threshold,cooldown_seconds) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(deployment_id) DO UPDATE SET priority=excluded.priority,weight=excluded.weight,residency=excluded.residency,failure_threshold=excluded.failure_threshold,cooldown_seconds=excluded.cooldown_seconds").bind(id).bind(p.priority).bind(p.weight).bind(p.residency).bind(p.failure_threshold).bind(p.cooldown_seconds).execute(&mut *tx).await?;
    resources::audit(
        &mut tx,
        &u,
        None,
        "routing.deployment_updated",
        "deployment",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn price(input_token_limit: i64, input_line: Value) -> Price {
        serde_json::from_value(json!({
            "pricing_version":3,"input_token_limit":input_token_limit,"output_token_limit":0,
            "price_lines":[
                input_line,
                {"meter":"output_tokens","not_applicable":true},
                {"meter":"cache_read_tokens","not_applicable":true},
                {"meter":"cache_write_tokens","not_applicable":true},
                {"meter":"cache_write_5m_tokens","not_applicable":true},
                {"meter":"cache_write_1h_tokens","not_applicable":true},
                {"meter":"input_characters","microusd_per_batch":"15000000","batch":1000000,"unit_label":"/M characters","sku_label":"Characters"}
            ],
            "max_units":{"input_characters":"200"}
        }))
        .unwrap()
    }
    #[test]
    fn zero_input_ceiling_requires_inapplicable_input_token_meters() {
        let na = json!({"meter":"input_tokens","not_applicable":true});
        assert!(validate_price(price(0, na.clone())).is_ok());
        assert!(validate_price(price(1, na.clone())).is_ok());
        assert!(validate_price(price(-1, na)).is_err());
        let priced = json!({"meter":"input_tokens","microusd_per_batch":"0","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"});
        assert!(validate_price(price(0, priced.clone())).is_err());
        assert!(validate_price(price(1, priced)).is_ok());
        // v1/v2 keep a positive input ceiling.
        let v1: Price = serde_json::from_value(json!({"input_microusd_per_million":"1","output_microusd_per_million":"1","input_token_limit":0,"output_token_limit":1})).unwrap();
        assert!(validate_price(v1).is_err());
    }
}

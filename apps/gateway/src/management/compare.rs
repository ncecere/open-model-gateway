//! Model compare: 2-4 models side by side (docs/management-api.md "Model compare").
//!
//! Per model: type, protocols, serving state, the latest price of its first
//! enabled route (exact integer micro-USD price lines; no price is unknown,
//! never zero), the token ceilings from that price, and 30-day observed
//! metrics from request telemetry. Metrics use the Logs visibility rules:
//! workspace scope = the caller's own activity (workspace-wide for shared
//! administrators); platform scope = Team/Project activity only, aggregated,
//! never personal workspaces.
use super::logs::{FILTERED, LogQuery, LogScope, bind_filters, listed_roots};
use super::models_ux::WORKLOAD;
use super::resources::{ENABLED_ROUTE, PRICED_ROUTE};
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CompareQuery {
    ids: String,
}
/// 2-4 distinct model ids, in the order given.
pub(crate) fn parse_ids(raw: &str) -> Result<Vec<Uuid>, ApiError> {
    if raw.len() > 4 * 37 {
        return Err(invalid());
    }
    let mut ids: Vec<Uuid> = Vec::new();
    for part in raw.split(',') {
        let id = Uuid::parse_str(part.trim()).map_err(|_| invalid())?;
        if ids.contains(&id) {
            return Err(invalid());
        }
        ids.push(id);
    }
    if !(2..=4).contains(&ids.len()) {
        return Err(invalid());
    }
    Ok(ids)
}
/// Models visible in a workspace's catalog (enabled, and eligible or selected).
const WORKSPACE_VISIBLE: &str = "m.enabled AND (workspace_model_allowed($2,m.id) OR EXISTS(SELECT 1 FROM catalog_models cm JOIN workspaces w ON w.id=$2 WHERE cm.model_id=m.id AND ((EXISTS(SELECT 1 FROM workspace_catalog_overrides h WHERE h.workspace_id=w.id) AND EXISTS(SELECT 1 FROM workspace_catalog_override_items i WHERE i.workspace_id=w.id AND i.catalog_id=cm.catalog_id)) OR (NOT EXISTS(SELECT 1 FROM workspace_catalog_overrides h WHERE h.workspace_id=w.id) AND EXISTS(SELECT 1 FROM workspace_type_catalogs t WHERE t.kind=w.kind AND t.catalog_id=cm.catalog_id)))))";
/// Model facts plus the latest price of its first enabled route (routing priority order).
fn model_sql(visible: &str) -> String {
    let serving = &*super::resources::SERVING_ROUTE;
    format!(
        "SELECT m.id,jsonb_build_object('id',m.id,'public_name',m.public_name,'display_name',m.display_name,'enabled',m.enabled,'protocols',m.supported_protocols,'workload',{WORKLOAD},'routes',(SELECT count(*) FROM deployments d WHERE d.model_id=m.id),'enabled_routes',(SELECT count(*) FROM deployments d WHERE d.model_id=m.id AND {ENABLED_ROUTE}),'serving_routes',(SELECT count(*) FROM deployments d WHERE d.model_id=m.id AND {serving}),'priced_enabled_routes',(SELECT count(*) FROM deployments d WHERE d.model_id=m.id AND {ENABLED_ROUTE} AND {PRICED_ROUTE})),(SELECT CASE WHEN dp.id IS NOT NULL THEN jsonb_build_object('pricing_version',dp.pricing_version,'input_microusd_per_million',dp.input_microusd_per_million::text,'output_microusd_per_million',dp.output_microusd_per_million::text,'cache_pricing',dp.cache_pricing,'price_lines',dp.price_lines,'input_token_limit',dp.input_token_limit,'output_token_limit',dp.output_token_limit,'created_at',dp.created_at) END FROM deployments d LEFT JOIN deployment_routing r ON r.deployment_id=d.id LEFT JOIN LATERAL (SELECT * FROM deployment_prices x WHERE x.deployment_id=d.id ORDER BY x.created_at DESC,x.id DESC LIMIT 1) dp ON true WHERE d.model_id=m.id AND {ENABLED_ROUTE} ORDER BY coalesce(r.priority,0),d.created_at,d.id LIMIT 1) FROM models m WHERE m.id=ANY($1) AND {visible}"
    )
}
fn token_line(meter: &str, amount: &str, sku: &str) -> Value {
    json!({"meter":meter,"microusd_per_batch":amount,"batch":1_000_000,"unit_label":"/M tokens","sku_label":sku})
}
/// One price as v3-shaped `lines` (+ `display_lines`). v1/v2 token rates become
/// token lines; v2 cache rates become cache lines when priced or not applicable
/// (unknown cache rates stay absent: unknown, never zero).
pub(crate) fn normalized_price(raw: &Value) -> Value {
    let lines = if raw["pricing_version"] == 3 {
        raw["price_lines"].clone()
    } else {
        let mut lines = Vec::new();
        for (meter, field, sku) in [
            ("input_tokens", "input_microusd_per_million", "Input"),
            ("output_tokens", "output_microusd_per_million", "Output"),
        ] {
            if let Some(a) = raw[field].as_str() {
                lines.push(token_line(meter, a, sku));
            }
        }
        if let Some(cache) = raw["cache_pricing"].as_object() {
            for (key, meter, sku) in [
                ("read", "cache_read_tokens", "Cache read"),
                ("write", "cache_write_tokens", "Cache write"),
                ("write_5m", "cache_write_5m_tokens", "Cache write (5 min)"),
                ("write_1h", "cache_write_1h_tokens", "Cache write (1 hour)"),
            ] {
                match cache.get(key).and_then(|r| r["status"].as_str()) {
                    Some("priced") => {
                        if let Some(a) = cache[key]["microusd_per_million"].as_str() {
                            lines.push(token_line(meter, a, sku));
                        }
                    }
                    Some("not_applicable") => {
                        lines.push(json!({"meter":meter,"not_applicable":true}))
                    }
                    _ => {}
                }
            }
        }
        Value::Array(lines)
    };
    let mut out = json!({"pricing_version":raw["pricing_version"],"price_lines":lines,"input_token_limit":raw["input_token_limit"],"output_token_limit":raw["output_token_limit"],"created_at":raw["created_at"]});
    governance::prices::add_display(&mut out);
    if let Some(o) = out.as_object_mut() {
        if let Some(l) = o.remove("price_lines") {
            o.insert("lines".into(), l);
        }
        o.remove("display_summary");
    }
    out
}
/// Observed metrics of one model's root requests over the default 30-day window.
const METRICS: &str = "jsonb_build_object('requests',count(*)::text,'completed',count(*) FILTER(WHERE status<>'in_progress')::text,'failed',count(*) FILTER(WHERE status='failed')::text,'error_rate',CASE WHEN count(*) FILTER(WHERE status<>'in_progress')>0 THEN round(count(*) FILTER(WHERE status='failed')::numeric/count(*) FILTER(WHERE status<>'in_progress'),4)::text END,'latency_p50_ms',percentile_disc(0.5) WITHIN GROUP (ORDER BY latency_ms),'ttft_p50_ms',percentile_disc(0.5) WITHIN GROUP (ORDER BY ttft_ms),'ttft_requests',count(ttft_ms)::text,'tokens_per_second',CASE WHEN coalesce(sum(decode_ms) FILTER(WHERE decode_ms>0),0)>0 THEN round(sum(final_output_tokens) FILTER(WHERE decode_ms>0)*1000.0/sum(decode_ms) FILTER(WHERE decode_ms>0),2)::text END)";
async fn compare(
    mut tx: Transaction<'_, Postgres>,
    scope: LogScope,
    ids: Vec<Uuid>,
    visible: &str,
) -> ApiResult {
    let rows: Vec<(Uuid, Value, Option<Value>)> = sqlx::query_as(&model_sql(visible))
        .bind(&ids)
        .bind(scope.workspace)
        .fetch_all(&mut *tx)
        .await?;
    // All or nothing: an id the caller can't see is indistinguishable from a missing one.
    if rows.len() != ids.len() {
        return Err(missing());
    }
    let window = LogQuery::default();
    let period = window.filters()?;
    let sql = format!(
        "{}, f AS (SELECT * FROM roots WHERE {FILTERED}) SELECT {METRICS} FROM f",
        listed_roots(&scope, false)
    );
    let mut data = Vec::new();
    for id in &ids {
        let (_, mut model, price) = rows
            .iter()
            .find(|r| r.0 == *id)
            .cloned()
            .ok_or_else(missing)?;
        let q = LogQuery {
            model: model["public_name"].as_str().map(str::to_owned),
            ..LogQuery::default()
        };
        let f = q.filters()?;
        let (metrics,): (Value,) = bind_filters(sqlx::query_as(&sql), &scope, &f)
            .fetch_one(&mut *tx)
            .await?;
        model["price"] = price.as_ref().map_or(Value::Null, normalized_price);
        model["metrics"] = metrics;
        data.push(model);
    }
    tx.commit().await?;
    Ok(Json(
        json!({"scope":if scope.platform {"platform"} else {"workspace"},"activity":if scope.platform {"shared_workspaces"} else if scope.all {"workspace"} else {"own"},"period":{"start":period.start,"end":period.end},"data":data}),
    ))
}
/// GET /workspaces/{ws}/models/compare?ids=…: models of this workspace's catalog.
pub(super) async fn workspace_compare(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(q): Query<CompareQuery>,
) -> ApiResult {
    let ids = parse_ids(&q.ids)?;
    let (tx, scope) = super::logs::workspace_scope(&s, &u, ws, &LogQuery::default()).await?;
    compare(tx, scope, ids, WORKSPACE_VISIBLE).await
}
/// GET /platform/models/compare?ids=…: any model; Team/Project activity only.
pub(super) async fn platform_compare(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(q): Query<CompareQuery>,
) -> ApiResult {
    let ids = parse_ids(&q.ids)?;
    let (tx, scope) = super::logs::platform_scope(&s, &u, &LogQuery::default()).await?;
    compare(tx, scope, ids, "$2::uuid IS NULL").await
}
pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/workspaces/{ws}/models/compare",
            get(workspace_compare),
        )
        .route("/api/v1/platform/models/compare", get(platform_compare))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_are_two_to_four_distinct_uuids() {
        let a = Uuid::new_v4().to_string();
        let b = Uuid::new_v4().to_string();
        assert_eq!(parse_ids(&format!("{a},{b}")).unwrap().len(), 2);
        assert!(parse_ids(&a).is_err());
        assert!(parse_ids(&format!("{a},{a}")).is_err());
        assert!(parse_ids(&format!("{a},nope")).is_err());
        let five = (0..5)
            .map(|_| Uuid::new_v4().to_string())
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_ids(&five).is_err());
    }
    #[test]
    fn legacy_prices_become_exact_lines_and_unknown_cache_stays_absent() {
        let v = normalized_price(
            &json!({"pricing_version":2,"input_microusd_per_million":"150000","output_microusd_per_million":"600000","cache_pricing":{"read":{"status":"priced","microusd_per_million":"75000"},"write":{"status":"unknown"},"write_5m":{"status":"not_applicable"},"write_1h":{"status":"unknown"}},"input_token_limit":128000,"output_token_limit":16000,"created_at":"2026-01-01T00:00:00Z"}),
        );
        let lines = v["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["microusd_per_batch"], "150000");
        assert_eq!(lines[2]["meter"], "cache_read_tokens");
        assert_eq!(lines[3]["not_applicable"], true);
        assert!(!v.to_string().contains("cache_write_tokens"));
        assert_eq!(v["display_lines"].as_array().unwrap().len(), 4);
        assert_eq!(v["input_token_limit"], 128000);
    }
}

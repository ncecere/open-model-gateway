//! Models catalog additions: filtered/sorted platform model list with
//! per-workload counts, route detail with data policy and prices, and a
//! workspace-visible catalog with eligibility. Rates are exact decimal strings
//! derived from integer micro-USD price lines (never floats).
use super::resources::{ENABLED_ROUTE, PRICED_ROUTE, model_json, search};
use super::*;
use crate::providers::openrouter::DataCollection;

/// Workload of model `m` (protocol sets are single-workload).
pub(super) const WORKLOAD: &str = "CASE WHEN m.supported_protocols && ARRAY['chat_completions','responses','messages']::text[] THEN 'generation' ELSE m.supported_protocols[1] END";
/// Base (untiered) rate per million tokens for `meter` from a price row `dp`.
fn rate(meter: &str, legacy: &str) -> String {
    format!(
        "CASE WHEN dp.pricing_version IN (1,2) THEN dp.{legacy}::numeric ELSE (SELECT min((l->>'microusd_per_batch')::numeric*1000000/(l->>'batch')::numeric) FROM jsonb_array_elements(dp.price_lines) l WHERE l->>'meter'='{meter}' AND l->>'microusd_per_batch' IS NOT NULL AND l->>'min_prompt_tokens' IS NULL) END"
    )
}
/// Cheapest base rate across enabled routes' latest prices for model `m` (null when unknown).
fn min_rate(meter: &str, legacy: &str) -> String {
    format!(
        "(SELECT trim_scale(min({}))::text FROM deployments d JOIN LATERAL (SELECT * FROM deployment_prices x WHERE x.deployment_id=d.id ORDER BY x.created_at DESC,x.id DESC LIMIT 1) dp ON true WHERE d.model_id=m.id AND {ENABLED_ROUTE})",
        rate(meter, legacy)
    )
}
/// Data policy of a connection (`p`), with `$cfg` the configured OpenRouter setting.
const ROUTE_POLICY: &str = "CASE WHEN p.provider='openrouter' THEN $cfg ELSE 'unknown' END";
fn workload_valid(t: &str) -> bool {
    matches!(
        t,
        "generation"
            | "embeddings"
            | "images"
            | "audio_transcriptions"
            | "audio_speech"
            | "rerank"
            | "systemone"
            | "realtime"
            | "videos"
            | "batches"
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelListQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    q: Option<String>,
    enabled: Option<bool>,
    provider_connection_id: Option<Uuid>,
    #[serde(rename = "type")]
    workload: Option<String>,
    max_input_price: Option<String>,
    data_policy: Option<String>,
    include_deprecated: Option<bool>,
    sort: Option<String>,
}
pub(super) async fn platform_models(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<ModelListQuery>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let (l, o) = Page {
        limit: p.limit,
        offset: p.offset,
    }
    .bounds()?;
    if p.workload.as_deref().is_some_and(|t| !workload_valid(t))
        || p.data_policy
            .as_deref()
            .is_some_and(|d| !matches!(d, "allow" | "deny" | "unknown"))
    {
        return Err(invalid());
    }
    let max_price = p
        .max_input_price
        .as_deref()
        .map(governance_money)
        .transpose()?;
    let order = match p.sort.as_deref() {
        None | Some("name") => "m.public_name,m.id",
        Some("newest") => "m.created_at DESC,m.id",
        Some("price") => "m.min_input::numeric NULLS LAST,m.public_name,m.id",
        Some(_) => return Err(invalid()),
    };
    let min_input = min_rate("input_tokens", "input_microusd_per_million");
    let policy = ROUTE_POLICY.replace("$cfg", "$7");
    // Every filter except `type`; counts per workload use the same predicate.
    let filters = format!(
        "($1::text IS NULL OR strpos(lower(m.public_name),lower($1))>0 OR strpos(lower(m.display_name),lower($1))>0) AND ($2::boolean IS NULL OR m.enabled=$2) AND ($3::uuid IS NULL OR EXISTS(SELECT 1 FROM deployments d WHERE d.model_id=m.id AND d.provider_connection_id=$3)) AND ($4::numeric IS NULL OR ({min_input})::numeric<=$4) AND ($5::text IS NULL OR EXISTS(SELECT 1 FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.model_id=m.id AND {ENABLED_ROUTE} AND {policy}=$5)) AND ($6 OR m.enabled)"
    );
    let include = p.include_deprecated.unwrap_or(true);
    let cfg = DataCollection::configured().as_str();
    let data: Vec<Value> = sqlx::query_scalar(&format!("SELECT {} || jsonb_build_object('workload',{WORKLOAD},'min_input_microusd_per_million',min_input,'created_at',m.created_at) FROM (SELECT m.*,({min_input}) min_input FROM models m) m WHERE {filters} AND ($8::text IS NULL OR {WORKLOAD}=$8) ORDER BY {order} LIMIT $9 OFFSET $10", model_json()))
        .bind(search(p.q.as_deref())?)
        .bind(p.enabled)
        .bind(p.provider_connection_id)
        .bind(max_price)
        .bind(p.data_policy.as_deref())
        .bind(include)
        .bind(cfg)
        .bind(p.workload.as_deref())
        .bind(l)
        .bind(o)
        .fetch_all(&mut *tx)
        .await?;
    let counts: Value = sqlx::query_scalar(&format!("SELECT coalesce(jsonb_object_agg(w,n),'{{}}'::jsonb) FROM (SELECT {WORKLOAD} w,count(*) n FROM models m WHERE {filters} GROUP BY 1) c"))
        .bind(search(p.q.as_deref())?)
        .bind(p.enabled)
        .bind(p.provider_connection_id)
        .bind(max_price)
        .bind(p.data_policy.as_deref())
        .bind(include)
        .bind(cfg)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut all = serde_json::Map::new();
    for w in [
        "generation",
        "embeddings",
        "images",
        "audio_transcriptions",
        "audio_speech",
        "rerank",
        "systemone",
        "realtime",
        "videos",
        "batches",
    ] {
        all.insert(w.into(), json!(counts[w].as_i64().unwrap_or(0)));
    }
    Ok(Json(json!({"data":data,"counts":all})))
}
fn governance_money(v: &str) -> Result<i64, ApiError> {
    if v.is_empty() || v.len() > 19 || !v.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    v.parse().map_err(|_| invalid())
}
pub(super) async fn deployment_detail(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let (mut v, provider): (Value, String) = sqlx::query_as(&format!("SELECT jsonb_build_object('id',d.id,'model_id',d.model_id,'model_public_name',m.public_name,'provider_connection_id',d.provider_connection_id,'provider_name',p.name,'provider',p.provider,'upstream_model',d.upstream_model,'enabled',d.enabled,'connection_enabled',p.enabled,'region',p.region,'protocols',m.supported_protocols,'workload',{WORKLOAD},'residency',(SELECT r.residency FROM deployment_routing r WHERE r.deployment_id=d.id)),p.provider FROM deployments d JOIN models m ON m.id=d.model_id JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.id=$1")).bind(id).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    let mut price: Option<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',id,'pricing_version',pricing_version,'created_at',created_at,'input_microusd_per_million',input_microusd_per_million::text,'output_microusd_per_million',output_microusd_per_million::text,'cache_pricing',cache_pricing,'price_lines',price_lines,'max_units',max_units,'batch_price_lines',batch_price_lines,'input_token_limit',input_token_limit,'output_token_limit',output_token_limit) FROM deployment_prices WHERE deployment_id=$1 ORDER BY created_at DESC,id DESC LIMIT 1").bind(id).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    if let Some(p) = price.as_mut() {
        governance::prices::add_display(p);
    }
    // Configuration-derived facts only (no upstream capability probing).
    let mut features = Vec::new();
    if let Some(p) = &price {
        let lines = p["price_lines"].as_array().cloned().unwrap_or_default();
        if lines.iter().any(|l| {
            l["meter"]
                .as_str()
                .is_some_and(|m| m.starts_with("cache_") && l["microusd_per_batch"].is_string())
        }) || p["cache_pricing"]["read"]["status"] == "priced"
        {
            features.push("cache_pricing");
        }
        if lines.iter().any(|l| l.get("min_prompt_tokens").is_some()) {
            features.push("prompt_size_tiers");
        }
    }
    if provider == "openrouter"
        && v["upstream_model"]
            .as_str()
            .is_some_and(|m| m.ends_with(":free"))
    {
        features.push("openrouter_free_variant");
    }
    v["data_policy"] = super::requests::data_policy(&provider);
    v["price"] = price.unwrap_or(Value::Null);
    v["features"] = json!(features);
    Ok(Json(v))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CatalogQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    q: Option<String>,
    #[serde(rename = "type")]
    workload: Option<String>,
    sort: Option<String>,
}
/// Workspace-visible catalog: enabled models eligible for (or assigned to) the workspace.
pub(super) async fn workspace_catalog(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<CatalogQuery>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    a.metadata_read()?;
    let (l, o) = Page {
        limit: p.limit,
        offset: p.offset,
    }
    .bounds()?;
    if p.workload.as_deref().is_some_and(|t| !workload_valid(t)) {
        return Err(invalid());
    }
    let min_input = min_rate("input_tokens", "input_microusd_per_million");
    let order = match p.sort.as_deref() {
        None | Some("name") => "m.public_name,m.id".to_owned(),
        Some("newest") => "m.created_at DESC,m.id".to_owned(),
        Some("price") => format!("({min_input})::numeric NULLS LAST,m.public_name,m.id"),
        Some(_) => return Err(invalid()),
    };
    let eligible = "EXISTS(SELECT 1 FROM catalog_models cm WHERE cm.model_id=m.id AND ((EXISTS(SELECT 1 FROM workspace_catalog_overrides h WHERE h.workspace_id=w.id) AND EXISTS(SELECT 1 FROM workspace_catalog_override_items i WHERE i.workspace_id=w.id AND i.catalog_id=cm.catalog_id)) OR (NOT EXISTS(SELECT 1 FROM workspace_catalog_overrides h WHERE h.workspace_id=w.id) AND EXISTS(SELECT 1 FROM workspace_type_catalogs t WHERE t.kind=w.kind AND t.catalog_id=cm.catalog_id))))";
    let data: Vec<Value> = sqlx::query_scalar(&format!("SELECT jsonb_build_object('model_id',m.id,'public_name',m.public_name,'display_name',m.display_name,'description',m.description,'protocols',m.supported_protocols,'workload',{WORKLOAD},'eligibility',CASE WHEN workspace_model_allowed(w.id,m.id) AND EXISTS(SELECT 1 FROM workspace_model_grants g WHERE g.workspace_id=w.id AND g.model_id=m.id AND g.source='direct') THEN 'direct' WHEN workspace_model_allowed(w.id,m.id) THEN 'selected' ELSE 'available_from_catalog' END,'reason',CASE WHEN workspace_model_allowed(w.id,m.id) AND EXISTS(SELECT 1 FROM workspace_model_grants g WHERE g.workspace_id=w.id AND g.model_id=m.id AND g.source='direct') THEN 'Assigned to this workspace by a Platform Admin' WHEN workspace_model_allowed(w.id,m.id) THEN 'Added to this workspace from an available catalog' ELSE 'In a catalog available to this workspace; not added yet' END,'min_input_microusd_per_million',{},'min_output_microusd_per_million',{},'routes',(SELECT count(*) FROM deployments d WHERE d.model_id=m.id AND {ENABLED_ROUTE}),'priced_routes',(SELECT count(*) FROM deployments d WHERE d.model_id=m.id AND {ENABLED_ROUTE} AND {PRICED_ROUTE}),'created_at',m.created_at) FROM models m CROSS JOIN workspaces w WHERE w.id=$1 AND m.enabled AND ({eligible} OR workspace_model_allowed(w.id,m.id)) AND ($2::text IS NULL OR strpos(lower(m.public_name),lower($2))>0 OR strpos(lower(m.display_name),lower($2))>0) AND ($3::text IS NULL OR {WORKLOAD}=$3) ORDER BY {order} LIMIT $4 OFFSET $5", min_input, min_rate("output_tokens", "output_microusd_per_million")))
        .bind(ws)
        .bind(search(p.q.as_deref())?)
        .bind(p.workload.as_deref())
        .bind(l)
        .bind(o)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}

//! Guided model setup and the platform setup/at-a-glance overview.
//!
//! Model setup composes the individual model, deployment, price and catalog-membership
//! helpers inside one exclusive catalog transaction, so validation, lock order
//! (catalog advisory lock -> installation row -> scoped rows), error codes and audit
//! events are identical to the individual endpoints and any failure creates nothing.
use super::*;
use catalogs::{Membership, replace_membership};
use governance::prices::{Price, insert_price, validate_price};
use resources::{
    DeploymentInput, ENABLED_ROUTE, ModelInput, PRICED_ROUTE, insert_deployment, insert_model,
    ready_model,
};

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/platform/model-setup", post(model_setup))
        .route("/api/v1/platform/overview", get(overview))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteInput {
    provider_connection_id: Uuid,
    upstream_model: String,
    enabled: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelSetup {
    model: ModelInput,
    route: RouteInput,
    #[serde(default)]
    price: Option<Price>,
    #[serde(default)]
    catalog_ids: Vec<Uuid>,
}
async fn model_setup(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<ModelSetup>,
) -> Result<(StatusCode, Json<Value>), ManagementError> {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    let model_id = insert_model(&mut tx, &u, b.model).await?;
    let deployment_id = insert_deployment(
        &mut tx,
        &u,
        DeploymentInput {
            model_id,
            provider_connection_id: b.route.provider_connection_id,
            upstream_model: b.route.upstream_model,
            enabled: b.route.enabled,
        },
    )
    .await?;
    let price_id = match b.price {
        Some(p) => Some(insert_price(&mut tx, &u, deployment_id, validate_price(p)?).await?),
        None => None,
    };
    replace_membership(&mut tx, &u, Membership::Model(model_id, b.catalog_ids)).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"model_id":model_id,"deployment_id":deployment_id,"price_id":price_id})),
    ))
}
/// Aggregate counts only: no names, emails, keys, credential references or request details.
async fn overview(State(s): State<Store>, Extension(u): Extension<BrowserPrincipal>) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    sqlx::query("SET LOCAL statement_timeout='10s'")
        .execute(&mut *tx)
        .await?;
    let ready = ready_model();
    let mut v: Value = sqlx::query_scalar(&format!("WITH counts AS (SELECT (SELECT count(*) FROM provider_connections) connections,(SELECT count(*) FROM provider_connections WHERE enabled) enabled_connections,(SELECT count(*) FROM models) models,(SELECT count(*) FROM models m WHERE {ready}) ready_models,(SELECT count(*) FROM deployments d WHERE {ENABLED_ROUTE}) enabled_routes,(SELECT count(*) FROM deployments d WHERE {ENABLED_ROUTE} AND {PRICED_ROUTE}) priced_enabled_routes,(SELECT count(*) FROM catalogs) catalogs,(SELECT count(*) FROM effective_platform_roles) entitled_users,(SELECT count(*) FROM oidc_group_mappings WHERE enabled) oidc_mappings,(SELECT count(*) FROM workspaces WHERE kind='team' AND disabled_at IS NULL) teams,(SELECT count(*) FROM workspaces WHERE kind='project' AND disabled_at IS NULL) projects), activity AS (SELECT count(e.id) attempts,coalesce(sum(r.actual_microusd),0) known_cost FROM inference_executions e LEFT JOIN governance_reservations r ON r.execution_id=e.id WHERE e.started_at>=statement_timestamp()-interval '7 days') SELECT jsonb_build_object('setup',jsonb_build_object('connections',c.connections,'enabled_connections',c.enabled_connections,'models',c.models,'ready_models',c.ready_models,'enabled_routes',c.enabled_routes,'priced_enabled_routes',c.priced_enabled_routes,'catalogs',c.catalogs,'type_defaults',(SELECT jsonb_build_object('personal',count(*) FILTER(WHERE kind='personal'),'team',count(*) FILTER(WHERE kind='team'),'project',count(*) FILTER(WHERE kind='project')) FROM workspace_type_catalogs),'entitled_users',c.entitled_users,'oidc_mappings',c.oidc_mappings),'glance',jsonb_build_object('entitled_users',c.entitled_users,'teams',c.teams,'projects',c.projects,'ready_models',c.ready_models,'attempts_7d',a.attempts::text,'known_cost_7d_microusd',a.known_cost::text)) FROM counts c CROSS JOIN activity a"))
        .fetch_one(&mut *tx)
        .await?;
    // Installation-wide budget windows (amount, used, settled, held): totals only.
    v["installation_budgets"] = super::usage::installation_budgets(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(v))
}

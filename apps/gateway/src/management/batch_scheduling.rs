//! Admin › model › route › Batch scheduling (0022; docs/batches.md#scheduling-on-self-hosted-models).
//!
//! - `GET /api/v1/platform/deployments/{deployment}/batch-scheduling`
//!   (platform readers): the route's settings (defaults when unset), whether
//!   it accepts a priority hint, and its batch queue: lines waiting and
//!   running, the pause reason and the last server metrics reading.
//! - `PUT …` (platform writers): replace the settings. A metrics URL must be
//!   `/metrics` on an approved local endpoint's origin
//!   (`GATEWAY_LOCAL_UPSTREAMS`); priority hints need a vLLM-compatible route.
//!   Audited as `batch_scheduling.updated`.
use super::*;
use crate::jobs::schedule::{self, RouteSettings};

pub(super) fn routes() -> Router<Store> {
    Router::new().route(
        "/api/v1/platform/deployments/{deployment}/batch-scheduling",
        get(read).put(write),
    )
}

async fn provider(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<String, ApiError> {
    sqlx::query_scalar("SELECT p.provider FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.id=$1")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)
}

async fn read(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let provider = provider(&mut tx, id).await?;
    let settings = schedule::load_settings(&mut *tx, id).await?;
    let status = schedule::route_status(&mut *tx, id).await?;
    tx.commit().await?;
    Ok(Json(schedule::settings_document(
        &settings, &provider, status,
    )))
}

async fn write(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(settings): Json<RouteSettings>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    let provider = provider(&mut tx, id).await?;
    // The same approvals the gateway loads at startup (server-controlled).
    let approvals = crate::providers::local::endpoints::ApprovedEndpoints::from_env(
        &std::env::var("GATEWAY_ENV").unwrap_or_else(|_| "production".into()),
    )
    .map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Local endpoint approvals are invalid",
        )
    })?;
    settings
        .validate(&provider, &approvals)
        .map_err(|message| ApiError(StatusCode::BAD_REQUEST, message))?;
    schedule::save_settings(&mut tx, id, &settings, u.user_id).await?;
    resources::audit(
        &mut tx,
        &u,
        None,
        "batch_scheduling.updated",
        "deployment",
        Some(id),
        json!({
            "max_concurrency": settings.max_concurrency,
            "yield_live_threshold": settings.yield_live_threshold,
            "server_metrics": settings.metrics.is_some(),
            "priority": settings.priority,
            "window": settings.window.is_some(),
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}

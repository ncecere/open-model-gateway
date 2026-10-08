//! Live platform policy, pinned pricing and privacy-scoped accounting reports.
use super::*;
use crate::{
    billing::{CachePricing, CostComponents},
    inference::{error::InferenceError, types::Usage},
};
mod policies;
pub(super) use policies::{
    BudgetInput, Limits, check_initial_key_limits, initial_key_limits, installation_limits,
    json_budgets, json_limits, key_limits, key_usage, layers_with, lineage, lineage_budget_windows,
    store_initial_key_limits, workspace_budget_windows,
};
pub(super) mod prices;
mod reports;
pub(crate) mod suggestion;
#[cfg(test)]
mod tests;
pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/workspaces/{ws}/policy",
            get(policies::workspace_policy).put(policies::put_workspace_policy),
        )
        .route(
            "/api/v1/workspaces/{ws}/keys/{key}/policy",
            get(policies::key_policy).put(policies::put_key_policy),
        )
        .route(
            "/api/v1/workspaces/{ws}/cost-report",
            get(reports::workspace_report),
        )
        .route(
            "/api/v1/workspaces/{ws}/cost-summary",
            get(reports::cost_summary),
        )
        .route("/api/v1/workspaces/{ws}/costs", get(reports::costs))
        .route(
            "/api/v1/workspaces/{ws}/usage-export",
            get(reports::usage_export),
        )
        .route(
            "/api/v1/workspaces/{ws}/costs/{execution}/reconcile",
            post(reports::reconcile),
        )
}
pub(super) fn platform_routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/platform/installation/policy",
            get(policies::installation_policy).put(policies::put_installation_policy),
        )
        .route(
            "/api/v1/platform/workspace-types/{kind}/policy",
            get(policies::type_policy).put(policies::put_type_policy),
        )
        .route(
            "/api/v1/platform/workspaces/{ws}/policy",
            get(policies::platform_workspace_policy)
                .put(policies::put_platform_workspace_policy)
                .delete(policies::reset_platform_workspace_policy),
        )
        .route(
            "/api/v1/platform/deployments/{deployment}/prices",
            get(prices::prices).post(prices::create_price),
        )
        .route(
            "/api/v1/platform/deployments/{deployment}/price-suggestion",
            get(suggestion::price_suggestion),
        )
        .route(
            "/api/v1/platform/models/{model}/routing",
            get(prices::model_routing).put(prices::put_model_routing),
        )
        .route(
            "/api/v1/platform/deployments/{deployment}/routing",
            get(prices::deployment_routing).put(prices::put_deployment_routing),
        )
        .route(
            "/api/v1/platform/cost-report",
            get(reports::platform_report),
        )
}
fn nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(d)
}
fn positive_limit(v: i64) -> bool {
    (1..=i64::from(i32::MAX)).contains(&v)
}
fn money(v: &str, positive: bool) -> Result<i64, ApiError> {
    if v.is_empty() || v.len() > 19 || !v.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let n: i64 = v.parse().map_err(|_| invalid())?;
    if positive && n == 0 {
        return Err(invalid());
    }
    Ok(n)
}
fn residency_valid(v: &str) -> bool {
    (1..=64).contains(&v.len())
        && v.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && v.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

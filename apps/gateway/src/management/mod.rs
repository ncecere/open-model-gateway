mod access;
mod alerts;
mod batch_scheduling;
mod batches;
mod catalogs;
mod compare;
mod directory;
mod files;
mod governance;
mod key_safety;
mod keys;
mod logs;
mod me;
mod members;
mod models_ux;
mod requests;
pub(crate) mod resources;
mod settings;
mod setup;
mod storage_usage;
#[cfg(all(test, feature = "integration-tests"))]
mod tests;
mod usage;

use crate::{
    identity::{BrowserPrincipal, IdentityState, require_session},
    store::Store,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use resources::audit;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

type ApiResult = Result<Json<Value>, ApiError>;
#[derive(Debug)]
pub(crate) struct ApiError(pub(crate) StatusCode, pub(crate) &'static str);
/// Stable machine-readable reasons for selected errors (additive `error.reason`).
const REASONS: &[(&str, &str)] = &[
    (PERSONAL_NAME_FIXED, "personal_workspace_name_fixed"),
    (PERSONAL_LIMITS, "personal_limits_platform_controlled"),
    (STACKED_LEGACY, "stacked_budgets_require_budgets_field"),
    (KEY_REVOKED, "key_revoked"),
    (KEY_DISABLED, "key_disabled"),
    (MEMBER_VISIBILITY, "workspace_wide_visibility_required"),
    (settings::KEY_LIFETIME, "key_lifetime_exceeds_maximum"),
    (settings::SETTING_LOCKED, "setting_locked_by_environment"),
    (
        settings::REFERENCE_NOT_ALLOWED,
        "credential_reference_not_allowed",
    ),
    (settings::PLAINTEXT_REMOTE, "plaintext_requires_loopback"),
    (settings::EMAIL_NOT_CONFIGURED, "email_not_configured"),
    (settings::EMAIL_TEST_LIMIT, "email_test_rate_limited"),
    (
        settings::storage::STORAGE_OFF,
        "file_storage_not_configured",
    ),
    (
        settings::storage::STORAGE_UNHEALTHY,
        "file_storage_unhealthy",
    ),
    (
        settings::storage::STORAGE_TEST_LIMIT,
        "storage_test_rate_limited",
    ),
    (alerts::PERSONAL_BUILTIN_ONLY, "personal_alerts_built_in"),
    (alerts::RULE_LIMIT, "alert_rule_limit"),
    (alerts::KIND_FIXED, "alert_rule_kind_fixed"),
];
const PERSONAL_NAME_FIXED: &str = "Personal workspace name is fixed";
const PERSONAL_LIMITS: &str = "Personal workspace limits are set by the platform";
const STACKED_LEGACY: &str = "This layer has several budgets; update them with the budgets field";
const KEY_REVOKED: &str = "Revoked keys can never be re-enabled";
const KEY_DISABLED: &str = "Disabled keys must be enabled first";
const MEMBER_VISIBILITY: &str = "Grouping by member requires workspace-wide visibility";
fn personal_name_fixed() -> ApiError {
    ApiError(StatusCode::CONFLICT, PERSONAL_NAME_FIXED)
}
fn personal_limits() -> ApiError {
    ApiError(StatusCode::FORBIDDEN, PERSONAL_LIMITS)
}
fn stacked_legacy() -> ApiError {
    ApiError(StatusCode::CONFLICT, STACKED_LEGACY)
}
fn key_revoked() -> ApiError {
    ApiError(StatusCode::CONFLICT, KEY_REVOKED)
}
fn key_disabled() -> ApiError {
    ApiError(StatusCode::CONFLICT, KEY_DISABLED)
}
fn member_visibility() -> ApiError {
    ApiError(StatusCode::FORBIDDEN, MEMBER_VISIBILITY)
}
/// Policy rejections with a stable reason plus one non-sensitive detail
/// (`period` or `limit` name). Never amounts: the caller learns which rule
/// failed, not another scope's value. `(message, reason, detail key, detail value)`.
const POLICY_REASONS: &[(&str, &str, &str, &str)] = &[
    (
        "The day budget exceeds a parent day budget",
        "exceeds_parent_budget",
        "period",
        "day",
    ),
    (
        "The week budget exceeds a parent week budget",
        "exceeds_parent_budget",
        "period",
        "week",
    ),
    (
        "The month budget exceeds a parent month budget",
        "exceeds_parent_budget",
        "period",
        "month",
    ),
    (
        "The lifetime budget exceeds a parent lifetime budget",
        "exceeds_parent_budget",
        "period",
        "lifetime",
    ),
    (
        "requests_per_minute exceeds a parent limit",
        "exceeds_parent_rate",
        "limit",
        "requests_per_minute",
    ),
    (
        "tokens_per_minute exceeds a parent limit",
        "exceeds_parent_rate",
        "limit",
        "tokens_per_minute",
    ),
    (
        "concurrent_requests exceeds a parent limit",
        "exceeds_parent_rate",
        "limit",
        "concurrent_requests",
    ),
    (
        "concurrent_jobs exceeds a parent limit",
        "exceeds_parent_rate",
        "limit",
        "concurrent_jobs",
    ),
    (
        "storage_bytes exceeds a parent limit",
        "exceeds_parent_rate",
        "limit",
        "storage_bytes",
    ),
    (
        "A stored day budget cannot be raised",
        "stored_budget_raise_not_allowed",
        "period",
        "day",
    ),
    (
        "A stored week budget cannot be raised",
        "stored_budget_raise_not_allowed",
        "period",
        "week",
    ),
    (
        "A stored month budget cannot be raised",
        "stored_budget_raise_not_allowed",
        "period",
        "month",
    ),
    (
        "A stored lifetime budget cannot be raised",
        "stored_budget_raise_not_allowed",
        "period",
        "lifetime",
    ),
    (
        "A stored day budget cannot be removed or moved to another period",
        "period_change_not_allowed",
        "period",
        "day",
    ),
    (
        "A stored week budget cannot be removed or moved to another period",
        "period_change_not_allowed",
        "period",
        "week",
    ),
    (
        "A stored month budget cannot be removed or moved to another period",
        "period_change_not_allowed",
        "period",
        "month",
    ),
    (
        "A stored lifetime budget cannot be removed or moved to another period",
        "period_change_not_allowed",
        "period",
        "lifetime",
    ),
    (
        "A stored requests_per_minute cap cannot be raised or removed",
        "stored_rate_loosen_not_allowed",
        "limit",
        "requests_per_minute",
    ),
    (
        "A stored tokens_per_minute cap cannot be raised or removed",
        "stored_rate_loosen_not_allowed",
        "limit",
        "tokens_per_minute",
    ),
    (
        "A stored concurrent_requests cap cannot be raised or removed",
        "stored_rate_loosen_not_allowed",
        "limit",
        "concurrent_requests",
    ),
    (
        "A stored concurrent_jobs cap cannot be raised or removed",
        "stored_rate_loosen_not_allowed",
        "limit",
        "concurrent_jobs",
    ),
    (
        "A stored storage_bytes cap cannot be raised or removed",
        "stored_rate_loosen_not_allowed",
        "limit",
        "storage_bytes",
    ),
];
/// Policy rejection with `reason` and detail value (a period or limit name).
pub(crate) fn policy_rejection(status: StatusCode, reason: &str, detail: &str) -> ApiError {
    POLICY_REASONS
        .iter()
        .find(|(_, r, _, v)| *r == reason && *v == detail)
        .map_or_else(
            || ApiError(status, "Invalid request"),
            |(m, ..)| ApiError(status, m),
        )
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = json!({"code":self.0.as_u16().to_string(),"message":self.1});
        if let Some((_, reason)) = REASONS.iter().find(|(m, _)| *m == self.1) {
            error["reason"] = json!(reason);
        }
        if let Some((_, reason, key, value)) = POLICY_REASONS.iter().find(|(m, ..)| *m == self.1) {
            error["reason"] = json!(reason);
            error[*key] = json!(value);
        }
        (self.0, Json(json!({ "error": error }))).into_response()
    }
}
impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        if e.as_database_error()
            .is_some_and(|e| matches!(e.code().as_deref(), Some("23505" | "23503" | "23514")))
        {
            Self(
                StatusCode::CONFLICT,
                "Resource conflicts with existing configuration",
            )
        } else {
            Self(
                StatusCode::SERVICE_UNAVAILABLE,
                "Management storage unavailable",
            )
        }
    }
}
fn denied() -> ApiError {
    ApiError(StatusCode::FORBIDDEN, "Access denied")
}
fn missing() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "Resource not found")
}
fn invalid() -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "Invalid request")
}
fn ok() -> Json<Value> {
    Json(json!({"ok":true}))
}
fn identifier(id: Uuid) -> Json<Value> {
    Json(json!({"id":id}))
}
fn valid_name(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 120 && !s.chars().any(char::is_control)
}
fn shared(s: &str) -> bool {
    matches!(s, "team" | "project")
}
fn valid_role(s: &str) -> bool {
    matches!(s, "owner" | "admin" | "member")
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Page {
    limit: Option<i64>,
    offset: Option<i64>,
}
impl Page {
    fn bounds(&self) -> Result<(i64, i64), ApiError> {
        let l = self.limit.unwrap_or(100);
        let o = self.offset.unwrap_or(0);
        if !(1..=200).contains(&l) || !(0..=100000).contains(&o) {
            return Err(invalid());
        }
        Ok((l, o))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Name {
    name: String,
}
pub fn router(identity: IdentityState) -> Router<Store> {
    let sign_in = settings::SignIn(identity.clone());
    routes()
        .layer(Extension(sign_in))
        .route_layer(middleware::from_fn_with_state(identity, require_session))
}
/// Session routes with their own (larger) body cap; mount outside the shared
/// 2 MiB request limit.
pub fn upload_router(identity: IdentityState) -> Router<Store> {
    files::upload_routes().route_layer(middleware::from_fn_with_state(identity, require_session))
}
fn routes() -> Router<Store> {
    Router::new()
        .merge(directory::routes())
        .merge(resources::platform_routes())
        .merge(catalogs::routes())
        .merge(setup::routes())
        .merge(governance::routes())
        .merge(governance::platform_routes())
        .merge(logs::routes())
        .merge(settings::routes())
        .merge(key_safety::routes())
        .merge(compare::routes())
        .merge(alerts::routes())
        .merge(files::routes())
        .merge(batches::routes())
        .merge(batch_scheduling::routes())
        .route("/api/v1/me", get(me))
        .route("/api/v1/me/summary", get(me::summary))
        .route("/api/v1/me/keys", get(me::my_keys))
        .route(
            "/api/v1/workspaces/{ws}/access",
            get(access::workspace_access),
        )
        .route(
            "/api/v1/workspaces/{ws}/keys/{key}/access",
            get(access::key_access),
        )
        .route(
            "/api/v1/workspaces/{ws}/catalog",
            get(models_ux::workspace_catalog),
        )
        .route(
            "/api/v1/workspaces/{ws}/usage/overview",
            get(usage::workspace_overview),
        )
        .route(
            "/api/v1/workspaces/{ws}/usage/explore",
            get(usage::workspace_explore),
        )
        .route(
            "/api/v1/platform/usage/overview",
            get(usage::platform_overview),
        )
        .route(
            "/api/v1/workspaces/{ws}/usage/storage",
            get(storage_usage::workspace_storage_usage),
        )
        .route(
            "/api/v1/platform/usage/storage",
            get(storage_usage::platform_storage_usage),
        )
        .route(
            "/api/v1/platform/usage/explore",
            get(usage::platform_explore),
        )
        .route(
            "/api/v1/workspaces/{ws}/members",
            get(members::workspace_members).post(members::add_workspace_member),
        )
        .route(
            "/api/v1/workspaces/{ws}/members/{user}",
            axum::routing::delete(members::remove_workspace_member),
        )
        .route(
            "/api/v1/workspaces/{ws}/invitations",
            get(members::invitations).post(members::invite),
        )
        .route(
            "/api/v1/workspaces/{ws}/invitations/{id}",
            axum::routing::delete(members::revoke_invite),
        )
        .route("/api/v1/invitations/accept", post(members::accept_invite))
        .route(
            "/api/v1/workspaces/{ws}/keys",
            get(keys::keys).post(keys::create_key),
        )
        .route(
            "/api/v1/workspaces/{ws}/keys/{key}",
            get(keys::key)
                .delete(keys::revoke_key)
                .patch(keys::update_key),
        )
        .route(
            "/api/v1/workspaces/{ws}/keys/{key}/stats",
            get(keys::key_stats),
        )
        .route(
            "/api/v1/workspaces/{ws}/member-candidates",
            get(members::member_candidates),
        )
        .route(
            "/api/v1/platform/workspaces/{ws}/members",
            get(members::platform_members).post(members::platform_add_member),
        )
        .route(
            "/api/v1/platform/workspaces/{ws}/members/{user}",
            axum::routing::delete(members::platform_remove_member),
        )
        .route(
            "/api/v1/workspaces/{ws}/keys/{key}/rotate",
            post(keys::rotate_key),
        )
        .route(
            "/api/v1/workspaces/{ws}/service-accounts",
            get(keys::accounts).post(keys::create_account),
        )
        .route(
            "/api/v1/workspaces/{ws}/service-accounts/{id}",
            patch(keys::update_account),
        )
        .route("/api/v1/workspaces/{ws}/executions", get(executions))
        .route("/api/v1/workspaces/{ws}/usage", get(usage))
        .route("/api/v1/workspaces/{ws}/audit", get(workspace_audit))
}
pub(crate) type WorkspaceContextRow = (
    Uuid,
    String,
    String,
    Option<Uuid>,
    Option<String>,
    Option<String>,
);

async fn me(State(s): State<Store>, Extension(u): Extension<BrowserPrincipal>) -> ApiResult {
    let mut tx = resources::installation_tx(&s).await?;
    let role = resources::platform_role(&mut tx, u.user_id).await?;
    // Presentation settings (Admin > Settings > General) everyone may see.
    let installation: Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',i.id,'name',i.name,'support_url',s.support_url,'logo_url',s.logo_url,'key_max_lifetime_days',s.human_key_max_lifetime_days) FROM installation i JOIN installation_settings s ON s.singleton WHERE i.singleton",
    )
    .fetch_one(&mut *tx)
    .await?;
    // Presentation of the signed-in identity uses the signature-verified session
    // claim, not an editable directory/profile email.
    let email = &u.email;
    // Optional display name from the last verified sign-in (presentation only).
    let display_name: Option<String> =
        sqlx::query_scalar("SELECT display_name FROM users WHERE id=$1")
            .bind(u.user_id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
    let workspaces: Vec<Value> = my_workspaces(&mut tx, u.user_id)
        .await?
        .into_iter()
        .map(|(id, name, kind, owner_user_id, membership, source)| {
            let personal = kind == "personal";
            let role = if personal { Some("owner".to_owned()) } else { membership };
            let admin = matches!(role.as_deref(), Some("owner" | "admin"));
            json!({"id":id,"name":name,"kind":kind,"owner_user_id":owner_user_id,"role":role,"membership_source":if personal {Some("manual".to_owned())} else {source},"capabilities":resources::capabilities(&kind,admin,role.is_some())})
        })
        .collect();
    tx.commit().await?;
    Ok(Json(
        json!({"installation":installation,"user":{"id":u.user_id,"email":email,"display_name":display_name,"platform_role":role},"workspaces":workspaces,"capabilities":{"platform_read":role=="admin"||role=="auditor","platform_write":role=="admin","create_workspace":role=="admin"}}),
    ))
}
/// The caller's own workspace context: their personal workspace plus shared
/// workspaces where they hold an effective membership (real role). Platform
/// roles never add entries.
pub(crate) async fn my_workspaces(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<Vec<WorkspaceContextRow>, ApiError> {
    Ok(sqlx::query_as("SELECT w.id,w.name,w.kind,w.owner_user_id,m.role,(SELECT CASE WHEN count(DISTINCT g.source)>1 THEN 'mixed' ELSE min(g.source) END FROM workspace_membership_grants g WHERE g.workspace_id=w.id AND g.user_id=$1 AND g.revoked_at IS NULL) FROM workspaces w LEFT JOIN effective_workspace_memberships m ON m.workspace_id=w.id AND m.user_id=$1 WHERE w.disabled_at IS NULL AND ((w.kind='personal' AND w.owner_user_id=$1) OR (w.kind IN ('team','project') AND m.user_id IS NOT NULL)) ORDER BY CASE w.kind WHEN 'personal' THEN 0 ELSE 1 END,w.name,w.id").bind(user).fetch_all(&mut **tx).await?)
}
async fn executions(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    let (l, o) = p.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',e.id,'public_model',e.public_model,'provider',e.provider,'state',e.state,'streamed',e.streamed,'input_tokens',e.input_tokens::text,'output_tokens',e.output_tokens::text,'elapsed_ms',e.elapsed_ms,'started_at',e.started_at,'error_code',e.error_code) FROM inference_executions e JOIN api_keys k ON k.id=e.api_key_id WHERE e.workspace_id=$1 AND ($2 OR k.issued_to_user_id=$3) ORDER BY e.started_at DESC,e.id LIMIT $4 OFFSET $5").bind(ws).bind(a.view_all_activity).bind(u.user_id).bind(l).bind(o).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn usage(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    let v:Value=sqlx::query_scalar("SELECT jsonb_build_object('requests',count(*)::text,'input_tokens',CASE WHEN count(*) FILTER(WHERE e.input_tokens IS NULL)=0 THEN coalesce(sum(e.input_tokens),0)::text ELSE NULL END,'output_tokens',CASE WHEN count(*) FILTER(WHERE e.output_tokens IS NULL)=0 THEN coalesce(sum(e.output_tokens),0)::text ELSE NULL END,'unknown_usage_requests',(count(*) FILTER(WHERE e.input_tokens IS NULL OR e.output_tokens IS NULL))::text) FROM inference_executions e JOIN api_keys k ON k.id=e.api_key_id WHERE e.workspace_id=$1 AND ($2 OR k.issued_to_user_id=$3) AND e.started_at>=now()-interval '30 days'").bind(ws).bind(a.view_all_activity).bind(u.user_id).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(v))
}
async fn workspace_audit(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    let (l, o) = p.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'actor_user_id',actor_user_id,'action',action,'resource_type',resource_type,'resource_id',resource_id,'metadata',metadata,'created_at',created_at) FROM audit_events WHERE workspace_id=$1 AND ($2 OR actor_user_id=$3) ORDER BY created_at DESC,id LIMIT $4 OFFSET $5").bind(ws).bind(a.view_all_activity).bind(u.user_id).bind(l).bind(o).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}

mod access;
mod access_requests;
mod access_reviews;
mod admin_tokens;
mod alerts;
mod audit_export;
mod authorization;
mod batch_scheduling;
mod batches;
pub(crate) mod branding;
mod catalogs;
#[cfg(test)]
mod char_limits_tests;
mod compare;
mod deployment_bulk;
mod deployment_configuration;
mod deployment_history;
mod directory;
mod errors;
mod files;
pub(crate) mod governance;
mod group_mapping_preview;
mod key_safety;
mod keys;
mod legal_holds;
mod logs;
mod me;
mod members;
mod models_ux;
#[cfg(test)]
#[path = "../management_openapi/mod.rs"]
pub(crate) mod openapi;
#[cfg(test)]
#[path = "openapi_semantic_tests.rs"]
pub(crate) mod openapi_semantic;
mod provider_costs;
mod request_notifications;
mod requests;
pub(crate) mod resources;
mod scheduled_reports;
mod sessions;
mod settings;
mod setup;
mod storage_usage;
mod subject_export;
#[cfg(all(test, feature = "integration-tests"))]
mod tests;
mod usage;
mod workspace_templates;

use crate::{
    governance::locks,
    identity::{BrowserPrincipal, IdentityState, ManagementPrincipal, authenticate_management},
    store::Store,
};

/// The acting identity of a management request: whose authority governs it
/// (`user_id`), and which admin token, if any, made the call (`token_id`,
/// for audit attribution). Implemented by both principal types so every
/// shared preflight/transaction helper below (`resources::audit`,
/// `catalog_tx`, `workspace_access` and friends) accepts either without the
/// ~180 existing `Extension<BrowserPrincipal>` handlers needing to change at
/// all (#37 PR2): `BrowserPrincipal` already implements this trait, so every
/// call site that still passes one keeps compiling unchanged, and
/// `route_policy::TokenPolicy::Denied`/`BrowserAdminOnly` refuse an admin
/// token before any handler using a generic helper is ever reached by one.
pub(crate) trait Actor {
    fn user_id(&self) -> Uuid;
    fn token_id(&self) -> Option<Uuid> {
        None
    }
}
impl Actor for BrowserPrincipal {
    fn user_id(&self) -> Uuid {
        self.user_id
    }
}
impl Actor for ManagementPrincipal {
    fn user_id(&self) -> Uuid {
        ManagementPrincipal::user_id(self)
    }
    fn token_id(&self) -> Option<Uuid> {
        ManagementPrincipal::token_id(self)
    }
}
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
pub(crate) use errors::*;
use resources::audit;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

fn ok() -> Json<Value> {
    Json(json!({"ok":true}))
}
fn identifier(id: Uuid) -> Json<Value> {
    Json(json!({"id":id}))
}
pub(crate) use crate::text::chars_at_most;
/// A model or catalog description: at most 2000 characters of text without
/// control characters, except the tab, line feed and carriage return of
/// multi-line text (#428: NUL would reach PostgreSQL and surface as 503).
pub(super) fn description_valid(s: &str) -> bool {
    chars_at_most(s, 2000)
        && !s
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
}
fn valid_name(s: &str) -> bool {
    !s.trim().is_empty() && chars_at_most(s, 120) && !s.chars().any(char::is_control)
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
        .route_layer(middleware::from_fn_with_state(
            identity.store().clone(),
            authorization::require_route_access,
        ))
        .layer(Extension(sign_in))
        .route_layer(middleware::from_fn_with_state(
            identity,
            authenticate_management,
        ))
}
/// Public, sessionless routes: the installation logo (the sign-in page shows
/// it before login).
pub fn public_router() -> Router<Store> {
    branding::public_routes()
}
/// Session routes with their own (larger) body cap; mount outside the shared
/// 2 MiB request limit.
pub fn upload_router(identity: IdentityState) -> Router<Store> {
    files::upload_routes()
        .merge(provider_costs::upload_routes())
        .route_layer(middleware::from_fn_with_state(
            identity.store().clone(),
            authorization::require_route_access,
        ))
        .route_layer(middleware::from_fn_with_state(
            identity,
            authenticate_management,
        ))
}
#[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
fn routes() -> Router<Store> {
    Router::new()
        .merge(scheduled_reports::routes())
        .merge(access_requests::routes())
        .merge(workspace_templates::routes())
        .merge(access_reviews::routes())
        .merge(admin_tokens::routes())
        .merge(provider_costs::routes())
        .merge(directory::routes())
        .merge(group_mapping_preview::routes())
        .merge(resources::platform_routes())
        .merge(catalogs::routes())
        .merge(setup::routes())
        .merge(governance::routes())
        .merge(governance::platform_routes())
        .merge(logs::routes())
        .merge(settings::routes())
        .merge(audit_export::routes())
        .merge(subject_export::routes())
        .merge(legal_holds::routes())
        .merge(sessions::routes())
        .merge(key_safety::routes())
        .merge(compare::routes())
        .merge(alerts::routes())
        .merge(files::routes())
        .merge(batches::routes())
        .merge(batch_scheduling::routes())
        .route("/api/v1/me", get(me))
        .route("/api/v1/me/summary", get(me::summary))
        .route("/api/v1/me/keys", get(me::my_keys))
        .route("/api/v1/me/models", get(me::models))
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
            "/api/v1/workspaces/{ws}/key-issuance",
            get(keys::key_issuance),
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
            "/api/v1/platform/workspaces/{ws}/member-candidates",
            get(members::platform_member_candidates),
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

async fn me(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    files: Option<Extension<crate::filestore::FileStoreRuntime>>,
    inference: Option<Extension<crate::config::InferenceBaseUrl>>,
) -> ApiResult {
    // Lock-free primary snapshot (`crate::reporting`): no catalog or
    // installation lock; the role and workspace list are live as of its start.
    let mut tx = s.snapshot().await?;
    let role = resources::platform_role_snapshot(&mut tx, u.user_id).await?;
    // Presentation settings (Admin > Settings > General) everyone may see.
    // `logo_url` is deprecated (never shown); `logo` is the uploaded logo.
    let mut installation: Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',i.id,'name',i.name,'support_url',s.support_url,'logo_url',s.logo_url,'key_max_lifetime_days',s.human_key_max_lifetime_days) FROM installation i JOIN installation_settings s ON s.singleton WHERE i.singleton",
    )
    .fetch_one(&mut *tx)
    .await?;
    let files = files.map(|Extension(r)| r).unwrap_or_default();
    installation["logo"] = branding::public_logo(&mut *tx, &files).await?;
    installation["inference_base_url"] = json!(inference.and_then(|Extension(url)| url.0));
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
/// Workspace activity reads (executions, 30-day usage): members and admins
/// only, authorized in a lock-free snapshot, rows from it or the reporting
/// replica (`crate::reporting`).
async fn activity_tx(
    s: &Store,
    u: &BrowserPrincipal,
    ws: Uuid,
) -> Result<(Transaction<'static, Postgres>, resources::WorkspaceAccess), ApiError> {
    let mut tx = s.snapshot().await?;
    let a = resources::workspace_access_snapshot(&mut tx, u, ws).await?;
    resources::detail_access(&a)?;
    Ok((s.reporting(tx).await?, a))
}
async fn executions(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (mut tx, a) = activity_tx(&s, &u, ws).await?;
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
    let (mut tx, a) = activity_tx(&s, &u, ws).await?;
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
    // Names only where the caller could already see them: themselves, and for
    // Team/Project admins the current members (the members list) and service
    // accounts; keys they can list; models and the workspace.
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',a.id,'actor_user_id',a.actor_user_id,'actor_name',(SELECT coalesce(nullif(btrim(x.display_name),''),x.email) FROM users x WHERE x.id=a.actor_user_id AND (x.id=$3 OR ($6 AND EXISTS(SELECT 1 FROM effective_workspace_memberships em WHERE em.workspace_id=$1 AND em.user_id=x.id)))),'action',a.action,'resource_type',a.resource_type,'resource_id',a.resource_id,'metadata',a.metadata,'created_at',a.created_at,'target_name',CASE a.resource_type
      WHEN 'key' THEN (SELECT x.name FROM api_keys x WHERE x.id=a.resource_id AND x.workspace_id=$1 AND ($2 OR x.issued_to_user_id=$3))
      WHEN 'model' THEN (SELECT coalesce(nullif(x.display_name,''),x.public_name) FROM models x WHERE x.id=a.resource_id)
      WHEN 'service_account' THEN CASE WHEN $6 THEN (SELECT x.name FROM service_accounts x WHERE x.id=a.resource_id AND x.workspace_id=$1) END
      WHEN 'workspace' THEN (SELECT x.name FROM workspaces x WHERE x.id=$1 AND a.resource_id=$1)
      WHEN 'user' THEN (SELECT coalesce(nullif(btrim(x.display_name),''),x.email) FROM users x WHERE x.id=a.resource_id AND (x.id=$3 OR ($6 AND EXISTS(SELECT 1 FROM effective_workspace_memberships em WHERE em.workspace_id=$1 AND em.user_id=x.id))))
      END) FROM audit_events a WHERE a.workspace_id=$1 AND ($2 OR a.actor_user_id=$3) ORDER BY a.created_at DESC,a.id LIMIT $4 OFFSET $5").bind(ws).bind(a.view_all_activity).bind(u.user_id).bind(l).bind(o).bind(shared(&a.kind) && a.admin).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}

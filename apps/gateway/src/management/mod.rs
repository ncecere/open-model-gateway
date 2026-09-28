mod directory;
mod governance;
mod keys;
mod members;
#[cfg(all(test, feature = "integration-tests"))]
mod project_tests;
mod resources;
#[cfg(all(test, feature = "integration-tests"))]
mod session_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod tests;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::{
    identity::{BrowserPrincipal, IdentityState, require_session},
    store::Store,
};

type ApiResult = Result<Json<Value>, ApiError>;
#[derive(Debug)]
pub(crate) struct ApiError(StatusCode, &'static str);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({"error":{"code":self.0.as_u16().to_string(),"message":self.1}})),
        )
            .into_response()
    }
}
impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        if error
            .as_database_error()
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
fn admin(role: &str) -> bool {
    matches!(role, "owner" | "admin" | "operator")
}
fn owner(role: &str) -> bool {
    matches!(role, "owner" | "operator")
}
fn shared(kind: &str) -> bool {
    matches!(kind, "team" | "project")
}
fn valid_role(role: &str) -> bool {
    matches!(role, "owner" | "admin" | "member")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Page {
    limit: Option<i64>,
    offset: Option<i64>,
}
impl Page {
    fn bounds(&self) -> Result<(i64, i64), ApiError> {
        let limit = self.limit.unwrap_or(100);
        let offset = self.offset.unwrap_or(0);
        if !(1..=200).contains(&limit) || !(0..=100000).contains(&offset) {
            return Err(invalid());
        }
        Ok((limit, offset))
    }
}

pub fn router(identity: IdentityState) -> Router<Store> {
    routes().route_layer(middleware::from_fn_with_state(identity, require_session))
}

fn routes() -> Router<Store> {
    Router::new()
        .merge(governance::routes())
        .merge(resources::platform_routes())
        .merge(governance::platform_routes())
        .merge(directory::routes())
        .route("/api/v1/me", get(me))
        .route("/api/v1/orgs", post(create_org))
        .route("/api/v1/orgs/{org}/workspaces", post(create_workspace))
        .route(
            "/api/v1/orgs/{org}/personal-workspace",
            post(personal_workspace),
        )
        .route("/api/v1/orgs/{org}/members", get(members::org_members))
        .route(
            "/api/v1/orgs/{org}/members/{user}",
            patch(members::update_org_member),
        )
        .route(
            "/api/v1/workspaces/{ws}/members",
            get(members::workspace_members).post(members::add_workspace_member),
        )
        .route(
            "/api/v1/workspaces/{ws}/members/{user}",
            patch(members::update_workspace_member),
        )
        .route(
            "/api/v1/orgs/{org}/invitations",
            get(members::invitations).post(members::invite),
        )
        .route(
            "/api/v1/orgs/{org}/invitations/{id}",
            axum::routing::delete(members::revoke_invite),
        )
        .route("/api/v1/invitations/accept", post(members::accept_invite))
        .route(
            "/api/v1/orgs/{org}/providers",
            get(resources::providers).post(resources::create_provider),
        )
        .route(
            "/api/v1/orgs/{org}/providers/{id}",
            patch(resources::update_provider),
        )
        .route(
            "/api/v1/orgs/{org}/models",
            get(resources::models).post(resources::create_model),
        )
        .route(
            "/api/v1/orgs/{org}/models/{id}",
            patch(resources::update_model),
        )
        .route(
            "/api/v1/orgs/{org}/models/{id}/personal-access",
            axum::routing::put(resources::personal_access),
        )
        .route(
            "/api/v1/orgs/{org}/deployments",
            get(resources::deployments).post(resources::create_deployment),
        )
        .route(
            "/api/v1/orgs/{org}/deployments/{id}",
            patch(resources::update_deployment),
        )
        .route(
            "/api/v1/workspaces/{ws}/grants",
            get(resources::grants).post(resources::grant),
        )
        .route(
            "/api/v1/workspaces/{ws}/grants/{model}",
            axum::routing::delete(resources::revoke_grant),
        )
        .route(
            "/api/v1/workspaces/{ws}/keys",
            get(keys::keys).post(keys::create_key),
        )
        .route(
            "/api/v1/workspaces/{ws}/keys/{key}",
            axum::routing::delete(keys::revoke_key),
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
        .route("/api/v1/orgs/{org}/audit", get(audit_log))
}

async fn require_org(
    store: &Store,
    user: &BrowserPrincipal,
    org: Uuid,
    need_admin: bool,
) -> Result<String, ApiError> {
    let row: Option<(Option<String>,)> = sqlx::query_as("SELECT m.role FROM organizations o LEFT JOIN organization_memberships m ON m.organization_id=o.id AND m.user_id=$2 AND m.disabled_at IS NULL WHERE o.id=$1 AND o.disabled_at IS NULL")
        .bind(org).bind(user.user_id).fetch_optional(&store.pool).await?;
    let role = row.ok_or_else(missing)?.0;
    let role = if user.platform_admin {
        "operator".to_string()
    } else {
        role.ok_or_else(denied)?
    };
    if need_admin && !admin(&role) {
        return Err(denied());
    }
    Ok(role)
}
struct WorkspaceAccess {
    org: Uuid,
    role: String,
    kind: String,
}
async fn require_workspace(
    store: &Store,
    user: &BrowserPrincipal,
    ws: Uuid,
    need_admin: bool,
) -> Result<WorkspaceAccess, ApiError> {
    let row: Option<(Uuid,String,Option<Uuid>,Option<String>)> = sqlx::query_as("SELECT w.organization_id,w.kind,w.owner_user_id,m.role FROM workspaces w LEFT JOIN workspace_memberships m ON m.organization_id=w.organization_id AND m.workspace_id=w.id AND m.user_id=$2 AND m.disabled_at IS NULL WHERE w.id=$1 AND w.disabled_at IS NULL")
        .bind(ws).bind(user.user_id).fetch_optional(&store.pool).await?;
    let (org, kind, personal_owner, membership) = row.ok_or_else(missing)?;
    let org_role = require_org(store, user, org, false).await?;
    let role = if kind == "personal" {
        if personal_owner != Some(user.user_id) {
            return Err(denied());
        }
        "owner".to_owned()
    } else if owner(&org_role) || membership.as_deref() == Some("owner") {
        "owner".to_owned()
    } else if admin(&org_role) {
        "admin".to_owned()
    } else {
        membership.ok_or_else(denied)?
    };
    if need_admin && !admin(&role) {
        return Err(denied());
    }
    Ok(WorkspaceAccess { org, role, kind })
}
// Mutations serialize with admissions and membership changes at the organization,
// then lock workspace, memberships, and current user before any account/key rows.
async fn locked_workspace_access(
    tx: &mut Transaction<'_, Postgres>,
    user: &BrowserPrincipal,
    org: Uuid,
    ws: Uuid,
    need_admin: bool,
) -> Result<WorkspaceAccess, ApiError> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM organizations WHERE id=$1 AND disabled_at IS NULL FOR NO KEY UPDATE",
    )
    .bind(org)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(missing)?;
    let (kind, personal_owner): (String, Option<Uuid>) = sqlx::query_as("SELECT kind,owner_user_id FROM workspaces WHERE organization_id=$1 AND id=$2 AND disabled_at IS NULL FOR NO KEY UPDATE")
        .bind(org).bind(ws).fetch_optional(&mut **tx).await?.ok_or_else(missing)?;
    sqlx::query("SELECT user_id FROM organization_memberships WHERE organization_id=$1 AND user_id=$2 FOR SHARE")
        .bind(org).bind(user.user_id).execute(&mut **tx).await?;
    let membership: Option<String> = sqlx::query_scalar("SELECT role FROM workspace_memberships WHERE organization_id=$1 AND workspace_id=$2 AND user_id=$3 AND disabled_at IS NULL FOR SHARE")
        .bind(org).bind(ws).bind(user.user_id).fetch_optional(&mut **tx).await?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND disabled_at IS NULL FOR SHARE",
    )
    .bind(user.user_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(denied)?;
    let org_role = locked_org_role(tx, user, org).await?;
    let role = if kind == "personal" {
        if personal_owner != Some(user.user_id) {
            return Err(denied());
        }
        "owner".to_owned()
    } else if !shared(&kind) {
        return Err(denied());
    } else if owner(&org_role) || membership.as_deref() == Some("owner") {
        "owner".to_owned()
    } else if admin(&org_role) {
        "admin".to_owned()
    } else {
        membership.ok_or_else(denied)?
    };
    if need_admin && !admin(&role) {
        return Err(denied());
    }
    Ok(WorkspaceAccess { org, role, kind })
}

async fn locked_org_role(
    tx: &mut Transaction<'_, Postgres>,
    user: &BrowserPrincipal,
    org: Uuid,
) -> Result<String, ApiError> {
    let role:Option<String>=sqlx::query_scalar("SELECT CASE WHEN u.platform_admin THEN 'operator' ELSE m.role END FROM users u JOIN organizations o ON o.id=$2 AND o.disabled_at IS NULL LEFT JOIN organization_memberships m ON m.organization_id=o.id AND m.user_id=u.id AND m.disabled_at IS NULL WHERE u.id=$1 AND u.disabled_at IS NULL")
        .bind(user.user_id).bind(org).fetch_optional(&mut **tx).await?.flatten();
    role.ok_or_else(denied)
}

async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
    org: Option<Uuid>,
    ws: Option<Uuid>,
    action: &str,
    target: Option<Uuid>,
) -> Result<(), ApiError> {
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,organization_id,workspace_id,action,target_id) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(Uuid::new_v4()).bind(user).bind(org).bind(ws).bind(action).bind(target).execute(&mut **tx).await?;
    Ok(())
}
async fn ensure_personal(
    tx: &mut Transaction<'_, Postgres>,
    org: Uuid,
    user: Uuid,
) -> Result<Uuid, ApiError> {
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR SHARE")
        .bind(org)
        .execute(&mut **tx)
        .await?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,organization_id,name,kind,owner_user_id) VALUES($1,$2,'Personal','personal',$3) ON CONFLICT (organization_id,owner_user_id) WHERE kind='personal' DO NOTHING")
        .bind(id).bind(org).bind(user).execute(&mut **tx).await?;
    let id:Uuid=sqlx::query_scalar("SELECT id FROM workspaces WHERE organization_id=$1 AND owner_user_id=$2 AND kind='personal' AND disabled_at IS NULL").bind(org).bind(user).fetch_optional(&mut **tx).await?.ok_or_else(denied)?;
    sqlx::query("INSERT INTO workspace_model_grants(organization_id,workspace_id,model_id) SELECT organization_id,$2,model_id FROM organization_model_grants WHERE organization_id=$1 AND personal_enabled ON CONFLICT DO NOTHING").bind(org).bind(id).execute(&mut **tx).await?;
    Ok(id)
}
// Session capabilities describe existing endpoint authorization, not new grants.
// Keep membership distinct from inherited authority: human key issuance needs it.
#[derive(sqlx::FromRow)]
struct SessionOrganization {
    id: Uuid,
    name: String,
    slug: String,
    role: String,
    membership_role: Option<String>,
}
impl SessionOrganization {
    fn into_json(self, platform_admin: bool) -> Value {
        let is_admin = admin(&self.role);
        let is_member = self.membership_role.is_some();
        json!({
            "id": self.id, "name": self.name, "slug": self.slug, "role": self.role,
            "membership_role": self.membership_role,
            "authority_source": if platform_admin { "platform" } else { "direct" },
            "capabilities": {
                "create_workspace": is_admin && is_member,
                "create_personal_workspace": is_member,
                "manage_members": is_admin,
                "manage_owners": owner(&self.role),
                "delegate_models": is_admin,
                "manage_policy": is_admin,
            },
        })
    }
}

#[derive(sqlx::FromRow)]
struct SessionWorkspace {
    id: Uuid,
    organization_id: Uuid,
    name: String,
    kind: String,
    role: String,
    membership_role: Option<String>,
    organization_membership_role: Option<String>,
}
impl SessionWorkspace {
    fn into_json(self, platform_admin: bool) -> Value {
        let personal = self.kind == "personal";
        let is_shared = shared(&self.kind);
        let is_admin = admin(&self.role);
        let org_role = self.organization_membership_role.as_deref();
        let org_admin = platform_admin || org_role.is_some_and(admin);
        // Match require_workspace's priority, including direct shared ownership
        // above inherited organization admin. Personal ownership is always local.
        let authority_source = if personal {
            "personal"
        } else if platform_admin {
            "platform"
        } else if org_role == Some("owner") {
            "organization"
        } else if self.membership_role.as_deref() == Some("owner") {
            "direct"
        } else if org_admin {
            "organization"
        } else {
            "direct"
        };
        let own_key_denial_reason = if org_role.is_none() {
            Some("organization_membership_required")
        } else if !personal && self.membership_role.is_none() {
            Some("workspace_membership_required")
        } else {
            None
        };
        json!({
            "id": self.id, "organization_id": self.organization_id,
            "name": self.name, "kind": self.kind, "role": self.role,
            "membership_role": self.membership_role,
            "authority_source": authority_source,
            "own_key_denial_reason": own_key_denial_reason,
            "capabilities": {
                "issue_own_key": own_key_denial_reason.is_none(),
                "manage_members": is_shared && is_admin,
                "manage_service_accounts": is_shared && is_admin,
                "manage_owners": is_shared && owner(&self.role),
                "delegate_models": org_admin,
                // Governance uses the organization role for personal spaces,
                // not their effective owner role. Existing ceilings and the
                // shared-admin tightening-only rules still apply.
                "manage_policy": if personal { org_admin } else { is_admin },
                "view_all_activity": is_admin,
            },
        })
    }
}

async fn me(State(store): State<Store>, Extension(user): Extension<BrowserPrincipal>) -> ApiResult {
    let mut tx = store.pool.begin().await?;
    let platform_admin: bool = sqlx::query_scalar(
        "SELECT platform_admin FROM users WHERE id=$1 AND disabled_at IS NULL FOR SHARE",
    )
    .bind(user.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(denied)?;
    // These projections use only the existing runtime SELECT grants; visibility,
    // live user locking, and effective roles remain the same as before.
    let organizations = sqlx::query_as::<_, SessionOrganization>(
        r#"SELECT o.id, o.name, o.slug,
                  CASE WHEN $2 THEN 'operator' ELSE m.role END AS role,
                  m.role AS membership_role
           FROM organizations o
           LEFT JOIN organization_memberships m
             ON m.organization_id=o.id AND m.user_id=$1 AND m.disabled_at IS NULL
           WHERE o.disabled_at IS NULL AND ($2 OR m.user_id IS NOT NULL)
           ORDER BY o.name"#,
    )
    .bind(user.user_id)
    .bind(platform_admin)
    .fetch_all(&mut *tx)
    .await?;
    let workspaces = sqlx::query_as::<_, SessionWorkspace>(
        r#"SELECT w.id, w.organization_id, w.name, w.kind,
                  CASE WHEN w.kind='personal' OR $2 OR om.role='owner' OR wm.role='owner'
                       THEN 'owner' WHEN om.role='admin' THEN 'admin' ELSE wm.role END AS role,
                  CASE WHEN w.kind='personal' THEN 'owner' ELSE wm.role END AS membership_role,
                  om.role AS organization_membership_role
           FROM workspaces w
           JOIN organizations o ON o.id=w.organization_id AND o.disabled_at IS NULL
           LEFT JOIN organization_memberships om
             ON om.organization_id=w.organization_id AND om.user_id=$1 AND om.disabled_at IS NULL
           LEFT JOIN workspace_memberships wm
             ON wm.organization_id=w.organization_id AND wm.workspace_id=w.id
             AND wm.user_id=$1 AND wm.disabled_at IS NULL
           WHERE w.disabled_at IS NULL AND ($2 OR om.user_id IS NOT NULL)
             AND ((w.kind='personal' AND w.owner_user_id=$1)
                  OR (w.kind IN ('team','project')
                      AND ($2 OR om.role IN ('owner','admin') OR wm.user_id IS NOT NULL)))
           ORDER BY w.name"#,
    )
    .bind(user.user_id)
    .bind(platform_admin)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let organizations: Vec<Value> = organizations
        .into_iter()
        .map(|org| org.into_json(platform_admin))
        .collect();
    let workspaces: Vec<Value> = workspaces
        .into_iter()
        .map(|ws| ws.into_json(platform_admin))
        .collect();
    Ok(Json(
        json!({"user":{"id":user.user_id,"email":user.email,"platform_admin":platform_admin},"organizations":organizations,"workspaces":workspaces}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewOrg {
    name: String,
    slug: String,
}
async fn create_org(
    State(store): State<Store>,
    Extension(user): Extension<BrowserPrincipal>,
    Json(body): Json<NewOrg>,
) -> ApiResult {
    if !user.platform_admin {
        return Err(denied());
    }
    if !valid_name(&body.name)
        || body.slug.is_empty()
        || body.slug.len() > 80
        || !body
            .slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(invalid());
    }
    let id = Uuid::new_v4();
    let mut tx = store.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
        .execute(&mut *tx)
        .await?;
    let actor: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM users WHERE id=$1 AND platform_admin AND disabled_at IS NULL FOR SHARE",
    )
    .bind(user.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    actor.ok_or_else(denied)?;
    sqlx::query("INSERT INTO organizations(id,name,slug) VALUES($1,$2,$3)")
        .bind(id)
        .bind(body.name)
        .bind(body.slug)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,'owner')",
    )
    .bind(id)
    .bind(user.user_id)
    .execute(&mut *tx)
    .await?;
    ensure_personal(&mut tx, id, user.user_id).await?;
    audit(
        &mut tx,
        user.user_id,
        Some(id),
        None,
        "organization.created",
        Some(id),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Name {
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewWorkspace {
    name: String,
    kind: Option<String>,
}
async fn create_workspace(
    State(store): State<Store>,
    Extension(user): Extension<BrowserPrincipal>,
    Path(org): Path<Uuid>,
    Json(body): Json<NewWorkspace>,
) -> ApiResult {
    require_org(&store, &user, org, true).await?;
    let kind = body.kind.as_deref().unwrap_or("team");
    if !valid_name(&body.name) || !shared(kind) {
        return Err(invalid());
    }
    let id = Uuid::new_v4();
    let mut tx = store.pool.begin().await?;
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(org)
        .execute(&mut *tx)
        .await?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND disabled_at IS NULL FOR SHARE",
    )
    .bind(user.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(denied)?;
    if !admin(&locked_org_role(&mut tx, &user, org).await?) {
        return Err(denied());
    }
    // Operators must hold an active organization membership before owning a shared workspace.
    sqlx::query_scalar::<_,Uuid>("SELECT user_id FROM organization_memberships WHERE organization_id=$1 AND user_id=$2 AND disabled_at IS NULL FOR SHARE").bind(org).bind(user.user_id).fetch_optional(&mut *tx).await?.ok_or_else(denied)?;
    sqlx::query("INSERT INTO workspaces(id,organization_id,name,kind) VALUES($1,$2,$3,$4)")
        .bind(id)
        .bind(org)
        .bind(&body.name)
        .bind(kind)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,'owner')").bind(org).bind(id).bind(user.user_id).execute(&mut *tx).await?;
    audit(
        &mut tx,
        user.user_id,
        Some(org),
        Some(id),
        "workspace.created",
        Some(id),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}
async fn personal_workspace(
    State(store): State<Store>,
    Extension(user): Extension<BrowserPrincipal>,
    Path(org): Path<Uuid>,
) -> ApiResult {
    require_org(&store, &user, org, false).await?;
    let mut tx = store.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
        .execute(&mut *tx)
        .await?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM organizations WHERE id=$1 AND disabled_at IS NULL FOR NO KEY UPDATE",
    )
    .bind(org)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND disabled_at IS NULL FOR SHARE",
    )
    .bind(user.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(denied)?;
    let member:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_memberships WHERE organization_id=$1 AND user_id=$2 AND disabled_at IS NULL)").bind(org).bind(user.user_id).fetch_one(&mut *tx).await?;
    if !member {
        return Err(denied());
    }
    let id = ensure_personal(&mut tx, org, user.user_id).await?;
    audit(
        &mut tx,
        user.user_id,
        Some(org),
        Some(id),
        "workspace.personal_requested",
        Some(id),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}
async fn executions(
    State(store): State<Store>,
    Extension(user): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(page): Query<Page>,
) -> ApiResult {
    let access = require_workspace(&store, &user, ws, false).await?;
    let (limit, offset) = page.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',e.id,'public_model',e.public_model,'provider',e.provider,'state',e.state,'streamed',e.streamed,'input_tokens',e.input_tokens,'output_tokens',e.output_tokens,'elapsed_ms',e.elapsed_ms,'started_at',e.started_at,'error_code',e.error_code) FROM inference_executions e JOIN api_keys k ON k.id=e.api_key_id AND k.organization_id=e.organization_id WHERE e.organization_id=$1 AND e.workspace_id=$2 AND ($3 OR k.issued_to_user_id=$4) ORDER BY e.started_at DESC,e.id LIMIT $5 OFFSET $6")
        .bind(access.org).bind(ws).bind(admin(&access.role)).bind(user.user_id).bind(limit).bind(offset).fetch_all(&store.pool).await?;
    Ok(Json(json!({"data":data})))
}
async fn usage(
    State(store): State<Store>,
    Extension(user): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
) -> ApiResult {
    let access = require_workspace(&store, &user, ws, false).await?;
    let value:Value=sqlx::query_scalar("SELECT jsonb_build_object('requests',count(*),'input_tokens',coalesce(sum(e.input_tokens),0),'output_tokens',coalesce(sum(e.output_tokens),0),'unknown_usage_requests',count(*) FILTER(WHERE e.input_tokens IS NULL OR e.output_tokens IS NULL)) FROM inference_executions e JOIN api_keys k ON k.id=e.api_key_id AND k.organization_id=e.organization_id WHERE e.organization_id=$1 AND e.workspace_id=$2 AND ($3 OR k.issued_to_user_id=$4) AND e.started_at>=now()-interval '30 days'")
        .bind(access.org).bind(ws).bind(admin(&access.role)).bind(user.user_id).fetch_one(&store.pool).await?;
    Ok(Json(value))
}
async fn audit_log(
    State(store): State<Store>,
    Extension(user): Extension<BrowserPrincipal>,
    Path(org): Path<Uuid>,
    Query(page): Query<Page>,
) -> ApiResult {
    require_org(&store, &user, org, true).await?;
    let (limit, offset) = page.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',a.id,'actor_user_id',a.actor_user_id,'action',a.action,'target_id',a.target_id,'workspace_id',a.workspace_id,'created_at',a.created_at) FROM audit_events a LEFT JOIN workspaces w ON w.organization_id=a.organization_id AND w.id=a.workspace_id WHERE a.organization_id=$1 AND (a.workspace_id IS NULL OR w.kind IN ('team','project') OR w.owner_user_id=$2) ORDER BY a.created_at DESC,a.id LIMIT $3 OFFSET $4")
        .bind(org).bind(user.user_id).bind(limit).bind(offset).fetch_all(&store.pool).await?;
    Ok(Json(json!({"data":data})))
}

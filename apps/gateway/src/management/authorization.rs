//! Preflight for the declared access level, before extraction. Handlers retain
//! object checks and live authority rechecks under their own transaction locks.
//!
//! #37 PR2 adds the admin-token half: scope and IP-allowlist enforcement, and
//! a per-attempt audit record (start before dispatch, completion before
//! returning) for every admitted token attempt -- including a denial, which
//! is still an admitted *attempt* by an authenticated token, just not one
//! the route let through.
use super::*;
use crate::{
    http::RequestId,
    identity::{AdminTokenContext, ManagementPrincipal},
    route_policy::{self, Access, TokenPolicy, TokenScope},
};
use axum::{
    extract::{FromRequestParts, MatchedPath, RawPathParams, Request},
    middleware::Next,
};

async fn database_access(
    s: &Store,
    u: &impl Actor,
    access: Access,
    ws: Option<Uuid>,
    key: Option<Uuid>,
) -> Result<(), ApiError> {
    if access == Access::Browser {
        return Ok(());
    } // authenticate_management checked entitlement.
    if matches!(
        access,
        Access::Anonymous
            | Access::Health
            | Access::InferenceKey
            | Access::Scim
            | Access::BrowserLogout
    ) {
        // These use separate routers; accidental management merges fail closed.
        return Err(denied());
    }
    let mut tx = s.snapshot().await?;
    let result = async {
        match access {
            Access::PlatformRead | Access::PlatformWrite => {
                let role = resources::platform_role_snapshot(&mut tx, u.user_id()).await?;
                if role == "admin" || (access == Access::PlatformRead && role == "auditor") {
                    Ok(())
                } else {
                    Err(denied())
                }
            }
            Access::WorkspacePlatformWrite => {
                resources::workspace_platform_write_snapshot(&mut tx, u, ws.ok_or_else(missing)?)
                    .await
            }
            _ => {
                let ws = ws.ok_or_else(missing)?;
                let a = resources::workspace_boundary_snapshot(
                    &mut tx,
                    u,
                    ws,
                    matches!(
                        access,
                        Access::WorkspaceMetadata | Access::WorkspaceAdminOrReader
                    ),
                )
                .await?;
                match access {
                    Access::WorkspaceMetadata => a.metadata_read(),
                    Access::WorkspaceMember => resources::detail_access(&a),
                    Access::WorkspaceKey | Access::WorkspaceKeyRotate => {
                        resources::detail_access(&a)?;
                        let key = key.ok_or_else(missing)?;
                        keys::key_access_snapshot(
                            &mut tx,
                            u,
                            ws,
                            key,
                            a.admin,
                            access == Access::WorkspaceKeyRotate,
                        )
                        .await
                    }
                    Access::WorkspaceAdmin => resources::manage(&a),
                    Access::WorkspaceAdminOrReader
                        if a.admin || (a.platform_reader && shared(&a.kind)) =>
                    {
                        Ok(())
                    }
                    _ => Err(denied()),
                }
            }
        }
    }
    .await;
    // Release this primary connection, even on a refusal, before the handler
    // starts its own transaction. Never hold two pool connections per request.
    tx.rollback().await?;
    result
}

/// Admin tokens never reach this far for a workspace-member/key/admin/
/// inference/SCIM route (`TokenPolicy::Denied`), so only the Scope-bearing
/// `Access` variants above are ever evaluated for one; this set is just the
/// routes among those whose *handler* has actually been converted to accept
/// either principal (#37 PR2). A Scope-declared route not yet listed here
/// answers 501 for a token instead of reaching a handler that still expects
/// only a browser session -- an explicit, audited "not implemented yet",
/// never a crash and never a silent bypass. Exact method + router template.
pub(super) const TOKEN_READY_ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/v1/platform/providers"),
    ("POST", "/api/v1/platform/providers"),
    ("GET", "/api/v1/platform/providers/{id}"),
    ("PATCH", "/api/v1/platform/providers/{id}"),
    ("DELETE", "/api/v1/platform/providers/{id}"),
    ("GET", "/api/v1/platform/models"),
    ("POST", "/api/v1/platform/models"),
    ("GET", "/api/v1/platform/models/{id}"),
    ("PATCH", "/api/v1/platform/models/{id}"),
    ("DELETE", "/api/v1/platform/models/{id}"),
    ("GET", "/api/v1/platform/deployments"),
    ("POST", "/api/v1/platform/deployments"),
    ("GET", "/api/v1/platform/deployments/{id}"),
    ("PATCH", "/api/v1/platform/deployments/{id}"),
    ("DELETE", "/api/v1/platform/deployments/{id}"),
    ("GET", "/api/v1/platform/audit"),
    ("GET", "/api/v1/platform/server-policy"),
    ("GET", "/api/v1/workspaces/{ws}"),
    ("GET", "/api/v1/platform/workspaces"),
    ("POST", "/api/v1/platform/workspaces"),
    ("GET", "/api/v1/platform/workspaces/{ws}"),
    ("PATCH", "/api/v1/platform/workspaces/{ws}"),
    ("GET", "/api/v1/platform/users"),
    ("POST", "/api/v1/platform/users"),
    ("GET", "/api/v1/platform/users/{user}"),
    ("PATCH", "/api/v1/platform/users/{user}"),
    ("POST", "/api/v1/platform/users/{user}/roles"),
    ("POST", "/api/v1/platform/users/{user}/remove-personal-data"),
    ("DELETE", "/api/v1/platform/users/{user}/roles/{role}"),
    ("GET", "/api/v1/platform/oidc/group-mappings"),
    ("POST", "/api/v1/platform/oidc/group-mappings"),
    ("PATCH", "/api/v1/platform/oidc/group-mappings/{id}"),
    ("DELETE", "/api/v1/platform/oidc/group-mappings/{id}"),
    ("GET", "/api/v1/platform/cost-centers"),
    ("POST", "/api/v1/platform/cost-centers"),
    ("GET", "/api/v1/platform/cost-centers/{id}"),
    ("PATCH", "/api/v1/platform/cost-centers/{id}"),
    ("DELETE", "/api/v1/platform/cost-centers/{id}"),
    ("GET", "/api/v1/platform/settings/general"),
    ("PUT", "/api/v1/platform/settings/general"),
    ("GET", "/api/v1/platform/settings/privacy"),
    ("PUT", "/api/v1/platform/settings/privacy"),
    ("GET", "/api/v1/platform/settings/email"),
    ("PUT", "/api/v1/platform/settings/email"),
    ("POST", "/api/v1/platform/settings/email/test"),
    ("GET", "/api/v1/platform/settings/sign-in"),
    ("GET", "/api/v1/platform/settings/storage"),
    ("PUT", "/api/v1/platform/settings/storage"),
    ("POST", "/api/v1/platform/settings/storage/test"),
    ("PUT", "/api/v1/platform/settings/general/logo"),
    ("DELETE", "/api/v1/platform/settings/general/logo"),
    ("GET", "/api/v1/platform/settings/audit-export"),
    ("POST", "/api/v1/platform/settings/audit-export/test"),
    ("GET", "/api/v1/platform/legal-holds"),
    ("POST", "/api/v1/platform/legal-holds"),
    ("POST", "/api/v1/platform/legal-holds/{hold}/release"),
    (
        "PUT",
        "/api/v1/platform/deployments/{deployment}/batch-scheduling",
    ),
    (
        "GET",
        "/api/v1/platform/deployments/{deployment}/batch-scheduling",
    ),
    ("GET", "/api/v1/platform/installation/policy"),
    ("PUT", "/api/v1/platform/installation/policy"),
    ("GET", "/api/v1/platform/workspace-types/{kind}/policy"),
    ("PUT", "/api/v1/platform/workspace-types/{kind}/policy"),
    ("GET", "/api/v1/platform/workspaces/{ws}/policy"),
    ("PUT", "/api/v1/platform/workspaces/{ws}/policy"),
    ("DELETE", "/api/v1/platform/workspaces/{ws}/policy"),
    ("GET", "/api/v1/workspaces/{ws}/access"),
    ("GET", "/api/v1/workspaces/{ws}/cost-report"),
    ("GET", "/api/v1/workspaces/{ws}/cost-summary"),
    ("GET", "/api/v1/workspaces/{ws}/costs"),
    ("GET", "/api/v1/workspaces/{ws}/usage-export"),
    (
        "POST",
        "/api/v1/workspaces/{ws}/costs/{execution}/reconcile",
    ),
    ("GET", "/api/v1/platform/cost-report"),
    ("GET", "/api/v1/platform/deployments/{deployment}/prices"),
    ("POST", "/api/v1/platform/deployments/{deployment}/prices"),
    (
        "GET",
        "/api/v1/platform/deployments/{deployment}/price-suggestion",
    ),
    ("GET", "/api/v1/platform/models/{model}/routing"),
    ("PUT", "/api/v1/platform/models/{model}/routing"),
    ("GET", "/api/v1/platform/deployments/{deployment}/routing"),
    ("PUT", "/api/v1/platform/deployments/{deployment}/routing"),
    ("GET", "/api/v1/platform/catalogs"),
    ("POST", "/api/v1/platform/catalogs"),
    ("GET", "/api/v1/platform/catalogs/{id}"),
    ("PATCH", "/api/v1/platform/catalogs/{id}"),
    ("DELETE", "/api/v1/platform/catalogs/{id}"),
    ("GET", "/api/v1/platform/catalogs/{id}/models"),
    ("PUT", "/api/v1/platform/catalogs/{id}/models"),
    ("GET", "/api/v1/platform/models/{id}/catalogs"),
    ("PUT", "/api/v1/platform/models/{id}/catalogs"),
    ("GET", "/api/v1/platform/workspace-types/{kind}/catalogs"),
    ("PUT", "/api/v1/platform/workspace-types/{kind}/catalogs"),
    ("GET", "/api/v1/platform/catalog-defaults"),
    ("PUT", "/api/v1/platform/catalog-defaults"),
    ("GET", "/api/v1/platform/workspaces/{ws}/catalogs"),
    ("PUT", "/api/v1/platform/workspaces/{ws}/catalogs"),
    ("DELETE", "/api/v1/platform/workspaces/{ws}/catalogs"),
    ("GET", "/api/v1/workspaces/{ws}/available-models"),
    ("GET", "/api/v1/workspaces/{ws}/models"),
    ("POST", "/api/v1/platform/workspaces/{ws}/models"),
    ("DELETE", "/api/v1/platform/workspaces/{ws}/models/{model}"),
    ("GET", "/api/v1/platform/models/compare"),
    ("GET", "/api/v1/platform/usage/overview"),
    ("GET", "/api/v1/platform/usage/storage"),
    ("GET", "/api/v1/platform/usage/explore"),
    ("GET", "/api/v1/platform/workspaces/{ws}/member-candidates"),
    ("GET", "/api/v1/platform/workspaces/{ws}/members"),
    ("POST", "/api/v1/platform/workspaces/{ws}/members"),
    ("DELETE", "/api/v1/platform/workspaces/{ws}/members/{user}"),
    ("GET", "/api/v1/platform/users/{user}/export"),
    ("GET", "/api/v1/platform/key-safety"),
    ("GET", "/api/v1/platform/logs/requests"),
    ("GET", "/api/v1/platform/logs/requests/{root}"),
    ("GET", "/api/v1/platform/logs/generations"),
    ("GET", "/api/v1/platform/logs/sessions"),
    ("GET", "/api/v1/platform/logs/sessions/{ws}/{session}"),
    ("GET", "/api/v1/platform/logs/metrics"),
    ("POST", "/api/v1/platform/model-setup"),
    ("GET", "/api/v1/platform/overview"),
    ("GET", "/api/v1/workspaces/{ws}/catalog"),
    ("GET", "/api/v1/workspaces/{ws}/policy"),
    ("GET", "/api/v1/workspaces/{ws}/members"),
    ("GET", "/api/v1/workspaces/{ws}/usage/overview"),
    ("GET", "/api/v1/workspaces/{ws}/usage/explore"),
    ("GET", "/api/v1/workspaces/{ws}/alerts/rules"),
    ("GET", "/api/v1/workspaces/{ws}/alerts/rules/{id}"),
    ("GET", "/api/v1/workspaces/{ws}/alerts/events"),
    ("GET", "/api/v1/platform/alerts/rules"),
    ("POST", "/api/v1/platform/alerts/rules"),
    ("GET", "/api/v1/platform/alerts/rules/{id}"),
    ("PUT", "/api/v1/platform/alerts/rules/{id}"),
    ("DELETE", "/api/v1/platform/alerts/rules/{id}"),
    ("GET", "/api/v1/platform/alerts/events"),
    ("POST", "/api/v1/platform/users/{user}/sessions/revoke"),
    ("GET", "/api/v1/platform/batches"),
];
fn token_ready(method: &str, path: &str) -> bool {
    TOKEN_READY_ROUTES
        .iter()
        .any(|(m, p)| *m == method && *p == path)
}

/// Everything an `admin_token_attempts` row (start or completion) needs,
/// bundled so [`record_attempt`] takes one argument per *kind* of fact
/// rather than one per column.
struct Attempt<'a> {
    request_id: Uuid,
    token_id: Uuid,
    owner_user_id: Uuid,
    method: &'a str,
    template: &'a str,
    required_scope: Option<&'static str>,
    outcome: &'static str,
}
async fn record_attempt(
    pool: &sqlx::PgPool,
    phase: &'static str,
    attempt: &Attempt<'_>,
    status: Option<u16>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO admin_token_attempts(id,request_id,phase,token_id,owner_user_id,route_method,route_template,required_scope,outcome,status_code) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(Uuid::new_v4())
    .bind(attempt.request_id)
    .bind(phase)
    .bind(attempt.token_id)
    .bind(attempt.owner_user_id)
    .bind(attempt.method)
    .bind(attempt.template)
    .bind(attempt.required_scope)
    .bind(attempt.outcome)
    .bind(status.map(|s| s as i16))
    .execute(pool)
    .await?;
    Ok(())
}

fn audit_unavailable() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "Audit storage unavailable; the request was not processed",
    )
}

/// The admin-token half of [`require_route_access`]: scope, IP allowlist and
/// every admitted attempt audited individually, start before dispatch and
/// completion before returning. Audit failure prevents dispatch (a failed
/// start insert never calls `next`) or, for a failed completion insert,
/// replaces an otherwise-successful response with 503 rather than let an
/// unaudited attempt go out silently. A `BrowserPrincipal` is never
/// manufactured: the handler extension inserted below (only when the route
/// is admitted and token-ready) is `ManagementPrincipal::AdminToken`.
async fn handle_admin_token(
    s: Store,
    context: AdminTokenContext,
    request_id: Uuid,
    mut parts: axum::http::request::Parts,
    body: axum::body::Body,
    next: Next,
) -> Response {
    let method = parts.method.clone();
    let template = parts
        .extensions
        .get::<MatchedPath>()
        .map_or_else(String::new, |p| p.as_str().to_owned());
    let decision = route_policy::token_policy(method.as_str(), &template);
    let access = route_policy::lookup(method.as_str(), &template);
    let required_scope = match decision {
        Some(TokenPolicy::Scope(scope)) => Some(scope),
        _ => None,
    };
    let mut allowed = false;
    if let (Some(scope), Some(access)) = (required_scope, access) {
        let scope_ok = context.scopes.iter().any(|s| s.as_str() == scope.as_str());
        let ip_ok = crate::admin_tokens::ip_allowed(
            context.ip_allowlist.as_deref(),
            context.client_address,
        );
        if scope_ok && ip_ok {
            let ws = workspace_param(&mut parts, &s, access).await;
            let key = key_param(&mut parts, &s, access).await;
            let principal = ManagementPrincipal::AdminToken(context.clone());
            allowed = database_access(&s, &principal, access, ws, key)
                .await
                .is_ok();
        }
    }
    let required_scope_str = required_scope.map(TokenScope::as_str);
    let outcome = if allowed { "allowed" } else { "denied" };
    let attempt = Attempt {
        request_id,
        token_id: context.token_id,
        owner_user_id: context.owner_user_id,
        method: method.as_str(),
        template: &template,
        required_scope: required_scope_str,
        outcome,
    };
    if let Err(error) = record_attempt(&s.pool, "start", &attempt, None).await {
        crate::management::log_storage_error(&error);
        return audit_unavailable().into_response();
    }
    let ready = allowed && token_ready(method.as_str(), &template);
    let mut response = if !allowed {
        denied().into_response()
    } else if !ready {
        ApiError(
            StatusCode::NOT_IMPLEMENTED,
            "This route does not yet accept admin API tokens",
        )
        .into_response()
    } else {
        if let Err(error) = s
            .touch_admin_token_use(context.token_id, context.client_address)
            .await
        {
            tracing::warn!(%error, "could not record admin token use");
        }
        parts
            .extensions
            .insert(ManagementPrincipal::AdminToken(context.clone()));
        next.run(Request::from_parts(parts, body)).await
    };
    let status = response.status().as_u16();
    if let Err(error) = record_attempt(&s.pool, "completion", &attempt, Some(status)).await {
        crate::management::log_storage_error(&error);
        response = audit_unavailable().into_response();
    }
    response
}

pub(super) async fn require_route_access(
    State(s): State<Store>,
    request: Request,
    next: Next,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let Some(principal) = parts.extensions.get::<ManagementPrincipal>().cloned() else {
        return denied().into_response();
    };
    if parts.extensions.get::<MatchedPath>().is_none() {
        return denied().into_response();
    }
    match principal {
        ManagementPrincipal::AdminToken(context) => {
            let request_id = parts
                .extensions
                .get::<RequestId>()
                .map_or_else(Uuid::new_v4, |id| id.0);
            handle_admin_token(s, context, request_id, parts, body, next).await
        }
        ManagementPrincipal::Browser(u) => {
            let method = parts.method.clone();
            let path = parts
                .extensions
                .get::<MatchedPath>()
                .expect("checked above")
                .as_str()
                .to_owned();
            let result = async {
                // Exact router template and method, never an untrusted URL prefix.
                let access = route_policy::lookup(method.as_str(), &path).ok_or_else(denied)?;
                let ws = workspace_param(&mut parts, &s, access).await;
                let key = key_param(&mut parts, &s, access).await;
                database_access(&s, &u, access, ws, key).await
            }
            .await;
            match result {
                Ok(()) => next.run(Request::from_parts(parts, body)).await,
                Err(error) => error.into_response(),
            }
        }
    }
}

fn workspace_scoped(access: Access) -> bool {
    matches!(
        access,
        Access::WorkspaceMetadata
            | Access::WorkspaceMember
            | Access::WorkspaceKey
            | Access::WorkspaceKeyRotate
            | Access::WorkspaceAdmin
            | Access::WorkspaceAdminOrReader
            | Access::WorkspacePlatformWrite
    )
}

async fn workspace_param(
    parts: &mut axum::http::request::Parts,
    s: &Store,
    access: Access,
) -> Option<Uuid> {
    if !workspace_scoped(access) {
        return None;
    }
    let params = RawPathParams::from_request_parts(parts, s).await.ok()?;
    params
        .iter()
        .find(|(n, _)| *n == "ws")
        .and_then(|(_, value)| Uuid::parse_str(value).ok())
}

async fn key_param(
    parts: &mut axum::http::request::Parts,
    s: &Store,
    access: Access,
) -> Option<Uuid> {
    if !matches!(access, Access::WorkspaceKey | Access::WorkspaceKeyRotate) {
        return None;
    }
    let params = RawPathParams::from_request_parts(parts, s).await.ok()?;
    params
        .iter()
        .find(|(n, _)| *n == "key")
        .and_then(|(_, value)| Uuid::parse_str(value).ok())
}

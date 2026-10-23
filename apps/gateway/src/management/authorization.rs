//! Preflight for the declared access level, before extraction. Handlers retain
//! object checks and live authority rechecks under their own transaction locks.
use super::*;
use crate::route_policy::{self, Access};
use axum::{
    extract::{FromRequestParts, MatchedPath, RawPathParams, Request},
    middleware::Next,
};

async fn database_access(
    s: &Store,
    u: &BrowserPrincipal,
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
                let role = resources::platform_role_snapshot(&mut tx, u.user_id).await?;
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

pub(super) async fn require_route_access(
    State(s): State<Store>,
    request: Request,
    next: Next,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let result = async {
        let u = parts
            .extensions
            .get::<BrowserPrincipal>()
            .ok_or_else(denied)?
            .clone();
        let path = parts.extensions.get::<MatchedPath>().ok_or_else(denied)?;
        // Exact router template and method, never an untrusted URL prefix.
        let access =
            route_policy::lookup(parts.method.as_str(), path.as_str()).ok_or_else(denied)?;
        let workspace_scoped = matches!(
            access,
            Access::WorkspaceMetadata
                | Access::WorkspaceMember
                | Access::WorkspaceKey
                | Access::WorkspaceKeyRotate
                | Access::WorkspaceAdmin
                | Access::WorkspaceAdminOrReader
                | Access::WorkspacePlatformWrite
        );
        let (ws, key) = if workspace_scoped {
            let params = RawPathParams::from_request_parts(&mut parts, &s)
                .await
                .map_err(|_| missing())?;
            let parse = |name| {
                params
                    .iter()
                    .find(|(n, _)| *n == name)
                    .and_then(|(_, value)| Uuid::parse_str(value).ok())
                    .ok_or_else(missing)
            };
            let ws = parse("ws")?;
            let key = if matches!(access, Access::WorkspaceKey | Access::WorkspaceKeyRotate) {
                Some(parse("key")?)
            } else {
                None
            };
            (Some(ws), key)
        } else {
            (None, None)
        };
        database_access(&s, &u, access, ws, key).await
    }
    .await;
    match result {
        Ok(()) => next.run(Request::from_parts(parts, body)).await,
        Err(error) => error.into_response(),
    }
}

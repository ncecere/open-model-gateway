//! Browser Admin-only API for admin API tokens (#37 PR2). Declared
//! `TokenPolicy::BrowserAdminOnly` in `route_policy.rs`: never reachable by
//! a token itself, regardless of any scope it holds. GET/POST
//! `/api/v1/platform/admin-tokens`, DELETE `/api/v1/platform/admin-tokens/{token}`.
use super::*;
use crate::admin_tokens::{self, CreateAdminTokenError, NewAdminTokenError};

/// Every admin-tokens operation requires a currently effective Platform
/// Admin specifically (not Auditor), checked live: no installation or
/// catalog lock is relevant to this independent resource.
async fn admin_tx<'a>(
    s: &'a Store,
    u: &BrowserPrincipal,
) -> Result<Transaction<'a, Postgres>, ApiError> {
    let mut tx = crate::db::begin(&s.pool).await?;
    resources::platform_write(&mut tx, u).await?;
    Ok(tx)
}

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/platform/admin-tokens", get(list).post(create))
        .route(
            "/api/v1/platform/admin-tokens/{token}",
            axum::routing::delete(revoke),
        )
}

const LIST_SQL: &str = "SELECT jsonb_build_object(\
 'id',t.id,\
 'owner_user_id',t.owner_user_id,\
 'owner_email',u.email,\
 'owner_display_name',u.display_name,\
 'name',t.name,\
 'scopes',t.scopes,\
 'ip_allowlist',t.ip_allowlist::text[],\
 'created_at',t.created_at,\
 'expires_at',t.expires_at,\
 'revoked_at',t.revoked_at,\
 'last_used_at',t.last_used_at,\
 'last_used_address',host(t.last_used_address)\
) FROM admin_api_tokens t JOIN users u ON u.id=t.owner_user_id \
 ORDER BY t.created_at DESC,t.id LIMIT $1 OFFSET $2";

/// Installation-wide: every admin token, any owner, metadata only -- never
/// a digest or anything that could reconstruct the plaintext.
async fn list(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<Page>,
) -> ApiResult {
    let mut tx = admin_tx(&s, &u).await?;
    let (l, o) = p.bounds()?;
    let data: Vec<Value> = sqlx::query_scalar(LIST_SQL)
        .bind(l)
        .bind(o)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewAdminTokenInput {
    name: String,
    scopes: Vec<String>,
    expires_in_days: i32,
    #[serde(default)]
    ip_allowlist: Option<Vec<String>>,
}

fn create_error(error: CreateAdminTokenError) -> ApiError {
    match error {
        CreateAdminTokenError::Invalid(NewAdminTokenError::InvalidName) => {
            ApiError(StatusCode::BAD_REQUEST, "Invalid token name")
        }
        CreateAdminTokenError::Invalid(NewAdminTokenError::InvalidScopes) => {
            ApiError(StatusCode::BAD_REQUEST, "Invalid token scopes")
        }
        CreateAdminTokenError::Invalid(NewAdminTokenError::InvalidExpiry) => {
            ApiError(StatusCode::BAD_REQUEST, "expires_in_days must be 1 to 90")
        }
        CreateAdminTokenError::Invalid(NewAdminTokenError::InvalidAllowlist) => {
            ApiError(StatusCode::BAD_REQUEST, "Invalid IP allowlist")
        }
        CreateAdminTokenError::Entropy(e) => e.into(),
        // The browser-session preflight above already required the caller
        // to be a currently effective Admin, and "create only for oneself"
        // means owner == caller; either hitting one of these three storage-
        // level guards here means a concurrent change raced the check (the
        // owner lost Admin between the preflight and the INSERT) or an
        // internal contract was not honored. Never a caller input problem.
        CreateAdminTokenError::OwnerNotAdmin => ApiError(
            StatusCode::CONFLICT,
            "Your Platform Admin access changed; sign in again and retry",
        ),
        CreateAdminTokenError::UnsupportedIsolation | CreateAdminTokenError::OwnerLockNotHeld => {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Management storage unavailable",
            )
        }
        CreateAdminTokenError::Database(e) => e.into(),
    }
}

/// Creation is for the caller only: `owner_user_id` is always the
/// authenticated browser session's user, never taken from the request body
/// (#37 PR2 coordinator decision: "creation only for oneself"). Takes the
/// owner's user-scope authority lock exclusive before the first write
/// (`governance::locks::exclusive`, AGENTS.md's canonical order), as
/// `create_admin_token`'s own contract requires of every caller.
async fn create(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<NewAdminTokenInput>,
) -> ApiResult {
    let mut tx = admin_tx(&s, &u).await?;
    locks::exclusive(&mut tx, [locks::Scope::User(u.user_id)]).await?;
    let created = admin_tokens::create_admin_token(
        &mut tx,
        u.user_id,
        &b.name,
        &b.scopes,
        b.expires_in_days,
        b.ip_allowlist.as_deref(),
    )
    .await
    .map_err(create_error)?;
    audit(
        &mut tx,
        &u,
        None,
        "admin_token.created",
        "admin_token",
        Some(created.id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    // The plaintext is returned exactly once, here, and never again
    // (`no-store` is also already the default for every management
    // response: `http::request_context`).
    Ok(Json(json!({
        "id": created.id,
        "token": created.token,
        "scopes": b.scopes,
        "created_at": created.created_at,
        "expires_at": created.expires_at,
    })))
}

/// Idempotent, and any currently effective Admin may revoke any token
/// (#37 PR2 coordinator decision), not only its owner.
async fn revoke(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(token): Path<Uuid>,
) -> ApiResult {
    let mut tx = admin_tx(&s, &u).await?;
    admin_tokens::revoke_admin_token(&mut tx, token).await?;
    audit(
        &mut tx,
        &u,
        None,
        "admin_token.revoked",
        "admin_token",
        Some(token),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}

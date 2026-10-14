//! "Your sessions" (#79): a signed-in user lists and revokes their own
//! dashboard sessions; a Platform Admin can revoke all of one person's.
//!
//! Only the session's public `id`, its fixed-vocabulary device label and its
//! times are returned, never the token hash, an address or a raw user agent.
//! An admin sees how many sessions a person has (user detail), never which
//! devices. Revoking a session ends dashboard access only: inference keys are
//! separate credentials and are untouched, and revocation is final (#145).

use super::*;
use crate::identity::CurrentSession;
use axum::routing::delete;

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/me/sessions", get(list))
        .route("/api/v1/me/sessions/{session}", delete(revoke_one))
        .route("/api/v1/me/sessions/revoke-others", post(revoke_others))
        .route(
            "/api/v1/platform/users/{user}/sessions/revoke",
            post(revoke_all_for_user),
        )
}

/// Live: not revoked, not expired, and still bound to a verified sign-in.
const LIVE: &str = "revoked_at IS NULL AND expires_at>now() AND (idle_expires_at IS NULL OR idle_expires_at>now()) AND verified_email IS NOT NULL";

async fn list(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    current: Option<Extension<CurrentSession>>,
) -> ApiResult {
    let current = current.map(|Extension(c)| c.0);
    let data: Value = sqlx::query_scalar(&format!(
        "SELECT coalesce(jsonb_agg(jsonb_build_object('id',id,'client',client_label,'created_at',created_at,'last_seen_at',last_seen_at,'expires_at',expires_at,'current',id IS NOT DISTINCT FROM $2) ORDER BY id IS NOT DISTINCT FROM $2 DESC,coalesce(last_seen_at,created_at) DESC,id),'[]'::jsonb) FROM browser_sessions WHERE user_id=$1 AND {LIVE}"
    ))
    .bind(u.user_id)
    .bind(current)
    .fetch_one(&s.pool)
    .await?;
    Ok(Json(json!({ "data": data })))
}

/// Revoke one of the caller's own sessions (the current one too: that signs
/// this browser out). Another user's, an unknown or an already-ended session
/// is 404, so IDs can't be probed.
async fn revoke_one(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(session): Path<Uuid>,
) -> ApiResult {
    let mut tx = crate::db::begin(&s.pool).await?;
    let revoked = sqlx::query(&format!(
        "UPDATE browser_sessions SET revoked_at=now() WHERE id=$1 AND user_id=$2 AND {LIVE}"
    ))
    .bind(session)
    .bind(u.user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if revoked == 0 {
        return Err(missing());
    }
    audit(
        &mut tx,
        &u,
        None,
        "session.revoked",
        "user",
        Some(u.user_id),
        json!({"count":revoked}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "revoked": revoked })))
}

/// Sign out everywhere else: every live session of the caller except this one.
async fn revoke_others(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    current: Option<Extension<CurrentSession>>,
) -> ApiResult {
    // Without a known current session there is nothing to keep, and revoking
    // everything would be a surprise: refuse rather than guess.
    let Some(Extension(CurrentSession(current))) = current else {
        return Err(invalid());
    };
    let mut tx = crate::db::begin(&s.pool).await?;
    let revoked = sqlx::query(&format!(
        "UPDATE browser_sessions SET revoked_at=now() WHERE user_id=$1 AND id<>$2 AND {LIVE}"
    ))
    .bind(u.user_id)
    .bind(current)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if revoked > 0 {
        audit(
            &mut tx,
            &u,
            None,
            "session.others_revoked",
            "user",
            Some(u.user_id),
            json!({"count":revoked}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Json(json!({ "revoked": revoked })))
}

/// Platform Admin: end every dashboard session of one person (for example a
/// lost laptop) without suspending them. Audited with the count only.
async fn revoke_all_for_user(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(user): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE id=$1 AND cleaned_at IS NULL")
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    let revoked = sqlx::query(&format!(
        "UPDATE browser_sessions SET revoked_at=now() WHERE user_id=$1 AND {LIVE}"
    ))
    .bind(user)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    audit(
        &mut tx,
        &u,
        None,
        "user.sessions_revoked",
        "user",
        Some(user),
        json!({"count":revoked}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "revoked": revoked })))
}

/// Live session count for the platform user detail (no device details).
pub(super) async fn live_count(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<i64, ApiError> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT count(*) FROM browser_sessions WHERE user_id=$1 AND {LIVE}"
    ))
    .bind(user)
    .fetch_one(&mut **tx)
    .await?)
}

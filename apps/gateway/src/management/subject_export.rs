//! Subject export (#74): a person downloads their own data, and a Platform
//! Admin downloads only what admins already see about a person.
//!
//! - `GET /api/v1/me/export?from=YYYY-MM-DD&to=YYYY-MM-DD`: the caller's
//!   profile, roles, memberships, API key metadata (never secrets), live
//!   sessions, their keys' request records, the metadata of files they
//!   uploaded (not contents) and their own audit entries, for a range of at
//!   most 93 days (default: the last 30). Each list is capped and flagged
//!   `truncated` when the cap is hit.
//! - `GET /api/v1/platform/users/{id}/export`: profile, roles, shared
//!   memberships and platform-visible audit entries by or about the person;
//!   never their personal workspace, keys, sessions or requests.
//!
//! One JSON document, generated on demand and never stored; its SHA-256 is
//! the `X-OMG-Export-SHA256` header. Audited (`subject.exported`,
//! `user.exported`) and rate-limited from the audit log.

use super::*;
use axum::response::{IntoResponse, Response};
use chrono::{Duration as Days, NaiveDate, Utc};
use sha2::{Digest, Sha256};

pub const FORMAT: &str = "omg.subject-export.v1";
const MAX_DAYS: i64 = 93;
const DEFAULT_DAYS: i64 = 30;
const REQUEST_CAP: i64 = 50_000;
const LIST_CAP: i64 = 10_000;
const EXPORTS_PER_HOUR: i64 = 3;
/// The whole export, and each statement in it.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
const STATEMENT_TIMEOUT: &str = "SET LOCAL statement_timeout='15s'";

fn busy() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "Exports are busy; try again shortly",
    )
}

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/me/export", get(mine))
        .route("/api/v1/platform/users/{user}/export", get(platform))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Range {
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
}

/// `[from, to]` inclusive UTC days, at most 93; default the last 30.
fn range(q: &Range) -> Result<(NaiveDate, NaiveDate), ApiError> {
    let today = Utc::now().date_naive();
    let to = q.to.unwrap_or(today);
    let from = q.from.unwrap_or(to - Days::days(DEFAULT_DAYS - 1));
    if from > to || (to - from).num_days() + 1 > MAX_DAYS || to > today {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Choose a range of up to 93 days, ending today or earlier",
        ));
    }
    Ok((from, to))
}

/// Rate-limit and audit an export (its own transaction, before reading).
async fn record(
    s: &Store,
    u: &impl Actor,
    action: &'static str,
    target: Uuid,
) -> Result<(), ApiError> {
    let mut tx = crate::db::begin(&s.pool).await?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await?;
    // One count at a time per person and kind: parallel requests can't all
    // pass the limit.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1||':'||$2::text,0))")
        .bind(action)
        .bind(u.user_id())
        .execute(&mut *tx)
        .await?;
    let recent: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE actor_user_id=$1 AND action=$2 AND created_at>now()-interval '1 hour'")
        .bind(u.user_id())
        .bind(action)
        .fetch_one(&mut *tx)
        .await?;
    if recent >= EXPORTS_PER_HOUR {
        return Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many exports; try again later",
        ));
    }
    audit(&mut tx, u, None, action, "user", Some(target), json!({})).await?;
    tx.commit().await?;
    Ok(())
}

/// A capped list: `{items, truncated}`.
async fn capped(
    tx: &mut Transaction<'_, Postgres>,
    select: &str,
    cap: i64,
    binds: impl FnOnce(
        sqlx::query::QueryScalar<'_, Postgres, Value, sqlx::postgres::PgArguments>,
    )
        -> sqlx::query::QueryScalar<'_, Postgres, Value, sqlx::postgres::PgArguments>,
) -> Result<Value, ApiError> {
    let sql = format!(
        "SELECT coalesce(jsonb_agg(x),'[]'::jsonb) FROM ({select} LIMIT {}) x",
        cap + 1
    );
    let mut items: Value = binds(sqlx::query_scalar(&sql)).fetch_one(&mut **tx).await?;
    let list = items.as_array_mut().expect("jsonb_agg is an array");
    let truncated = list.len() as i64 > cap;
    list.truncate(cap as usize);
    Ok(json!({ "items": items, "truncated": truncated }))
}

fn download(body: Value, name: &str) -> Result<Response, ApiError> {
    let bytes = serde_json::to_vec_pretty(&body)
        .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "export"))?;
    let sha = hex::encode(Sha256::digest(&bytes));
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/json".to_owned(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
            (axum::http::header::CACHE_CONTROL, "no-store".to_owned()),
            (
                axum::http::HeaderName::from_static("x-omg-export-sha256"),
                sha,
            ),
        ],
        bytes,
    )
        .into_response())
}

const PROFILE: &str = "SELECT jsonb_build_object('id',u.id,'email',u.email,'display_name',u.display_name,'created_at',u.created_at,'first_signed_in_at',u.first_signed_in_at,'last_signed_in_at',u.last_signed_in_at,'disabled_at',u.disabled_at,'platform_role',(SELECT role FROM effective_platform_roles r WHERE r.user_id=u.id)) FROM users u WHERE u.id=$1 AND u.cleaned_at IS NULL";
const ROLES: &str = "SELECT g.role,g.source,g.created_at,g.revoked_at FROM platform_role_grants g WHERE g.user_id=$1 ORDER BY g.created_at,g.role";

async fn mine(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(q): Query<Range>,
) -> Result<Response, ApiError> {
    let (from, to) = range(&q)?;
    // At most two exports build at once per replica (`Store::export_slots`):
    // each can hold tens of MB, so a burst can't exhaust memory or the pool.
    let _permit = s
        .export_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| busy())?;
    tokio::time::timeout(DEADLINE, export_mine(&s, &u, from, to))
        .await
        .map_err(|_| busy())?
}

async fn export_mine(
    s: &Store,
    u: &BrowserPrincipal,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Response, ApiError> {
    record(s, u, "subject.exported", u.user_id).await?;
    let mut tx = s.snapshot().await?;
    sqlx::query(STATEMENT_TIMEOUT).execute(&mut *tx).await?;
    let me = u.user_id;
    let profile: Value = sqlx::query_scalar(PROFILE)
        .bind(me)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    let (start, end) = (
        from.and_hms_opt(0, 0, 0).expect("midnight").and_utc(),
        (to + Days::days(1))
            .and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_utc(),
    );
    let roles = capped(&mut tx, ROLES, LIST_CAP, |q| q.bind(me)).await?;
    let memberships = capped(&mut tx, "SELECT w.id AS workspace_id,w.name,w.kind,g.role,g.source,g.created_at,g.revoked_at FROM workspace_membership_grants g JOIN workspaces w ON w.id=g.workspace_id WHERE g.user_id=$1 UNION ALL SELECT w.id,w.name,w.kind,'owner','personal',w.created_at,NULL FROM workspaces w WHERE w.kind='personal' AND w.owner_user_id=$1 ORDER BY 6", LIST_CAP, |q| q.bind(me)).await?;
    let keys = capped(&mut tx, "SELECT k.id,k.name,k.workspace_id,k.created_at,k.expires_at,k.revoked_at,k.disabled_at FROM api_keys k WHERE k.issued_to_user_id=$1 ORDER BY k.created_at,k.id", LIST_CAP, |q| q.bind(me)).await?;
    let sessions = capped(&mut tx, "SELECT s.id,s.client_label AS client,s.created_at,s.last_seen_at,s.expires_at FROM browser_sessions s WHERE s.user_id=$1 AND s.revoked_at IS NULL AND s.expires_at>now() ORDER BY s.created_at", LIST_CAP, |q| q.bind(me)).await?;
    let requests = capped(&mut tx, "SELECT e.id,e.root_request_id,e.attempt_number,e.started_at,e.completed_at,e.workspace_id,e.public_model AS model,e.workload_kind,e.state,e.error_code,e.input_tokens,e.output_tokens,e.client_app,e.client_session_id,(SELECT r.actual_microusd::text FROM governance_reservations r WHERE r.execution_id=e.id AND r.admitted_at=e.started_at) AS cost_microusd FROM inference_executions e JOIN api_keys k ON k.id=e.api_key_id AND k.workspace_id=e.workspace_id WHERE k.issued_to_user_id=$1 AND e.started_at>=$2 AND e.started_at<$3 ORDER BY e.started_at,e.id", REQUEST_CAP, |q| q.bind(me).bind(start).bind(end)).await?;
    let files = capped(&mut tx, "SELECT f.id,f.purpose,f.workspace_id,f.filename,f.content_type,f.size_bytes,f.created_at,f.expires_at,f.deleted_at FROM stored_files f WHERE f.created_by_user_id=$1 ORDER BY f.created_at,f.id", LIST_CAP, |q| q.bind(me)).await?;
    let audit_entries = capped(&mut tx, "SELECT a.id,a.created_at AS occurred_at,a.action,a.resource_type,a.resource_id,a.workspace_id,a.metadata FROM audit_events a WHERE a.actor_user_id=$1 AND a.created_at>=$2 AND a.created_at<$3 ORDER BY a.created_at,a.id", LIST_CAP, |q| q.bind(me).bind(start).bind(end)).await?;
    tx.commit().await?;
    let installation: (Uuid, String) = sqlx::query_as("SELECT id,name FROM installation")
        .fetch_one(&s.pool)
        .await?;
    let body = json!({
        "format": FORMAT,
        "generated_at": Utc::now(),
        "installation": { "id": installation.0, "name": installation.1 },
        "range": { "from": from, "to": to },
        "subject": profile,
        "platform_roles": roles,
        "memberships": memberships,
        "api_keys": keys,
        "sessions": sessions,
        "requests": requests,
        "files": files,
        "audit": audit_entries,
        "notes": [
            "API keys are metadata only; secrets are never stored in readable form.",
            "Files are metadata only; download contents through the Files API while they exist.",
            "Prompts and responses are never stored by the gateway.",
        ],
    });
    download(body, &format!("omg-export-{from}-{to}.json"))
}

/// Platform Admins and Auditors: what admins already see about a person.
async fn platform(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(user): Path<Uuid>,
) -> Result<Response, ApiError> {
    {
        let mut tx = crate::db::begin(&s.pool).await?;
        resources::platform_read(&mut tx, u.user_id()).await?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1)")
            .bind(user)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        if !exists {
            return Err(missing());
        }
    }
    // At most two exports build at once per replica (`Store::export_slots`):
    // each can hold tens of MB, so a burst can't exhaust memory or the pool.
    let _permit = s
        .export_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| busy())?;
    tokio::time::timeout(DEADLINE, export_platform(&s, &u, user))
        .await
        .map_err(|_| busy())?
}

async fn export_platform(s: &Store, u: &impl Actor, user: Uuid) -> Result<Response, ApiError> {
    record(s, u, "user.exported", user).await?;
    let mut tx = s.snapshot().await?;
    sqlx::query(STATEMENT_TIMEOUT).execute(&mut *tx).await?;
    let profile: Value = sqlx::query_scalar("SELECT jsonb_build_object('id',u.id,'email',u.email,'display_name',u.display_name,'created_at',u.created_at,'first_signed_in_at',u.first_signed_in_at,'last_signed_in_at',u.last_signed_in_at,'disabled_at',u.disabled_at,'cleaned_at',u.cleaned_at,'platform_role',(SELECT role FROM effective_platform_roles r WHERE r.user_id=u.id)) FROM users u WHERE u.id=$1")
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
    let roles = capped(&mut tx, ROLES, LIST_CAP, |q| q.bind(user)).await?;
    let memberships = capped(&mut tx, "SELECT w.id AS workspace_id,w.name,w.kind,g.role,g.source,g.created_at,g.revoked_at FROM workspace_membership_grants g JOIN workspaces w ON w.id=g.workspace_id WHERE g.user_id=$1 AND w.kind IN ('team','project') ORDER BY g.created_at", LIST_CAP, |q| q.bind(user)).await?;
    // The platform audit view's scope: no workspace, or a team or project.
    let audit_entries = capped(&mut tx, "SELECT a.id,a.created_at AS occurred_at,a.action,a.resource_type,a.resource_id,a.workspace_id,a.actor_user_id,a.metadata FROM audit_events a LEFT JOIN workspaces w ON w.id=a.workspace_id WHERE (a.actor_user_id=$1 OR (a.resource_type='user' AND a.resource_id=$1)) AND (a.workspace_id IS NULL OR w.kind IN ('team','project')) ORDER BY a.created_at DESC,a.id", LIST_CAP, |q| q.bind(user)).await?;
    tx.commit().await?;
    let body = json!({
        "format": FORMAT,
        "generated_at": Utc::now(),
        "scope": "platform",
        "subject": profile,
        "platform_roles": roles,
        "shared_memberships": memberships,
        "audit": audit_entries,
        "notes": ["What Platform Admins can see: no personal workspace, keys, sessions or requests."],
    });
    download(body, &format!("omg-user-{user}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_bounded_and_end_today_or_earlier() {
        let today = Utc::now().date_naive();
        let q = |from: Option<NaiveDate>, to: Option<NaiveDate>| range(&Range { from, to });
        assert_eq!(q(None, None).unwrap(), (today - Days::days(29), today));
        assert!(
            q(Some(today - Days::days(92)), Some(today)).is_ok(),
            "93 days"
        );
        assert!(
            q(Some(today - Days::days(93)), Some(today)).is_err(),
            "94 days"
        );
        assert!(
            q(Some(today), Some(today - Days::days(1))).is_err(),
            "backwards"
        );
        assert!(q(None, Some(today + Days::days(1))).is_err(), "future");
    }
}

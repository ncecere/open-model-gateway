//! #83: browser-only, independent approval of manual member grants (slice
//! A) and of catalog model selections for a shared workspace (slice B).
//! All writes use the existing management transaction and canonical locks.
use super::*;
use sha2::{Digest, Sha256};

type RequestResult = Result<Json<Value>, ManagementError>;
/// Common fields for every viewer: the requester's own `member`/`admin` and the two approver
/// queues. `workspace_name_at_request` is a frozen submitted reference (UX fix, #83): it is
/// captured once at INSERT and never updated, so it never discloses a later rename or a
/// workspace becoming non-discoverable. It is not the live workspace name.
const PROJECTION: &str = "jsonb_build_object('id',r.id,'requester',r.requester,'target_kind',r.target_kind,'workspace_id',r.workspace_id,'workspace_name_at_request',r.workspace_name_at_request,'model_id',r.model_id,'model_name_at_request',r.model_name_at_request,'requested_role',r.requested_role,'status',r.status,'reason',r.reason,'created_at',r.created_at,'expires_at',r.expires_at,'decided_by',r.decided_by,'decided_at',r.decided_at,'membership_grant_id',(SELECT membership_grant_id FROM access_request_applications a WHERE a.request_id=r.id))";
/// Approver-only: adds the requester's display name/email (requires a `JOIN users ru ON
/// ru.id=r.requester` in the FROM clause). Workspace owners/admins already see this pair for
/// any entitled user through the Members "Add member" picker (`members::candidates`), and
/// Platform Admins/Auditors through `GET /platform/users`; this projection discloses nothing
/// beyond what those existing, equally scoped reads already show the same viewers. It is used
/// only for the workspace- and platform-scoped queues, never for the requester's own `mine`/
/// `detail` reads, so a non-approver never receives another requester's identity.
///
/// The `CASE WHEN` guard below mirrors `members::candidates`' own WHERE clause exactly
/// (disabled, cleaned, no email, or no current platform entitlement): a requester outside
/// that picker's reach gets both fields redacted to `NULL` here too, so this projection never
/// discloses more than that picker actually would for the requester's *current* state, not
/// just their state at submission.
const PROJECTION_WITH_REQUESTER: &str = "jsonb_build_object('id',r.id,'requester',r.requester,'requester_display_name',CASE WHEN ru.disabled_at IS NULL AND ru.cleaned_at IS NULL AND ru.email IS NOT NULL AND EXISTS(SELECT 1 FROM effective_platform_roles er WHERE er.user_id=ru.id) THEN ru.display_name END,'requester_email',CASE WHEN ru.disabled_at IS NULL AND ru.cleaned_at IS NULL AND ru.email IS NOT NULL AND EXISTS(SELECT 1 FROM effective_platform_roles er WHERE er.user_id=ru.id) THEN ru.email END,'target_kind',r.target_kind,'workspace_id',r.workspace_id,'workspace_name_at_request',r.workspace_name_at_request,'model_id',r.model_id,'model_name_at_request',r.model_name_at_request,'requested_role',r.requested_role,'status',r.status,'reason',r.reason,'created_at',r.created_at,'expires_at',r.expires_at,'decided_by',r.decided_by,'decided_at',r.decided_at,'membership_grant_id',(SELECT membership_grant_id FROM access_request_applications a WHERE a.request_id=r.id))";
fn conflict() -> ApiError {
    ApiError(StatusCode::CONFLICT, "Request is no longer applicable")
}

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/access-request-targets", get(targets))
        .route("/api/v1/me/access-requests", get(mine).post(create))
        .route("/api/v1/me/access-requests/{id}", get(detail))
        .route("/api/v1/me/access-requests/{id}/withdraw", post(withdraw))
        .route(
            "/api/v1/workspaces/{ws}/access-requests",
            get(workspace_queue),
        )
        .route(
            "/api/v1/workspaces/{ws}/access-requests/{id}/approve",
            post(workspace_approve),
        )
        .route(
            "/api/v1/workspaces/{ws}/access-requests/{id}/deny",
            post(workspace_deny),
        )
        .route("/api/v1/platform/access-requests", get(platform_queue))
        .route(
            "/api/v1/platform/access-requests/{id}/approve",
            post(platform_approve),
        )
        .route(
            "/api/v1/platform/access-requests/{id}/deny",
            post(platform_deny),
        )
        .route(
            "/api/v1/platform/workspaces/{ws}/discoverable",
            axum::routing::put(set_discoverable),
        )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Submission {
    workspace_id: Uuid,
    reason: Option<String>,
    idempotency_key: String,
    /// #83 slice B: request selection of this model for the workspace. Last
    /// and defaulted, so the three-element positional form still parses.
    #[serde(default)]
    model_id: Option<Uuid>,
}
#[cfg(test)]
#[test]
fn management_openapi_submission_request_vectors() {
    super::openapi::requests::vectors::<Submission>(
        "POST",
        "/api/v1/me/access-requests",
        json!({"workspace_id":Uuid::nil(),"reason":"Join team","idempotency_key":"vector"}),
    );
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Discoverable {
    discoverable: bool,
}

#[cfg(test)]
#[test]
fn management_openapi_slice2_discoverable_request_vectors() {
    super::openapi::requests::vectors::<Discoverable>(
        "PUT",
        "/api/v1/platform/workspaces/{ws}/discoverable",
        json!({"discoverable":true}),
    );
}

async fn scope_locks(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    users: &[Uuid],
) -> Result<(), ApiError> {
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(ws)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)?;
    if !shared(&kind) {
        return Err(missing());
    }
    let kind = locks::WorkspaceType::parse(&kind).ok_or_else(missing)?;
    let mut scopes = vec![locks::Scope::Type(kind), locks::Scope::Workspace(ws)];
    scopes.extend(users.iter().copied().map(locks::Scope::User));
    locks::exclusive(tx, scopes).await?;
    Ok(())
}
async fn eligible_target(tx: &mut Transaction<'_, Postgres>, ws: Uuid) -> Result<(), ApiError> {
    let eligible:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=$1 AND kind IN ('team','project') AND discoverable AND disabled_at IS NULL)")
        .bind(ws).fetch_one(&mut **tx).await?;
    if !eligible {
        return Err(missing());
    }
    Ok(())
}
fn kind_of(model: Option<Uuid>) -> &'static str {
    if model.is_some() {
        "model_selection"
    } else {
        "workspace_membership"
    }
}
async fn effective_member(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    user: Uuid,
) -> Result<bool, ApiError> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM effective_workspace_memberships m JOIN workspaces w ON w.id=m.workspace_id WHERE m.workspace_id=$1 AND m.user_id=$2 AND w.kind IN ('team','project'))")
        .bind(ws)
        .bind(user)
        .fetch_one(&mut **tx)
        .await?)
}
async fn workspace_authority(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Uuid,
) -> Result<(), ApiError> {
    let a = resources::workspace_access_snapshot(tx, u, ws).await?;
    if !shared(&a.kind) {
        return Err(missing());
    }
    resources::manage(&a)
}
async fn one(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> ApiResult {
    let sql = format!("SELECT {PROJECTION} FROM access_requests r WHERE r.id=$1");
    Ok(Json(
        sqlx::query_scalar(&sql)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(missing)?,
    ))
}
/// Expire only this bounded page/requester set; audit and state share a commit.
/// No grant changes, hence no authority scope is acquired after a write.
async fn expire(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ids: &[Uuid],
) -> Result<(), ApiError> {
    let rows:Vec<(Uuid,Uuid,String)>=sqlx::query_as("UPDATE access_requests SET status='expired',decided_by=NULL WHERE id=ANY($1) AND status='pending' AND expires_at<=clock_timestamp() RETURNING id,workspace_id,target_kind")
        .bind(ids).fetch_all(&mut **tx).await?;
    for (id, ws, kind) in rows {
        event(tx, u, ws, id, &kind, "expired").await?;
    }
    Ok(())
}
async fn event(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Uuid,
    id: Uuid,
    kind: &str,
    outcome: &str,
) -> Result<(), ApiError> {
    audit(
        tx,
        u,
        Some(ws),
        &format!("access_request.{outcome}"),
        "access_request",
        Some(id),
        json!({"kind":kind,"outcome":outcome}),
    )
    .await
}
async fn targets(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (l, o) = p.bounds()?;
    let mut tx = resources::read_snapshot(&s).await?;
    resources::platform_role_snapshot(&mut tx, u.user_id).await?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',w.id,'name',w.name,'kind',w.kind) FROM workspaces w WHERE w.discoverable AND w.disabled_at IS NULL AND w.kind IN ('team','project') AND NOT EXISTS(SELECT 1 FROM workspace_membership_grants g WHERE g.workspace_id=w.id AND g.user_id=$1 AND g.revoked_at IS NULL) ORDER BY w.name,w.id LIMIT $2 OFFSET $3")
        .bind(u.user_id).bind(l).bind(o).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn set_discoverable(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(b): Json<Discoverable>,
) -> ApiResult {
    let mut tx = resources::installation_tx(&s).await?;
    scope_locks(&mut tx, ws, &[u.user_id]).await?;
    resources::platform_write(&mut tx, &u).await?;
    sqlx::query("UPDATE workspaces SET discoverable=$2 WHERE id=$1 AND kind IN ('team','project')")
        .bind(ws)
        .bind(b.discoverable)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        Some(ws),
        "workspace.discoverability_set",
        "workspace",
        Some(ws),
        json!({"enabled":b.discoverable}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
fn insertion_error(e: sqlx::Error) -> ManagementError {
    if let Some(db) = e.as_database_error() {
        if db.code().as_deref() == Some("P0083") {
            let seconds = db
                .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
                .and_then(|e| e.detail())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(60);
            return ManagementError::detailed(
                StatusCode::TOO_MANY_REQUESTS,
                "access_request_rate_limited",
                "Too many access requests".into(),
                json!({"retry_after_seconds":seconds}),
            );
        }
        if db.code().as_deref() == Some("P0084") {
            return queue_full();
        }
        if db.code().as_deref() == Some("23505") {
            return conflict().into();
        }
    }
    e.into()
}
/// The workspace's model-request queue (or the requester's share of it) is
/// full until an owner/admin decides or a request expires: not a rate limit.
fn queue_full() -> ManagementError {
    ManagementError::detailed(
        StatusCode::CONFLICT,
        "model_request_queue_full",
        "Too many model requests are waiting for this workspace; ask an owner or admin to review them.".into(),
        json!({}),
    )
}
/// Lock-free prechecks for a model request, repeated under the locks: a
/// refused request never takes the exclusive authority locks.
async fn model_request_precheck(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    user: Uuid,
    model: Uuid,
) -> Result<(), ManagementError> {
    if !effective_member(tx, ws, user).await? {
        return Err(missing().into());
    }
    match catalogs::requestable_model(tx, ws, model).await? {
        None => return Err(missing().into()),
        Some(true) => return Err(conflict().into()),
        Some(false) => {}
    }
    let (mine, all): (i64, i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE requester=$2),count(*) FROM access_requests WHERE workspace_id=$1 AND target_kind='model_selection' AND status='pending' AND expires_at>clock_timestamp()")
        .bind(ws)
        .bind(user)
        .fetch_one(&mut **tx)
        .await?;
    if mine >= 5 || all >= 100 {
        return Err(queue_full());
    }
    Ok(())
}
async fn create(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<Submission>,
) -> RequestResult {
    if !(1..=128).contains(&b.idempotency_key.len())
        || !b.idempotency_key.bytes().all(|b| b.is_ascii_graphic())
        || b.reason
            .as_ref()
            .is_some_and(|s| s.chars().count() > 1000 || s.chars().any(char::is_control))
    {
        return Err(invalid().into());
    }
    let digest = Sha256::digest(b.idempotency_key.as_bytes()).to_vec();
    // Membership payloads keep their original form, so a replay across the
    // upgrade still matches.
    let canonical = match b.model_id {
        None => json!({"workspace_id":b.workspace_id,"reason":b.reason}),
        Some(model) => json!({"workspace_id":b.workspace_id,"model_id":model,"reason":b.reason}),
    };
    let payload = Sha256::digest(serde_json::to_vec(&canonical).map_err(|_| invalid())?).to_vec();
    let mut tx = resources::installation_tx(&s).await?;
    // Replays keep only submitted references even if discovery was later disabled.
    let replay:Option<(Uuid,Vec<u8>)>=sqlx::query_as("SELECT id,payload_digest FROM access_requests WHERE requester=$1 AND idempotency_digest=$2")
        .bind(u.user_id).bind(&digest).fetch_optional(&mut *tx).await?;
    if let Some((id, old)) = replay {
        resources::platform_role_snapshot(&mut tx, u.user_id).await?;
        if old != payload {
            return Err(conflict().into());
        }
        expire(&mut tx, &u, &[id]).await?;
        let result = one(&mut tx, id).await?;
        tx.commit().await?;
        return Ok(result);
    }
    // A model request that would be refused is refused before any
    // exclusive authority lock (everything is rechecked under the locks).
    if let Some(model) = b.model_id {
        model_request_precheck(&mut tx, b.workspace_id, u.user_id, model).await?;
    }
    scope_locks(&mut tx, b.workspace_id, &[u.user_id]).await?;
    resources::platform_role_snapshot(&mut tx, u.user_id).await?;
    if let Some(model) = b.model_id {
        // Only a current member, for a model within the workspace's catalog
        // visibility: anything else is indistinguishable from unknown.
        if !effective_member(&mut tx, b.workspace_id, u.user_id).await? {
            return Err(missing().into());
        }
        match catalogs::requestable_model(&mut tx, b.workspace_id, model).await? {
            None => return Err(missing().into()),
            Some(true) => return Err(conflict().into()),
            Some(false) => {}
        }
    } else {
        eligible_target(&mut tx, b.workspace_id).await?;
        let member:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND revoked_at IS NULL)")
            .bind(b.workspace_id).bind(u.user_id).fetch_one(&mut *tx).await?;
        if member {
            return Err(conflict().into());
        }
    }
    let due:Vec<Uuid>=sqlx::query_scalar("SELECT id FROM access_requests WHERE requester=$1 AND status='pending' AND expires_at<=clock_timestamp() ORDER BY id LIMIT 20")
        .bind(u.user_id).fetch_all(&mut *tx).await?;
    expire(&mut tx, &u, &due).await?;
    let id:Uuid=sqlx::query_scalar("INSERT INTO access_requests(requester,workspace_id,model_id,reason,idempotency_digest,payload_digest) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(u.user_id).bind(b.workspace_id).bind(b.model_id).bind(b.reason).bind(digest).bind(payload).fetch_one(&mut *tx).await.map_err(insertion_error)?;
    event(
        &mut tx,
        &u,
        b.workspace_id,
        id,
        kind_of(b.model_id),
        "created",
    )
    .await?;
    let result = one(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MineQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    /// #83 slice B: only this workspace's requests.
    workspace_id: Option<Uuid>,
    /// Only `pending` (unexpired): a read-only snapshot that expires nothing,
    /// for the Models page's Requested state.
    status: Option<String>,
}
async fn mine(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(q): Query<MineQuery>,
) -> ApiResult {
    let p = Page {
        limit: q.limit,
        offset: q.offset,
    };
    match q.status.as_deref() {
        None => queue(&s, &u, None, false, p, q.workspace_id).await,
        Some("pending") => {
            let (l, o) = p.bounds()?;
            let mut tx = resources::read_snapshot(&s).await?;
            resources::platform_role_snapshot(&mut tx, u.user_id).await?;
            let data:Vec<Value>=sqlx::query_scalar(&format!("SELECT {PROJECTION} FROM access_requests r WHERE r.requester=$1 AND ($2::uuid IS NULL OR r.workspace_id=$2) AND r.status='pending' AND r.expires_at>clock_timestamp() ORDER BY r.created_at DESC,r.id LIMIT $3 OFFSET $4"))
                .bind(u.user_id).bind(q.workspace_id).bind(l).bind(o).fetch_all(&mut *tx).await?;
            tx.commit().await?;
            Ok(Json(json!({"data":data})))
        }
        _ => Err(invalid()),
    }
}
async fn workspace_queue(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<Page>,
) -> ApiResult {
    queue(&s, &u, Some(ws), false, p, None).await
}
async fn platform_queue(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<Page>,
) -> ApiResult {
    queue(&s, &u, None, true, p, None).await
}
async fn queue(
    s: &Store,
    u: &BrowserPrincipal,
    ws: Option<Uuid>,
    platform: bool,
    p: Page,
    // The requester's own history, narrowed to one workspace.
    mine_ws: Option<Uuid>,
) -> ApiResult {
    let (l, o) = p.bounds()?;
    let mut tx = resources::installation_tx(s).await?;
    if platform {
        resources::platform_read(&mut tx, u).await?;
    } else if let Some(ws) = ws {
        workspace_authority(&mut tx, u, ws).await?;
    } else {
        resources::platform_role_snapshot(&mut tx, u.user_id).await?;
    }
    // Always bind $3 (even the platform path) so placeholder arity is stable.
    let (filter, scope) = if platform {
        ("($3::uuid IS NULL) AND ($4::uuid IS NULL)", None)
    } else if let Some(ws) = ws {
        ("r.workspace_id=$3 AND ($4::uuid IS NULL)", Some(ws))
    } else {
        (
            "r.requester=$3 AND ($4::uuid IS NULL OR r.workspace_id=$4)",
            Some(u.user_id),
        )
    };
    // $4 (the requester's own workspace filter) is always bound, null elsewhere.
    let ids:Vec<Uuid>=sqlx::query_scalar(&format!("SELECT r.id FROM access_requests r WHERE {filter} ORDER BY r.created_at DESC,r.id LIMIT $1 OFFSET $2"))
        .bind(l).bind(o).bind(scope).bind(mine_ws).fetch_all(&mut *tx).await?;
    expire(&mut tx, u, &ids).await?;
    // Only the approver-scoped workspace/platform queues join the requester's identity; the
    // requester's own `mine` queue never needs or receives it (see PROJECTION_WITH_REQUESTER).
    let (projection, join) = if platform || ws.is_some() {
        (
            PROJECTION_WITH_REQUESTER,
            "JOIN users ru ON ru.id=r.requester ",
        )
    } else {
        (PROJECTION, "")
    };
    let data:Vec<Value>=sqlx::query_scalar(&format!("SELECT {projection} FROM access_requests r {join}WHERE r.id=ANY($1) ORDER BY r.created_at DESC,r.id"))
        .bind(&ids).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn detail(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_role_snapshot(&mut tx, u.user_id).await?;
    owned(&mut tx, u.user_id, id).await?;
    expire(&mut tx, &u, &[id]).await?;
    let result = one(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}
async fn owned(tx: &mut Transaction<'_, Postgres>, user: Uuid, id: Uuid) -> Result<Uuid, ApiError> {
    sqlx::query_scalar("SELECT workspace_id FROM access_requests WHERE id=$1 AND requester=$2")
        .bind(id)
        .bind(user)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)
}
async fn withdraw(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_role_snapshot(&mut tx, u.user_id).await?;
    let ws = owned(&mut tx, u.user_id, id).await?;
    expire(&mut tx, &u, &[id]).await?;
    let (status, kind): (String, String) =
        sqlx::query_as("SELECT status,target_kind FROM access_requests WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if status != "pending" && status != "withdrawn" {
        tx.commit().await?;
        return Err(conflict());
    }
    if status == "pending" {
        sqlx::query("UPDATE access_requests SET status='withdrawn',decided_by=$2 WHERE id=$1")
            .bind(id)
            .bind(u.user_id)
            .execute(&mut *tx)
            .await?;
        event(&mut tx, &u, ws, id, &kind, "withdrawn").await?;
    }
    let result = one(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}
async fn workspace_approve(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    decision(&s, &u, Some(ws), id, true).await
}
async fn workspace_deny(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    decision(&s, &u, Some(ws), id, false).await
}
async fn platform_approve(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    decision(&s, &u, None, id, true).await
}
async fn platform_deny(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    decision(&s, &u, None, id, false).await
}
async fn decision(
    s: &Store,
    u: &BrowserPrincipal,
    path_ws: Option<Uuid>,
    id: Uuid,
    approve: bool,
) -> ApiResult {
    let mut tx = resources::installation_tx(s).await?;
    type SubmissionScope = (Uuid, Uuid, chrono::DateTime<chrono::Utc>, Option<Uuid>);
    let (ws, requester, submitted_at, model): SubmissionScope = sqlx::query_as(
        "SELECT workspace_id,requester,created_at,model_id FROM access_requests WHERE id=$1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    if path_ws.is_some_and(|w| w != ws) {
        return Err(missing());
    }
    scope_locks(&mut tx, ws, &[u.user_id, requester]).await?;
    if path_ws.is_some() {
        workspace_authority(&mut tx, u, ws).await?;
    } else {
        resources::platform_write(&mut tx, u).await?;
        // A workspace's model selection needs its own owner/admin: platform
        // authority never substitutes for workspace membership.
        if model.is_some() {
            return Err(denied());
        }
    }
    if requester == u.user_id {
        return Err(denied());
    }
    expire(&mut tx, u, &[id]).await?;
    let status: String =
        sqlx::query_scalar("SELECT status FROM access_requests WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let outcome = if approve { "approved" } else { "denied" };
    if status == outcome {
        let result = one(&mut tx, id).await?;
        tx.commit().await?;
        return Ok(result);
    }
    if status != "pending" {
        tx.commit().await?;
        return Err(conflict());
    }
    if let (true, Some(model)) = (approve, model) {
        // The requester must still be entitled and a member; the model still
        // eligible and not already usable. Otherwise the request stays pending.
        let ineligible = || ApiError(StatusCode::CONFLICT, "Requester is no longer eligible");
        match resources::platform_role_snapshot(&mut tx, requester).await {
            Ok(_) => {}
            Err(e) if e.0 == StatusCode::FORBIDDEN => return Err(ineligible()),
            Err(e) => return Err(e),
        }
        if !effective_member(&mut tx, ws, requester).await? {
            return Err(ineligible());
        }
        catalogs::apply_model_request(&mut tx, u, ws, model).await?;
        sqlx::query("INSERT INTO access_request_applications(request_id,workspace_id,requester,model_id) VALUES($1,$2,$3,$4)")
            .bind(id).bind(ws).bind(requester).bind(model).execute(&mut *tx).await?;
        event(&mut tx, u, ws, id, kind_of(Some(model)), "applied").await?;
    } else if approve {
        if let Err(e) = eligible_target(&mut tx, ws).await {
            return Err(if e.0 == StatusCode::NOT_FOUND {
                conflict()
            } else {
                e
            });
        }
        let grant = members::apply_membership_request(&mut tx, ws, requester, submitted_at).await?;
        sqlx::query("INSERT INTO access_request_applications(request_id,workspace_id,requester,membership_grant_id) VALUES($1,$2,$3,$4)")
            .bind(id).bind(ws).bind(requester).bind(grant).execute(&mut *tx).await?;
        event(&mut tx, u, ws, id, kind_of(None), "applied").await?;
    }
    sqlx::query("UPDATE access_requests SET status=$2,decided_by=$3 WHERE id=$1")
        .bind(id)
        .bind(outcome)
        .bind(u.user_id)
        .execute(&mut *tx)
        .await?;
    event(&mut tx, u, ws, id, kind_of(model), outcome).await?;
    let result = one(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}

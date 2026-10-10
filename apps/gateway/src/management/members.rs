use super::*;
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};

const MEMBERS_SQL: &str = "SELECT jsonb_build_object('user_id',g.user_id,'email',u.email,'display_name',u.display_name,'role',m.role,'disabled_at',u.disabled_at,'membership_source',CASE WHEN count(DISTINCT g.source)>1 THEN 'mixed' ELSE min(g.source) END,'grants',jsonb_agg(jsonb_build_object('id',g.id,'role',g.role,'source',g.source,'mapping_id',g.mapping_id) ORDER BY g.source,g.id)) FROM workspace_membership_grants g JOIN users u ON u.id=g.user_id LEFT JOIN effective_workspace_memberships m ON m.workspace_id=g.workspace_id AND m.user_id=g.user_id WHERE g.workspace_id=$1 AND g.revoked_at IS NULL GROUP BY g.user_id,u.email,u.display_name,u.disabled_at,m.role ORDER BY u.email,g.user_id LIMIT $2 OFFSET $3";
pub(super) async fn workspace_members(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    if !shared(&a.kind) {
        return Err(invalid());
    }
    if !a.admin {
        resources::platform_read(&mut tx, u.user_id).await?
    }
    let (l, o) = p.bounds()?;
    let data: Vec<Value> = sqlx::query_scalar(MEMBERS_SQL)
        .bind(ws)
        .bind(l)
        .bind(o)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
/// Locks an active shared workspace for platform-level membership administration.
async fn platform_shared(tx: &mut Transaction<'_, Postgres>, ws: Uuid) -> Result<(), ApiError> {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE id=$1 AND kind IN ('team','project') AND disabled_at IS NULL FOR NO KEY UPDATE")
        .bind(ws)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)?;
    Ok(())
}
/// Admin › Teams/Projects: membership rows for Platform Admins/Auditors.
pub(super) async fn platform_members(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<Page>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    // Read-only: platform readers may list members of a disabled Team/Project too.
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM workspaces WHERE id=$1 AND kind IN ('team','project')",
    )
    .bind(ws)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    let (l, o) = p.bounds()?;
    let data: Vec<Value> = sqlx::query_scalar(MEMBERS_SQL)
        .bind(ws)
        .bind(l)
        .bind(o)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
/// Platform Admin sets a manual grant (including owners) in a shared workspace.
pub(super) async fn platform_add_member(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(b): Json<MemberInput>,
) -> ApiResult {
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_write(&mut tx, u.user_id).await?;
    platform_shared(&mut tx, ws).await?;
    set_member(&mut tx, &u, ws, &b).await?;
    tx.commit().await?;
    Ok(ok())
}
pub(super) async fn platform_remove_member(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, user)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_write(&mut tx, u.user_id).await?;
    platform_shared(&mut tx, ws).await?;
    unset_member(&mut tx, &u, ws, user).await?;
    tx.commit().await?;
    Ok(ok())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CandidateQuery {
    q: String,
}
/// Member picker: entitled active users matching an email fragment who hold
/// no grant in this shared workspace. Owner/admin only; email and id only.
pub(super) async fn member_candidates(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<CandidateQuery>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    if !shared(&a.kind) {
        return Err(invalid());
    }
    resources::manage(&a)?;
    let q = p.q.trim().to_lowercase();
    if q.is_empty() || q.chars().count() > 200 || q.chars().any(char::is_control) {
        return Err(invalid());
    }
    let data: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('user_id',u.id,'email',u.email,'display_name',u.display_name) FROM users u JOIN effective_platform_roles r ON r.user_id=u.id WHERE u.disabled_at IS NULL AND u.cleaned_at IS NULL AND u.email IS NOT NULL AND strpos(lower(u.email),$2)>0 AND NOT EXISTS(SELECT 1 FROM workspace_membership_grants g WHERE g.workspace_id=$1 AND g.user_id=u.id AND g.revoked_at IS NULL) ORDER BY strpos(lower(u.email),$2)<>1,lower(u.email),u.id LIMIT 20")
        .bind(ws)
        .bind(&q)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MemberInput {
    user_id: Uuid,
    role: String,
}

async fn protect_owner(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    user: Uuid,
) -> Result<(), ApiError> {
    let others:i64=sqlx::query_scalar("SELECT count(*) FROM effective_workspace_memberships WHERE workspace_id=$1 AND role='owner' AND user_id<>$2").bind(ws).bind(user).fetch_one(&mut **tx).await?;
    if others == 0 {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Cannot remove the last workspace owner",
        ));
    }
    Ok(())
}
async fn manual_member(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    user: Uuid,
    role: &str,
) -> Result<(), ApiError> {
    // Replacing the manual grant revokes the old one: the member's
    // admissions finish first (scoped lock order). Called before any write.
    locks::exclusive(tx, [locks::Scope::User(user)]).await?;
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2 AND source='manual' AND revoked_at IS NULL").bind(ws).bind(user).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO workspace_membership_grants(id,workspace_id,user_id,role,source) VALUES($1,$2,$3,$4,'manual')").bind(Uuid::new_v4()).bind(ws).bind(user).bind(role).execute(&mut **tx).await?;
    Ok(())
}
pub(super) async fn add_workspace_member(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(b): Json<MemberInput>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::manage(&a)?;
    if !shared(&a.kind) {
        return Err(invalid());
    }
    let old: Option<String> = sqlx::query_scalar("SELECT role FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND source='manual' AND revoked_at IS NULL").bind(ws).bind(b.user_id).fetch_optional(&mut *tx).await?;
    // Owner changes in Workspace mode require actual ownership; Platform Admins
    // use the platform membership routes.
    if (old.as_deref() == Some("owner") || b.role == "owner") && !a.owner {
        return Err(denied());
    }
    set_member(&mut tx, &u, ws, &b).await?;
    tx.commit().await?;
    Ok(ok())
}
async fn set_member(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Uuid,
    b: &MemberInput,
) -> Result<(), ApiError> {
    if !valid_role(&b.role) {
        return Err(invalid());
    }
    resources::platform_role(tx, b.user_id).await?;
    let old:Option<String>=sqlx::query_scalar("SELECT role FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND source='manual' AND revoked_at IS NULL").bind(ws).bind(b.user_id).fetch_optional(&mut **tx).await?;
    if old.as_deref() == Some("owner") && b.role != "owner" {
        protect_owner(tx, ws, b.user_id).await?;
    }
    manual_member(tx, ws, b.user_id, &b.role).await?;
    audit(
        tx,
        u,
        Some(ws),
        "workspace.manual_membership_set",
        "user",
        Some(b.user_id),
        json!({"role":b.role,"source":"manual"}),
    )
    .await
}
pub(super) async fn remove_workspace_member(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, user)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::manage(&a)?;
    if !shared(&a.kind) {
        return Err(invalid());
    }
    let old:Option<String>=sqlx::query_scalar("SELECT role FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND source='manual' AND revoked_at IS NULL").bind(ws).bind(user).fetch_optional(&mut *tx).await?;
    if old.as_deref() == Some("owner") && !a.owner {
        return Err(denied());
    }
    unset_member(&mut tx, &u, ws, user).await?;
    tx.commit().await?;
    Ok(ok())
}
async fn unset_member(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Uuid,
    user: Uuid,
) -> Result<(), ApiError> {
    // The member, then the lineages of their keys here (revoked below once
    // no membership remains), before any write.
    let mut scopes = locks::key_lineages(tx, "issued_to_user_id=$2", Some(ws), Some(user)).await?;
    scopes.push(locks::Scope::User(user));
    locks::exclusive(tx, scopes).await?;
    let old:Option<String>=sqlx::query_scalar("SELECT role FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND source='manual' AND revoked_at IS NULL").bind(ws).bind(user).fetch_optional(&mut **tx).await?;
    if old.as_deref() == Some("owner") {
        protect_owner(tx, ws, user).await?;
    }
    // Group-managed entries are intentionally untouched, including when no manual entry exists.
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2 AND source='manual' AND revoked_at IS NULL").bind(ws).bind(user).execute(&mut **tx).await?;
    sqlx::query("UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE workspace_id=$1 AND issued_to_user_id=$2 AND NOT EXISTS(SELECT 1 FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2)").bind(ws).bind(user).execute(&mut **tx).await?;
    audit(
        tx,
        u,
        Some(ws),
        "workspace.manual_membership_revoked",
        "user",
        Some(user),
        json!({"source":"manual"}),
    )
    .await
}
pub(super) async fn invitations(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::manage(&a)?;
    if !shared(&a.kind) {
        return Err(invalid());
    }
    let (l, o) = p.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'email',email,'workspace_id',workspace_id,'role',role,'expires_at',expires_at,'accepted_at',accepted_at,'revoked_at',revoked_at) FROM workspace_invitations WHERE workspace_id=$1 ORDER BY created_at DESC,id LIMIT $2 OFFSET $3").bind(ws).bind(l).bind(o).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InviteInput {
    email: String,
    role: String,
}
pub(super) async fn invite(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(b): Json<InviteInput>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::manage(&a)?;
    let email = b.email.trim().to_lowercase();
    if !shared(&a.kind)
        || !directory::email_valid(&email)
        || !matches!(b.role.as_str(), "admin" | "member")
    {
        return Err(invalid());
    }
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    let hash = Sha256::digest(token.as_bytes());
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO workspace_invitations(id,workspace_id,email,role,token_hash,expires_at,created_by) VALUES($1,$2,$3,$4,$5,now()+interval '72 hours',$6)").bind(id).bind(ws).bind(&email).bind(&b.role).bind(hash.as_slice()).bind(u.user_id).execute(&mut *tx).await?;
    // Admin > Settings > Email, when configured; the one-time code is still returned for copying.
    let mail = super::settings::invitation_mail(&mut tx, ws).await?;
    audit(
        &mut tx,
        &u,
        Some(ws),
        "invitation.created",
        "invitation",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    // Sent after commit, never while holding the installation lock.
    let delivery = super::settings::send_invitation(mail, &email, &b.role, &token).await;
    Ok(Json(
        json!({"id":id,"token":token,"email_delivery":delivery}),
    ))
}
pub(super) async fn revoke_invite(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::manage(&a)?;
    if !shared(&a.kind) {
        return Err(invalid());
    }
    if sqlx::query("UPDATE workspace_invitations SET revoked_at=coalesce(revoked_at,now()) WHERE workspace_id=$1 AND id=$2").bind(ws).bind(id).execute(&mut *tx).await?.rows_affected()!=1{return Err(missing())}
    audit(
        &mut tx,
        &u,
        Some(ws),
        "invitation.revoked",
        "invitation",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AcceptInput {
    token: String,
}
pub(super) async fn accept_invite(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<AcceptInput>,
) -> ApiResult {
    if b.token.len() != 64 || !b.token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_role(&mut tx, u.user_id).await?;
    // require_session supplies the signature-verified email persisted at OIDC callback.
    // Mutable profile email is not invitation evidence, even after a fresh sign-in.
    let hash = Sha256::digest(b.token.as_bytes());
    let(id,ws,role,inviter):(Uuid,Uuid,String,Uuid)=sqlx::query_as("SELECT id,workspace_id,role,created_by FROM workspace_invitations WHERE token_hash=$1 AND lower(email)=lower($2) AND revoked_at IS NULL AND accepted_at IS NULL AND expires_at>now() FOR UPDATE").bind(hash.as_slice()).bind(&u.email).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    let actor = BrowserPrincipal {
        user_id: inviter,
        email: String::new(),
        platform_admin: false,
        platform_auditor: false,
    };
    let a = resources::workspace_access(&mut tx, &actor, ws).await?;
    resources::manage(&a)?;
    if !shared(&a.kind) {
        return Err(denied());
    }
    // Do not downgrade an existing manual owner/admin when accepting a lower-role invite.
    let existing:Option<String>=sqlx::query_scalar("SELECT role FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND source='manual' AND revoked_at IS NULL").bind(ws).bind(u.user_id).fetch_optional(&mut *tx).await?;
    let resulting = if existing.as_deref() == Some("owner") {
        "owner"
    } else if existing.as_deref() == Some("admin") {
        "admin"
    } else {
        &role
    };
    manual_member(&mut tx, ws, u.user_id, resulting).await?;
    sqlx::query("UPDATE workspace_invitations SET accepted_at=now() WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        Some(ws),
        "invitation.accepted",
        "invitation",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"workspace_id":ws})))
}

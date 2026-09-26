use super::*;
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};

pub(super) async fn org_members(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(org): Path<Uuid>,
    Query(page): Query<Page>,
) -> ApiResult {
    require_org(&s, &u, org, true).await?;
    let (limit, offset) = page.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('user_id',m.user_id,'email',u.email,'role',m.role,'disabled_at',m.disabled_at) FROM organization_memberships m JOIN users u ON u.id=m.user_id WHERE m.organization_id=$1 ORDER BY u.email LIMIT $2 OFFSET $3").bind(org).bind(limit).bind(offset).fetch_all(&s.pool).await?;
    Ok(Json(json!({"data":data})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MemberUpdate {
    role: String,
    disabled: bool,
}
pub(super) async fn update_org_member(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((org, user)): Path<(Uuid, Uuid)>,
    Json(b): Json<MemberUpdate>,
) -> ApiResult {
    require_org(&s, &u, org, true).await?;
    if !valid_role(&b.role) {
        return Err(invalid());
    }
    let mut tx = s.pool.begin().await?;
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(org)
        .execute(&mut *tx)
        .await?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND disabled_at IS NULL FOR SHARE",
    )
    .bind(u.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(denied)?;
    let actor_role = locked_org_role(&mut tx, &u, org).await?;
    if !admin(&actor_role) {
        return Err(denied());
    }
    let old: String = sqlx::query_scalar(
        "SELECT role FROM organization_memberships WHERE organization_id=$1 AND user_id=$2",
    )
    .bind(org)
    .bind(user)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    if (old == "owner" || b.role == "owner") && !owner(&actor_role) {
        return Err(denied());
    }
    if old == "owner" && (b.disabled || b.role != "owner") {
        let others:i64=sqlx::query_scalar("SELECT count(*) FROM organization_memberships m JOIN users u ON u.id=m.user_id AND u.disabled_at IS NULL WHERE m.organization_id=$1 AND m.role='owner' AND m.disabled_at IS NULL AND m.user_id<>$2").bind(org).bind(user).fetch_one(&mut *tx).await?;
        if others == 0 {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "Cannot remove the last organization owner",
            ));
        }
    }
    sqlx::query("UPDATE organization_memberships SET role=$3,disabled_at=CASE WHEN $4 THEN now() ELSE NULL END WHERE organization_id=$1 AND user_id=$2").bind(org).bind(user).bind(b.role).bind(b.disabled).execute(&mut *tx).await?;
    if b.disabled {
        sqlx::query("UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE organization_id=$1 AND issued_to_user_id=$2").bind(org).bind(user).execute(&mut *tx).await?;
    }
    audit(
        &mut tx,
        u.user_id,
        Some(org),
        None,
        "organization.member_updated",
        Some(user),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
pub(super) async fn workspace_members(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(page): Query<Page>,
) -> ApiResult {
    let a = require_workspace(&s, &u, ws, true).await?;
    if !shared(&a.kind) {
        return Err(invalid());
    }
    let (limit, offset) = page.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('user_id',m.user_id,'email',u.email,'role',m.role,'disabled_at',m.disabled_at) FROM workspace_memberships m JOIN users u ON u.id=m.user_id WHERE m.organization_id=$1 AND m.workspace_id=$2 ORDER BY u.email LIMIT $3 OFFSET $4").bind(a.org).bind(ws).bind(limit).bind(offset).fetch_all(&s.pool).await?;
    Ok(Json(json!({"data":data})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MemberInput {
    user_id: Uuid,
    role: String,
}
async fn locked_workspace_role(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    org: Uuid,
    ws: Uuid,
) -> Result<String, ApiError> {
    let access = locked_workspace_access(tx, u, org, ws, true).await?;
    if !shared(&access.kind) {
        return Err(invalid());
    }
    Ok(access.role)
}
pub(super) async fn add_workspace_member(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(b): Json<MemberInput>,
) -> ApiResult {
    let a = require_workspace(&s, &u, ws, true).await?;
    if !shared(&a.kind) || !valid_role(&b.role) {
        return Err(invalid());
    }
    let mut tx = s.pool.begin().await?;
    let actor_role = locked_workspace_role(&mut tx, &u, a.org, ws).await?;
    if !admin(&actor_role) || (b.role == "owner" && !owner(&actor_role)) {
        return Err(denied());
    }
    let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_memberships m JOIN users u ON u.id=m.user_id AND u.disabled_at IS NULL WHERE m.organization_id=$1 AND m.user_id=$2 AND m.disabled_at IS NULL)").bind(a.org).bind(b.user_id).fetch_one(&mut *tx).await?;
    if !active {
        return Err(missing());
    }
    sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,$4)").bind(a.org).bind(ws).bind(b.user_id).bind(b.role).execute(&mut *tx).await?;
    audit(
        &mut tx,
        u.user_id,
        Some(a.org),
        Some(ws),
        "workspace.member_added",
        Some(b.user_id),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
pub(super) async fn update_workspace_member(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, user)): Path<(Uuid, Uuid)>,
    Json(b): Json<MemberUpdate>,
) -> ApiResult {
    let a = require_workspace(&s, &u, ws, true).await?;
    if !shared(&a.kind) || !valid_role(&b.role) {
        return Err(invalid());
    }
    let mut tx = s.pool.begin().await?;
    let actor_role = locked_workspace_role(&mut tx, &u, a.org, ws).await?;
    if !admin(&actor_role) {
        return Err(denied());
    }
    let old:String=sqlx::query_scalar("SELECT role FROM workspace_memberships WHERE organization_id=$1 AND workspace_id=$2 AND user_id=$3").bind(a.org).bind(ws).bind(user).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    if (old == "owner" || b.role == "owner") && !owner(&actor_role) {
        return Err(denied());
    }
    if old == "owner" && (b.disabled || b.role != "owner") {
        let others:i64=sqlx::query_scalar("SELECT count(*) FROM workspace_memberships m JOIN organization_memberships om ON om.organization_id=m.organization_id AND om.user_id=m.user_id AND om.disabled_at IS NULL JOIN users u ON u.id=m.user_id AND u.disabled_at IS NULL WHERE m.organization_id=$1 AND m.workspace_id=$2 AND m.role='owner' AND m.disabled_at IS NULL AND m.user_id<>$3").bind(a.org).bind(ws).bind(user).fetch_one(&mut *tx).await?;
        if others == 0 {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "Cannot remove the last workspace owner",
            ));
        }
    }
    sqlx::query("UPDATE workspace_memberships SET role=$4,disabled_at=CASE WHEN $5 THEN now() ELSE NULL END WHERE organization_id=$1 AND workspace_id=$2 AND user_id=$3").bind(a.org).bind(ws).bind(user).bind(b.role).bind(b.disabled).execute(&mut *tx).await?;
    if b.disabled {
        sqlx::query("UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE organization_id=$1 AND workspace_id=$2 AND issued_to_user_id=$3").bind(a.org).bind(ws).bind(user).execute(&mut *tx).await?;
    }
    audit(
        &mut tx,
        u.user_id,
        Some(a.org),
        Some(ws),
        "workspace.member_updated",
        Some(user),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}

pub(super) async fn invitations(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(org): Path<Uuid>,
    Query(page): Query<Page>,
) -> ApiResult {
    require_org(&s, &u, org, true).await?;
    let (limit, offset) = page.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'email',email,'workspace_id',workspace_id,'organization_role',organization_role,'workspace_role',workspace_role,'expires_at',expires_at,'accepted_at',accepted_at,'revoked_at',revoked_at) FROM invitations WHERE organization_id=$1 ORDER BY created_at DESC,id LIMIT $2 OFFSET $3").bind(org).bind(limit).bind(offset).fetch_all(&s.pool).await?;
    Ok(Json(json!({"data":data})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InviteInput {
    email: String,
    organization_role: String,
    workspace_id: Option<Uuid>,
    workspace_role: String,
}
pub(super) async fn invite(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(org): Path<Uuid>,
    Json(b): Json<InviteInput>,
) -> ApiResult {
    require_org(&s, &u, org, true).await?;
    let email = b.email.trim().to_lowercase();
    if email.len() > 320
        || !email.contains('@')
        || email.chars().any(|c| c.is_whitespace() || c.is_control())
        || !matches!(b.organization_role.as_str(), "admin" | "member")
        || !matches!(b.workspace_role.as_str(), "admin" | "member")
    {
        return Err(invalid());
    }
    if let Some(ws) = b.workspace_id {
        let a = require_workspace(&s, &u, ws, true).await?;
        if a.org != org || !shared(&a.kind) {
            return Err(denied());
        }
    }
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    let hash = Sha256::digest(token.as_bytes());
    let id = Uuid::new_v4();
    let mut tx = s.pool.begin().await?;
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(org)
        .execute(&mut *tx)
        .await?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND disabled_at IS NULL FOR SHARE",
    )
    .bind(u.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(denied)?;
    if !admin(&locked_org_role(&mut tx, &u, org).await?) {
        return Err(denied());
    }
    if let Some(ws) = b.workspace_id {
        locked_workspace_role(&mut tx, &u, org, ws).await?;
    }
    sqlx::query("INSERT INTO invitations(id,organization_id,workspace_id,email,organization_role,workspace_role,token_hash,invited_by,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,now()+interval '72 hours')").bind(id).bind(org).bind(b.workspace_id).bind(email).bind(b.organization_role).bind(b.workspace_role).bind(hash.as_slice()).bind(u.user_id).execute(&mut *tx).await?;
    audit(
        &mut tx,
        u.user_id,
        Some(org),
        b.workspace_id,
        "invitation.created",
        Some(id),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"id":id,"token":token})))
}
pub(super) async fn revoke_invite(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    require_org(&s, &u, org, true).await?;
    let mut tx = s.pool.begin().await?;
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(org)
        .execute(&mut *tx)
        .await?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND disabled_at IS NULL FOR SHARE",
    )
    .bind(u.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(denied)?;
    if !admin(&locked_org_role(&mut tx, &u, org).await?) {
        return Err(denied());
    }
    let count=sqlx::query("UPDATE invitations SET revoked_at=coalesce(revoked_at,now()) WHERE organization_id=$1 AND id=$2").bind(org).bind(id).execute(&mut *tx).await?.rows_affected();
    if count != 1 {
        return Err(missing());
    }
    audit(
        &mut tx,
        u.user_id,
        Some(org),
        None,
        "invitation.revoked",
        Some(id),
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
    if b.token.len() != 64 || !b.token.bytes().all(|v| v.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let hash = Sha256::digest(b.token.as_bytes());
    // Discover only the lock root, then lock catalog/org before the invitation.
    // All invitation mutations use the same order (including revocation).
    let org: Uuid =
        sqlx::query_scalar("SELECT organization_id FROM invitations WHERE token_hash=$1")
            .bind(hash.as_slice())
            .fetch_optional(&s.pool)
            .await?
            .ok_or_else(missing)?;
    let mut tx = s.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
        .execute(&mut *tx)
        .await?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM organizations WHERE id=$1 AND disabled_at IS NULL FOR NO KEY UPDATE",
    )
    .bind(org)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(denied)?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND disabled_at IS NULL FOR SHARE",
    )
    .bind(u.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(denied)?;
    let row:Option<(Uuid,Uuid,Option<Uuid>,String,String,Uuid)>=sqlx::query_as("SELECT id,organization_id,workspace_id,organization_role,workspace_role,invited_by FROM invitations WHERE token_hash=$1 AND lower(email)=lower($2) AND revoked_at IS NULL AND accepted_at IS NULL AND expires_at>now() FOR UPDATE")
        .bind(hash.as_slice()).bind(&u.email).fetch_optional(&mut *tx).await?;
    let (id, org, ws, org_role, ws_role, inviter) = row.ok_or_else(missing)?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND disabled_at IS NULL FOR SHARE",
    )
    .bind(inviter)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(denied)?;
    // Invitation authority is checked again at acceptance, not only at issuance.
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organizations o JOIN users u ON u.id=$2 AND u.disabled_at IS NULL LEFT JOIN organization_memberships m ON m.organization_id=o.id AND m.user_id=u.id AND m.disabled_at IS NULL WHERE o.id=$1 AND o.disabled_at IS NULL AND (u.platform_admin OR m.role IN ('owner','admin')))").bind(org).bind(inviter).fetch_one(&mut *tx).await?;
    if !valid {
        return Err(denied());
    }
    sqlx::query("INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(org).bind(u.user_id).bind(org_role).execute(&mut *tx).await?;
    let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_memberships WHERE organization_id=$1 AND user_id=$2 AND disabled_at IS NULL)").bind(org).bind(u.user_id).fetch_one(&mut *tx).await?;
    if !active {
        return Err(denied());
    }
    ensure_personal(&mut tx, org, u.user_id).await?;
    if let Some(ws) = ws {
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspaces WHERE organization_id=$1 AND id=$2 AND kind IN ('team','project') AND disabled_at IS NULL)").bind(org).bind(ws).fetch_one(&mut *tx).await?;
        if !active {
            return Err(denied());
        }
        sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING").bind(org).bind(ws).bind(u.user_id).bind(ws_role).execute(&mut *tx).await?;
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_memberships WHERE organization_id=$1 AND workspace_id=$2 AND user_id=$3 AND disabled_at IS NULL)").bind(org).bind(ws).bind(u.user_id).fetch_one(&mut *tx).await?;
        if !active {
            return Err(denied());
        }
    }
    sqlx::query("UPDATE invitations SET accepted_at=now() WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        u.user_id,
        Some(org),
        ws,
        "invitation.accepted",
        Some(id),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"organization_id":org})))
}

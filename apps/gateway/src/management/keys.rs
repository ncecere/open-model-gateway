use super::*;
use crate::auth::NewApiKey;

pub(super) async fn keys(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(page): Query<Page>,
) -> ApiResult {
    let a = require_workspace(&s, &u, ws, false).await?;
    let (limit, offset) = page.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'name',name,'issued_to_user_id',issued_to_user_id,'service_account_id',service_account_id,'created_at',created_at,'expires_at',expires_at,'revoked_at',revoked_at) FROM api_keys WHERE organization_id=$1 AND workspace_id=$2 AND ($3 OR issued_to_user_id=$4) ORDER BY created_at DESC,id LIMIT $5 OFFSET $6")
        .bind(a.org).bind(ws).bind(admin(&a.role)).bind(u.user_id).bind(limit).bind(offset).fetch_all(&s.pool).await?;
    Ok(Json(json!({"data":data})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NewKey {
    name: String,
    expires_in_days: i32,
    service_account_id: Option<Uuid>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Expiry {
    expires_in_days: i32,
}
fn expiry(days: i32) -> Result<(), ApiError> {
    if !(1..=365).contains(&days) {
        Err(invalid())
    } else {
        Ok(())
    }
}
async fn lock_key_membership(
    tx: &mut Transaction<'_, Postgres>,
    org: Uuid,
    ws: Uuid,
    user: Uuid,
    kind: &str,
) -> Result<(), ApiError> {
    let member:Option<Uuid>=sqlx::query_scalar("SELECT user_id FROM organization_memberships WHERE organization_id=$1 AND user_id=$2 AND disabled_at IS NULL FOR UPDATE").bind(org).bind(user).fetch_optional(&mut **tx).await?;
    if member.is_none() {
        return Err(denied());
    }
    if shared(kind) {
        let member:Option<Uuid>=sqlx::query_scalar("SELECT user_id FROM workspace_memberships WHERE organization_id=$1 AND workspace_id=$2 AND user_id=$3 AND disabled_at IS NULL FOR UPDATE").bind(org).bind(ws).bind(user).fetch_optional(&mut **tx).await?;
        if member.is_none() {
            return Err(denied());
        }
    }
    Ok(())
}
async fn insert_key(
    tx: &mut Transaction<'_, Postgres>,
    org: Uuid,
    ws: Uuid,
    user: Option<Uuid>,
    service: Option<Uuid>,
    name: &str,
    days: i32,
) -> Result<NewApiKey, ApiError> {
    let key = NewApiKey::generate();
    sqlx::query("INSERT INTO api_keys(id,organization_id,workspace_id,issued_to_user_id,service_account_id,name,secret_hash,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,now()+make_interval(days=>$8))")
        .bind(key.id).bind(org).bind(ws).bind(user).bind(service).bind(name).bind(key.digest.as_slice()).bind(days).execute(&mut **tx).await?;
    Ok(key)
}
pub(super) async fn create_key(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(b): Json<NewKey>,
) -> ApiResult {
    let a = require_workspace(&s, &u, ws, false).await?;
    expiry(b.expires_in_days)?;
    if !valid_name(&b.name) {
        return Err(invalid());
    }
    let mut tx = s.pool.begin().await?;
    let a = locked_workspace_access(&mut tx, &u, a.org, ws, false).await?;
    if b.service_account_id.is_some() && (!admin(&a.role) || !shared(&a.kind)) {
        return Err(denied());
    }
    if let Some(id) = b.service_account_id {
        let active:Option<Uuid>=sqlx::query_scalar("SELECT id FROM service_accounts WHERE organization_id=$1 AND workspace_id=$2 AND id=$3 AND disabled_at IS NULL FOR UPDATE").bind(a.org).bind(ws).bind(id).fetch_optional(&mut *tx).await?;
        if active.is_none() {
            return Err(missing());
        }
    } else {
        lock_key_membership(&mut tx, a.org, ws, u.user_id, &a.kind).await?;
    }
    let key = insert_key(
        &mut tx,
        a.org,
        ws,
        if b.service_account_id.is_none() {
            Some(u.user_id)
        } else {
            None
        },
        b.service_account_id,
        &b.name,
        b.expires_in_days,
    )
    .await?;
    audit(
        &mut tx,
        u.user_id,
        Some(a.org),
        Some(ws),
        "key.created",
        Some(key.id),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"id":key.id,"token":key.token})))
}
pub(super) async fn revoke_key(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    let a = require_workspace(&s, &u, ws, false).await?;
    let mut tx = s.pool.begin().await?;
    let a = locked_workspace_access(&mut tx, &u, a.org, ws, false).await?;
    let changed=sqlx::query("UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE organization_id=$1 AND workspace_id=$2 AND id=$3 AND ($4 OR issued_to_user_id=$5)").bind(a.org).bind(ws).bind(id).bind(admin(&a.role)).bind(u.user_id).execute(&mut *tx).await?.rows_affected();
    if changed != 1 {
        return Err(missing());
    }
    audit(
        &mut tx,
        u.user_id,
        Some(a.org),
        Some(ws),
        "key.revoked",
        Some(id),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
pub(super) async fn rotate_key(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
    Json(b): Json<Expiry>,
) -> ApiResult {
    let a = require_workspace(&s, &u, ws, false).await?;
    expiry(b.expires_in_days)?;
    let mut tx = s.pool.begin().await?;
    let a = locked_workspace_access(&mut tx, &u, a.org, ws, false).await?;
    let row:Option<(String,Option<Uuid>,Option<Uuid>)>=sqlx::query_as("SELECT name,issued_to_user_id,service_account_id FROM api_keys WHERE organization_id=$1 AND workspace_id=$2 AND id=$3 AND revoked_at IS NULL").bind(a.org).bind(ws).bind(id).fetch_optional(&mut *tx).await?;
    let (name, issued_user, service) = row.ok_or_else(missing)?;
    if issued_user != Some(u.user_id) && !(service.is_some() && admin(&a.role)) {
        return Err(denied());
    }
    if let Some(service) = service {
        let active:Option<Uuid>=sqlx::query_scalar("SELECT id FROM service_accounts WHERE organization_id=$1 AND workspace_id=$2 AND id=$3 AND disabled_at IS NULL FOR UPDATE").bind(a.org).bind(ws).bind(service).fetch_optional(&mut *tx).await?;
        if active.is_none() {
            return Err(denied());
        }
    }
    if let Some(user) = issued_user {
        lock_key_membership(&mut tx, a.org, ws, user, &a.kind).await?;
    }
    // Account/membership before key is the common lock order with disablement.
    let live:Option<Uuid>=sqlx::query_scalar("SELECT id FROM api_keys WHERE organization_id=$1 AND workspace_id=$2 AND id=$3 AND revoked_at IS NULL FOR UPDATE").bind(a.org).bind(ws).bind(id).fetch_optional(&mut *tx).await?;
    if live.is_none() {
        return Err(missing());
    }
    let key = insert_key(
        &mut tx,
        a.org,
        ws,
        issued_user,
        service,
        &name,
        b.expires_in_days,
    )
    .await?;
    sqlx::query("UPDATE api_keys SET governance_key_id=(SELECT governance_key_id FROM api_keys WHERE organization_id=$1 AND workspace_id=$2 AND id=$3) WHERE organization_id=$1 AND workspace_id=$2 AND id=$4")
        .bind(a.org).bind(ws).bind(id).bind(key.id).execute(&mut *tx).await?;
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        u.user_id,
        Some(a.org),
        Some(ws),
        "key.rotated",
        Some(key.id),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"id":key.id,"token":key.token})))
}
pub(super) async fn accounts(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(page): Query<Page>,
) -> ApiResult {
    let a = require_workspace(&s, &u, ws, true).await?;
    let (limit, offset) = page.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'name',name,'disabled_at',disabled_at) FROM service_accounts WHERE organization_id=$1 AND workspace_id=$2 ORDER BY name,id LIMIT $3 OFFSET $4").bind(a.org).bind(ws).bind(limit).bind(offset).fetch_all(&s.pool).await?;
    Ok(Json(json!({"data":data})))
}
pub(super) async fn create_account(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(b): Json<Name>,
) -> ApiResult {
    let a = require_workspace(&s, &u, ws, true).await?;
    if !shared(&a.kind) || !valid_name(&b.name) {
        return Err(invalid());
    }
    let id = Uuid::new_v4();
    let mut tx = s.pool.begin().await?;
    let a = locked_workspace_access(&mut tx, &u, a.org, ws, true).await?;
    if !shared(&a.kind) {
        return Err(invalid());
    }
    sqlx::query(
        "INSERT INTO service_accounts(id,organization_id,workspace_id,name) VALUES($1,$2,$3,$4)",
    )
    .bind(id)
    .bind(a.org)
    .bind(ws)
    .bind(b.name)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        u.user_id,
        Some(a.org),
        Some(ws),
        "service_account.created",
        Some(id),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Disabled {
    disabled: bool,
}
pub(super) async fn update_account(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
    Json(b): Json<Disabled>,
) -> ApiResult {
    let a = require_workspace(&s, &u, ws, true).await?;
    let mut tx = s.pool.begin().await?;
    let a = locked_workspace_access(&mut tx, &u, a.org, ws, true).await?;
    if !shared(&a.kind) {
        return Err(invalid());
    }
    let changed=sqlx::query("UPDATE service_accounts SET disabled_at=CASE WHEN $4 THEN now() ELSE NULL END WHERE organization_id=$1 AND workspace_id=$2 AND id=$3").bind(a.org).bind(ws).bind(id).bind(b.disabled).execute(&mut *tx).await?.rows_affected();
    if changed != 1 {
        return Err(missing());
    }
    if b.disabled {
        sqlx::query("UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE organization_id=$1 AND workspace_id=$2 AND service_account_id=$3").bind(a.org).bind(ws).bind(id).execute(&mut *tx).await?;
    }
    audit(
        &mut tx,
        u.user_id,
        Some(a.org),
        Some(ws),
        "service_account.updated",
        Some(id),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}

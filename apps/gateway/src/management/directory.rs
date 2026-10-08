use super::*;

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/platform/workspaces",
            get(workspaces).post(create_workspace),
        )
        .route(
            "/api/v1/platform/workspaces/{ws}",
            get(platform_workspace).patch(update_workspace),
        )
        .route(
            "/api/v1/workspaces/{ws}",
            get(workspace).patch(rename_workspace),
        )
        .route("/api/v1/platform/users", get(users).post(create_user))
        .route(
            "/api/v1/platform/users/{user}",
            get(platform_user).patch(update_user),
        )
        .route("/api/v1/platform/users/{user}/roles", post(grant_role))
        .route(
            "/api/v1/platform/users/{user}/roles/{role}",
            axum::routing::delete(revoke_role),
        )
        .route(
            "/api/v1/platform/oidc/group-mappings",
            get(mappings).post(create_mapping),
        )
        .route(
            "/api/v1/platform/oidc/group-mappings/{id}",
            patch(update_mapping).delete(delete_mapping),
        )
        .route(
            "/api/v1/platform/cost-centers",
            get(cost_centers).post(create_cost_center),
        )
        .route(
            "/api/v1/platform/cost-centers/{id}",
            get(cost_center)
                .patch(update_cost_center)
                .delete(archive_cost_center),
        )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    q: Option<String>,
    kind: Option<String>,
    status: Option<String>,
}
// Display-only shared-workspace metadata: member_count counts users holding at
// least one non-revoked grant (the rows of the members endpoint, suspended users
// included); cost_center resolves the current allocation pointer by name.
const SHARED_WORKSPACE_JSON: &str = "jsonb_build_object('id',w.id,'name',w.name,'kind',w.kind,'cost_center_id',w.cost_center_id,'cost_center',CASE WHEN c.id IS NULL THEN NULL ELSE jsonb_build_object('id',c.id,'name',c.name,'code',c.code) END,'member_count',(SELECT count(DISTINCT g.user_id) FROM workspace_membership_grants g WHERE g.workspace_id=w.id AND g.revoked_at IS NULL),'disabled_at',w.disabled_at,'created_at',w.created_at) FROM workspaces w LEFT JOIN cost_centers c ON c.id=w.cost_center_id";
async fn workspaces(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<DirectoryQuery>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let (l, o) = Page {
        limit: p.limit,
        offset: p.offset,
    }
    .bounds()?;
    if p.kind.as_deref().is_some_and(|k| !shared(k))
        || p.q.as_ref().is_some_and(|q| q.chars().count() > 200)
        || p.status
            .as_deref()
            .is_some_and(|v| !matches!(v, "active" | "disabled"))
    {
        return Err(invalid());
    }
    let sql = format!(
        "SELECT {SHARED_WORKSPACE_JSON} WHERE w.kind IN ('team','project') AND ($1::text IS NULL OR w.kind=$1) AND ($2::text IS NULL OR strpos(lower(w.name),lower($2))>0) AND ($3::text IS NULL OR ($3='active')=(w.disabled_at IS NULL)) ORDER BY w.name,w.id LIMIT $4 OFFSET $5"
    );
    let data: Vec<Value> = sqlx::query_scalar(&sql)
        .bind(p.kind)
        .bind(p.q)
        .bind(p.status)
        .bind(l)
        .bind(o)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewWorkspace {
    name: String,
    kind: String,
    owner_user_id: Uuid,
}
async fn create_workspace(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<NewWorkspace>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    if !valid_name(&b.name) || !shared(&b.kind) {
        return Err(invalid());
    }
    resources::platform_role(&mut tx, b.owner_user_id).await?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,$2,$3)")
        .bind(id)
        .bind(b.name)
        .bind(&b.kind)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO workspace_membership_grants(id,workspace_id,user_id,role,source) VALUES($1,$2,$3,'owner','manual')").bind(Uuid::new_v4()).bind(id).bind(b.owner_user_id).execute(&mut *tx).await?;
    audit(
        &mut tx,
        &u,
        Some(id),
        "workspace.created",
        "workspace",
        Some(id),
        json!({"kind":b.kind}),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}
async fn workspace(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    let mut v:Value=sqlx::query_scalar("SELECT jsonb_build_object('id',w.id,'name',w.name,'kind',w.kind,'owner_user_id',w.owner_user_id,'cost_center_id',w.cost_center_id,'cost_center',CASE WHEN c.id IS NULL THEN NULL ELSE jsonb_build_object('id',c.id,'name',c.name,'code',c.code) END,'created_at',w.created_at) FROM workspaces w LEFT JOIN cost_centers c ON c.id=w.cost_center_id WHERE w.id=$1").bind(ws).fetch_one(&mut *tx).await?;
    let membership: Option<String> = sqlx::query_scalar(
        "SELECT role FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(ws)
    .bind(u.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let source:Option<String>=sqlx::query_scalar("SELECT CASE WHEN count(DISTINCT source)>1 THEN 'mixed' ELSE min(source) END FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND revoked_at IS NULL").bind(ws).bind(u.user_id).fetch_one(&mut *tx).await?;
    v["role"] = json!(if a.kind == "personal" {
        Some("owner")
    } else {
        membership.as_deref()
    });
    v["membership_source"] = json!(if a.kind == "personal" {
        Some("manual")
    } else {
        source.as_deref()
    });
    v["capabilities"] = a.capabilities();
    tx.commit().await?;
    Ok(Json(v))
}
async fn platform_workspace(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let sql =
        format!("SELECT {SHARED_WORKSPACE_JSON} WHERE w.id=$1 AND w.kind IN ('team','project')");
    let v: Value = sqlx::query_scalar(&sql)
        .bind(ws)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    tx.commit().await?;
    Ok(Json(v))
}
// Option<Option<T>> needs an explicit deserializer to distinguish omitted from null.
fn supplied<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::<T>::deserialize(d)?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceUpdate {
    name: Option<String>,
    disabled: Option<bool>,
    #[serde(default, deserialize_with = "supplied")]
    cost_center_id: Option<Option<Uuid>>,
}
async fn update_workspace(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(b): Json<WorkspaceUpdate>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1 FOR UPDATE")
        .bind(ws)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    // Allocation is platform-only and can include personal totals, but name/state metadata stays private.
    if kind == "personal" && (b.name.is_some() || b.disabled.is_some()) {
        return Err(denied());
    }
    if b.name.as_ref().is_some_and(|n| !valid_name(n)) {
        return Err(invalid());
    }
    if let Some(Some(cc)) = b.cost_center_id {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM cost_centers WHERE id=$1 AND archived_at IS NULL",
        )
        .bind(cc)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    }
    sqlx::query("UPDATE workspaces SET name=coalesce($2,name),disabled_at=CASE WHEN $3::boolean IS NULL THEN disabled_at WHEN $3 THEN now() ELSE NULL END,cost_center_id=CASE WHEN $4 THEN $5 ELSE cost_center_id END WHERE id=$1").bind(ws).bind(b.name).bind(b.disabled).bind(b.cost_center_id.is_some()).bind(b.cost_center_id.flatten()).execute(&mut *tx).await?;
    if b.disabled == Some(true) {
        sqlx::query(
            "UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE workspace_id=$1",
        )
        .bind(ws)
        .execute(&mut *tx)
        .await?;
        catalogs::retire(&mut tx).await?;
    }
    audit(
        &mut tx,
        &u,
        Some(ws),
        "workspace.updated",
        "workspace",
        Some(ws),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
async fn rename_workspace(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(b): Json<Name>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    // The personal workspace is always named "Personal".
    if a.kind == "personal" {
        return Err(personal_name_fixed());
    }
    resources::manage(&a)?;
    if !valid_name(&b.name) {
        return Err(invalid());
    }
    sqlx::query("UPDATE workspaces SET name=$2 WHERE id=$1")
        .bind(ws)
        .bind(b.name)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        Some(ws),
        "workspace.updated",
        "workspace",
        Some(ws),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
// Shared by the user list and detail. Sign-in dates come from browser session
// creation; the count covers Team/Project grants only, never personal workspaces.
const USER_JSON: &str = "jsonb_build_object('id',u.id,'email',u.email,'display_name',u.display_name,'platform_role',r.role,'disabled_at',u.disabled_at,'cleanup_due_at',u.cleanup_due_at,'cleaned_at',u.cleaned_at,'disable_reason',u.disable_reason,'oidc_link_allowed',u.oidc_link_allowed,'created_at',u.created_at,'last_sign_in_at',(SELECT max(s.created_at) FROM browser_sessions s WHERE s.user_id=u.id),'shared_workspace_count',(SELECT count(DISTINCT g.workspace_id) FROM workspace_membership_grants g JOIN workspaces w ON w.id=g.workspace_id WHERE g.user_id=u.id AND g.revoked_at IS NULL AND w.kind IN ('team','project')),'role_grants',coalesce((SELECT jsonb_agg(jsonb_build_object('id',g.id,'role',g.role,'source',g.source,'mapping_id',g.mapping_id,'revoked_at',g.revoked_at) ORDER BY g.created_at,g.id) FROM platform_role_grants g WHERE g.user_id=u.id),'[]'::jsonb)) FROM users u LEFT JOIN effective_platform_roles r ON r.user_id=u.id";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    q: Option<String>,
    role: Option<String>,
    status: Option<String>,
}
async fn users(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<UserQuery>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let (l, o) = Page {
        limit: p.limit,
        offset: p.offset,
    }
    .bounds()?;
    if p.q.as_ref().is_some_and(|q| q.chars().count() > 200)
        || p.role
            .as_deref()
            .is_some_and(|r| r != "none" && !platform_role_valid(r))
        || p.status
            .as_deref()
            .is_some_and(|v| !matches!(v, "active" | "suspended"))
    {
        return Err(invalid());
    }
    // Suspended includes cleaned identities; role "none" means no effective entitlement.
    let sql = format!(
        "SELECT {USER_JSON} WHERE ($1::text IS NULL OR strpos(lower(coalesce(u.email,'')||' '||coalesce(u.display_name,'')),lower($1))>0) AND ($2::text IS NULL OR ($2='none' AND r.role IS NULL) OR r.role=$2) AND ($3::text IS NULL OR ($3='active')=(u.disabled_at IS NULL AND u.cleaned_at IS NULL)) ORDER BY u.email,u.id LIMIT $4 OFFSET $5"
    );
    let data: Vec<Value> = sqlx::query_scalar(&sql)
        .bind(p.q)
        .bind(p.role)
        .bind(p.status)
        .bind(l)
        .bind(o)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn platform_user(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let sql = format!("SELECT {USER_JSON} WHERE u.id=$1");
    let mut data: Value = sqlx::query_scalar(&sql)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    data["first_sign_in_at"] = sqlx::query_scalar::<_, Option<Value>>(
        "SELECT to_jsonb(min(created_at)) FROM browser_sessions WHERE user_id=$1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?
    .unwrap_or(Value::Null);
    // Team/Project grants only: personal workspaces, keys and activity are never returned.
    data["shared_memberships"] = sqlx::query_scalar("SELECT coalesce(jsonb_agg(m ORDER BY m->>'name',m->>'workspace_id'),'[]'::jsonb) FROM (SELECT jsonb_build_object('workspace_id',w.id,'name',w.name,'kind',w.kind,'disabled_at',w.disabled_at,'role',CASE max(CASE g.role WHEN 'owner' THEN 3 WHEN 'admin' THEN 2 ELSE 1 END) WHEN 3 THEN 'owner' WHEN 2 THEN 'admin' ELSE 'member' END,'sources',jsonb_agg(DISTINCT g.source),'grants',jsonb_agg(jsonb_build_object('role',g.role,'source',g.source) ORDER BY g.source,g.role)) AS m FROM workspace_membership_grants g JOIN workspaces w ON w.id=g.workspace_id WHERE g.user_id=$1 AND g.revoked_at IS NULL AND w.kind IN ('team','project') GROUP BY w.id,w.name,w.kind,w.disabled_at) x")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(data))
}
fn platform_role_valid(r: &str) -> bool {
    matches!(r, "user" | "auditor" | "admin")
}
pub(super) fn email_valid(s: &str) -> bool {
    s.len() <= 320
        && s.split('@').count() == 2
        && s.split('@').all(|s| !s.is_empty())
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewUser {
    email: String,
    platform_role: String,
}
async fn create_user(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<NewUser>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    let email = b.email.trim().to_lowercase();
    if !email_valid(&email) || !platform_role_valid(&b.platform_role) {
        return Err(invalid());
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,oidc_link_allowed) VALUES($1,$2,true)")
        .bind(id)
        .bind(email)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source,granted_by) VALUES($1,$2,$3,'manual',$4)").bind(Uuid::new_v4()).bind(id).bind(&b.platform_role).bind(u.user_id).execute(&mut *tx).await?;
    audit(
        &mut tx,
        &u,
        None,
        "user.provisioned",
        "user",
        Some(id),
        json!({"role":b.platform_role}),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}
/// Protect active manual ownership when an administrator suspends a user.
/// Entitlement-loss lifecycle deliberately does not preserve unauthorized access.
async fn protect_owners(tx: &mut Transaction<'_, Postgres>, target: Uuid) -> Result<(), ApiError> {
    let stranded:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_membership_grants g JOIN workspaces w ON w.id=g.workspace_id WHERE g.user_id=$1 AND g.role='owner' AND g.revoked_at IS NULL AND w.disabled_at IS NULL AND w.kind IN ('team','project') AND NOT EXISTS(SELECT 1 FROM effective_workspace_memberships m JOIN effective_platform_roles p ON p.user_id=m.user_id WHERE m.workspace_id=g.workspace_id AND m.role='owner' AND m.user_id<>$1))").bind(target).fetch_one(&mut **tx).await?;
    if stranded {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Cannot remove the last workspace owner",
        ));
    }
    Ok(())
}
async fn protect_admin(tx: &mut Transaction<'_, Postgres>, target: Uuid) -> Result<(), ApiError> {
    let last:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM effective_platform_roles WHERE user_id=$1 AND role='admin') AND NOT EXISTS(SELECT 1 FROM effective_platform_roles WHERE user_id<>$1 AND role='admin')").bind(target).fetch_one(&mut **tx).await?;
    if last {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Cannot remove the last platform administrator",
        ));
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserUpdate {
    email: Option<String>,
    disabled: Option<bool>,
    oidc_link_allowed: Option<bool>,
}
async fn update_user(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<UserUpdate>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND cleaned_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    let email = b.email.map(|s| s.trim().to_lowercase());
    if email.as_ref().is_some_and(|s| !email_valid(s)) {
        return Err(invalid());
    }
    let email_changed = if let Some(ref new_email) = email {
        sqlx::query_scalar::<_, bool>(
            "SELECT lower(email) IS DISTINCT FROM lower($2::text) FROM users WHERE id=$1",
        )
        .bind(id)
        .bind(new_email)
        .fetch_one(&mut *tx)
        .await?
    } else {
        false
    };
    if b.disabled == Some(true) {
        protect_admin(&mut tx, id).await?;
        protect_owners(&mut tx, id).await?;
    }
    // Reactivation requires existing live role grants; no cleanup tombstone revival.
    if b.disabled == Some(false) {
        let entitled:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM platform_role_grants WHERE user_id=$1 AND revoked_at IS NULL)").bind(id).fetch_one(&mut *tx).await?;
        if !entitled {
            return Err(denied());
        }
    }
    sqlx::query("UPDATE users SET email=coalesce($2,email),oidc_link_allowed=coalesce($3,oidc_link_allowed),disabled_at=CASE WHEN $4::boolean IS NULL THEN disabled_at WHEN $4 THEN coalesce(disabled_at,now()) ELSE NULL END,disable_reason=CASE WHEN $4 THEN 'admin_suspension' WHEN $4=false THEN NULL ELSE disable_reason END,cleanup_due_at=CASE WHEN $4 THEN now()+interval '30 days' WHEN $4=false THEN NULL ELSE cleanup_due_at END WHERE id=$1").bind(id).bind(email).bind(b.oidc_link_allowed).bind(b.disabled).execute(&mut *tx).await?;
    if b.disabled == Some(true) {
        revoke_human_credentials(&mut tx, id).await?;
    } else if email_changed {
        // Mutable profile email is not fresh OIDC proof. Require a new verified sign-in;
        // an in-flight invitation also compares its principal snapshot with current email.
        sqlx::query(
            "UPDATE browser_sessions SET revoked_at=coalesce(revoked_at,now()) WHERE user_id=$1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    audit(
        &mut tx,
        &u,
        None,
        "user.updated",
        "user",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
pub(super) async fn revoke_human_credentials(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE issued_to_user_id=$1",
    )
    .bind(user)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE browser_sessions SET revoked_at=coalesce(revoked_at,now()) WHERE user_id=$1",
    )
    .bind(user)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleInput {
    role: String,
}
async fn grant_role(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<RoleInput>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    if !platform_role_valid(&b.role) {
        return Err(invalid());
    }
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND cleaned_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source,granted_by) VALUES($1,$2,$3,'manual',$4) ON CONFLICT DO NOTHING").bind(Uuid::new_v4()).bind(id).bind(&b.role).bind(u.user_id).execute(&mut *tx).await?;
    audit(
        &mut tx,
        &u,
        None,
        "role.granted",
        "user",
        Some(id),
        json!({"role":b.role,"source":"manual"}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
async fn revoke_role(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((id, role)): Path<(Uuid, String)>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    if !platform_role_valid(&role) {
        return Err(invalid());
    }
    if role == "admin" {
        let retained:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM platform_role_grants WHERE user_id=$1 AND role='admin' AND source='group' AND revoked_at IS NULL)").bind(id).fetch_one(&mut *tx).await?;
        if !retained {
            protect_admin(&mut tx, id).await?;
        }
    }
    let remaining:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM platform_role_grants WHERE user_id=$1 AND revoked_at IS NULL AND NOT(role=$2 AND source IN ('manual','bootstrap')))").bind(id).bind(&role).fetch_one(&mut *tx).await?;
    if !remaining {
        protect_owners(&mut tx, id).await?;
    }
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role=$2 AND source IN ('manual','bootstrap') AND revoked_at IS NULL").bind(id).bind(&role).execute(&mut *tx).await?;
    deactivate_unentitled(&mut tx, &[id]).await?;
    audit(
        &mut tx,
        &u,
        None,
        "role.revoked",
        "user",
        Some(id),
        json!({"role":role}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
async fn deactivate_unentitled(
    tx: &mut Transaction<'_, Postgres>,
    affected: &[Uuid],
) -> Result<(), ApiError> {
    let ids:Vec<Uuid>=sqlx::query_scalar("UPDATE users u SET disabled_at=now(),cleanup_due_at=now()+interval '30 days',disable_reason='entitlement_loss' WHERE id=ANY($1) AND disabled_at IS NULL AND cleaned_at IS NULL AND NOT EXISTS(SELECT 1 FROM platform_role_grants g WHERE g.user_id=u.id AND g.revoked_at IS NULL) RETURNING id").bind(affected).fetch_all(&mut **tx).await?;
    for id in ids {
        revoke_human_credentials(tx, id).await?
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MappingInput {
    issuer: String,
    group_value: String,
    target_kind: String,
    platform_role: Option<String>,
    workspace_id: Option<Uuid>,
    workspace_role: Option<String>,
    enabled: bool,
}
fn mapping_valid(b: &MappingInput) -> bool {
    let issuer = reqwest::Url::parse(&b.issuer).ok();
    issuer.is_some_and(|u| {
        (u.scheme() == "https"
            || (u.scheme() == "http"
                && std::env::var("GATEWAY_ENV").as_deref() == Ok("development")
                && u.host_str().is_some_and(|h| {
                    h == "localhost"
                        || h.trim_matches(['[', ']'])
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                })))
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
    }) && b.issuer.len() <= 2048
        && !b.group_value.is_empty()
        && b.group_value.len() <= 512
        && !b.group_value.chars().any(char::is_control)
        && match b.target_kind.as_str() {
            "platform" => {
                b.platform_role.as_deref().is_some_and(platform_role_valid)
                    && b.workspace_id.is_none()
                    && b.workspace_role.is_none()
            }
            "workspace" => {
                b.platform_role.is_none()
                    && b.workspace_id.is_some()
                    && b.workspace_role
                        .as_deref()
                        .is_some_and(|r| matches!(r, "admin" | "member"))
            }
            _ => false,
        }
}
async fn validate_mapping(
    tx: &mut Transaction<'_, Postgres>,
    b: &MappingInput,
) -> Result<(), ApiError> {
    if !mapping_valid(b) {
        return Err(invalid());
    }
    if let Some(ws) = b.workspace_id {
        sqlx::query_scalar::<_,Uuid>("SELECT id FROM workspaces WHERE id=$1 AND kind IN ('team','project') AND disabled_at IS NULL").bind(ws).fetch_optional(&mut **tx).await?.ok_or_else(missing)?;
    }
    Ok(())
}
async fn mappings(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<Page>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let (l, o) = p.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(m) FROM oidc_group_mappings m ORDER BY issuer,group_value,id LIMIT $1 OFFSET $2").bind(l).bind(o).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn create_mapping(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<MappingInput>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    validate_mapping(&mut tx, &b).await?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role,workspace_id,workspace_role,enabled) VALUES($1,$2,$3,$4,$5,$6,$7,$8)").bind(id).bind(b.issuer).bind(b.group_value).bind(b.target_kind).bind(b.platform_role).bind(b.workspace_id).bind(b.workspace_role).bind(b.enabled).execute(&mut *tx).await?;
    audit(
        &mut tx,
        &u,
        None,
        "mapping.created",
        "group_mapping",
        Some(id),
        json!({"enabled":b.enabled}),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}
async fn revoke_mapping_grants(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<(), ApiError> {
    // Evaluate administrator protection against the post-removal grant set, not session hints.
    let strands_admin:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM effective_platform_roles WHERE role='admin') AND NOT EXISTS(SELECT 1 FROM platform_role_grants g JOIN users u ON u.id=g.user_id WHERE g.role='admin' AND g.revoked_at IS NULL AND u.disabled_at IS NULL AND u.cleaned_at IS NULL AND g.mapping_id IS DISTINCT FROM $1)").bind(id).fetch_one(&mut **tx).await?;
    if strands_admin {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Cannot remove the last platform administrator",
        ));
    }
    let platform_affected:Vec<Uuid>=sqlx::query_scalar("UPDATE platform_role_grants SET revoked_at=now() WHERE mapping_id=$1 AND source='group' AND revoked_at IS NULL RETURNING user_id").bind(id).fetch_all(&mut **tx).await?;
    let affected:Vec<(Uuid,Uuid)>=sqlx::query_as("UPDATE workspace_membership_grants SET revoked_at=now() WHERE mapping_id=$1 AND source='group' AND revoked_at IS NULL RETURNING workspace_id,user_id").bind(id).fetch_all(&mut **tx).await?;
    for (ws, user) in affected {
        sqlx::query("UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE workspace_id=$1 AND issued_to_user_id=$2 AND NOT EXISTS(SELECT 1 FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2)").bind(ws).bind(user).execute(&mut **tx).await?;
    }
    deactivate_unentitled(tx, &platform_affected).await?;
    Ok(())
}
async fn update_mapping(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(patch): Json<Value>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    let mut current:Value=sqlx::query_scalar("SELECT jsonb_build_object('issuer',issuer,'group_value',group_value,'target_kind',target_kind,'platform_role',platform_role,'workspace_id',workspace_id,'workspace_role',workspace_role,'enabled',enabled) FROM oidc_group_mappings WHERE id=$1 FOR UPDATE").bind(id).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    let before = current.clone();
    let fields = patch.as_object().ok_or_else(invalid)?;
    for (k, v) in fields {
        if !matches!(
            k.as_str(),
            "issuer"
                | "group_value"
                | "target_kind"
                | "platform_role"
                | "workspace_id"
                | "workspace_role"
                | "enabled"
        ) {
            return Err(invalid());
        }
        current[k] = v.clone();
    }
    let b: MappingInput = serde_json::from_value(current.clone()).map_err(|_| invalid())?;
    validate_mapping(&mut tx, &b).await?;
    if current != before {
        revoke_mapping_grants(&mut tx, id).await?;
    }
    sqlx::query("UPDATE oidc_group_mappings SET issuer=$2,group_value=$3,target_kind=$4,platform_role=$5,workspace_id=$6,workspace_role=$7,enabled=$8 WHERE id=$1").bind(id).bind(b.issuer).bind(b.group_value).bind(b.target_kind).bind(b.platform_role).bind(b.workspace_id).bind(b.workspace_role).bind(b.enabled).execute(&mut *tx).await?;
    audit(
        &mut tx,
        &u,
        None,
        "mapping.updated",
        "group_mapping",
        Some(id),
        json!({"enabled":b.enabled}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
async fn delete_mapping(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM oidc_group_mappings WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    revoke_mapping_grants(&mut tx, id).await?;
    // Retain mapping provenance on revoked grants while deleting control-plane definition.
    sqlx::query("DELETE FROM oidc_group_mappings WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        None,
        "mapping.deleted",
        "group_mapping",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
async fn cost_centers(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<Page>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let (l, o) = p.bounds()?;
    let data: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(c) FROM cost_centers c ORDER BY name,id LIMIT $1 OFFSET $2",
    )
    .bind(l)
    .bind(o)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn cost_center(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let v: Value = sqlx::query_scalar("SELECT to_jsonb(c) FROM cost_centers c WHERE id=$1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    tx.commit().await?;
    Ok(Json(v))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CostCenterInput {
    name: String,
    code: String,
}
async fn create_cost_center(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<CostCenterInput>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    if !valid_name(&b.name) || !valid_name(&b.code) {
        return Err(invalid());
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO cost_centers(id,name,code) VALUES($1,$2,$3)")
        .bind(id)
        .bind(b.name)
        .bind(b.code)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        None,
        "cost_center.created",
        "cost_center",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}
async fn update_cost_center(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<CostCenterInput>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    if !valid_name(&b.name) || !valid_name(&b.code) {
        return Err(invalid());
    }
    if sqlx::query("UPDATE cost_centers SET name=$2,code=$3 WHERE id=$1 AND archived_at IS NULL")
        .bind(id)
        .bind(b.name)
        .bind(b.code)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        != 1
    {
        return Err(missing());
    }
    audit(
        &mut tx,
        &u,
        None,
        "cost_center.updated",
        "cost_center",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
async fn archive_cost_center(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    if sqlx::query("UPDATE cost_centers SET archived_at=coalesce(archived_at,now()) WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        != 1
    {
        return Err(missing());
    }
    sqlx::query("UPDATE workspaces SET cost_center_id=NULL WHERE cost_center_id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        None,
        "cost_center.archived",
        "cost_center",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}

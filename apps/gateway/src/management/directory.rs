use super::*;

#[cfg(all(test, feature = "integration-tests"))]
mod tests;

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/platform/users", get(users))
        .route("/api/v1/platform/teams", get(platform_teams))
        .route("/api/v1/platform/projects", get(platform_projects))
        .route("/api/v1/orgs", get(organizations))
        .route("/api/v1/orgs/{org}/teams", get(teams))
        .route("/api/v1/orgs/{org}/projects", get(projects))
        .route("/api/v1/orgs/{org}", patch(rename_organization))
        .route("/api/v1/workspaces/{ws}", patch(rename_shared_workspace))
}

// A session is not a durable grant of platform authority. Keep the current user
// protected from disable/demotion until the read or audited mutation completes.
async fn current_operator(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<bool, ApiError> {
    sqlx::query_scalar(
        "SELECT platform_admin FROM users WHERE id=$1 AND disabled_at IS NULL FOR SHARE",
    )
    .bind(user)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(denied)
}

async fn users(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(page): Query<Page>,
) -> ApiResult {
    let mut tx = s.pool.begin().await?;
    if !current_operator(&mut tx, u.user_id).await? {
        return Err(denied());
    }
    let (limit, offset) = page.bounds()?;
    // Explicit projection: this directory is not an identity, workspace, key,
    // or usage inventory, even for operators.
    let data: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',id,'email',email,'platform_admin',platform_admin,'disabled_at',disabled_at,'created_at',created_at) FROM users ORDER BY email,id LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlatformWorkspacesQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    organization_id: Option<Uuid>,
}

async fn platform_teams(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(query): Query<PlatformWorkspacesQuery>,
) -> ApiResult {
    platform_workspaces(s, u, query, "team").await
}

async fn platform_projects(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(query): Query<PlatformWorkspacesQuery>,
) -> ApiResult {
    platform_workspaces(s, u, query, "project").await
}

async fn platform_workspaces(
    s: Store,
    u: BrowserPrincipal,
    query: PlatformWorkspacesQuery,
    kind: &'static str,
) -> ApiResult {
    let mut tx = s.pool.begin().await?;
    if !current_operator(&mut tx, u.user_id).await? {
        return Err(denied());
    }
    let (limit, offset) = Page {
        limit: query.limit,
        offset: query.offset,
    }
    .bounds()?;
    // Operators get a fixed-kind cross-organization shared directory, never personal
    // workspaces or their owners, memberships, keys, or usage.
    let data: Vec<Value> = sqlx::query_scalar(
        r#"SELECT jsonb_build_object('id',w.id,'organization_id',o.id,'organization_name',o.name,'name',w.name,'kind',w.kind,'role','operator')
        FROM workspaces w JOIN organizations o ON o.id=w.organization_id
        WHERE o.disabled_at IS NULL AND w.disabled_at IS NULL AND w.kind=$4
          AND ($1::uuid IS NULL OR o.id=$1)
        ORDER BY o.name,o.id,w.name,w.id LIMIT $2 OFFSET $3"#,
    )
    .bind(query.organization_id)
    .bind(limit)
    .bind(offset)
    .bind(kind)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}

async fn organizations(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(page): Query<Page>,
) -> ApiResult {
    let (limit, offset) = page.bounds()?;
    let mut tx = s.pool.begin().await?;
    let operator = current_operator(&mut tx, u.user_id).await?;
    let data: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',o.id,'name',o.name,'slug',o.slug,'role',CASE WHEN $2 THEN 'operator' ELSE m.role END,'membership_role',m.role,'created_at',o.created_at) FROM organizations o LEFT JOIN organization_memberships m ON m.organization_id=o.id AND m.user_id=$1 AND m.disabled_at IS NULL WHERE o.disabled_at IS NULL AND ($2 OR m.user_id IS NOT NULL) ORDER BY o.name,o.id LIMIT $3 OFFSET $4",
    )
    .bind(u.user_id)
    .bind(operator)
    .bind(limit)
    .bind(offset)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}

async fn teams(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(org): Path<Uuid>,
    Query(page): Query<Page>,
) -> ApiResult {
    org_workspaces(s, u, org, page, "team").await
}

async fn projects(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(org): Path<Uuid>,
    Query(page): Query<Page>,
) -> ApiResult {
    org_workspaces(s, u, org, page, "project").await
}

async fn org_workspaces(
    s: Store,
    u: BrowserPrincipal,
    org: Uuid,
    page: Page,
    kind: &'static str,
) -> ApiResult {
    let (limit, offset) = page.bounds()?;
    let mut tx = s.pool.begin().await?;
    let operator = current_operator(&mut tx, u.user_id).await?;
    locked_org_role(&mut tx, &u, org).await?;
    let data: Vec<Value> = sqlx::query_scalar(
        r#"SELECT jsonb_build_object('id',w.id,'organization_id',w.organization_id,'name',w.name,'kind',w.kind,'role',CASE WHEN $3 OR om.role='owner' OR wm.role='owner' THEN 'owner' WHEN om.role='admin' THEN 'admin' ELSE wm.role END)
        FROM workspaces w JOIN organizations o ON o.id=w.organization_id AND o.disabled_at IS NULL
        LEFT JOIN organization_memberships om ON om.organization_id=w.organization_id AND om.user_id=$2 AND om.disabled_at IS NULL
        LEFT JOIN workspace_memberships wm ON wm.organization_id=w.organization_id AND wm.workspace_id=w.id AND wm.user_id=$2 AND wm.disabled_at IS NULL
        WHERE w.organization_id=$1 AND w.disabled_at IS NULL AND w.kind=$6
          AND ($3 OR om.user_id IS NOT NULL) AND ($3 OR om.role IN ('owner','admin') OR wm.user_id IS NOT NULL)
        ORDER BY w.name,w.id LIMIT $4 OFFSET $5"#,
    )
    .bind(org)
    .bind(u.user_id)
    .bind(operator)
    .bind(limit)
    .bind(offset)
    .bind(kind)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}

async fn lock_organization(tx: &mut Transaction<'_, Postgres>, org: Uuid) -> Result<(), ApiError> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM organizations WHERE id=$1 AND disabled_at IS NULL FOR NO KEY UPDATE",
    )
    .bind(org)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(missing)?;
    Ok(())
}

async fn lock_org_membership(
    tx: &mut Transaction<'_, Postgres>,
    org: Uuid,
    user: Uuid,
) -> Result<(), ApiError> {
    sqlx::query("SELECT user_id FROM organization_memberships WHERE organization_id=$1 AND user_id=$2 FOR SHARE")
        .bind(org)
        .bind(user)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn rename_organization(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(org): Path<Uuid>,
    Json(body): Json<Name>,
) -> ApiResult {
    if !valid_name(&body.name) {
        return Err(invalid());
    }
    let mut tx = s.pool.begin().await?;
    lock_organization(&mut tx, org).await?;
    lock_org_membership(&mut tx, org, u.user_id).await?;
    current_operator(&mut tx, u.user_id).await?;
    if !admin(&locked_org_role(&mut tx, &u, org).await?) {
        return Err(denied());
    }
    sqlx::query("UPDATE organizations SET name=$2 WHERE id=$1")
        .bind(org)
        .bind(body.name)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        u.user_id,
        Some(org),
        None,
        "organization.updated",
        Some(org),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}

async fn rename_shared_workspace(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(body): Json<Name>,
) -> ApiResult {
    if !valid_name(&body.name) {
        return Err(invalid());
    }
    // Discover only the lock root here; do not trust this unlocked lookup for
    // workspace state or authority. Lock order matches membership mutations.
    let org: Uuid = sqlx::query_scalar("SELECT organization_id FROM workspaces WHERE id=$1")
        .bind(ws)
        .fetch_optional(&s.pool)
        .await?
        .ok_or_else(missing)?;
    let mut tx = s.pool.begin().await?;
    lock_organization(&mut tx, org).await?;
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE organization_id=$1 AND id=$2 AND disabled_at IS NULL FOR NO KEY UPDATE")
        .bind(org).bind(ws).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    if !shared(&kind) {
        return Err(missing());
    }
    lock_org_membership(&mut tx, org, u.user_id).await?;
    let membership: Option<String> = sqlx::query_scalar("SELECT role FROM workspace_memberships WHERE organization_id=$1 AND workspace_id=$2 AND user_id=$3 AND disabled_at IS NULL FOR SHARE")
        .bind(org).bind(ws).bind(u.user_id).fetch_optional(&mut *tx).await?;
    current_operator(&mut tx, u.user_id).await?;
    let org_role = locked_org_role(&mut tx, &u, org).await?;
    let role = if owner(&org_role) || membership.as_deref() == Some("owner") {
        "owner"
    } else if admin(&org_role) {
        "admin"
    } else {
        membership.as_deref().ok_or_else(denied)?
    };
    if !admin(role) {
        return Err(denied());
    }
    sqlx::query("UPDATE workspaces SET name=$3 WHERE organization_id=$1 AND id=$2")
        .bind(org)
        .bind(ws)
        .bind(body.name)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        u.user_id,
        Some(org),
        Some(ws),
        "workspace.updated",
        Some(ws),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}

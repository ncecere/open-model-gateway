use super::*;

#[cfg(all(test, feature = "integration-tests"))]
mod tests;

pub(super) fn platform_routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/platform/providers",
            get(platform_providers).post(platform_create_provider),
        )
        .route(
            "/api/v1/platform/providers/{id}",
            get(platform_provider).patch(platform_update_provider),
        )
        .route(
            "/api/v1/platform/models",
            get(platform_models).post(platform_create_model),
        )
        .route(
            "/api/v1/platform/models/{id}",
            get(platform_model).patch(platform_update_model),
        )
        .route(
            "/api/v1/platform/deployments",
            get(platform_deployments).post(platform_create_deployment),
        )
        .route(
            "/api/v1/platform/deployments/{id}",
            get(platform_deployment).patch(platform_update_deployment),
        )
        .route(
            "/api/v1/platform/orgs/{org}/models/{model}",
            axum::routing::put(assign_model).delete(unassign_model),
        )
        .route(
            "/api/v1/orgs/{org}/users/{user}/grants",
            get(user_grants).post(grant_user),
        )
        .route(
            "/api/v1/orgs/{org}/users/{user}/grants/{model}",
            axum::routing::delete(revoke_user),
        )
        .route("/api/v1/platform/audit", get(platform_audit))
}

// The catalog lock always precedes organization and authority locks. Never authorize
// from BrowserPrincipal.platform_admin: it can predate a demotion or suspension.
pub(super) async fn catalog_lock(
    tx: &mut Transaction<'_, Postgres>,
    exclusive: bool,
) -> Result<(), ApiError> {
    sqlx::query(if exclusive {
        "SELECT pg_advisory_xact_lock(72419502)"
    } else {
        "SELECT pg_advisory_xact_lock_shared(72419502)"
    })
    .execute(&mut **tx)
    .await?;
    Ok(())
}
pub(super) async fn operator_lock(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
) -> Result<(), ApiError> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND platform_admin AND disabled_at IS NULL FOR SHARE",
    )
    .bind(u.user_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(denied)?;
    Ok(())
}
pub(super) async fn catalog_tx<'a>(
    s: &'a Store,
    u: &BrowserPrincipal,
    exclusive: bool,
) -> Result<Transaction<'a, Postgres>, ApiError> {
    let mut tx = s.pool.begin().await?;
    catalog_lock(&mut tx, exclusive).await?;
    operator_lock(&mut tx, u).await?;
    Ok(tx)
}
pub(super) async fn org_tx<'a>(
    s: &'a Store,
    org: Uuid,
) -> Result<Transaction<'a, Postgres>, ApiError> {
    let mut tx = s.pool.begin().await?;
    catalog_lock(&mut tx, false).await?;
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM organizations WHERE id=$1 AND disabled_at IS NULL FOR NO KEY UPDATE",
    )
    .bind(org)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    Ok(tx)
}
pub(super) async fn live_org_role(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    org: Uuid,
) -> Result<String, ApiError> {
    let operator: bool = sqlx::query_scalar(
        "SELECT platform_admin FROM users WHERE id=$1 AND disabled_at IS NULL FOR SHARE",
    )
    .bind(u.user_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(denied)?;
    if operator {
        return Ok("operator".into());
    }
    sqlx::query_scalar("SELECT role FROM organization_memberships WHERE organization_id=$1 AND user_id=$2 AND disabled_at IS NULL FOR SHARE")
        .bind(org).bind(u.user_id).fetch_optional(&mut **tx).await?.ok_or_else(denied)
}
pub(super) async fn org_admin(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    org: Uuid,
) -> Result<(), ApiError> {
    if !admin(&live_org_role(tx, u, org).await?) {
        return Err(denied());
    }
    Ok(())
}
pub(super) async fn workspace_role(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    org: Uuid,
    ws: Uuid,
) -> Result<String, ApiError> {
    let role = live_org_role(tx, u, org).await?;
    let (kind, owner): (String, Option<Uuid>) = sqlx::query_as("SELECT kind,owner_user_id FROM workspaces WHERE organization_id=$1 AND id=$2 AND disabled_at IS NULL FOR SHARE")
        .bind(org).bind(ws).fetch_optional(&mut **tx).await?.ok_or_else(missing)?;
    if kind == "personal" {
        if owner != Some(u.user_id) {
            return Err(denied());
        }
        // Owning a personal workspace does not confer delegated administrator rights.
        return Ok(role);
    }
    if admin(&role) {
        return Ok(role);
    }
    sqlx::query_scalar("SELECT role FROM workspace_memberships WHERE organization_id=$1 AND workspace_id=$2 AND user_id=$3 AND disabled_at IS NULL FOR SHARE")
        .bind(org).bind(ws).bind(u.user_id).fetch_optional(&mut **tx).await?.ok_or_else(denied)
}
pub(super) async fn workspace_org(s: &Store, ws: Uuid) -> Result<Uuid, ApiError> {
    sqlx::query_scalar("SELECT organization_id FROM workspaces WHERE id=$1 AND disabled_at IS NULL")
        .bind(ws)
        .fetch_optional(&s.pool)
        .await?
        .ok_or_else(missing)
}
pub(super) async fn workspace_tx<'a>(
    s: &'a Store,
    u: &BrowserPrincipal,
    ws: Uuid,
) -> Result<(Transaction<'a, Postgres>, WorkspaceAccess), ApiError> {
    let org = workspace_org(s, ws).await?;
    let mut tx = org_tx(s, org).await?;
    let mut role = workspace_role(&mut tx, u, org, ws).await?;
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(ws)
        .fetch_one(&mut *tx)
        .await?;
    if kind == "personal" {
        role = "owner".into();
    }
    Ok((tx, WorkspaceAccess { org, role, kind }))
}
async fn assigned(
    tx: &mut Transaction<'_, Postgres>,
    org: Uuid,
    model: Uuid,
) -> Result<(), ApiError> {
    sqlx::query_scalar::<_,Uuid>("SELECT model_id FROM organization_model_grants WHERE organization_id=$1 AND model_id=$2 FOR SHARE")
        .bind(org).bind(model).fetch_optional(&mut **tx).await?.ok_or_else(missing)?;
    Ok(())
}
// Keep scalar query fields explicit: serde_urlencoded does not support numeric
// fields flattened through an untyped map. Reuse Page's bounds validation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    q: Option<String>,
    enabled: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    q: Option<String>,
    enabled: Option<bool>,
    model_id: Option<Uuid>,
    provider_connection_id: Option<Uuid>,
}
fn catalog_search(q: Option<&str>) -> Result<Option<&str>, ApiError> {
    if q.is_some_and(|q| q.chars().count() > 200) {
        return Err(invalid());
    }
    Ok(q.map(str::trim).filter(|q| !q.is_empty()))
}
async fn platform_collection(
    s: &Store,
    u: &BrowserPrincipal,
    p: &CatalogQuery,
    query: &str,
) -> ApiResult {
    let mut tx = catalog_tx(s, u, false).await?;
    let (limit, offset) = Page {
        limit: p.limit,
        offset: p.offset,
    }
    .bounds()?;
    let q = catalog_search(p.q.as_deref())?;
    let data: Vec<Value> = sqlx::query_scalar(query)
        .bind(limit)
        .bind(offset)
        .bind(q)
        .bind(p.enabled)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn platform_providers(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<CatalogQuery>,
) -> ApiResult {
    // strpos is a literal substring match: %, _ and backslash are not patterns.
    platform_collection(&s,&u,&p,"SELECT jsonb_build_object('id',id,'name',name,'provider',provider,'endpoint',endpoint,'region',region,'enabled',enabled) FROM provider_connections WHERE ($3::text IS NULL OR strpos(lower(name),lower($3))>0 OR strpos(lower(provider),lower($3))>0 OR strpos(lower(endpoint),lower($3))>0 OR strpos(lower(region),lower($3))>0) AND ($4::boolean IS NULL OR enabled=$4) ORDER BY name,id LIMIT $1 OFFSET $2").await
}
async fn platform_models(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<CatalogQuery>,
) -> ApiResult {
    platform_collection(&s,&u,&p,"SELECT jsonb_build_object('id',id,'public_name',public_name,'display_name',display_name,'enabled',enabled) FROM models WHERE ($3::text IS NULL OR strpos(lower(public_name),lower($3))>0 OR strpos(lower(display_name),lower($3))>0) AND ($4::boolean IS NULL OR enabled=$4) ORDER BY public_name,id LIMIT $1 OFFSET $2").await
}
async fn platform_deployments(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<DeploymentQuery>,
) -> ApiResult {
    let mut tx = catalog_tx(&s, &u, false).await?;
    let (limit, offset) = Page {
        limit: p.limit,
        offset: p.offset,
    }
    .bounds()?;
    let q = catalog_search(p.q.as_deref())?;
    let data: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',id,'model_id',model_id,'provider_connection_id',provider_connection_id,'upstream_model',upstream_model,'enabled',enabled) FROM deployments WHERE ($3::text IS NULL OR strpos(lower(upstream_model),lower($3))>0) AND ($4::boolean IS NULL OR enabled=$4) AND ($5::uuid IS NULL OR model_id=$5) AND ($6::uuid IS NULL OR provider_connection_id=$6) ORDER BY created_at,id LIMIT $1 OFFSET $2")
        .bind(limit).bind(offset).bind(q).bind(p.enabled).bind(p.model_id).bind(p.provider_connection_id)
        .fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn platform_detail(s: &Store, u: &BrowserPrincipal, id: Uuid, query: &str) -> ApiResult {
    let mut tx = catalog_tx(s, u, false).await?;
    let data: Value = sqlx::query_scalar(query)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    tx.commit().await?;
    Ok(Json(data))
}
async fn platform_provider(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    platform_detail(&s,&u,id,"SELECT jsonb_build_object('id',id,'name',name,'provider',provider,'endpoint',endpoint,'region',region,'enabled',enabled) FROM provider_connections WHERE id=$1").await
}
async fn platform_model(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    platform_detail(&s,&u,id,"SELECT jsonb_build_object('id',id,'public_name',public_name,'display_name',display_name,'enabled',enabled) FROM models WHERE id=$1").await
}
async fn platform_deployment(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    platform_detail(&s,&u,id,"SELECT jsonb_build_object('id',id,'model_id',model_id,'provider_connection_id',provider_connection_id,'upstream_model',upstream_model,'enabled',enabled) FROM deployments WHERE id=$1").await
}
pub(super) async fn models(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(org): Path<Uuid>,
    Query(p): Query<Page>,
) -> ApiResult {
    let mut tx = org_tx(&s, org).await?;
    org_admin(&mut tx, &u, org).await?;
    let (limit, offset) = p.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',m.id,'public_name',g.public_name,'display_name',m.display_name,'enabled',m.enabled,'personal_enabled',g.personal_enabled) FROM organization_model_grants g JOIN models m ON m.id=g.model_id WHERE g.organization_id=$1 ORDER BY g.public_name,m.id LIMIT $2 OFFSET $3")
        .bind(org).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
// Legacy infrastructure paths intentionally do not expose configuration. Clients
// must use the platform routes; there is no organization-admin infrastructure API.
pub(super) async fn providers() -> ApiResult {
    Err(denied())
}
pub(super) async fn deployments() -> ApiResult {
    Err(denied())
}
pub(super) async fn create_provider() -> ApiResult {
    Err(denied())
}
pub(super) async fn update_provider() -> ApiResult {
    Err(denied())
}
pub(super) async fn create_model() -> ApiResult {
    Err(denied())
}
pub(super) async fn update_model() -> ApiResult {
    Err(denied())
}
pub(super) async fn create_deployment() -> ApiResult {
    Err(denied())
}
pub(super) async fn update_deployment() -> ApiResult {
    Err(denied())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderInput {
    name: String,
    provider: String,
    credential_ref: String,
    endpoint: Option<String>,
    region: Option<String>,
    enabled: bool,
}
fn credential_valid(provider: &str, reference: &str) -> bool {
    if provider == "bedrock" {
        return reference == "aws:default";
    }
    let Some(name) = reference.strip_prefix("env:") else {
        return false;
    };
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && std::env::var("GATEWAY_SECRET_ENV_ALLOWLIST")
            .unwrap_or_default()
            .split(',')
            .any(|s| s.trim() == name)
}
fn provider_valid(body: &ProviderInput) -> bool {
    if !valid_name(&body.name) || !credential_valid(&body.provider, &body.credential_ref) {
        return false;
    }
    match body.provider.as_str() {
        "openai" | "anthropic" => {
            let base = if body.provider == "openai" {
                "https://api.openai.com/v1"
            } else {
                "https://api.anthropic.com/v1"
            };
            body.endpoint
                .as_deref()
                .is_none_or(|v| v == base || v == format!("{base}/"))
                && body.region.as_deref().is_none_or(str::is_empty)
        }
        "bedrock" => {
            body.endpoint.is_none()
                && body.region.as_ref().is_some_and(|v| {
                    !v.is_empty()
                        && v.len() <= 32
                        && v.bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                })
        }
        _ => false,
    }
}
async fn platform_create_provider(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<ProviderInput>,
) -> ApiResult {
    let mut tx = catalog_tx(&s, &u, true).await?;
    if !provider_valid(&b) {
        return Err(invalid());
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,region,enabled) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(id).bind(b.name).bind(b.provider).bind(b.credential_ref).bind(b.endpoint).bind(b.region).bind(b.enabled).execute(&mut *tx).await?;
    audit(&mut tx, u.user_id, None, None, "provider.created", Some(id)).await?;
    tx.commit().await?;
    Ok(identifier(id))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderUpdate {
    enabled: bool,
    credential_ref: Option<String>,
}
async fn platform_update_provider(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<ProviderUpdate>,
) -> ApiResult {
    let mut tx = catalog_tx(&s, &u, true).await?;
    let provider: String =
        sqlx::query_scalar("SELECT provider FROM provider_connections WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(missing)?;
    if b.credential_ref
        .as_ref()
        .is_some_and(|r| !credential_valid(&provider, r))
    {
        return Err(invalid());
    }
    sqlx::query("UPDATE provider_connections SET enabled=$2,credential_ref=coalesce($3,credential_ref) WHERE id=$1").bind(id).bind(b.enabled).bind(b.credential_ref).execute(&mut *tx).await?;
    audit(&mut tx, u.user_id, None, None, "provider.updated", Some(id)).await?;
    tx.commit().await?;
    Ok(ok())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelInput {
    public_name: String,
    display_name: String,
    enabled: bool,
}
fn alias_valid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/-_.:".contains(&c))
}
async fn platform_create_model(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<ModelInput>,
) -> ApiResult {
    let mut tx = catalog_tx(&s, &u, true).await?;
    if !valid_name(&b.display_name) || !alias_valid(&b.public_name) {
        return Err(invalid());
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO models(id,public_name,display_name,enabled) VALUES($1,$2,$3,$4)")
        .bind(id)
        .bind(b.public_name)
        .bind(b.display_name)
        .bind(b.enabled)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, u.user_id, None, None, "model.created", Some(id)).await?;
    tx.commit().await?;
    Ok(identifier(id))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Enabled {
    enabled: bool,
}
async fn toggle(
    s: Store,
    u: BrowserPrincipal,
    id: Uuid,
    b: Enabled,
    table: &str,
    action: &str,
) -> ApiResult {
    let mut tx = catalog_tx(&s, &u, true).await?;
    let changed = sqlx::query(&format!("UPDATE {table} SET enabled=$2 WHERE id=$1"))
        .bind(id)
        .bind(b.enabled)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if changed != 1 {
        return Err(missing());
    }
    audit(&mut tx, u.user_id, None, None, action, Some(id)).await?;
    tx.commit().await?;
    Ok(ok())
}
async fn platform_update_model(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<Enabled>,
) -> ApiResult {
    toggle(s, u, id, b, "models", "model.updated").await
}
async fn platform_update_deployment(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<Enabled>,
) -> ApiResult {
    toggle(s, u, id, b, "deployments", "deployment.updated").await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentInput {
    model_id: Uuid,
    provider_connection_id: Uuid,
    upstream_model: String,
    enabled: bool,
}
async fn platform_create_deployment(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<DeploymentInput>,
) -> ApiResult {
    let mut tx = catalog_tx(&s, &u, true).await?;
    if b.upstream_model.trim().is_empty()
        || b.upstream_model.len() > 512
        || b.upstream_model.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,$4,$5)").bind(id).bind(b.model_id).bind(b.provider_connection_id).bind(b.upstream_model).bind(b.enabled).execute(&mut *tx).await?;
    audit(
        &mut tx,
        u.user_id,
        None,
        None,
        "deployment.created",
        Some(id),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Assignment {
    public_name: Option<String>,
}
async fn assign_model(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((org, model)): Path<(Uuid, Uuid)>,
    Json(b): Json<Assignment>,
) -> ApiResult {
    let mut tx = org_tx(&s, org).await?;
    operator_lock(&mut tx, &u).await?;
    let canonical: String =
        sqlx::query_scalar("SELECT public_name FROM models WHERE id=$1 FOR SHARE")
            .bind(model)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(missing)?;
    let alias = b.public_name.unwrap_or(canonical);
    if !alias_valid(&alias) {
        return Err(invalid());
    }
    sqlx::query("INSERT INTO organization_model_grants(organization_id,model_id,public_name) VALUES($1,$2,$3) ON CONFLICT(organization_id,model_id) DO UPDATE SET public_name=excluded.public_name").bind(org).bind(model).bind(alias).execute(&mut *tx).await?;
    audit(
        &mut tx,
        u.user_id,
        Some(org),
        None,
        "model.organization_assigned",
        Some(model),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
async fn unassign_model(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((org, model)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    let mut tx = org_tx(&s, org).await?;
    operator_lock(&mut tx, &u).await?;
    sqlx::query("DELETE FROM workspace_model_grants WHERE organization_id=$1 AND model_id=$2")
        .bind(org)
        .bind(model)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM user_model_grants WHERE organization_id=$1 AND model_id=$2")
        .bind(org)
        .bind(model)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM organization_model_grants WHERE organization_id=$1 AND model_id=$2")
        .bind(org)
        .bind(model)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        u.user_id,
        Some(org),
        None,
        "model.organization_revoked",
        Some(model),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
pub(super) async fn personal_access(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((org, id)): Path<(Uuid, Uuid)>,
    Json(b): Json<Enabled>,
) -> ApiResult {
    let mut tx = org_tx(&s, org).await?;
    org_admin(&mut tx, &u, org).await?;
    assigned(&mut tx, org, id).await?;
    sqlx::query("UPDATE organization_model_grants SET personal_enabled=$3 WHERE organization_id=$1 AND model_id=$2").bind(org).bind(id).bind(b.enabled).execute(&mut *tx).await?;
    if b.enabled {
        sqlx::query("INSERT INTO workspace_model_grants(organization_id,workspace_id,model_id) SELECT organization_id,id,$2 FROM workspaces WHERE organization_id=$1 AND kind='personal' ON CONFLICT DO NOTHING").bind(org).bind(id).execute(&mut *tx).await?;
    } else {
        sqlx::query("DELETE FROM workspace_model_grants g USING workspaces w WHERE g.organization_id=$1 AND g.model_id=$2 AND w.organization_id=g.organization_id AND w.id=g.workspace_id AND w.kind='personal'").bind(org).bind(id).execute(&mut *tx).await?;
    }
    audit(
        &mut tx,
        u.user_id,
        Some(org),
        None,
        "model.personal_access_updated",
        Some(id),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
pub(super) async fn grants(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (mut tx, a) = workspace_tx(&s, &u, ws).await?;
    let (limit, offset) = p.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('model_id',m.id,'public_name',o.public_name,'display_name',m.display_name,'workspace_granted',g.model_id IS NOT NULL,'individual_granted',ug.model_id IS NOT NULL) FROM organization_model_grants o JOIN models m ON m.id=o.model_id LEFT JOIN workspace_model_grants g ON g.organization_id=o.organization_id AND g.model_id=o.model_id AND g.workspace_id=$2 LEFT JOIN user_model_grants ug ON ug.organization_id=o.organization_id AND ug.model_id=o.model_id AND ug.user_id=$5 AND $6::boolean WHERE o.organization_id=$1 AND (g.model_id IS NOT NULL OR ug.model_id IS NOT NULL) ORDER BY o.public_name LIMIT $3 OFFSET $4")
        .bind(a.org).bind(ws).bind(limit).bind(offset).bind(u.user_id).bind(a.kind == "personal").fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Grant {
    model_id: Uuid,
}
pub(super) async fn grant(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(b): Json<Grant>,
) -> ApiResult {
    change_workspace_grant(s, u, ws, b.model_id, true).await
}
pub(super) async fn revoke_grant(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, model)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    change_workspace_grant(s, u, ws, model, false).await
}
async fn change_workspace_grant(
    s: Store,
    u: BrowserPrincipal,
    ws: Uuid,
    model: Uuid,
    add: bool,
) -> ApiResult {
    let (mut tx, a) = workspace_tx(&s, &u, ws).await?;
    org_admin(&mut tx, &u, a.org).await?;
    assigned(&mut tx, a.org, model).await?;
    sqlx::query(if add {"INSERT INTO workspace_model_grants(organization_id,workspace_id,model_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING"} else {"DELETE FROM workspace_model_grants WHERE organization_id=$1 AND workspace_id=$2 AND model_id=$3"})
        .bind(a.org).bind(ws).bind(model).execute(&mut *tx).await?;
    audit(
        &mut tx,
        u.user_id,
        Some(a.org),
        Some(ws),
        if add {
            "model.granted"
        } else {
            "model.grant_revoked"
        },
        Some(model),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
async fn active_target(
    tx: &mut Transaction<'_, Postgres>,
    org: Uuid,
    user: Uuid,
) -> Result<(), ApiError> {
    sqlx::query_scalar::<_,Uuid>("SELECT u.id FROM users u JOIN organization_memberships m ON m.user_id=u.id WHERE m.organization_id=$1 AND u.id=$2 AND u.disabled_at IS NULL AND m.disabled_at IS NULL FOR SHARE OF u,m").bind(org).bind(user).fetch_optional(&mut **tx).await?.ok_or_else(missing)?;
    Ok(())
}
async fn user_grants(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((org, user)): Path<(Uuid, Uuid)>,
    Query(p): Query<Page>,
) -> ApiResult {
    let mut tx = org_tx(&s, org).await?;
    org_admin(&mut tx, &u, org).await?;
    active_target(&mut tx, org, user).await?;
    let (limit, offset) = p.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('model_id',m.id,'public_name',o.public_name,'display_name',m.display_name) FROM user_model_grants g JOIN organization_model_grants o ON o.organization_id=g.organization_id AND o.model_id=g.model_id JOIN models m ON m.id=g.model_id WHERE g.organization_id=$1 AND g.user_id=$2 ORDER BY o.public_name LIMIT $3 OFFSET $4").bind(org).bind(user).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn grant_user(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((org, user)): Path<(Uuid, Uuid)>,
    Json(b): Json<Grant>,
) -> ApiResult {
    change_user_grant(s, u, org, user, b.model_id, true).await
}
async fn revoke_user(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((org, user, model)): Path<(Uuid, Uuid, Uuid)>,
) -> ApiResult {
    change_user_grant(s, u, org, user, model, false).await
}
async fn change_user_grant(
    s: Store,
    u: BrowserPrincipal,
    org: Uuid,
    user: Uuid,
    model: Uuid,
    add: bool,
) -> ApiResult {
    let mut tx = org_tx(&s, org).await?;
    org_admin(&mut tx, &u, org).await?;
    active_target(&mut tx, org, user).await?;
    assigned(&mut tx, org, model).await?;
    sqlx::query(if add {"INSERT INTO user_model_grants(organization_id,user_id,model_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING"} else {"DELETE FROM user_model_grants WHERE organization_id=$1 AND user_id=$2 AND model_id=$3"}).bind(org).bind(user).bind(model).execute(&mut *tx).await?;
    // Deliberately no personal-workspace lookup or metadata in this API or audit.
    audit(
        &mut tx,
        u.user_id,
        Some(org),
        None,
        if add {
            "model.user_granted"
        } else {
            "model.user_revoked"
        },
        Some(user),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
async fn platform_audit(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<Page>,
) -> ApiResult {
    let mut tx = catalog_tx(&s, &u, false).await?;
    let (limit, offset) = p.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',a.id,'actor_user_id',a.actor_user_id,'organization_id',a.organization_id,'workspace_id',a.workspace_id,'action',a.action,'target_id',a.target_id,'created_at',a.created_at) FROM audit_events a LEFT JOIN workspaces w ON w.organization_id=a.organization_id AND w.id=a.workspace_id WHERE a.workspace_id IS NULL OR w.kind IN ('team','project') OR w.owner_user_id=$1 ORDER BY a.created_at DESC,a.id LIMIT $2 OFFSET $3").bind(u.user_id).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}

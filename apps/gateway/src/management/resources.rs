use super::*;

/// The catalog advisory lock precedes the single installation lock everywhere.
/// Holding installation through audit/commit serializes admission and authority mutations.
pub(crate) async fn catalog_lock(
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
/// The lock-free read-only snapshot ([`Store::snapshot`]) for management
/// lists: it neither waits for nor blocks the installation and catalog locks
/// that management writes hold, and PostgreSQL cancels any statement that
/// runs longer than its 10 s timeout. Read-only handlers only.
pub(crate) async fn read_snapshot(s: &Store) -> Result<Transaction<'static, Postgres>, ApiError> {
    Ok(s.snapshot().await?)
}
/// [`read_snapshot`] for a Platform Admin or Auditor (live role check).
pub(crate) async fn platform_read_tx(
    s: &Store,
    u: &impl Actor,
) -> Result<Transaction<'static, Postgres>, ApiError> {
    let mut tx = read_snapshot(s).await?;
    platform_read_snapshot(&mut tx, u.user_id()).await?;
    Ok(tx)
}
/// [`workspace_tx`] for read-only handlers: the same live access rules in a
/// [`read_snapshot`], without row locks.
pub(crate) async fn workspace_snapshot_tx(
    s: &Store,
    u: &impl Actor,
    ws: Uuid,
) -> Result<(Transaction<'static, Postgres>, WorkspaceAccess), ApiError> {
    let mut tx = read_snapshot(s).await?;
    let a = workspace_access_snapshot(&mut tx, u, ws).await?;
    Ok((tx, a))
}
pub(crate) async fn installation_tx(s: &Store) -> Result<Transaction<'_, Postgres>, ApiError> {
    let mut tx = crate::db::begin(&s.pool).await?;
    catalog_lock(&mut tx, false).await?;
    installation_lock(&mut tx).await?;
    Ok(tx)
}
async fn installation_lock(tx: &mut Transaction<'_, Postgres>) -> Result<(), ApiError> {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM installation WHERE singleton FOR NO KEY UPDATE")
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)?;
    Ok(())
}
pub(crate) async fn platform_role(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<String, ApiError> {
    role_inner(tx, user, true).await
}
/// [`platform_role`] without row locks, for `crate::reporting` snapshots
/// (read-only transactions cannot lock rows). Same live checks, evaluated in
/// the caller's snapshot.
pub(crate) async fn platform_role_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<String, ApiError> {
    role_inner(tx, user, false).await
}
async fn role_inner(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
    lock: bool,
) -> Result<String, ApiError> {
    if !lock {
        // This view already requires an active, non-cleaned user and live grants.
        return sqlx::query_scalar("SELECT role FROM effective_platform_roles WHERE user_id=$1")
            .bind(user)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(denied);
    }
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=$1 AND disabled_at IS NULL AND cleaned_at IS NULL FOR SHARE",
    )
    .bind(user)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(denied)?;
    sqlx::query_scalar("SELECT role FROM effective_platform_roles WHERE user_id=$1")
        .bind(user)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(denied)
}
pub(crate) async fn platform_read(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<(), ApiError> {
    if !matches!(platform_role(tx, user).await?.as_str(), "admin" | "auditor") {
        return Err(denied());
    }
    Ok(())
}
/// [`platform_read`] in a lock-free snapshot (`crate::reporting`).
pub(crate) async fn platform_read_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<(), ApiError> {
    if !matches!(
        platform_role_snapshot(tx, user).await?.as_str(),
        "admin" | "auditor"
    ) {
        return Err(denied());
    }
    Ok(())
}
pub(crate) async fn platform_write(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<(), ApiError> {
    if platform_role(tx, user).await? != "admin" {
        return Err(denied());
    }
    Ok(())
}
pub(crate) async fn catalog_tx<'a>(
    s: &'a Store,
    u: &impl Actor,
    write: bool,
) -> Result<Transaction<'a, Postgres>, ApiError> {
    let mut tx = crate::db::begin(&s.pool).await?;
    catalog_lock(&mut tx, write).await?;
    installation_lock(&mut tx).await?;
    if write {
        platform_write(&mut tx, u.user_id()).await?
    } else {
        platform_read(&mut tx, u.user_id()).await?
    }
    Ok(tx)
}
/// Workspace authority comes only from actual membership (or personal
/// ownership). Platform Admins/Auditors without membership keep read-only
/// metadata access (`platform_reader`), never owner powers, keys or requests.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceAccess {
    pub(crate) kind: String,
    pub(crate) owner: bool,
    pub(crate) admin: bool,
    pub(crate) view_all_activity: bool,
    pub(crate) member: bool,
    pub(crate) platform_reader: bool,
    pub(crate) platform_writer: bool,
    /// Only from [`workspace_read_tx`]: a disabled shared workspace opened
    /// read-only by a Platform Admin/Auditor (no member or admin powers).
    pub(crate) disabled: bool,
}
/// Presentation snapshot of workspace powers (shared by `/me` and workspace GET).
pub(crate) fn capabilities(kind: &str, admin: bool, member: bool) -> Value {
    let personal = kind == "personal";
    json!({"issue_own_key":personal||member,"manage_members":!personal&&admin,"manage_service_accounts":!personal&&admin,"manage_policy":!personal&&admin,"delegate_models":admin,"view_all_activity":admin,"rename":!personal&&admin,"manage_keys":admin})
}
impl WorkspaceAccess {
    pub(crate) fn capabilities(&self) -> Value {
        capabilities(&self.kind, self.admin, self.member)
    }
    /// Read-only workspace metadata (models, members): members, or platform readers for shared workspaces.
    pub(crate) fn metadata_read(&self) -> Result<(), ApiError> {
        if self.admin || self.member || (self.platform_reader && shared(&self.kind)) {
            Ok(())
        } else {
            Err(denied())
        }
    }
}
pub(crate) async fn workspace_tx<'a>(
    s: &'a Store,
    u: &impl Actor,
    ws: Uuid,
) -> Result<(Transaction<'a, Postgres>, WorkspaceAccess), ApiError> {
    let mut tx = installation_tx(s).await?;
    let a = workspace_access(&mut tx, u, ws).await?;
    Ok((tx, a))
}
/// Read-only variant of [`workspace_tx`]: Platform Admins/Auditors may also
/// open a disabled Team/Project to read its configuration (limits, catalogs,
/// models, effective access). Everyone else gets the usual 404, and a disabled
/// workspace never yields member/admin powers, keys or request details.
/// Handlers using it must not mutate.
pub(crate) async fn workspace_read_tx<'a>(
    s: &'a Store,
    u: &impl Actor,
    ws: Uuid,
) -> Result<(Transaction<'a, Postgres>, WorkspaceAccess), ApiError> {
    let mut tx = installation_tx(s).await?;
    let a = access_inner(&mut tx, u, ws, true, true).await?;
    Ok((tx, a))
}
pub(crate) async fn workspace_access(
    tx: &mut Transaction<'_, Postgres>,
    u: &impl Actor,
    ws: Uuid,
) -> Result<WorkspaceAccess, ApiError> {
    access_inner(tx, u, ws, false, true).await
}
/// [`workspace_access`] in a lock-free snapshot (`crate::reporting`): the
/// same live membership/ownership rules, without row locks. Only for
/// read-only handlers; a disabled workspace is missing, as for
/// [`workspace_access`].
pub(crate) async fn workspace_access_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    u: &impl Actor,
    ws: Uuid,
) -> Result<WorkspaceAccess, ApiError> {
    access_inner(tx, u, ws, false, false).await
}
/// Route preflight, in a primary read snapshot; handlers retain their own
/// transaction/locking checks so a preflight cannot preserve revoked authority.
pub(super) async fn workspace_boundary_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    u: &impl Actor,
    ws: Uuid,
    allow_disabled: bool,
) -> Result<WorkspaceAccess, ApiError> {
    access_inner(tx, u, ws, allow_disabled, false).await
}
/// Administrative metadata writes require the platform role first, even for
/// an unknown workspace. Do not perform a workspace read for a non-admin.
pub(super) async fn workspace_platform_write_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    u: &impl Actor,
    ws: Uuid,
) -> Result<(), ApiError> {
    let role = platform_role_snapshot(tx, u.user_id()).await?;
    if role != "admin" {
        return Err(denied());
    }
    let (kind, owner, disabled): (String, Option<Uuid>, bool) = sqlx::query_as(
        "SELECT kind,owner_user_id,disabled_at IS NOT NULL FROM workspaces WHERE id=$1",
    )
    .bind(ws)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(missing)?;
    // Membership does not grant this platform-controlled operation. The same
    // decision still enforces personal ownership and disabled-workspace rules.
    let a = workspace_authority(u.user_id(), &role, kind, owner, disabled, None, false)?;
    debug_assert!(a.platform_writer);
    a.metadata_read()
}

async fn access_inner(
    tx: &mut Transaction<'_, Postgres>,
    u: &impl Actor,
    ws: Uuid,
    allow_disabled: bool,
    lock: bool,
) -> Result<WorkspaceAccess, ApiError> {
    let (role, kind, owner_user, disabled, membership) = if lock {
        let role = role_inner(tx, u.user_id(), true).await?;
        let (kind, owner_user, disabled): (String, Option<Uuid>, bool) = sqlx::query_as(
            "SELECT kind,owner_user_id,disabled_at IS NOT NULL FROM workspaces WHERE id=$1 AND ($2 OR disabled_at IS NULL) FOR NO KEY UPDATE",
        ).bind(ws).bind(allow_disabled).fetch_optional(&mut **tx).await?.ok_or_else(missing)?;
        let membership = if !disabled && shared(&kind) {
            sqlx::query_scalar("SELECT role FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2")
                .bind(ws).bind(u.user_id()).fetch_optional(&mut **tx).await?
        } else {
            None
        };
        (role, kind, owner_user, disabled, membership)
    } else {
        // The same live authority views as the locked path, fetched in one
        // statement. A LEFT JOIN preserves entitlement-before-missing-workspace
        // semantics. Authorization decisions below are shared by both paths.
        type AuthorityRow = (
            String,
            Option<String>,
            Option<Uuid>,
            Option<bool>,
            Option<String>,
        );
        let (role,kind,owner_user,disabled,membership): AuthorityRow = sqlx::query_as(
            "SELECT p.role,w.kind,w.owner_user_id,w.disabled_at IS NOT NULL,m.role FROM effective_platform_roles p LEFT JOIN workspaces w ON w.id=$2 LEFT JOIN effective_workspace_memberships m ON m.workspace_id=w.id AND m.user_id=p.user_id WHERE p.user_id=$1",
        ).bind(u.user_id()).bind(ws).fetch_optional(&mut **tx).await?.ok_or_else(denied)?;
        (
            role,
            kind.ok_or_else(missing)?,
            owner_user,
            disabled.ok_or_else(missing)?,
            membership,
        )
    };
    workspace_authority(
        u.user_id(),
        &role,
        kind,
        owner_user,
        disabled,
        membership,
        allow_disabled,
    )
}

#[cfg(test)]
#[path = "workspace_authority_tests.rs"]
mod authority_tests;

// One decision implementation for transactional handlers and route preflight.
fn workspace_authority(
    user: Uuid,
    role: &str,
    kind: String,
    owner_user: Option<Uuid>,
    disabled: bool,
    membership: Option<String>,
    allow_disabled: bool,
) -> Result<WorkspaceAccess, ApiError> {
    let platform_reader = matches!(role, "admin" | "auditor");
    let platform_writer = role == "admin";
    if disabled {
        if !(allow_disabled && platform_reader && shared(&kind)) {
            return Err(missing());
        }
        return Ok(WorkspaceAccess {
            kind,
            owner: false,
            admin: false,
            view_all_activity: false,
            member: false,
            platform_reader,
            platform_writer,
            disabled,
        });
    }
    if kind == "personal" {
        if owner_user != Some(user) {
            return Err(denied());
        }
        return Ok(WorkspaceAccess {
            kind,
            owner: true,
            admin: true,
            view_all_activity: true,
            member: true,
            platform_reader,
            platform_writer,
            disabled,
        });
    }
    if !shared(&kind) || (membership.is_none() && !platform_reader) {
        return Err(denied());
    }
    let owner = membership.as_deref() == Some("owner");
    let admin = owner || membership.as_deref() == Some("admin");
    Ok(WorkspaceAccess {
        kind,
        owner,
        admin,
        view_all_activity: admin,
        member: membership.is_some(),
        platform_reader,
        platform_writer,
        disabled,
    })
}
pub(crate) fn detail_access(a: &WorkspaceAccess) -> Result<(), ApiError> {
    if !a.admin && !a.member {
        return Err(denied());
    }
    Ok(())
}
pub(crate) fn manage(a: &WorkspaceAccess) -> Result<(), ApiError> {
    if !a.admin {
        return Err(denied());
    }
    Ok(())
}
/// Audit is inside the resource transaction; no plaintext, identity claims or arbitrary strings.
/// Only typed safe flags/counts/provenance are retained, not untrusted metadata fields.
/// Carries admin-token attribution (`u.token_id()`) when the acting
/// principal is a token (#37 PR2: "Business-action audit events carry
/// token attribution"); `NULL` for a browser session, exactly as before.
pub(crate) async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    u: &impl Actor,
    ws: Option<Uuid>,
    action: &str,
    resource_type: &str,
    resource_id: Option<Uuid>,
    metadata: Value,
) -> Result<(), ApiError> {
    let mut safe = serde_json::Map::new();
    if let Some(object) = metadata.as_object() {
        for (k, v) in object {
            if matches!(
                k.as_str(),
                "enabled"
                    | "disabled"
                    | "count"
                    | "mode"
                    | "source"
                    | "role"
                    | "kind"
                    | "pricing_version"
                    | "rotation"
                    | "budget_period"
                    | "previous_budget_period"
                    | "object_visibility"
                    | "previous_object_visibility"
                    | "reason"
                    | "email_changed"
                    | "oidc_link_changed"
            ) && (v.is_boolean()
                || v.is_number()
                || v.as_str().is_some_and(|s| {
                    s.len() <= 32 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                }))
            {
                safe.insert(k.clone(), v.clone());
            }
        }
    }
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,workspace_id,action,resource_type,resource_id,metadata,admin_token_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8)").bind(Uuid::now_v7()).bind(u.user_id()).bind(ws).bind(action).bind(resource_type).bind(resource_id).bind(Value::Object(safe)).bind(u.token_id()).execute(&mut **tx).await?;
    Ok(())
}
pub(super) fn platform_routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/platform/providers",
            get(providers).post(create_provider),
        )
        .route(
            "/api/v1/platform/providers/{id}",
            get(provider).patch(update_provider).delete(delete_provider),
        )
        .route(
            "/api/v1/platform/models",
            get(super::models_ux::platform_models).post(create_model),
        )
        .route(
            "/api/v1/platform/models/{id}",
            get(model).patch(update_model).delete(delete_model),
        )
        .route(
            "/api/v1/platform/deployments",
            get(deployments).post(create_deployment),
        )
        .route(
            "/api/v1/platform/deployments/{id}",
            get(super::models_ux::deployment_detail)
                .patch(update_deployment)
                .delete(delete_deployment),
        )
        .route("/api/v1/platform/audit", get(platform_audit))
        .route("/api/v1/platform/server-policy", get(server_policy))
}
/// Server-controlled provider policy relevant to readiness (Admin/Auditor).
/// Configuration values only; never credentials or endpoints.
async fn server_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
) -> ApiResult {
    let tx = catalog_tx(&s, &u, false).await?;
    tx.commit().await?;
    Ok(Json(server_policy_json(
        crate::providers::openrouter::DataCollection::configured(),
    )))
}
pub(crate) fn server_policy_json(
    data_collection: crate::providers::openrouter::DataCollection,
) -> Value {
    json!({"openrouter":{"data_collection":data_collection.as_str(),"free_models_available":data_collection == crate::providers::openrouter::DataCollection::Allow}})
}
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
pub(super) fn search(q: Option<&str>) -> Result<Option<&str>, ApiError> {
    if q.is_some_and(|s| s.chars().count() > 200) {
        return Err(invalid());
    }
    Ok(q.map(str::trim).filter(|s| !s.is_empty()))
}
async fn collection(s: &Store, u: &BrowserPrincipal, p: CatalogQuery, query: &str) -> ApiResult {
    let mut tx = catalog_tx(s, u, false).await?;
    let (l, o) = Page {
        limit: p.limit,
        offset: p.offset,
    }
    .bounds()?;
    let data: Vec<Value> = sqlx::query_scalar(query)
        .bind(l)
        .bind(o)
        .bind(search(p.q.as_deref())?)
        .bind(p.enabled)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn providers(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<CatalogQuery>,
) -> ApiResult {
    collection(&s,&u,p,"SELECT jsonb_build_object('id',id,'name',name,'provider',provider,'endpoint',endpoint,'region',region,'enabled',enabled,'auth_mode',CASE WHEN credential_ref='none' THEN 'none' ELSE 'credential' END,'aws_auth',CASE WHEN credential_ref='aws:default' THEN 'default' WHEN credential_ref LIKE 'aws:profile:%' THEN 'profile' WHEN credential_ref LIKE 'aws:role:%' THEN 'role' END,'model_count',(SELECT count(DISTINCT d.model_id) FROM deployments d WHERE d.provider_connection_id=provider_connections.id)) FROM provider_connections WHERE ($3::text IS NULL OR strpos(lower(name),lower($3))>0 OR strpos(lower(provider),lower($3))>0 OR strpos(lower(endpoint),lower($3))>0 OR strpos(lower(region),lower($3))>0) AND ($4::boolean IS NULL OR enabled=$4) ORDER BY name,id LIMIT $1 OFFSET $2").await
}
/// A route is usable only while both the deployment and its connection are enabled.
pub(super) const ENABLED_ROUTE: &str = "d.enabled AND EXISTS(SELECT 1 FROM provider_connections p WHERE p.id=d.provider_connection_id AND p.enabled)";
/// An enabled route whose connection profile serves at least one of its
/// model's protocols (`providers::capabilities`, the table the inference path
/// and route validation use). Routes accepted before that validation existed
/// may be enabled yet unable to serve; they count as not serving.
pub(super) static SERVING_ROUTE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "({ENABLED_ROUTE} AND {})",
        crate::providers::capabilities::route_serves_sql()
    )
});
/// Any immutable price version makes a route priced; the latest version is pinned at admission.
pub(super) const PRICED_ROUTE: &str =
    "EXISTS(SELECT 1 FROM deployment_prices dp WHERE dp.deployment_id=d.id)";
/// Active workspaces holding an independent direct assignment for model `m`.
pub(super) const DIRECT_WORKSPACES: &str = "(SELECT count(*) FROM workspace_model_grants g JOIN workspaces w ON w.id=g.workspace_id WHERE g.model_id=m.id AND g.source='direct' AND w.disabled_at IS NULL)";
/// Server-side mirror of the documented UI readiness derivation, used only for aggregate counts.
/// Video models are never ready: OpenAI shut down its Videos API on 2026-09-24 and no adapter offers video.
pub(super) fn ready_model() -> String {
    let serving = &*SERVING_ROUTE;
    format!(
        "(m.enabled AND NOT ('videos'=ANY(m.supported_protocols)) AND EXISTS(SELECT 1 FROM deployments d WHERE d.model_id=m.id AND {serving}) AND (EXISTS(SELECT 1 FROM catalog_models cm WHERE cm.model_id=m.id) OR {DIRECT_WORKSPACES}>0))"
    )
}
/// Configuration check (best effort): the smallest tokens-per-minute limit
/// among the workspace-type defaults that offer
/// model `m` through their default catalogs (every type when none does).
/// Overrides and local/key limits are not considered.
pub(super) const APPLICABLE_TYPE_TOKENS_PER_MINUTE: &str = "(SELECT min(t) FROM (SELECT tp.tokens_per_minute t FROM workspace_type_policies tp WHERE NOT EXISTS(SELECT 1 FROM workspace_type_catalogs wtc JOIN catalog_models cm ON cm.catalog_id=wtc.catalog_id WHERE cm.model_id=m.id) OR EXISTS(SELECT 1 FROM workspace_type_catalogs wtc JOIN catalog_models cm ON cm.catalog_id=wtc.catalog_id WHERE cm.model_id=m.id AND wtc.kind=tp.kind)) limits)";
/// The latest price version's worst-case per-attempt token reservation.
const ROUTE_TOKEN_CEILING: &str = "(SELECT dp.input_token_limit+dp.output_token_limit FROM deployment_prices dp WHERE dp.deployment_id=d.id ORDER BY dp.created_at DESC,dp.id DESC LIMIT 1)";
/// Model metadata plus aggregate readiness counts. Connection labels never include credential references.
/// `serving_routes` are enabled routes whose connection profile can serve the model;
/// `unsupported_routes`/`unsupported_profiles` are the enabled ones that cannot.
pub(super) fn model_json() -> String {
    let serving = &*SERVING_ROUTE;
    format!(
        "jsonb_build_object('id',m.id,'public_name',m.public_name,'display_name',m.display_name,'description',m.description,'supported_protocols',m.supported_protocols,'enabled',m.enabled,'created_at',m.created_at,'readiness',jsonb_build_object('routes',(SELECT count(*) FROM deployments d WHERE d.model_id=m.id),'enabled_routes',(SELECT count(*) FROM deployments d WHERE d.model_id=m.id AND {ENABLED_ROUTE}),'priced_enabled_routes',(SELECT count(*) FROM deployments d WHERE d.model_id=m.id AND {ENABLED_ROUTE} AND {PRICED_ROUTE}),'serving_routes',(SELECT count(*) FROM deployments d WHERE d.model_id=m.id AND {serving}),'unsupported_routes',(SELECT count(*) FROM deployments d WHERE d.model_id=m.id AND {ENABLED_ROUTE} AND NOT {serving}),'unsupported_profiles',(SELECT coalesce(jsonb_agg(DISTINCT p.provider ORDER BY p.provider),'[]'::jsonb) FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.model_id=m.id AND {ENABLED_ROUTE} AND NOT {serving}),'type_tokens_per_minute',{APPLICABLE_TYPE_TOKENS_PER_MINUTE},'routes_over_token_limit',(SELECT count(*) FROM deployments d WHERE d.model_id=m.id AND {ENABLED_ROUTE} AND {ROUTE_TOKEN_CEILING}>{APPLICABLE_TYPE_TOKENS_PER_MINUTE}),'openrouter_free_routes',(SELECT count(*) FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.model_id=m.id AND {ENABLED_ROUTE} AND p.provider='openrouter' AND d.upstream_model LIKE '%:free'),'catalogs',(SELECT count(*) FROM catalog_models cm WHERE cm.model_id=m.id),'direct_workspaces',{DIRECT_WORKSPACES},'connections',(SELECT coalesce(jsonb_agg(jsonb_build_object('id',c.id,'name',c.name) ORDER BY c.name,c.id),'[]'::jsonb) FROM (SELECT p.id,p.name FROM provider_connections p WHERE EXISTS(SELECT 1 FROM deployments d WHERE d.model_id=m.id AND d.provider_connection_id=p.id) ORDER BY p.name,p.id LIMIT 20) c)))"
    )
}
async fn deployments(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<DeploymentQuery>,
) -> ApiResult {
    let mut tx = catalog_tx(&s, &u, false).await?;
    let (l, o) = Page {
        limit: p.limit,
        offset: p.offset,
    }
    .bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',d.id,'model_id',d.model_id,'model_public_name',m.public_name,'provider_connection_id',d.provider_connection_id,'provider_name',p.name,'upstream_model',d.upstream_model,'enabled',d.enabled) FROM deployments d JOIN models m ON m.id=d.model_id JOIN provider_connections p ON p.id=d.provider_connection_id WHERE ($3::text IS NULL OR strpos(lower(d.upstream_model),lower($3))>0) AND ($4::boolean IS NULL OR d.enabled=$4) AND ($5::uuid IS NULL OR d.model_id=$5) AND ($6::uuid IS NULL OR d.provider_connection_id=$6) ORDER BY d.created_at,d.id LIMIT $1 OFFSET $2").bind(l).bind(o).bind(search(p.q.as_deref())?).bind(p.enabled).bind(p.model_id).bind(p.provider_connection_id).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn detail(s: &Store, u: &BrowserPrincipal, id: Uuid, query: &str) -> ApiResult {
    let mut tx = catalog_tx(s, u, false).await?;
    let v: Value = sqlx::query_scalar(query)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    tx.commit().await?;
    Ok(Json(v))
}
async fn provider(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    detail(&s,&u,id,"SELECT jsonb_build_object('id',id,'name',name,'provider',provider,'endpoint',endpoint,'region',region,'enabled',enabled,'auth_mode',CASE WHEN credential_ref='none' THEN 'none' ELSE 'credential' END,'aws_auth',CASE WHEN credential_ref='aws:default' THEN 'default' WHEN credential_ref LIKE 'aws:profile:%' THEN 'profile' WHEN credential_ref LIKE 'aws:role:%' THEN 'role' END,'model_count',(SELECT count(DISTINCT d.model_id) FROM deployments d WHERE d.provider_connection_id=provider_connections.id)) FROM provider_connections WHERE id=$1").await
}
async fn model(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    detail(
        &s,
        &u,
        id,
        &format!("SELECT {} FROM models m WHERE m.id=$1", model_json()),
    )
    .await
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
    /// Bedrock `aws:role:` only; stored inside the canonical reference, never returned.
    #[serde(default)]
    aws_external_id: Option<String>,
    /// Bedrock `aws:role:` only (STS `RoleSessionName`).
    #[serde(default)]
    aws_session_name: Option<String>,
}
/// Fixed OpenRouter HTTPS origin; must match `providers::openrouter::BASE`.
pub(crate) const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";
fn local(provider: &str) -> bool {
    matches!(provider, "vllm" | "sglang" | "ollama" | "openai_compatible")
}
fn credential_valid(
    provider: &str,
    r: &str,
    secrets: &crate::providers::secrets::EnvSecrets,
) -> bool {
    if local(provider) && r == "none" {
        return true;
    }
    if provider == "bedrock" {
        return crate::providers::bedrock::auth::AwsAuth::parse(r).is_some();
    }
    let Some(n) = r.strip_prefix("env:") else {
        return false;
    };
    !n.is_empty()
        && n.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && n.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && secrets.allows(r)
}
/// The stored reference: Bedrock role options arrive as separate fields and are folded
/// into the canonical `aws:role:` reference. Other profiles accept no AWS options.
fn stored_reference(
    provider: &str,
    reference: &str,
    external_id: Option<&str>,
    session_name: Option<&str>,
) -> Option<String> {
    if provider == "bedrock" {
        return crate::providers::bedrock::auth::AwsAuth::from_parts(
            reference,
            external_id,
            session_name,
        )
        .map(|a| a.reference());
    }
    (external_id.is_none() && session_name.is_none()).then(|| reference.to_owned())
}
fn provider_valid(b: &ProviderInput, secrets: &crate::providers::secrets::EnvSecrets) -> bool {
    valid_name(&b.name)
        && credential_valid(&b.provider, &b.credential_ref, secrets)
        && profile_valid(b)
}
/// Fixed-origin cloud profiles accept only their exact HTTPS base (or none).
fn profile_valid(b: &ProviderInput) -> bool {
    match b.provider.as_str() {
        "openai" | "anthropic" | "openrouter" => {
            let base = match b.provider.as_str() {
                "openai" => "https://api.openai.com/v1",
                "anthropic" => "https://api.anthropic.com/v1",
                _ => OPENROUTER_BASE,
            };
            b.endpoint
                .as_deref()
                .is_none_or(|s| s == base || s == format!("{base}/"))
                && b.region.as_deref().is_none_or(str::is_empty)
        }
        // Identity mode, allowlisted profile/endpoint and region (docs/bedrock.md).
        "bedrock" => crate::providers::bedrock::auth::connection_valid(
            &crate::providers::bedrock::auth::Policy::from_env(),
            &b.credential_ref,
            b.endpoint.as_deref(),
            b.region.as_deref(),
        ),
        p if local(p) => b.endpoint.as_deref().is_some_and(|endpoint| {
            crate::providers::local::endpoints::ApprovedEndpoints::from_env(
                &std::env::var("GATEWAY_ENV").unwrap_or_else(|_| "production".into()),
            )
            .is_ok_and(|approvals| {
                approvals
                    .validate_connection(endpoint, &b.credential_ref, b.region.as_deref())
                    .is_ok()
            })
        }),
        _ => false,
    }
}
async fn create_provider(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(mut b): Json<ProviderInput>,
) -> ApiResult {
    let mut tx = catalog_tx(&s, &u, true).await?;
    b.credential_ref = stored_reference(
        &b.provider,
        &b.credential_ref,
        b.aws_external_id.as_deref(),
        b.aws_session_name.as_deref(),
    )
    .ok_or_else(invalid)?;
    if !provider_valid(&b, &s.provider_secrets) {
        return Err(invalid());
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,region,enabled) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(id).bind(b.name).bind(b.provider).bind(b.credential_ref).bind(b.endpoint).bind(b.region).bind(b.enabled).execute(&mut *tx).await?;
    audit(
        &mut tx,
        &u,
        None,
        "provider.created",
        "provider",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderUpdate {
    enabled: bool,
    credential_ref: Option<String>,
    /// With a replacement `aws:role:` reference only; omitted means none.
    aws_external_id: Option<String>,
    aws_session_name: Option<String>,
    /// Bedrock only: an allowlisted HTTPS endpoint, or null for the regional endpoint.
    /// Omitted keeps the current value.
    #[serde(default, deserialize_with = "nullable_patch")]
    endpoint: Option<Option<String>>,
}
async fn update_provider(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<ProviderUpdate>,
) -> ApiResult {
    let mut tx = catalog_tx(&s, &u, true).await?;
    let(name,provider,credential_ref,endpoint,region):(String,String,String,Option<String>,Option<String>)=sqlx::query_as("SELECT name,provider,credential_ref,endpoint,region FROM provider_connections WHERE id=$1 FOR UPDATE").bind(id).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    let replacement = match b.credential_ref.as_deref() {
        Some(r) => Some(
            stored_reference(
                &provider,
                r,
                b.aws_external_id.as_deref(),
                b.aws_session_name.as_deref(),
            )
            .ok_or_else(invalid)?,
        ),
        // Role options belong to a restated role reference.
        None if b.aws_external_id.is_none() && b.aws_session_name.is_none() => None,
        None => return Err(invalid()),
    };
    if b.endpoint.is_some() && provider != "bedrock" {
        return Err(invalid());
    }
    let proposed = ProviderInput {
        name,
        provider,
        credential_ref: replacement.clone().unwrap_or(credential_ref),
        endpoint: b.endpoint.clone().unwrap_or(endpoint),
        region,
        enabled: b.enabled,
        aws_external_id: None,
        aws_session_name: None,
    };
    if !provider_valid(&proposed, &s.provider_secrets) {
        return Err(invalid());
    }
    sqlx::query("UPDATE provider_connections SET enabled=$2,credential_ref=coalesce($3,credential_ref),endpoint=CASE WHEN $4 THEN $5 ELSE endpoint END WHERE id=$1").bind(id).bind(b.enabled).bind(replacement).bind(b.endpoint.is_some()).bind(b.endpoint.flatten()).execute(&mut *tx).await?;
    audit(
        &mut tx,
        &u,
        None,
        "provider.updated",
        "provider",
        Some(id),
        json!({"enabled":b.enabled}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelInput {
    public_name: String,
    display_name: String,
    description: Option<String>,
    enabled: bool,
    #[serde(default = "default_protocols")]
    supported_protocols: Vec<String>,
}
fn default_protocols() -> Vec<String> {
    vec!["chat_completions".into()]
}
/// Known, distinct and workload-compatible: Chat/Responses/Messages may combine;
/// embeddings and each multimodal kind stand alone.
fn protocols_valid(p: &[String]) -> bool {
    crate::inference::types::ApiProtocol::valid_set(p)
}
fn alias_valid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/-_.:".contains(&c))
}
async fn create_model(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<ModelInput>,
) -> ApiResult {
    let mut tx = catalog_tx(&s, &u, true).await?;
    let id = insert_model(&mut tx, &u, b).await?;
    tx.commit().await?;
    Ok(identifier(id))
}
/// Validates, inserts and audits a model. The caller holds an exclusive `catalog_tx`.
pub(super) async fn insert_model(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    b: ModelInput,
) -> Result<Uuid, ApiError> {
    if !valid_name(&b.display_name)
        || !alias_valid(&b.public_name)
        || b.description.as_ref().is_some_and(|s| s.len() > 2000)
        || !protocols_valid(&b.supported_protocols)
    {
        return Err(invalid());
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO models(id,public_name,display_name,description,enabled,supported_protocols) VALUES($1,$2,$3,$4,$5,$6)").bind(id).bind(b.public_name).bind(b.display_name).bind(b.description).bind(b.enabled).bind(b.supported_protocols).execute(&mut **tx).await?;
    audit(tx, u, None, "model.created", "model", Some(id), json!({})).await?;
    Ok(id)
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
) -> DetailedResult {
    let mut tx = catalog_tx(&s, &u, true).await?;
    // Enabling a route its connection cannot serve would make the model look
    // ready while every request fails; disabling is always allowed.
    if table == "deployments" && b.enabled {
        route_capability(&mut tx, id).await?;
    }
    if sqlx::query(&format!("UPDATE {table} SET enabled=$2 WHERE id=$1"))
        .bind(id)
        .bind(b.enabled)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        != 1
    {
        return Err(missing().into());
    }
    if table == "models" && !b.enabled {
        catalogs::retire(&mut tx).await?;
    }
    audit(
        &mut tx,
        &u,
        None,
        "configuration.updated",
        table,
        Some(id),
        json!({"enabled":b.enabled}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelUpdate {
    public_name: Option<String>,
    display_name: Option<String>,
    #[serde(default, deserialize_with = "nullable_patch")]
    description: Option<Option<String>>,
    enabled: Option<bool>,
    #[serde(default, deserialize_with = "protocol_patch")]
    supported_protocols: Option<Vec<String>>,
}
fn protocol_patch<'de, D>(d: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Vec::<String>::deserialize(d).map(Some)
}
fn nullable_patch<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}
async fn update_model(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<ModelUpdate>,
) -> DetailedResult {
    let mut tx = catalog_tx(&s, &u, true).await?;
    if b.public_name.as_ref().is_some_and(|s| !alias_valid(s))
        || b.display_name.as_ref().is_some_and(|s| !valid_name(s))
        || b.description
            .as_ref()
            .and_then(Option::as_ref)
            .is_some_and(|s| s.len() > 2000)
        || b.supported_protocols
            .as_ref()
            .is_some_and(|p| !protocols_valid(p))
    {
        return Err(invalid().into());
    }
    // New protocols must stay servable by every enabled route of the model.
    if let Some(protocols) = &b.supported_protocols {
        let providers: Vec<String> = sqlx::query_scalar("SELECT DISTINCT p.provider FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.model_id=$1 AND d.enabled ORDER BY p.provider").bind(id).fetch_all(&mut *tx).await?;
        for provider in providers {
            if let Some(reason) =
                crate::providers::capabilities::unsupported_reason(&provider, protocols)
            {
                return Err(ManagementError::route_unsupported(format!(
                    "An enabled route can't serve these protocols. {reason}. Disable or replace that route first."
                )));
            }
        }
    }
    if sqlx::query("UPDATE models SET public_name=coalesce($2,public_name),display_name=coalesce($3,display_name),description=CASE WHEN $7 THEN $4 ELSE description END,enabled=coalesce($5,enabled),supported_protocols=coalesce($6,supported_protocols) WHERE id=$1").bind(id).bind(b.public_name).bind(b.display_name).bind(b.description.clone().flatten()).bind(b.enabled).bind(b.supported_protocols).bind(b.description.is_some()).execute(&mut *tx).await?.rows_affected()!=1{return Err(missing().into())}
    if b.enabled == Some(false) {
        catalogs::retire(&mut tx).await?;
    }
    audit(
        &mut tx,
        &u,
        None,
        "model.updated",
        "model",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
// Delete means retire; historical prices/executions and foreign keys are never removed.
async fn delete_provider(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> DetailedResult {
    toggle(s, u, id, Enabled { enabled: false }, "provider_connections").await
}
async fn delete_model(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> DetailedResult {
    toggle(s, u, id, Enabled { enabled: false }, "models").await
}
async fn delete_deployment(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> DetailedResult {
    toggle(s, u, id, Enabled { enabled: false }, "deployments").await
}
async fn update_deployment(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<Enabled>,
) -> DetailedResult {
    toggle(s, u, id, b, "deployments").await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeploymentInput {
    pub(super) model_id: Uuid,
    pub(super) provider_connection_id: Uuid,
    pub(super) upstream_model: String,
    pub(super) enabled: bool,
}
async fn create_deployment(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<DeploymentInput>,
) -> DetailedResult {
    let mut tx = catalog_tx(&s, &u, true).await?;
    let id = insert_deployment(&mut tx, &u, b).await?;
    tx.commit().await?;
    Ok(identifier(id))
}
/// Validates, inserts and audits a deployment. Unknown models/connections are
/// foreign-key conflicts. The caller holds an exclusive `catalog_tx`.
pub(super) async fn insert_deployment(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    b: DeploymentInput,
) -> Result<Uuid, ManagementError> {
    if b.upstream_model.trim().is_empty()
        || b.upstream_model.len() > 512
        || b.upstream_model.chars().any(char::is_control)
    {
        return Err(invalid().into());
    }
    // Bedrock routes take a model ID, an inference profile ID (`us.…`, `global.…`) or a
    // Bedrock ARN in the connection's own region. Unknown connections stay FK conflicts.
    let connection: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT provider,region FROM provider_connections WHERE id=$1")
            .bind(b.provider_connection_id)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some((provider, region)) = &connection
        && provider == "bedrock"
        && !region.as_deref().is_some_and(|r| {
            crate::providers::bedrock::auth::valid_upstream_model(&b.upstream_model, r)
        })
    {
        return Err(invalid().into());
    }
    // The connection profile must serve one of the model's protocols (the
    // same table the inference path uses for `unsupported_capability`).
    let protocols: Option<Vec<String>> =
        sqlx::query_scalar("SELECT supported_protocols FROM models WHERE id=$1")
            .bind(b.model_id)
            .fetch_optional(&mut **tx)
            .await?;
    if let (Some((provider, _)), Some(protocols)) = (&connection, &protocols)
        && let Some(reason) =
            crate::providers::capabilities::unsupported_reason(provider, protocols)
    {
        return Err(ManagementError::route_unsupported(format!("{reason}.")));
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,$4,$5)").bind(id).bind(b.model_id).bind(b.provider_connection_id).bind(b.upstream_model).bind(b.enabled).execute(&mut **tx).await?;
    audit(
        tx,
        u,
        None,
        "deployment.created",
        "deployment",
        Some(id),
        json!({}),
    )
    .await?;
    Ok(id)
}
/// Rejects enabling a route whose connection profile serves none of its
/// model's protocols (400 `route_unsupported_capability`).
async fn route_capability(
    tx: &mut Transaction<'_, Postgres>,
    deployment: Uuid,
) -> Result<(), ManagementError> {
    let row: Option<(String, Vec<String>)> = sqlx::query_as("SELECT p.provider,m.supported_protocols FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id JOIN models m ON m.id=d.model_id WHERE d.id=$1").bind(deployment).fetch_optional(&mut **tx).await?;
    if let Some((provider, protocols)) = row
        && let Some(reason) =
            crate::providers::capabilities::unsupported_reason(&provider, &protocols)
    {
        return Err(ManagementError::route_unsupported(format!("{reason}.")));
    }
    Ok(())
}
/// Audit actions recorded as a side effect of a person signing in (not changes they made).
pub(crate) const SIGN_IN_AUDIT_ACTIONS: &[&str] =
    &["identity.groups_synchronized", "identity.rebound"];
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    /// Only events this user performed (a filter; never widens visibility).
    actor_user_id: Option<String>,
    /// `true` excludes [`SIGN_IN_AUDIT_ACTIONS`].
    hide_sign_ins: Option<String>,
    /// Comma-separated exact action codes to exclude.
    exclude_actions: Option<String>,
}
impl AuditQuery {
    /// Strictly validated filters: an actor UUID and the action codes to exclude.
    fn filters(&self) -> Result<(Option<Uuid>, Vec<String>), ApiError> {
        let actor = self
            .actor_user_id
            .as_deref()
            .map(|v| Uuid::parse_str(v).map_err(|_| invalid()))
            .transpose()?;
        let mut excluded: Vec<String> = vec![];
        match self.hide_sign_ins.as_deref() {
            None | Some("false") => {}
            Some("true") => excluded.extend(SIGN_IN_AUDIT_ACTIONS.iter().map(|a| (*a).to_owned())),
            Some(_) => return Err(invalid()),
        }
        if let Some(list) = self.exclude_actions.as_deref() {
            let codes: Vec<&str> = list.split(',').collect();
            if codes.len() > 20
                || codes.iter().any(|c| {
                    c.is_empty()
                        || c.len() > 64
                        || !c.bytes().all(|b| {
                            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.'
                        })
                })
            {
                return Err(invalid());
            }
            excluded.extend(codes.into_iter().map(str::to_owned));
        }
        Ok((actor, excluded))
    }
}
async fn platform_audit(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<AuditQuery>,
) -> ApiResult {
    let mut tx = catalog_tx(&s, &u, false).await?;
    let (actor, excluded) = p.filters()?;
    let (l, o) = Page {
        limit: p.limit,
        offset: p.offset,
    }
    .bounds()?;
    // Names come from platform metadata this Admin/Auditor endpoint can
    // already read (never keys or requests), so the dashboard doesn't load
    // every user and workspace to label a page. Routes and prices have no
    // name of their own and are labelled from their route.
    let mut data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',a.id,'actor_user_id',a.actor_user_id,'actor_name',(SELECT coalesce(nullif(btrim(x.display_name),''),x.email,'Retained identity') FROM users x WHERE x.id=a.actor_user_id),'workspace_id',a.workspace_id,'workspace_name',w.name,'workspace_kind',w.kind,'action',a.action,'resource_type',a.resource_type,'resource_id',a.resource_id,'target_kind',CASE WHEN a.resource_type='workspace' THEN (SELECT x.kind FROM workspaces x WHERE x.id=a.resource_id AND x.kind<>'personal') END,'metadata',a.metadata,'created_at',a.created_at,'target_name',CASE a.resource_type
      WHEN 'user' THEN (SELECT coalesce(nullif(btrim(x.display_name),''),x.email,'Retained identity') FROM users x WHERE x.id=a.resource_id)
      WHEN 'workspace' THEN (SELECT x.name FROM workspaces x WHERE x.id=a.resource_id AND x.kind<>'personal')
      WHEN 'cost_center' THEN (SELECT x.name FROM cost_centers x WHERE x.id=a.resource_id)
      WHEN 'group_mapping' THEN (SELECT x.group_value FROM oidc_group_mappings x WHERE x.id=a.resource_id)
      WHEN 'model' THEN (SELECT coalesce(nullif(x.display_name,''),x.public_name) FROM models x WHERE x.id=a.resource_id)
      WHEN 'catalog' THEN (SELECT x.name FROM catalogs x WHERE x.id=a.resource_id)
      WHEN 'provider' THEN (SELECT x.name FROM provider_connections x WHERE x.id=a.resource_id)
      WHEN 'deployment' THEN (SELECT d.upstream_model||' on '||p.name FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.id=a.resource_id)
      WHEN 'price' THEN (SELECT 'Price v'||(SELECT count(*) FROM deployment_prices x WHERE x.deployment_id=dp.deployment_id AND (x.created_at,x.id)<=(dp.created_at,dp.id))||' for '||d.upstream_model||' on '||p.name FROM deployment_prices dp JOIN deployments d ON d.id=dp.deployment_id JOIN provider_connections p ON p.id=d.provider_connection_id WHERE dp.id=a.resource_id)
      END) FROM audit_events a LEFT JOIN workspaces w ON w.id=a.workspace_id WHERE (a.workspace_id IS NULL OR w.kind IN ('team','project')) AND ($3::uuid IS NULL OR a.actor_user_id=$3) AND NOT (a.action=ANY($4::text[])) ORDER BY a.created_at DESC,a.id LIMIT $1 OFFSET $2").bind(l + 1).bind(o).bind(actor).bind(&excluded).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    let more = data.len() > l as usize;
    data.truncate(l as usize);
    Ok(Json(json!({"data":data,"has_more":more})))
}
#[cfg(test)]
mod profile_tests {
    use super::*;
    fn input(provider: &str, endpoint: Option<&str>) -> ProviderInput {
        ProviderInput {
            name: "Router".into(),
            provider: provider.into(),
            credential_ref: "env:OPENROUTER_API_KEY".into(),
            endpoint: endpoint.map(str::to_owned),
            region: None,
            enabled: false,
            aws_external_id: None,
            aws_session_name: None,
        }
    }
    #[test]
    fn openrouter_is_a_fixed_https_cloud_profile() {
        assert!(profile_valid(&input("openrouter", None)));
        assert!(profile_valid(&input("openrouter", Some(OPENROUTER_BASE))));
        for bad in [
            "http://openrouter.ai/api/v1",
            "https://openrouter.ai/v1",
            "https://www.openrouter.ai/api/v1",
            "https://evil.invalid/api/v1",
        ] {
            assert!(!profile_valid(&input("openrouter", Some(bad))), "{bad}");
        }
        assert!(!credential_valid(
            "openrouter",
            "none",
            &crate::providers::secrets::EnvSecrets::default()
        ));
    }

    /// A Platform Admin chooses connection endpoints, but only inside what
    /// the operator approved: no cloud profile takes an endpoint other than
    /// its fixed origin, and a local profile takes none that is not approved
    /// out of band (the unit-test environment approves none), so metadata,
    /// loopback and private addresses cannot be saved (#196).
    #[test]
    fn connection_endpoints_cannot_name_internal_or_lookalike_hosts() {
        let internal = [
            "http://169.254.169.254/v1",
            "http://169.254.169.254/latest/meta-data/",
            "https://169.254.169.254/v1",
            "http://127.0.0.1:8000/v1",
            "http://localhost/v1",
            "http://[::1]/v1",
            "http://[::ffff:169.254.169.254]/v1",
            "http://[fd00:ec2::254]/v1",
            "http://10.0.0.5/v1",
            "http://192.168.1.1/v1",
            "http://2852039166/v1",
            "http://metadata.google.internal/v1",
            "https://api.openai.com.evil.invalid/v1",
            "https://api.openai.com@169.254.169.254/v1",
            "https://api.openai.com:444/v1",
            "https://api.openai.com/v1/../../x",
            "https://api.openai.com/v1?x=1",
        ];
        for provider in ["openai", "anthropic", "openrouter"] {
            for endpoint in internal {
                assert!(
                    !profile_valid(&input(provider, Some(endpoint))),
                    "{provider} {endpoint}"
                );
            }
        }
        for provider in ["openai_compatible", "vllm", "sglang", "ollama"] {
            for endpoint in internal {
                assert!(
                    !profile_valid(&input(provider, Some(endpoint))),
                    "{provider} {endpoint}"
                );
            }
            assert!(!profile_valid(&input(provider, None)), "{provider}");
        }
    }
}

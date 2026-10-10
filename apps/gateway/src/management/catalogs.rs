//! Multiple live catalogs, replacement overrides, and independent authorization sources.
use super::*;

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/platform/catalogs",
            get(catalogs).post(create_catalog),
        )
        .route(
            "/api/v1/platform/catalogs/{id}",
            get(catalog).patch(update_catalog).delete(delete_catalog),
        )
        .route(
            "/api/v1/platform/catalogs/{id}/models",
            get(catalog_models).put(put_catalog_models),
        )
        .route(
            "/api/v1/platform/models/{id}/catalogs",
            get(model_catalogs).put(put_model_catalogs),
        )
        .route(
            "/api/v1/platform/workspace-types/{kind}/catalogs",
            get(type_catalogs).put(put_type_catalogs),
        )
        .route(
            "/api/v1/platform/catalog-defaults",
            get(catalog_defaults).put(put_catalog_defaults),
        )
        .route(
            "/api/v1/platform/workspaces/{ws}/catalogs",
            get(workspace_catalogs)
                .put(put_workspace_catalogs)
                .delete(reset_workspace_catalogs),
        )
        .route(
            "/api/v1/workspaces/{ws}/available-models",
            get(available_models),
        )
        .route(
            "/api/v1/workspaces/{ws}/models",
            get(selected_models).post(select_model),
        )
        .route(
            "/api/v1/workspaces/{ws}/models/{model}",
            axum::routing::delete(deselect_model),
        )
        .route(
            "/api/v1/platform/workspaces/{ws}/models",
            post(direct_model),
        )
        .route(
            "/api/v1/platform/workspaces/{ws}/models/{model}",
            axum::routing::delete(revoke_direct_model),
        )
}
// Effective availability is a replacement list, never a union of defaults and overrides.
const ELIGIBLE: &str = "EXISTS(SELECT 1 FROM catalog_models cm WHERE cm.model_id=m.id AND ((EXISTS(SELECT 1 FROM workspace_catalog_overrides h WHERE h.workspace_id=w.id) AND EXISTS(SELECT 1 FROM workspace_catalog_override_items i WHERE i.workspace_id=w.id AND i.catalog_id=cm.catalog_id)) OR (NOT EXISTS(SELECT 1 FROM workspace_catalog_overrides h WHERE h.workspace_id=w.id) AND EXISTS(SELECT 1 FROM workspace_type_catalogs t WHERE t.kind=w.kind AND t.catalog_id=cm.catalog_id))))";
/// [`retire`] limited to one workspace, for a change of only that
/// workspace's grants (under its exclusive authority lock and the lineage
/// locks of its key allowlists).
async fn retire_workspace(tx: &mut Transaction<'_, Postgres>, ws: Uuid) -> Result<(), ApiError> {
    sqlx::query(&format!("DELETE FROM workspace_model_grants g USING workspaces w,models m WHERE g.workspace_id=$1 AND w.id=$1 AND g.model_id=m.id AND g.source='catalog' AND (w.disabled_at IS NOT NULL OR NOT m.enabled OR NOT({ELIGIBLE}))")).bind(ws).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM key_model_selections s WHERE s.workspace_id=$1 AND NOT workspace_model_allowed(s.workspace_id,s.model_id)").bind(ws).execute(&mut **tx).await?;
    Ok(())
}
/// Called under the exclusive catalog lock or the installation mutation lock.
/// Delete only ineligible catalog-source grants, retaining independent direct grants.
/// Selection headers survive so retired key allowlists become deny-all, not inheritance.
pub(super) async fn retire(tx: &mut Transaction<'_, Postgres>) -> Result<(), ApiError> {
    sqlx::query(&format!("DELETE FROM workspace_model_grants g USING workspaces w,models m WHERE g.workspace_id=w.id AND g.model_id=m.id AND g.source='catalog' AND (w.disabled_at IS NOT NULL OR NOT m.enabled OR NOT({ELIGIBLE}))")).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM key_model_selections s WHERE NOT workspace_model_allowed(s.workspace_id,s.model_id)").execute(&mut **tx).await?;
    Ok(())
}
fn kind_valid(k: &str) -> bool {
    matches!(k, "personal" | "team" | "project")
}
fn ids_valid(ids: &mut Vec<Uuid>) -> Result<(), ApiError> {
    if ids.len() > 200 {
        return Err(invalid());
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(())
}
async fn validate_ids(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[Uuid],
    table: &str,
) -> Result<(), ApiError> {
    let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE id=ANY($1)"))
        .bind(ids)
        .fetch_one(&mut **tx)
        .await?;
    if n != ids.len() as i64 {
        return Err(invalid());
    }
    Ok(())
}
/// A catalog row plus who gets it: model count and the first models (for icons),
/// the workspace types whose live defaults include it, and how many workspaces
/// made their own catalog choice that includes it.
const CATALOG_SUMMARY: &str = "to_jsonb(c) || jsonb_build_object('model_count',(SELECT count(*) FROM catalog_models cm WHERE cm.catalog_id=c.id),'models',coalesce((SELECT jsonb_agg(jsonb_build_object('id',x.id,'public_name',x.public_name,'display_name',x.display_name,'enabled',x.enabled) ORDER BY x.public_name,x.id) FROM (SELECT m.id,m.public_name,m.display_name,m.enabled FROM catalog_models cm JOIN models m ON m.id=cm.model_id WHERE cm.catalog_id=c.id ORDER BY m.public_name,m.id LIMIT 8) x),'[]'::jsonb),'default_for',coalesce((SELECT jsonb_agg(t.kind ORDER BY CASE t.kind WHEN 'personal' THEN 0 WHEN 'team' THEN 1 ELSE 2 END) FROM workspace_type_catalogs t WHERE t.catalog_id=c.id),'[]'::jsonb),'own_choice_count',(SELECT count(*) FROM workspace_catalog_override_items i WHERE i.catalog_id=c.id))";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogListQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    q: Option<String>,
}
async fn catalogs(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<CatalogListQuery>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let (l, o) = Page {
        limit: p.limit,
        offset: p.offset,
    }
    .bounds()?;
    if p.q.as_ref().is_some_and(|q| q.chars().count() > 200) {
        return Err(invalid());
    }
    let data:Vec<Value>=sqlx::query_scalar(&format!("SELECT {CATALOG_SUMMARY} FROM catalogs c WHERE $3::text IS NULL OR strpos(lower(c.name),lower($3))>0 ORDER BY c.name,c.id LIMIT $1 OFFSET $2")).bind(l).bind(o).bind(p.q).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn catalog(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    // Personal workspaces with their own choice are only counted: owner-private, never listed.
    let v: Value = sqlx::query_scalar(&format!("SELECT {CATALOG_SUMMARY} || jsonb_build_object('own_choice',jsonb_build_object('workspaces',coalesce((SELECT jsonb_agg(jsonb_build_object('id',w.id,'name',w.name,'kind',w.kind,'disabled',w.disabled_at IS NOT NULL) ORDER BY w.name,w.id) FROM workspace_catalog_override_items i JOIN workspaces w ON w.id=i.workspace_id WHERE i.catalog_id=c.id AND w.kind IN ('team','project')),'[]'::jsonb),'personal_count',(SELECT count(*) FROM workspace_catalog_override_items i JOIN workspaces w ON w.id=i.workspace_id WHERE i.catalog_id=c.id AND w.kind='personal'))) FROM catalogs c WHERE c.id=$1"))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    tx.commit().await?;
    Ok(Json(v))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogInput {
    name: String,
    description: Option<String>,
}
fn catalog_valid(b: &CatalogInput) -> bool {
    valid_name(&b.name) && b.description.as_ref().is_none_or(|s| s.len() <= 2000)
}
async fn create_catalog(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<CatalogInput>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    if !catalog_valid(&b) {
        return Err(invalid());
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO catalogs(id,name,description) VALUES($1,$2,$3)")
        .bind(id)
        .bind(b.name)
        .bind(b.description)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        None,
        "catalog.created",
        "catalog",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}
async fn update_catalog(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<CatalogInput>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    if !catalog_valid(&b) {
        return Err(invalid());
    }
    if sqlx::query("UPDATE catalogs SET name=$2,description=$3 WHERE id=$1")
        .bind(id)
        .bind(b.name)
        .bind(b.description)
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
        "catalog.updated",
        "catalog",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
async fn delete_catalog(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM catalogs WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    for table in [
        "catalog_models",
        "workspace_type_catalogs",
        "workspace_catalog_override_items",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE catalog_id=$1"))
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    retire(&mut tx).await?;
    sqlx::query("DELETE FROM catalogs WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        None,
        "catalog.deleted",
        "catalog",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
async fn catalog_models(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Query(p): Query<Page>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    validate_ids(&mut tx, &[id], "catalogs").await?;
    let (l, o) = p.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',m.id,'public_name',m.public_name,'display_name',m.display_name,'description',m.description,'supported_protocols',m.supported_protocols,'enabled',m.enabled) FROM catalog_models c JOIN models m ON m.id=c.model_id WHERE c.catalog_id=$1 ORDER BY m.public_name,m.id LIMIT $2 OFFSET $3").bind(id).bind(l).bind(o).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Models {
    model_ids: Vec<Uuid>,
}
async fn put_catalog_models(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<Models>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    replace_membership(&mut tx, &u, Membership::Catalog(id, b.model_ids)).await?;
    tx.commit().await?;
    Ok(ok())
}
/// One side of the catalog/model membership relation being replaced.
pub(super) enum Membership {
    /// Replace one catalog's models (`PUT /catalogs/{id}/models`).
    Catalog(Uuid, Vec<Uuid>),
    /// Replace one model's catalogs (`PUT /models/{id}/catalogs`, model setup).
    Model(Uuid, Vec<Uuid>),
}
/// Shared membership replacement. The caller holds the exclusive catalog lock and
/// installation row (`catalog_tx(write)`). Ineligible catalog-source grants and key
/// selections are retired in the same transaction; later re-adding never restores them.
/// Each changed catalog records `catalog.models_replaced` with its resulting model count.
pub(super) async fn replace_membership(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    m: Membership,
) -> Result<(), ApiError> {
    let changed: Vec<Uuid> = match m {
        Membership::Catalog(catalog, mut models) => {
            ids_valid(&mut models)?;
            validate_ids(tx, &[catalog], "catalogs").await?;
            validate_ids(tx, &models, "models").await?;
            sqlx::query("DELETE FROM catalog_models WHERE catalog_id=$1")
                .bind(catalog)
                .execute(&mut **tx)
                .await?;
            sqlx::query(
                "INSERT INTO catalog_models(catalog_id,model_id) SELECT $1,unnest($2::uuid[])",
            )
            .bind(catalog)
            .bind(&models)
            .execute(&mut **tx)
            .await?;
            vec![catalog]
        }
        Membership::Model(model, mut catalogs) => {
            ids_valid(&mut catalogs)?;
            validate_ids(tx, &catalogs, "catalogs").await?;
            let mut changed: Vec<Uuid> = sqlx::query_scalar("DELETE FROM catalog_models WHERE model_id=$1 AND NOT catalog_id=ANY($2) RETURNING catalog_id").bind(model).bind(&catalogs).fetch_all(&mut **tx).await?;
            changed.extend(sqlx::query_scalar::<_, Uuid>("INSERT INTO catalog_models(catalog_id,model_id) SELECT unnest($2::uuid[]),$1 ON CONFLICT DO NOTHING RETURNING catalog_id").bind(model).bind(&catalogs).fetch_all(&mut **tx).await?);
            changed.sort_unstable();
            changed
        }
    };
    retire(tx).await?;
    for catalog in changed {
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM catalog_models WHERE catalog_id=$1")
                .bind(catalog)
                .fetch_one(&mut **tx)
                .await?;
        audit(
            tx,
            u,
            None,
            "catalog.models_replaced",
            "catalog",
            Some(catalog),
            json!({"count":count}),
        )
        .await?;
    }
    Ok(())
}
async fn model_catalogs(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    model_exists(&mut tx, id).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT catalog_id FROM catalog_models WHERE model_id=$1 ORDER BY catalog_id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"catalog_ids":ids})))
}
async fn put_model_catalogs(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<CatalogIds>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    model_exists(&mut tx, id).await?;
    replace_membership(&mut tx, &u, Membership::Model(id, b.catalog_ids)).await?;
    tx.commit().await?;
    Ok(ok())
}
async fn model_exists(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<(), ApiError> {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM models WHERE id=$1 FOR SHARE")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)?;
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogIds {
    catalog_ids: Vec<Uuid>,
}
async fn type_catalogs(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(kind): Path<String>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    if !kind_valid(&kind) {
        return Err(invalid());
    }
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT catalog_id FROM workspace_type_catalogs WHERE kind=$1 ORDER BY catalog_id",
    )
    .bind(&kind)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"kind":kind,"catalog_ids":ids})))
}
async fn put_type_catalogs(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(kind): Path<String>,
    Json(mut b): Json<CatalogIds>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    if !kind_valid(&kind) {
        return Err(invalid());
    }
    ids_valid(&mut b.catalog_ids)?;
    validate_ids(&mut tx, &b.catalog_ids, "catalogs").await?;
    replace_type_defaults(&mut tx, &kind, &b.catalog_ids).await?;
    retire(&mut tx).await?;
    audit(
        &mut tx,
        &u,
        None,
        "catalog.type_defaults_replaced",
        "workspace_type",
        None,
        json!({"kind":kind,"count":b.catalog_ids.len()}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
/// Replaces one type's live default list (caller holds the exclusive catalog lock and retires afterwards).
async fn replace_type_defaults(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
    ids: &[Uuid],
) -> Result<(), ApiError> {
    sqlx::query("DELETE FROM workspace_type_catalogs WHERE kind=$1")
        .bind(kind)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO workspace_type_catalogs(kind,catalog_id) SELECT $1,unnest($2::uuid[])",
    )
    .bind(kind)
    .bind(ids)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
async fn type_default_ids(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
) -> Result<Vec<Uuid>, ApiError> {
    Ok(sqlx::query_scalar(
        "SELECT catalog_id FROM workspace_type_catalogs WHERE kind=$1 ORDER BY catalog_id",
    )
    .bind(kind)
    .fetch_all(&mut **tx)
    .await?)
}
/// The Defaults matrix: every type's live default catalogs at once.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultsMatrix {
    personal: Vec<Uuid>,
    team: Vec<Uuid>,
    project: Vec<Uuid>,
}
async fn catalog_defaults(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let mut v = json!({});
    for kind in ["personal", "team", "project"] {
        v[kind] = json!(type_default_ids(&mut tx, kind).await?);
    }
    tx.commit().await?;
    Ok(Json(v))
}
/// Saves the whole matrix atomically under one exclusive catalog lock. Only
/// changed types are replaced and audited; retirement runs once, so models
/// selected only through an unchecked catalog are retired and never restored.
async fn put_catalog_defaults(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<DefaultsMatrix>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    let mut changed = Vec::new();
    for (kind, mut ids) in [
        ("personal", b.personal),
        ("team", b.team),
        ("project", b.project),
    ] {
        ids_valid(&mut ids)?;
        validate_ids(&mut tx, &ids, "catalogs").await?;
        if type_default_ids(&mut tx, kind).await? != ids {
            replace_type_defaults(&mut tx, kind, &ids).await?;
            changed.push((kind, ids.len()));
        }
    }
    if !changed.is_empty() {
        retire(&mut tx).await?;
    }
    for (kind, count) in &changed {
        audit(
            &mut tx,
            &u,
            None,
            "catalog.type_defaults_replaced",
            "workspace_type",
            None,
            json!({"kind":kind,"count":count}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Json(
        json!({"ok":true,"changed":changed.iter().map(|(k,_)|*k).collect::<Vec<_>>()}),
    ))
}
async fn workspace_catalogs(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    // Administrative catalog settings contain no private keys or request details;
    // platform readers can still read them while the workspace is disabled.
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(ws)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    let replace: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workspace_catalog_overrides WHERE workspace_id=$1)",
    )
    .bind(ws)
    .fetch_one(&mut *tx)
    .await?;
    let ids:Vec<Uuid>=sqlx::query_scalar("SELECT catalog_id FROM workspace_catalog_override_items WHERE workspace_id=$1 ORDER BY catalog_id").bind(ws).fetch_all(&mut *tx).await?;
    let effective: Vec<Uuid> = if replace {
        ids.clone()
    } else {
        sqlx::query_scalar(
            "SELECT catalog_id FROM workspace_type_catalogs WHERE kind=$1 ORDER BY catalog_id",
        )
        .bind(kind)
        .fetch_all(&mut *tx)
        .await?
    };
    tx.commit().await?;
    Ok(Json(
        json!({"mode":if replace{"replace"}else{"inherit"},"catalog_ids":ids,"effective_catalog_ids":effective}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Override {
    mode: String,
    catalog_ids: Vec<Uuid>,
}
async fn put_workspace_catalogs(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(mut b): Json<Override>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    if b.mode != "replace" {
        return Err(invalid());
    }
    ids_valid(&mut b.catalog_ids)?;
    validate_ids(&mut tx, &b.catalog_ids, "catalogs").await?;
    validate_ids(&mut tx, &[ws], "workspaces").await?;
    sqlx::query(
        "INSERT INTO workspace_catalog_overrides(workspace_id) VALUES($1) ON CONFLICT DO NOTHING",
    )
    .bind(ws)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM workspace_catalog_override_items WHERE workspace_id=$1")
        .bind(ws)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO workspace_catalog_override_items(workspace_id,catalog_id) SELECT $1,unnest($2::uuid[])").bind(ws).bind(&b.catalog_ids).execute(&mut *tx).await?;
    retire(&mut tx).await?;
    audit(
        &mut tx,
        &u,
        Some(ws),
        "catalog.override_replaced",
        "workspace",
        Some(ws),
        json!({"mode":"replace","count":b.catalog_ids.len()}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
async fn reset_workspace_catalogs(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    validate_ids(&mut tx, &[ws], "workspaces").await?;
    sqlx::query("DELETE FROM workspace_catalog_override_items WHERE workspace_id=$1")
        .bind(ws)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM workspace_catalog_overrides WHERE workspace_id=$1")
        .bind(ws)
        .execute(&mut *tx)
        .await?;
    retire(&mut tx).await?;
    audit(
        &mut tx,
        &u,
        Some(ws),
        "catalog.override_reset",
        "workspace",
        Some(ws),
        json!({"mode":"inherit"}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    q: Option<String>,
}
async fn workspace_models(
    s: Store,
    u: BrowserPrincipal,
    ws: Uuid,
    p: ModelQuery,
    available: bool,
) -> ApiResult {
    // Read-only listing: platform readers may also read a disabled Team/Project,
    // whose remaining (unusable) authorizations are listed as configured.
    let (mut tx, a) = resources::workspace_read_tx(&s, &u, ws).await?;
    a.metadata_read()?;
    let (l, o) = Page {
        limit: p.limit,
        offset: p.offset,
    }
    .bounds()?;
    if p.q.as_ref().is_some_and(|q| q.chars().count() > 200) {
        return Err(invalid());
    }
    let data:Vec<Value>=sqlx::query_scalar(&format!("SELECT jsonb_build_object('id',m.id,'model_id',m.id,'public_name',m.public_name,'display_name',m.display_name,'description',m.description,'supported_protocols',m.supported_protocols,'enabled',m.enabled,'available_from_catalog',{ELIGIBLE},'selected',workspace_model_allowed(w.id,m.id),'catalog_granted',EXISTS(SELECT 1 FROM workspace_model_grants g WHERE g.workspace_id=w.id AND g.model_id=m.id AND g.source='catalog'),'direct_granted',EXISTS(SELECT 1 FROM workspace_model_grants g WHERE g.workspace_id=w.id AND g.model_id=m.id AND g.source='direct')) FROM models m CROSS JOIN workspaces w WHERE w.id=$1 AND m.enabled AND (($2 AND {ELIGIBLE}) OR workspace_model_allowed(w.id,m.id) OR ($6 AND EXISTS(SELECT 1 FROM workspace_model_grants g WHERE g.workspace_id=w.id AND g.model_id=m.id))) AND ($3::text IS NULL OR strpos(lower(m.public_name),lower($3))>0 OR strpos(lower(m.display_name),lower($3))>0) ORDER BY m.public_name,m.id LIMIT $4 OFFSET $5")).bind(ws).bind(available).bind(p.q).bind(l).bind(o).bind(a.disabled).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn available_models(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<ModelQuery>,
) -> ApiResult {
    workspace_models(s, u, ws, p, true).await
}
async fn selected_models(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<ModelQuery>,
) -> ApiResult {
    workspace_models(s, u, ws, p, false).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    model_id: Uuid,
}
async fn select_model(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(b): Json<Grant>,
) -> ApiResult {
    change_model(s, u, ws, b.model_id, true, false).await
}
async fn deselect_model(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, model)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    change_model(s, u, ws, model, false, false).await
}
async fn direct_model(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(b): Json<Grant>,
) -> ApiResult {
    change_model(s, u, ws, b.model_id, true, true).await
}
async fn revoke_direct_model(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, model)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    change_model(s, u, ws, model, false, true).await
}
async fn change_model(
    s: Store,
    u: BrowserPrincipal,
    ws: Uuid,
    model: Uuid,
    add: bool,
    direct: bool,
) -> ApiResult {
    let mut tx = resources::installation_tx(&s).await?;
    if direct {
        resources::platform_write(&mut tx, u.user_id).await?;
        validate_ids(&mut tx, &[ws], "workspaces").await?;
    } else {
        let a = resources::workspace_access(&mut tx, &u, ws).await?;
        resources::manage(&a)?;
    }
    validate_ids(&mut tx, &[model], "models").await?;
    if add && !direct {
        let allowed:bool=sqlx::query_scalar(&format!("SELECT m.enabled AND ({ELIGIBLE}) FROM models m CROSS JOIN workspaces w WHERE w.id=$1 AND m.id=$2 AND w.disabled_at IS NULL")).bind(ws).bind(model).fetch_optional(&mut *tx).await?.unwrap_or(false);
        if !allowed {
            return Err(denied());
        }
    }
    let source = if direct { "direct" } else { "catalog" };
    // The workspace, then (removal) the lineages whose model allowlists the
    // workspace-scoped retirement below may shrink.
    let mut scopes = if add {
        Vec::new()
    } else {
        locks::key_lineages(&mut tx, "governance_key_id IN (SELECT governance_key_id FROM key_model_selections WHERE workspace_id=$1)", Some(ws), None).await?
    };
    scopes.push(locks::Scope::Workspace(ws));
    locks::exclusive(&mut tx, scopes).await?;
    if add {
        sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(ws).bind(model).bind(source).execute(&mut *tx).await?;
    } else {
        sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1 AND model_id=$2 AND source=$3").bind(ws).bind(model).bind(source).execute(&mut *tx).await?;
        retire_workspace(&mut tx, ws).await?;
    }
    audit(
        &mut tx,
        &u,
        Some(ws),
        if add {
            "model.granted"
        } else {
            "model.grant_revoked"
        },
        "model",
        Some(model),
        json!({"source":source}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}

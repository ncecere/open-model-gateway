//! #84 slice A: authoring and review of Team/Project onboarding templates.
//!
//! A template has immutable versions. A version is created pending review,
//! reviewed by a Platform Admin other than its author, then published (one
//! published version per template; publishing retires the previous one) and
//! retired at any time. Content holds references only: the workspace kind,
//! catalogs (inherit, or a replacement list), catalog model ids, an optional
//! local limits body and member slots. No secrets, emails, platform roles or
//! group mappings. Applying a published version is slice B. Browser-only
//! Platform Admin routes.
use super::*;
use serde::Serialize;

const MAX_REFERENCES: usize = 50;
/// Admin and member slots besides the owner.
const MAX_EXTRA_SLOTS: usize = 9;
/// Versions per template (the migration enforces the same).
const MAX_VERSIONS: i32 = 100;
/// Templates per installation.
const MAX_TEMPLATES: i64 = 500;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NewTemplate {
    name: String,
    content: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NewVersion {
    content: Value,
}

/// The reviewed content (#84 decision 3). Stored re-serialized from this
/// struct, so the stored content is canonical and always complete.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Content {
    pub(super) kind: String,
    pub(super) catalogs: Catalogs,
    pub(super) model_ids: Vec<Uuid>,
    /// Stored as submitted (shape-checked); applying re-parses it as a
    /// policy body and never reads the stored form directly.
    #[serde(default)]
    pub(super) limits: Option<Value>,
    /// The one owner slot: always required, exactly one user.
    pub(super) owner: OwnerSlot,
    /// Admin and member slots.
    pub(super) slots: Vec<Slot>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OwnerSlot {
    pub(super) name: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Catalogs {
    pub(super) mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) catalog_ids: Option<Vec<Uuid>>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Slot {
    pub(super) name: String,
    pub(super) role: String,
    pub(super) required: bool,
    pub(super) multiple: bool,
}
fn slot_name_valid(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}
/// Shape and references: every catalog and model exists, distinct slot
/// names (the owner slot is its own field), a valid limits body.
pub(super) async fn validate_content(
    tx: &mut Transaction<'_, Postgres>,
    content: &Value,
) -> Result<Content, ApiError> {
    // Objects only inside content (serde would also take positional arrays),
    // and an inherit has no catalog_ids key at all, as the contract states.
    let objects = content.is_object()
        && content["catalogs"].is_object()
        && content["owner"].is_object()
        && content["slots"]
            .as_array()
            .is_some_and(|s| s.iter().all(Value::is_object))
        && (content["catalogs"]["mode"] != "inherit"
            || content["catalogs"].get("catalog_ids").is_none());
    if !objects {
        return Err(invalid());
    }
    let c: Content = serde_json::from_value(content.clone()).map_err(|_| invalid())?;
    if !matches!(c.kind.as_str(), "team" | "project") {
        return Err(invalid());
    }
    let catalogs = match (c.catalogs.mode.as_str(), &c.catalogs.catalog_ids) {
        ("inherit", None) => Vec::new(),
        ("replace", Some(ids)) if ids.len() <= MAX_REFERENCES => ids.clone(),
        _ => return Err(invalid()),
    };
    if c.model_ids.len() > MAX_REFERENCES || c.slots.len() > MAX_EXTRA_SLOTS {
        return Err(invalid());
    }
    if !slot_name_valid(&c.owner.name) {
        return Err(invalid());
    }
    let mut names = std::collections::BTreeSet::from([c.owner.name.as_str()]);
    for slot in &c.slots {
        if !slot_name_valid(&slot.name)
            || !matches!(slot.role.as_str(), "admin" | "member")
            || !names.insert(slot.name.as_str())
        {
            return Err(invalid());
        }
    }
    if let Some(limits) = &c.limits {
        // The object form only (the stored reference stays readable),
        // and the budgets list (not the single monthly_budget_microusd form).
        if !limits.is_object()
            || !limits["budgets"].is_array()
            || limits.get("monthly_budget_microusd").is_some()
            || limits.get("budget_period").is_some()
        {
            return Err(invalid());
        }
        governance::template_limits_valid(limits)?;
    }
    let distinct =
        |ids: &[Uuid]| ids.iter().collect::<std::collections::BTreeSet<_>>().len() == ids.len();
    if !distinct(&catalogs) || !distinct(&c.model_ids) {
        return Err(invalid());
    }
    let (known_catalogs, known_models): (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM catalogs WHERE id=ANY($1)),(SELECT count(*) FROM models WHERE id=ANY($2))")
        .bind(&catalogs)
        .bind(&c.model_ids)
        .fetch_one(&mut **tx)
        .await?;
    if known_catalogs as usize != catalogs.len() || known_models as usize != c.model_ids.len() {
        return Err(invalid());
    }
    Ok(c)
}

const VERSION_JSON: &str = "jsonb_build_object('version',v.version,'state',CASE WHEN v.retired_at IS NOT NULL THEN 'retired' WHEN v.published_at IS NOT NULL THEN 'published' WHEN v.reviewed_at IS NOT NULL THEN 'reviewed' ELSE 'pending_review' END,'content',v.content,'content_hash',encode(v.content_hash,'hex'),'author',v.author,'created_at',v.created_at,'reviewed_by',v.reviewed_by,'reviewed_at',v.reviewed_at,'published_at',v.published_at,'retired_at',v.retired_at)";

async fn one(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> ApiResult {
    let v: Value = sqlx::query_scalar(&format!("SELECT jsonb_build_object('id',t.id,'name',t.name,'created_by',t.created_by,'created_at',t.created_at,'versions',(SELECT coalesce(jsonb_agg({VERSION_JSON} ORDER BY v.version DESC),'[]'::jsonb) FROM workspace_template_versions v WHERE v.template_id=t.id)) FROM workspace_templates t WHERE t.id=$1"))
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)?;
    Ok(Json(v))
}
async fn event(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    id: Uuid,
    action: &'static str,
    version: i32,
) -> Result<(), ApiError> {
    audit(
        tx,
        u,
        None,
        action,
        "workspace_template",
        Some(id),
        json!({"version":version}),
    )
    .await
}
async fn insert_version(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    template: Uuid,
    content: &Value,
) -> Result<i32, ManagementError> {
    let canonical =
        serde_json::to_value(validate_content(tx, content).await?).map_err(|_| invalid())?;
    let version: i32 = sqlx::query_scalar(
        "SELECT coalesce(max(version),0)+1 FROM workspace_template_versions WHERE template_id=$1",
    )
    .bind(template)
    .fetch_one(&mut **tx)
    .await?;
    if version > MAX_VERSIONS {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "This template has the maximum number of versions",
        )
        .into());
    }
    // The trigger also computes the hash of the stored content; a CHECK binds it.
    sqlx::query("INSERT INTO workspace_template_versions(id,template_id,version,content,content_hash,author) VALUES($1,$2,$3,$4::jsonb,sha256(convert_to($4::jsonb::text,'UTF8')),$5)")
        .bind(Uuid::now_v7())
        .bind(template)
        .bind(version)
        .bind(&canonical)
        .bind(u.user_id)
        .execute(&mut **tx)
        .await
        .map_err(conflict_on_unique)?;
    Ok(version)
}
fn conflict_on_unique(e: sqlx::Error) -> ManagementError {
    if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505") {
        return ApiError(
            StatusCode::CONFLICT,
            "A template with this name or version exists",
        )
        .into();
    }
    e.into()
}

/// `GET /api/v1/platform/workspace-templates`.
pub(super) async fn list(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (l, o) = p.bounds()?;
    let mut tx = resources::platform_read_tx(&s, &u).await?;
    let data: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',t.id,'name',t.name,'created_at',t.created_at,'latest_version',(SELECT max(version) FROM workspace_template_versions v WHERE v.template_id=t.id),'published_version',(SELECT version FROM workspace_template_versions v WHERE v.template_id=t.id AND v.published_at IS NOT NULL AND v.retired_at IS NULL)) FROM workspace_templates t ORDER BY t.name,t.id LIMIT $1 OFFSET $2")
        .bind(l)
        .bind(o)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
/// `GET /api/v1/platform/workspace-templates/{id}`.
pub(super) async fn detail(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::platform_read_tx(&s, &u).await?;
    let result = one(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}
/// `POST /api/v1/platform/workspace-templates`: a template and its first
/// version, pending review.
pub(super) async fn create(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<NewTemplate>,
) -> Result<Json<Value>, ManagementError> {
    if !valid_name(&b.name) {
        return Err(invalid().into());
    }
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_write(&mut tx, &u).await?;
    // Content first: an invalid template is 400 whatever its name.
    validate_content(&mut tx, &b.content).await?;
    let templates: i64 = sqlx::query_scalar("SELECT count(*) FROM workspace_templates")
        .fetch_one(&mut *tx)
        .await?;
    if templates >= MAX_TEMPLATES {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "The installation has the maximum number of templates",
        )
        .into());
    }
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspace_templates(id,name,created_by) VALUES($1,$2,$3)")
        .bind(id)
        .bind(&b.name)
        .bind(u.user_id)
        .execute(&mut *tx)
        .await
        .map_err(conflict_on_unique)?;
    let version = insert_version(&mut tx, &u, id, &b.content).await?;
    event(&mut tx, &u, id, "workspace_template.created", version).await?;
    let result = one(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}
/// `POST /api/v1/platform/workspace-templates/{id}/versions`.
pub(super) async fn create_version(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<NewVersion>,
) -> Result<Json<Value>, ManagementError> {
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_write(&mut tx, &u).await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_templates WHERE id=$1)")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if !exists {
        return Err(missing().into());
    }
    let version = insert_version(&mut tx, &u, id, &b.content).await?;
    event(
        &mut tx,
        &u,
        id,
        "workspace_template.version_created",
        version,
    )
    .await?;
    let result = one(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}

#[derive(sqlx::FromRow)]
struct VersionRow {
    author: Uuid,
    reviewed: bool,
    published: bool,
    retired: bool,
}
async fn version_row(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    version: i32,
) -> Result<VersionRow, ApiError> {
    sqlx::query_as("SELECT author,reviewed_at IS NOT NULL AS reviewed,published_at IS NOT NULL AS published,retired_at IS NOT NULL AS retired FROM workspace_template_versions WHERE template_id=$1 AND version=$2 FOR SHARE")
        .bind(id)
        .bind(version)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)
}
fn step_conflict() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "This template version cannot take that step",
    )
}
async fn transition(
    s: &Store,
    u: &BrowserPrincipal,
    id: Uuid,
    version: i32,
    step: &'static str,
) -> ApiResult {
    let mut tx = resources::installation_tx(s).await?;
    resources::platform_write(&mut tx, u).await?;
    // The installation row lock (installation_tx) serializes template steps;
    // the row lock below is defence in depth.
    let row = version_row(&mut tx, id, version).await?;
    match step {
        "review" => {
            if row.reviewed || row.retired {
                return Err(step_conflict());
            }
            // Never the author (#84 decision 2).
            if row.author == u.user_id {
                return Err(denied());
            }
            sqlx::query("UPDATE workspace_template_versions SET reviewed_by=$3,reviewed_at=clock_timestamp() WHERE template_id=$1 AND version=$2")
                .bind(id).bind(version).bind(u.user_id).execute(&mut *tx).await?;
        }
        "publish" => {
            if !row.reviewed || row.published || row.retired {
                return Err(step_conflict());
            }
            // One published version per template: the previous one retires
            // (audited as its own retirement).
            let previous: Option<i32> = sqlx::query_scalar("UPDATE workspace_template_versions SET retired_at=clock_timestamp() WHERE template_id=$1 AND published_at IS NOT NULL AND retired_at IS NULL RETURNING version")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
            if let Some(previous) = previous {
                event(&mut tx, u, id, "workspace_template.retired", previous).await?;
            }
            sqlx::query("UPDATE workspace_template_versions SET published_at=clock_timestamp() WHERE template_id=$1 AND version=$2")
                .bind(id).bind(version).execute(&mut *tx).await?;
        }
        _ => {
            if row.retired {
                return Err(step_conflict());
            }
            sqlx::query("UPDATE workspace_template_versions SET retired_at=clock_timestamp() WHERE template_id=$1 AND version=$2")
                .bind(id).bind(version).execute(&mut *tx).await?;
        }
    }
    let action = match step {
        "review" => "workspace_template.reviewed",
        "publish" => "workspace_template.published",
        _ => "workspace_template.retired",
    };
    event(&mut tx, u, id, action, version).await?;
    let result = one(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}
/// `POST /api/v1/platform/workspace-templates/{id}/versions/{version}/review`.
pub(super) async fn review(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((id, version)): Path<(Uuid, i32)>,
) -> ApiResult {
    transition(&s, &u, id, version, "review").await
}
/// `POST /api/v1/platform/workspace-templates/{id}/versions/{version}/publish`.
pub(super) async fn publish(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((id, version)): Path<(Uuid, i32)>,
) -> ApiResult {
    transition(&s, &u, id, version, "publish").await
}
/// `POST /api/v1/platform/workspace-templates/{id}/versions/{version}/retire`.
pub(super) async fn retire(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((id, version)): Path<(Uuid, i32)>,
) -> ApiResult {
    transition(&s, &u, id, version, "retire").await
}

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/platform/workspace-templates",
            get(list).post(create),
        )
        .route("/api/v1/platform/workspace-templates/{id}", get(detail))
        .route(
            "/api/v1/platform/workspace-templates/{id}/versions",
            post(create_version),
        )
        .route(
            "/api/v1/platform/workspace-templates/{id}/versions/{version}/review",
            post(review),
        )
        .route(
            "/api/v1/platform/workspace-templates/{id}/versions/{version}/publish",
            post(publish),
        )
        .route(
            "/api/v1/platform/workspace-templates/{id}/versions/{version}/retire",
            post(retire),
        )
}

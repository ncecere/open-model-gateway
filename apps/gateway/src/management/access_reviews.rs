//! #90 slice 1: access review campaigns. A Platform Admin starts a one-shot
//! campaign that snapshots the live shared (Team/Project) memberships and/or
//! platform role grants it covers. Each item records one grant; the database
//! derives the item from the grant and accepts items only in the
//! transaction that creates the campaign. Reviewer decisions follow in later
//! slices. Browser-only: Platform Admins start campaigns; Platform Admins and
//! Auditors read them.
use super::*;

/// Open (not closed) campaigns at once.
const MAX_OPEN: i64 = 20;
/// Items in one campaign (narrow by workspace beyond that).
const MAX_ITEMS: i64 = 5000;
const MAX_WORKSPACES: usize = 50;
const DEFAULT_DUE_DAYS: i32 = 14;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NewCampaign {
    name: String,
    subjects: Vec<String>,
    #[serde(default)]
    workspace_ids: Vec<Uuid>,
    #[serde(default)]
    due_in_days: Option<i32>,
}

const CAMPAIGN_JSON: &str = "jsonb_build_object('id',c.id,'name',c.name,'subjects',to_jsonb(c.subjects),'workspace_ids',to_jsonb(c.workspace_ids),'created_by',c.created_by,'created_at',c.created_at,'due_at',c.due_at,'closed_at',c.closed_at,'item_count',(SELECT count(*) FROM access_review_items i WHERE i.campaign_id=c.id))";

/// One campaign with its item counts (by kind, by source, and how many items'
/// grants are no longer active).
async fn one(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> ApiResult {
    let v: Value = sqlx::query_scalar(&format!("SELECT {CAMPAIGN_JSON} || jsonb_build_object('counts',(SELECT jsonb_build_object('workspace_membership',count(*) FILTER (WHERE i.subject_kind='workspace_membership'),'platform_role',count(*) FILTER (WHERE i.subject_kind='platform_role'),'manual',count(*) FILTER (WHERE i.source='manual'),'group',count(*) FILTER (WHERE i.source='group'),'bootstrap',count(*) FILTER (WHERE i.source='bootstrap'),'inactive',count(*) FILTER (WHERE coalesce(m.revoked_at,p.revoked_at) IS NOT NULL)) FROM access_review_items i LEFT JOIN workspace_membership_grants m ON m.id=i.membership_grant_id LEFT JOIN platform_role_grants p ON p.id=i.platform_grant_id WHERE i.campaign_id=c.id)) FROM access_review_campaigns c WHERE c.id=$1"))
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)?;
    Ok(Json(v))
}

/// `GET /api/v1/platform/access-reviews`: newest first.
pub(super) async fn list(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (l, o) = p.bounds()?;
    let mut tx = resources::platform_read_tx(&s, &u).await?;
    let data: Vec<Value> = sqlx::query_scalar(&format!(
        "SELECT {CAMPAIGN_JSON} FROM access_review_campaigns c ORDER BY c.created_at DESC, c.id DESC LIMIT $1 OFFSET $2"
    ))
    .bind(l)
    .bind(o)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
/// `GET /api/v1/platform/access-reviews/{id}`.
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
/// `GET /api/v1/platform/access-reviews/{id}/items`: memberships by
/// workspace, then platform roles; each with the person (as the platform
/// user directory shows them) and whether the grant is still active.
pub(super) async fn items(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (l, o) = p.bounds()?;
    let mut tx = resources::platform_read_tx(&s, &u).await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM access_review_campaigns WHERE id=$1)")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if !exists {
        return Err(missing());
    }
    let data: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',i.id,'subject_kind',i.subject_kind,'workspace_id',i.workspace_id,'workspace_name',w.name,'user_id',i.user_id,'email',u.email,'display_name',u.display_name,'role',i.role,'source',i.source,'granted_at',i.granted_at,'active',coalesce(m.revoked_at,p.revoked_at) IS NULL) FROM access_review_items i JOIN users u ON u.id=i.user_id LEFT JOIN workspaces w ON w.id=i.workspace_id LEFT JOIN workspace_membership_grants m ON m.id=i.membership_grant_id LEFT JOIN platform_role_grants p ON p.id=i.platform_grant_id WHERE i.campaign_id=$1 ORDER BY i.subject_kind DESC, w.name, w.id, u.email, i.id LIMIT $2 OFFSET $3")
        .bind(id)
        .bind(l)
        .bind(o)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}

/// `POST /api/v1/platform/access-reviews`: starts a campaign and snapshots
/// its items in one statement.
pub(super) async fn create(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<NewCampaign>,
) -> Result<Json<Value>, ManagementError> {
    let due = b.due_in_days.unwrap_or(DEFAULT_DUE_DAYS);
    let subjects: std::collections::BTreeSet<&str> =
        b.subjects.iter().map(String::as_str).collect();
    let filter: std::collections::BTreeSet<Uuid> = b.workspace_ids.iter().copied().collect();
    if !valid_name(&b.name)
        || subjects.is_empty()
        || subjects.len() != b.subjects.len()
        || !subjects
            .iter()
            .all(|s| matches!(*s, "workspace_memberships" | "platform_roles"))
        || !(1..=90).contains(&due)
        || filter.len() != b.workspace_ids.len()
        || b.workspace_ids.len() > MAX_WORKSPACES
        || (!filter.is_empty() && !subjects.contains("workspace_memberships"))
    {
        return Err(invalid().into());
    }
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_write(&mut tx, &u).await?;
    if !filter.is_empty() {
        let shared: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM workspaces WHERE id=ANY($1) AND kind IN ('team','project')",
        )
        .bind(&b.workspace_ids)
        .fetch_one(&mut *tx)
        .await?;
        if shared as usize != filter.len() {
            return Err(invalid().into());
        }
    }
    let open: i64 =
        sqlx::query_scalar("SELECT count(*) FROM access_review_campaigns WHERE closed_at IS NULL")
            .fetch_one(&mut *tx)
            .await?;
    if open >= MAX_OPEN {
        return Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "access_review_limit",
            "There are already 20 open access reviews; close one first".into(),
            json!({}),
        ));
    }
    let id = Uuid::now_v7();
    let subject_list: Vec<String> = subjects.iter().map(|s| (*s).to_owned()).collect();
    sqlx::query("INSERT INTO access_review_campaigns(id,name,subjects,workspace_ids,created_by,due_at) VALUES($1,$2,$3,$4,$5,now()+make_interval(days=>$6))")
        .bind(id)
        .bind(&b.name)
        .bind(&subject_list)
        .bind(&b.workspace_ids)
        .bind(u.user_id)
        .bind(due)
        .execute(&mut *tx)
        .await?;
    // One statement, so one snapshot of the grants. The guard copies each
    // item's fields from its grant and rechecks the same rules.
    let inserted = sqlx::query("WITH m AS (SELECT 'workspace_membership' AS kind, g.id AS membership_grant_id, NULL::uuid AS platform_grant_id FROM workspace_membership_grants g JOIN workspaces w ON w.id=g.workspace_id JOIN users u ON u.id=g.user_id WHERE $2 AND g.revoked_at IS NULL AND w.kind IN ('team','project') AND w.disabled_at IS NULL AND (cardinality($4::uuid[])=0 OR w.id=ANY($4)) AND u.disabled_at IS NULL AND u.cleaned_at IS NULL), p AS (SELECT 'platform_role', NULL::uuid, g.id FROM platform_role_grants g JOIN users u ON u.id=g.user_id WHERE $3 AND g.revoked_at IS NULL AND u.disabled_at IS NULL AND u.cleaned_at IS NULL), s AS (SELECT * FROM m UNION ALL SELECT * FROM p LIMIT $5) INSERT INTO access_review_items(id,campaign_id,subject_kind,membership_grant_id,platform_grant_id) SELECT gen_random_uuid(),$1,kind,membership_grant_id,platform_grant_id FROM s")
        .bind(id)
        .bind(subjects.contains("workspace_memberships"))
        .bind(subjects.contains("platform_roles"))
        .bind(&b.workspace_ids)
        .bind(MAX_ITEMS + 1)
        .execute(&mut *tx)
        .await?
        .rows_affected() as i64;
    if inserted > MAX_ITEMS {
        return Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "access_review_too_large",
            "This review would have more than 5,000 items; narrow it to fewer workspaces".into(),
            json!({}),
        ));
    }
    audit(
        &mut tx,
        &u,
        None,
        "access_review.created",
        "access_review",
        Some(id),
        json!({"count":inserted}),
    )
    .await?;
    let result = one(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/platform/access-reviews", get(list).post(create))
        .route("/api/v1/platform/access-reviews/{id}", get(detail))
        .route("/api/v1/platform/access-reviews/{id}/items", get(items))
}

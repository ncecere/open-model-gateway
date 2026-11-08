//! #90 slice 1: access review campaigns. A Platform Admin starts a one-shot
//! campaign that snapshots the live shared (Team/Project) memberships and/or
//! elevated platform role grants (Admin, Auditor) it covers, and closes it. Each item records one grant; the database
//! derives the item from the grant and accepts items only in the
//! transaction that creates the campaign. Reviewer decisions follow in later
//! slices. Browser-only: Platform Admins start campaigns; Platform Admins and
//! Auditors read them.
use super::*;
use crate::governance::locks;

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

/// The latest decision and assignment of item `i` (as `d` and `a`).
const LATEST_DECISION: &str = "LEFT JOIN LATERAL (SELECT x.decision,x.decided_by,x.decided_at FROM access_review_decisions x WHERE x.item_id=i.id ORDER BY x.seq DESC LIMIT 1) d ON true LEFT JOIN LATERAL (SELECT y.reviewer_user_id FROM access_review_assignments y WHERE y.item_id=i.id ORDER BY y.seq DESC LIMIT 1) a ON true";
/// An item as reviewers and platform readers see it: the snapshot's role,
/// source and grant time; the workspace name and person as they are now;
/// whether it is still in effect; the latest decision and assignment.
const ITEM_JSON: &str = "jsonb_build_object('id',i.id,'campaign_id',i.campaign_id,'subject_kind',i.subject_kind,'workspace_id',i.workspace_id,'workspace_name',w.name,'user_id',i.user_id,'email',u.email,'display_name',u.display_name,'role',i.role,'source',i.source,'granted_at',i.granted_at,'active',access_review_item_active(i.id),'decision',d.decision,'decided_by',d.decided_by,'decided_at',d.decided_at,'reviewer_user_id',a.reviewer_user_id)";
const ITEM_FROM: &str = "access_review_items i JOIN users u ON u.id=i.user_id LEFT JOIN workspaces w ON w.id=i.workspace_id";

/// One campaign with its item counts (by kind, by source, and how many items'
/// grants are no longer active).
async fn one(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> ApiResult {
    let v: Value = sqlx::query_scalar(&format!("SELECT {CAMPAIGN_JSON} || jsonb_build_object('counts',(SELECT jsonb_build_object('by_kind',jsonb_build_object('workspace_membership',count(*) FILTER (WHERE i.subject_kind='workspace_membership'),'platform_role',count(*) FILTER (WHERE i.subject_kind='platform_role')),'by_source',jsonb_build_object('manual',count(*) FILTER (WHERE i.source='manual'),'group',count(*) FILTER (WHERE i.source='group'),'bootstrap',count(*) FILTER (WHERE i.source='bootstrap')),'inactive',count(*) FILTER (WHERE NOT access_review_item_active(i.id)),'by_decision',jsonb_build_object('pending',count(*) FILTER (WHERE d.decision IS NULL),'keep',count(*) FILTER (WHERE d.decision='keep'),'flag_idp',count(*) FILTER (WHERE d.decision='flag_idp')),'without_reviewer',count(*) FILTER (WHERE access_review_item_active(i.id) AND NOT (a.reviewer_user_id IS NOT NULL AND access_review_candidate(i.id,a.reviewer_user_id)) AND NOT EXISTS (SELECT FROM effective_platform_roles r LEFT JOIN effective_workspace_memberships e ON e.user_id=r.user_id AND e.workspace_id=i.workspace_id AND e.role IN ('owner','admin') WHERE r.user_id<>i.user_id AND CASE WHEN i.subject_kind='workspace_membership' THEN e.user_id IS NOT NULL ELSE r.role='admin' END))) FROM access_review_items i {LATEST_DECISION} WHERE i.campaign_id=c.id)) FROM access_review_campaigns c WHERE c.id=$1"))
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
/// `GET /api/v1/platform/access-reviews/{id}/items`: memberships (by
/// workspace id), then platform roles, in a stable snapshot order; each with
/// the workspace name and person as they are now (as the platform user
/// directory shows them) and whether the grant is still in effect.
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
    let data: Vec<Value> = sqlx::query_scalar(&format!("SELECT {ITEM_JSON} FROM {ITEM_FROM} {LATEST_DECISION} WHERE i.campaign_id=$1 ORDER BY i.subject_kind DESC, i.workspace_id, i.user_id, i.id LIMIT $2 OFFSET $3"))
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
    let inserted = sqlx::query("WITH m AS (SELECT 'workspace_membership' AS kind, g.id AS membership_grant_id, NULL::uuid AS platform_grant_id FROM workspace_membership_grants g JOIN workspaces w ON w.id=g.workspace_id JOIN users u ON u.id=g.user_id WHERE $2 AND g.revoked_at IS NULL AND w.kind IN ('team','project') AND w.disabled_at IS NULL AND (cardinality($4::uuid[])=0 OR w.id=ANY($4)) AND u.disabled_at IS NULL AND u.cleaned_at IS NULL), p AS (SELECT 'platform_role', NULL::uuid, g.id FROM platform_role_grants g JOIN users u ON u.id=g.user_id WHERE $3 AND g.revoked_at IS NULL AND g.role IN ('admin','auditor') AND u.disabled_at IS NULL AND u.cleaned_at IS NULL), s AS (SELECT * FROM m UNION ALL SELECT * FROM p LIMIT $5) INSERT INTO access_review_items(id,campaign_id,subject_kind,membership_grant_id,platform_grant_id) SELECT gen_random_uuid(),$1,kind,membership_grant_id,platform_grant_id FROM s")
        .bind(id)
        .bind(subjects.contains("workspace_memberships"))
        .bind(subjects.contains("platform_roles"))
        .bind(&b.workspace_ids)
        .bind(MAX_ITEMS + 1)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            // The guard rechecks each grant in its own snapshot: a grant
            // revoked (or a person or workspace disabled) while the
            // snapshot was taken refuses the whole campaign.
            if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23514") {
                ManagementError::detailed(
                    StatusCode::CONFLICT,
                    "access_review_changed",
                    "Access changed while the review was starting; try again".into(),
                    json!({}),
                )
            } else {
                e.into()
            }
        })?
        .rows_affected() as i64;
    if inserted > MAX_ITEMS {
        return Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "access_review_too_large",
            "This review would have more than 5,000 items; narrow it to fewer workspaces, or review memberships and platform roles separately".into(),
            json!({}),
        ));
    }
    if inserted == 0 {
        return Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "access_review_empty",
            "Nothing to review: no current access matches this review".into(),
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

/// `POST /api/v1/platform/access-reviews/{id}/close`: closes an open
/// campaign (once).
pub(super) async fn close(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ManagementError> {
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_write(&mut tx, &u).await?;
    let closed: Option<bool> = sqlx::query_scalar(
        "SELECT closed_at IS NOT NULL FROM access_review_campaigns WHERE id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    match closed {
        None => return Err(missing().into()),
        Some(true) => {
            return Err(ManagementError::detailed(
                StatusCode::CONFLICT,
                "access_review_closed",
                "This access review is already closed".into(),
                json!({}),
            ));
        }
        Some(false) => {}
    }
    sqlx::query("UPDATE access_review_campaigns SET closed_by=$2, closed_at=now() WHERE id=$1")
        .bind(id)
        .bind(u.user_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        None,
        "access_review.closed",
        "access_review",
        Some(id),
        json!({}),
    )
    .await?;
    let result = one(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Decision {
    decision: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Assignment {
    user_id: Option<Uuid>,
}

/// One item as the API shows it.
async fn one_item(tx: &mut Transaction<'_, Postgres>, item: Uuid) -> ApiResult {
    let v: Value = sqlx::query_scalar(&format!(
        "SELECT {ITEM_JSON} FROM {ITEM_FROM} {LATEST_DECISION} WHERE i.id=$1"
    ))
    .bind(item)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(missing)?;
    Ok(Json(v))
}

/// `GET /api/v1/me/access-reviews`: the items the viewer may decide now (open
/// campaigns, still in effect), each with its campaign and latest decision.
/// The viewer's authority is computed once; candidate rows come from indexes
/// (items of workspaces the viewer administers, platform items for Platform
/// Admins, items assigned to the viewer) and each is checked with the same
/// rule as `access_review_may_decide` (a test keeps them equal). Every
/// decision is still checked by that function and the database guard.
pub(super) async fn inbox(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (l, o) = p.bounds()?;
    let mut tx = resources::read_snapshot(&s).await?;
    resources::platform_role_snapshot(&mut tx, u.user_id).await?;
    let data: Vec<Value> = sqlx::query_scalar(&format!("WITH ws AS MATERIALIZED (SELECT e.workspace_id FROM effective_workspace_memberships e WHERE e.user_id=$1 AND e.role IN ('owner','admin')), adm AS MATERIALIZED (SELECT EXISTS (SELECT FROM effective_platform_roles r WHERE r.user_id=$1 AND r.role='admin') AS yes), cand AS MATERIALIZED (SELECT i.id FROM access_review_items i JOIN access_review_campaigns c ON c.id=i.campaign_id AND c.closed_at IS NULL WHERE i.workspace_id IN (SELECT workspace_id FROM ws) UNION SELECT i.id FROM access_review_items i JOIN access_review_campaigns c ON c.id=i.campaign_id AND c.closed_at IS NULL WHERE i.subject_kind='platform_role' AND (SELECT yes FROM adm) UNION SELECT y.item_id FROM access_review_assignments y WHERE y.reviewer_user_id=$1) SELECT {ITEM_JSON} || jsonb_build_object('campaign_name',c.name,'due_at',c.due_at) FROM cand JOIN {ITEM_FROM} ON i.id=cand.id JOIN access_review_campaigns c ON c.id=i.campaign_id AND c.closed_at IS NULL {LATEST_DECISION} LEFT JOIN workspace_membership_grants m ON m.id=i.membership_grant_id LEFT JOIN platform_role_grants pg ON pg.id=i.platform_grant_id WHERE i.user_id<>$1 AND coalesce(m.revoked_at,pg.revoked_at) IS NULL AND u.disabled_at IS NULL AND u.cleaned_at IS NULL AND (w.id IS NULL OR w.disabled_at IS NULL) AND CASE WHEN a.reviewer_user_id IS NOT NULL AND access_review_candidate(i.id,a.reviewer_user_id) THEN a.reviewer_user_id=$1 WHEN i.subject_kind='workspace_membership' THEN i.workspace_id IN (SELECT workspace_id FROM ws) ELSE (SELECT yes FROM adm) END ORDER BY c.due_at, c.id, i.subject_kind DESC, i.workspace_id, i.user_id, i.id LIMIT $2 OFFSET $3"))
        .bind(u.user_id)
        .bind(l)
        .bind(o)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}

/// `POST /api/v1/me/access-reviews/{item}/decision`: records the viewer's
/// decision. An item the viewer may not decide (not theirs, unknown, or its
/// campaign closed) is 404.
pub(super) async fn decide(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(item): Path<Uuid>,
    Json(b): Json<Decision>,
) -> Result<Json<Value>, ManagementError> {
    if !matches!(b.decision.as_str(), "keep" | "flag_idp") {
        return Err(invalid().into());
    }
    let mut tx = crate::db::begin(&s.pool).await?;
    // The reviewer's own scope (shared), so a concurrent demotion of the
    // reviewer serializes with the decision; then the campaign row, so
    // closing the campaign does too.
    locks::shared(&mut tx, [locks::Scope::User(u.user_id)]).await?;
    resources::platform_role(&mut tx, u.user_id).await?;
    sqlx::query("SELECT 1 FROM access_review_campaigns c JOIN access_review_items i ON i.campaign_id=c.id WHERE i.id=$1 FOR SHARE OF c")
        .bind(item)
        .fetch_optional(&mut *tx)
        .await?;
    let (may, active, group, ws): (bool, bool, bool, Option<Uuid>) = sqlx::query_as("SELECT access_review_may_decide(i.id,$2), access_review_item_active(i.id), i.source='group', i.workspace_id FROM access_review_items i WHERE i.id=$1")
        .bind(item)
        .bind(u.user_id)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or((false, false, false, None));
    if !may {
        return Err(missing().into());
    }
    if !active {
        return Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "access_review_item_inactive",
            "This access is no longer in effect; there is nothing to decide".into(),
            json!({}),
        ));
    }
    if b.decision == "flag_idp" && !group {
        return Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "access_review_flag_requires_group",
            "Only access provided by an identity provider group is flagged for the identity provider".into(),
            json!({}),
        ));
    }
    // The same reviewer repeating the latest decision changes nothing.
    let unchanged: bool = sqlx::query_scalar("SELECT coalesce((SELECT x.decision=$2 AND x.decided_by=$3 FROM access_review_decisions x WHERE x.item_id=$1 ORDER BY x.seq DESC LIMIT 1),false)")
        .bind(item)
        .bind(&b.decision)
        .bind(u.user_id)
        .fetch_one(&mut *tx)
        .await?;
    if unchanged {
        let result = one_item(&mut tx, item).await?;
        tx.commit().await?;
        return Ok(result);
    }
    sqlx::query(
        "INSERT INTO access_review_decisions(id,item_id,decision,decided_by) VALUES($1,$2,$3,$4)",
    )
    .bind(Uuid::now_v7())
    .bind(item)
    .bind(&b.decision)
    .bind(u.user_id)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        &u,
        ws,
        "access_review.decided",
        "access_review_item",
        Some(item),
        json!({"kind":b.decision}),
    )
    .await?;
    let result = one_item(&mut tx, item).await?;
    tx.commit().await?;
    Ok(result)
}

/// `POST /api/v1/platform/access-reviews/{id}/items/{item}/reviewer`: a
/// Platform Admin assigns the item to one eligible reviewer, or (`null`)
/// back to the automatic reviewers.
pub(super) async fn assign(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((id, item)): Path<(Uuid, Uuid)>,
    Json(b): Json<Assignment>,
) -> Result<Json<Value>, ManagementError> {
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_write(&mut tx, &u).await?;
    let state: Option<(bool, Option<Uuid>, Uuid)> = sqlx::query_as("SELECT c.closed_at IS NOT NULL, i.workspace_id, i.user_id FROM access_review_items i JOIN access_review_campaigns c ON c.id=i.campaign_id WHERE i.id=$1 AND c.id=$2 FOR SHARE OF c")
        .bind(item)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((closed, ws, subject)) = state else {
        return Err(missing().into());
    };
    if subject == u.user_id {
        return Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "access_review_own_access",
            "Another Platform Admin decides who reviews your own access".into(),
            json!({}),
        ));
    }
    if closed {
        return Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "access_review_closed",
            "This access review is already closed".into(),
            json!({}),
        ));
    }
    if let Some(reviewer) = b.user_id {
        let candidate: bool = sqlx::query_scalar("SELECT access_review_candidate($1,$2)")
            .bind(item)
            .bind(reviewer)
            .fetch_one(&mut *tx)
            .await?;
        if !candidate {
            return Err(ManagementError::detailed(
                StatusCode::CONFLICT,
                "access_review_reviewer_ineligible",
                "That person cannot review this access: choose an owner or admin of the workspace, or a Platform Admin".into(),
                json!({}),
            ));
        }
    }
    sqlx::query("INSERT INTO access_review_assignments(id,item_id,reviewer_user_id,assigned_by) VALUES($1,$2,$3,$4)")
        .bind(Uuid::now_v7())
        .bind(item)
        .bind(b.user_id)
        .bind(u.user_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        ws,
        "access_review.reviewer_assigned",
        "access_review_item",
        Some(item),
        json!({"kind": if b.user_id.is_some() { "assigned" } else { "cleared" }}),
    )
    .await?;
    let result = one_item(&mut tx, item).await?;
    tx.commit().await?;
    Ok(result)
}

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/platform/access-reviews", get(list).post(create))
        .route("/api/v1/platform/access-reviews/{id}", get(detail))
        .route("/api/v1/platform/access-reviews/{id}/items", get(items))
        .route("/api/v1/platform/access-reviews/{id}/close", post(close))
        .route(
            "/api/v1/platform/access-reviews/{id}/items/{item}/reviewer",
            post(assign),
        )
        .route("/api/v1/me/access-reviews", get(inbox))
        .route("/api/v1/me/access-reviews/{item}/decision", post(decide))
}

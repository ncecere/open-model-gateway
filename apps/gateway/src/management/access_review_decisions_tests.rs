//! #90 slice 2: reviewer decisions on real PostgreSQL. Reviewers are derived
//! live: a current owner/admin of the item's workspace (membership items) or
//! a current Platform Admin (platform role items), never the item's own
//! person, unless a Platform Admin assigned the item to a specific eligible
//! reviewer. Decisions (keep; flag_idp for group-provided access) are
//! append-only, the latest counts, while the campaign is open and the grant
//! is still in effect.
use super::*;

const REVIEWS: &str = "/api/v1/platform/access-reviews";
const INBOX: &str = "/api/v1/me/access-reviews";

async fn campaign(f: &Fixture) -> Value {
    let (status, c) = call(
        &f.s,
        &f.admin,
        "POST",
        REVIEWS,
        json!({"name":"Q4","subjects":["workspace_memberships","platform_roles"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{c}");
    c
}
async fn inbox(f: &Fixture, who: &BrowserPrincipal) -> Vec<Value> {
    let (status, page) = call(&f.s, who, "GET", &format!("{INBOX}?limit=200"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page["data"].as_array().unwrap().clone()
}
fn emails(items: &[Value]) -> std::collections::BTreeSet<String> {
    items
        .iter()
        .map(|i| {
            format!(
                "{}:{}",
                i["workspace_name"].as_str().unwrap_or("platform"),
                i["email"].as_str().unwrap()
            )
        })
        .collect()
}
/// The campaign's item for this person (and workspace, or platform).
async fn item(pool: &PgPool, campaign: &Value, user: Uuid, ws: Option<Uuid>) -> Uuid {
    sqlx::query_scalar("SELECT id FROM access_review_items WHERE campaign_id=$1 AND user_id=$2 AND workspace_id IS NOT DISTINCT FROM $3")
        .bind(id(campaign))
        .bind(user)
        .bind(ws)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn decide(
    f: &Fixture,
    who: &BrowserPrincipal,
    item: Uuid,
    decision: &str,
) -> (StatusCode, Value) {
    call(
        &f.s,
        who,
        "POST",
        &format!("{INBOX}/{item}/decision"),
        json!({"decision":decision}),
    )
    .await
}
async fn assign(
    f: &Fixture,
    who: &BrowserPrincipal,
    campaign: &Value,
    item: Uuid,
    reviewer: Option<Uuid>,
) -> (StatusCode, Value) {
    call(
        &f.s,
        who,
        "POST",
        &format!("{REVIEWS}/{}/items/{item}/reviewer", id(campaign)),
        json!({"user_id":reviewer}),
    )
    .await
}
fn reason(body: &Value) -> Option<&str> {
    body["error"]["reason"].as_str()
}
fn sqlstate(r: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> Option<String> {
    r.err().map(|e| {
        e.as_database_error()
            .and_then(|d| d.code())
            .map(|c| c.into_owned())
            .unwrap_or_else(|| format!("{e}"))
    })
}
/// Revokes a grant the way every authority change must: its scope lock first.
async fn revoke_membership(pool: &PgPool, ws: Uuid, user: Uuid) {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT omg_scope_lock_exclusive(72419512, omg_scope_key($1))")
        .bind(user)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2 AND revoked_at IS NULL")
        .bind(ws).bind(user).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}
async fn group_member(pool: &PgPool, ws: Uuid, user: Uuid) {
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source,mapping_id) VALUES($1,$2,'member','group',$3)")
        .bind(ws).bind(user).bind(Uuid::new_v4()).execute(pool).await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn reviewers_see_and_decide_only_their_items(pool: PgPool) {
    let f = fixture(&pool).await;
    group_member(&pool, f.project, f.outsider.user_id).await;
    let c = campaign(&f).await;
    // The workspace owner reviews its workspaces' members, never itself.
    assert_eq!(
        emails(&inbox(&f, &f.owner).await),
        ["Project:outsider@example.test", "Team:member@example.test"]
            .map(String::from)
            .into()
    );
    // Platform Admins review elevated platform roles, never their own.
    assert_eq!(
        emails(&inbox(&f, &f.admin).await),
        ["platform:auditor@example.test"].map(String::from).into()
    );
    // A member, an Auditor, an outsider review nothing.
    for who in [&f.member, &f.auditor, &f.outsider] {
        assert!(inbox(&f, who).await.is_empty(), "{}", who.email);
    }
    let member_item = item(&pool, &c, f.member.user_id, Some(f.team)).await;
    let group_item = item(&pool, &c, f.outsider.user_id, Some(f.project)).await;
    let owner_item = item(&pool, &c, f.owner.user_id, Some(f.team)).await;
    // Not theirs to decide: indistinguishable from unknown.
    for (who, target) in [
        (&f.member, owner_item),
        (&f.owner, owner_item),
        (&f.admin, member_item),
        (&f.auditor, member_item),
        (&f.owner, Uuid::new_v4()),
    ] {
        let (status, _) = decide(&f, who, target, "keep").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{} {target}", who.email);
    }
    // Keep; a reviewer may change the decision; the latest counts.
    let (status, d) = decide(&f, &f.owner, member_item, "keep").await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(
        (d["id"].as_str(), d["decision"].as_str()),
        (Some(member_item.to_string().as_str()), Some("keep"))
    );
    // Only group-provided access is flagged for the identity provider.
    let (status, body) = decide(&f, &f.owner, member_item, "flag_idp").await;
    assert_eq!(
        (status, reason(&body)),
        (
            StatusCode::CONFLICT,
            Some("access_review_flag_requires_group")
        )
    );
    let (status, d) = decide(&f, &f.owner, group_item, "flag_idp").await;
    assert_eq!(
        (status, d["decision"].as_str()),
        (StatusCode::OK, Some("flag_idp")),
        "{d}"
    );
    let (status, d) = decide(&f, &f.owner, group_item, "keep").await;
    assert_eq!(
        (status, d["decision"].as_str()),
        (StatusCode::OK, Some("keep"))
    );
    let (status, _) = decide(&f, &f.owner, group_item, "remove").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "remove arrives in a later slice"
    );
    // The inbox shows each item's latest decision.
    let mine = inbox(&f, &f.owner).await;
    assert!(mine.iter().all(|i| i["decision"] == "keep"), "{mine:?}");
    // The platform view shows decisions and counts them.
    let (status, items) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{REVIEWS}/{}/items?limit=200", id(&c)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let decided: Vec<&Value> = items["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| !i["decision"].is_null())
        .collect();
    assert_eq!(decided.len(), 2);
    assert!(
        decided
            .iter()
            .all(|i| i["decided_by"] == f.owner.user_id.to_string() && i["decided_at"].is_string())
    );
    let (status, detail) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{REVIEWS}/{}", id(&c)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let total = detail["item_count"].as_i64().unwrap();
    assert_eq!(
        detail["counts"]["by_decision"],
        json!({"pending": total - 2, "keep": 2, "flag_idp": 0})
    );
    // Audited per decision, in the workspace's audit.
    let audited: Vec<(Option<Uuid>, Value)> = sqlx::query_as("SELECT workspace_id,metadata FROM audit_events WHERE action='access_review.decided' AND resource_id=$1 ORDER BY created_at")
        .bind(group_item).fetch_all(&pool).await.unwrap();
    assert_eq!(
        audited,
        vec![
            (Some(f.project), json!({"kind":"flag_idp"})),
            (Some(f.project), json!({"kind":"keep"}))
        ]
    );
    // Browser-only.
    for (method, path) in [
        ("GET", INBOX),
        ("POST", "/api/v1/me/access-reviews/{item}/decision"),
        (
            "POST",
            "/api/v1/platform/access-reviews/{id}/items/{item}/reviewer",
        ),
    ] {
        let token = crate::route_policy::token_policy(method, path)
            .unwrap_or_else(|| panic!("{method} {path} undeclared"));
        assert!(
            matches!(token, crate::route_policy::TokenPolicy::Denied),
            "{method} {path}"
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_platform_admin_assigns_an_item_to_an_eligible_reviewer(pool: PgPool) {
    let f = fixture(&pool).await;
    let c = campaign(&f).await;
    let member_item = item(&pool, &c, f.member.user_id, Some(f.team)).await;
    // To a Platform Admin: only they decide it now.
    let (status, a) = assign(&f, &f.admin, &c, member_item, Some(f.admin.user_id)).await;
    assert_eq!(status, StatusCode::OK, "{a}");
    assert_eq!(a["reviewer_user_id"], f.admin.user_id.to_string());
    assert!(!emails(&inbox(&f, &f.owner).await).contains("Team:member@example.test"));
    assert!(emails(&inbox(&f, &f.admin).await).contains("Team:member@example.test"));
    assert_eq!(
        decide(&f, &f.owner, member_item, "keep").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        decide(&f, &f.admin, member_item, "keep").await.0,
        StatusCode::OK
    );
    // Back to automatic: the owner again.
    let (status, a) = assign(&f, &f.admin, &c, member_item, None).await;
    assert_eq!(
        (status, a["reviewer_user_id"].is_null()),
        (StatusCode::OK, true)
    );
    assert_eq!(
        decide(&f, &f.owner, member_item, "keep").await.0,
        StatusCode::OK
    );
    // Only someone who could review it: never a plain member, an Auditor or the person themselves.
    for who in [f.member.user_id, f.auditor.user_id, f.outsider.user_id] {
        let (status, body) = assign(&f, &f.admin, &c, member_item, Some(who)).await;
        assert_eq!(
            (status, reason(&body)),
            (
                StatusCode::CONFLICT,
                Some("access_review_reviewer_ineligible")
            ),
            "{who}"
        );
    }
    // Platform Admins only; the item must be in that campaign.
    for who in [&f.auditor, &f.owner, &f.member] {
        assert_eq!(
            assign(&f, who, &c, member_item, Some(f.admin.user_id))
                .await
                .0,
            StatusCode::FORBIDDEN,
            "{}",
            who.email
        );
    }
    let (status, other) = call(
        &f.s,
        &f.admin,
        "POST",
        REVIEWS,
        json!({"name":"Other","subjects":["platform_roles"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{other}");
    assert_eq!(
        assign(&f, &f.admin, &other, member_item, Some(f.admin.user_id))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        assign(&f, &f.admin, &c, Uuid::new_v4(), Some(f.admin.user_id))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='access_review.reviewer_assigned' AND resource_id=$1")
        .bind(member_item).fetch_one(&pool).await.unwrap();
    assert_eq!(audited, 2);
    // A closed campaign takes no assignment and no decision; its items leave inboxes.
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/close", id(&c)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = assign(&f, &f.admin, &c, member_item, Some(f.admin.user_id)).await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::CONFLICT, Some("access_review_closed"))
    );
    assert_eq!(
        decide(&f, &f.owner, member_item, "keep").await.0,
        StatusCode::NOT_FOUND
    );
    assert!(inbox(&f, &f.owner).await.is_empty());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn reviewer_authority_and_item_state_are_live(pool: PgPool) {
    let f = fixture(&pool).await;
    // A second workspace admin of the Team.
    member(&pool, f.team, f.outsider.user_id, "admin").await;
    let c = campaign(&f).await;
    let member_item = item(&pool, &c, f.member.user_id, Some(f.team)).await;
    assert!(emails(&inbox(&f, &f.outsider).await).contains("Team:member@example.test"));
    // Losing the admin role removes the item and the right to decide at once.
    revoke_membership(&pool, f.team, f.outsider.user_id).await;
    assert!(inbox(&f, &f.outsider).await.is_empty());
    assert_eq!(
        decide(&f, &f.outsider, member_item, "keep").await.0,
        StatusCode::NOT_FOUND
    );
    // An item whose grant is no longer in effect cannot be decided.
    revoke_membership(&pool, f.team, f.member.user_id).await;
    let (status, body) = decide(&f, &f.owner, member_item, "keep").await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::CONFLICT, Some("access_review_item_inactive"))
    );
    assert!(!emails(&inbox(&f, &f.owner).await).contains("Team:member@example.test"));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn the_database_binds_decisions_and_assignments(pool: PgPool) {
    let f = fixture(&pool).await;
    group_member(&pool, f.project, f.outsider.user_id).await;
    let c = campaign(&f).await;
    let member_item = item(&pool, &c, f.member.user_id, Some(f.team)).await;
    let decision =
        "INSERT INTO access_review_decisions(id,item_id,decision,decided_by) VALUES($1,$2,$3,$4)";
    let insert = |item: Uuid, d: &'static str, by: Uuid| {
        let pool = pool.clone();
        async move {
            sqlstate(
                sqlx::query(decision)
                    .bind(Uuid::new_v4())
                    .bind(item)
                    .bind(d)
                    .bind(by)
                    .execute(&pool)
                    .await,
            )
        }
    };
    assert_eq!(
        insert(member_item, "keep", f.member.user_id)
            .await
            .as_deref(),
        Some("23514"),
        "not a reviewer"
    );
    assert_eq!(
        insert(member_item, "keep", f.admin.user_id)
            .await
            .as_deref(),
        Some("23514"),
        "platform admin on a membership"
    );
    assert_eq!(
        insert(member_item, "flag_idp", f.owner.user_id)
            .await
            .as_deref(),
        Some("23514"),
        "flag on manual access"
    );
    assert_eq!(
        insert(member_item, "remove", f.owner.user_id)
            .await
            .as_deref(),
        Some("23514"),
        "remove not yet"
    );
    assert_eq!(
        insert(member_item, "keep", f.owner.user_id).await,
        None,
        "the owner keeps"
    );
    let assignment = "INSERT INTO access_review_assignments(id,item_id,reviewer_user_id,assigned_by) VALUES($1,$2,$3,$4)";
    let assign_sql = |reviewer: Option<Uuid>, by: Uuid| {
        let pool = pool.clone();
        async move {
            sqlstate(
                sqlx::query(assignment)
                    .bind(Uuid::new_v4())
                    .bind(member_item)
                    .bind(reviewer)
                    .bind(by)
                    .execute(&pool)
                    .await,
            )
        }
    };
    assert_eq!(
        assign_sql(Some(f.admin.user_id), f.owner.user_id)
            .await
            .as_deref(),
        Some("23514"),
        "assigned by a non-admin"
    );
    assert_eq!(
        assign_sql(Some(f.member.user_id), f.admin.user_id)
            .await
            .as_deref(),
        Some("23514"),
        "the person themselves"
    );
    assert_eq!(
        assign_sql(Some(f.auditor.user_id), f.admin.user_id)
            .await
            .as_deref(),
        Some("23514"),
        "not eligible"
    );
    assert_eq!(
        assign_sql(Some(f.admin.user_id), f.admin.user_id).await,
        None,
        "a Platform Admin"
    );
    // The decision clock is the server's; records never change.
    for (sql, code) in [
        (
            "UPDATE access_review_decisions SET decision='flag_idp'",
            "P0001",
        ),
        ("DELETE FROM access_review_decisions", "P0001"),
        (
            "UPDATE access_review_assignments SET reviewer_user_id=NULL",
            "P0001",
        ),
        ("DELETE FROM access_review_assignments", "P0001"),
    ] {
        assert_eq!(
            sqlstate(sqlx::query(sql).execute(&pool).await).as_deref(),
            Some(code),
            "{sql}"
        );
    }
    // After closing, nothing more.
    sqlx::query("UPDATE access_review_campaigns SET closed_by=$2, closed_at=now() WHERE id=$1")
        .bind(id(&c))
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT access_review_may_decide($1,$2)")
            .bind(member_item)
            .bind(f.admin.user_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "closed: nobody decides"
    );
    assert_eq!(
        insert(member_item, "keep", f.admin.user_id)
            .await
            .as_deref(),
        Some("23514"),
        "after close"
    );
    assert_eq!(
        assign_sql(None, f.admin.user_id).await.as_deref(),
        Some("23514"),
        "assignment after close"
    );
}

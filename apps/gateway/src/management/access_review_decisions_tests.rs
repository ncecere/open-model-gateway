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
    // The decision clock is the server's (a forged time is replaced); records never change.
    sqlx::query("INSERT INTO access_review_decisions(id,item_id,decision,decided_by,decided_at) VALUES($1,$2,'keep',$3,'2000-01-01')")
        .bind(Uuid::new_v4()).bind(member_item).bind(f.admin.user_id).execute(&pool).await.unwrap();
    let recent: bool = sqlx::query_scalar(
        "SELECT bool_and(decided_at > now() - interval '1 hour') FROM access_review_decisions",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(recent, "server clock");
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

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_role_items_are_decided_by_other_platform_admins(pool: PgPool) {
    let f = fixture(&pool).await;
    let second = user(&pool, "second-admin", "admin").await;
    let c = campaign(&f).await;
    let auditor_item = item(&pool, &c, f.auditor.user_id, None).await;
    let admin_item = item(&pool, &c, f.admin.user_id, None).await;
    assert_eq!(
        decide(&f, &f.owner, auditor_item, "keep").await.0,
        StatusCode::NOT_FOUND,
        "a workspace owner"
    );
    assert_eq!(
        decide(&f, &f.admin, admin_item, "keep").await.0,
        StatusCode::NOT_FOUND,
        "own platform role"
    );
    assert_eq!(
        decide(&f, &second, admin_item, "keep").await.0,
        StatusCode::OK,
        "another Platform Admin"
    );
    assert_eq!(
        decide(&f, &f.admin, auditor_item, "keep").await.0,
        StatusCode::OK
    );
    // A workspace owner is never a reviewer of platform roles.
    let (status, body) = assign(&f, &f.admin, &c, auditor_item, Some(f.owner.user_id)).await;
    assert_eq!(
        (status, reason(&body)),
        (
            StatusCode::CONFLICT,
            Some("access_review_reviewer_ineligible")
        )
    );
    // Nobody chooses the reviewer of their own access.
    let (status, body) = assign(&f, &f.admin, &c, admin_item, Some(second.user_id)).await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::CONFLICT, Some("access_review_own_access"))
    );
    let own = sqlx::query("INSERT INTO access_review_assignments(id,item_id,reviewer_user_id,assigned_by) VALUES($1,$2,$3,$4)")
        .bind(Uuid::new_v4()).bind(admin_item).bind(second.user_id).bind(f.admin.user_id).execute(&pool).await;
    assert_eq!(
        sqlstate(own).as_deref(),
        Some("23514"),
        "own access assigned in the database"
    );
    assert_eq!(
        assign(&f, &second, &c, admin_item, Some(second.user_id))
            .await
            .0,
        StatusCode::OK,
        "by another admin"
    );
    // A demoted Platform Admin no longer reviews platform roles.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT omg_scope_lock_exclusive(72419512, omg_scope_key($1))")
        .bind(second.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(second.user_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'user','manual')")
        .bind(second.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let demoted = BrowserPrincipal {
        platform_admin: false,
        platform_auditor: false,
        ..second.clone()
    };
    assert!(inbox(&f, &demoted).await.is_empty());
    assert_eq!(
        decide(&f, &demoted, auditor_item, "keep").await.0,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn an_assignment_falls_back_when_the_assignee_no_longer_qualifies(pool: PgPool) {
    let f = fixture(&pool).await;
    member(&pool, f.team, f.outsider.user_id, "admin").await;
    let c = campaign(&f).await;
    let member_item = item(&pool, &c, f.member.user_id, Some(f.team)).await;
    let (status, _) = assign(&f, &f.admin, &c, member_item, Some(f.outsider.user_id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        decide(&f, &f.owner, member_item, "keep").await.0,
        StatusCode::NOT_FOUND,
        "assigned elsewhere"
    );
    revoke_membership(&pool, f.team, f.outsider.user_id).await;
    // The assignee lost the role: the automatic reviewers decide again.
    assert!(emails(&inbox(&f, &f.owner).await).contains("Team:member@example.test"));
    assert_eq!(
        decide(&f, &f.owner, member_item, "keep").await.0,
        StatusCode::OK
    );
    // Items nobody may decide are counted: each sole owner's own access, the
    // only Platform Admin's own role; assigning one removes it from the count.
    let (status, detail) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{REVIEWS}/{}", id(&c)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["counts"]["without_reviewer"], 3, "{detail}");
    let owner_item = item(&pool, &c, f.owner.user_id, Some(f.team)).await;
    assert_eq!(
        assign(&f, &f.admin, &c, owner_item, Some(f.admin.user_id))
            .await
            .0,
        StatusCode::OK
    );
    let (_, detail) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{REVIEWS}/{}", id(&c)),
        Value::Null,
    )
    .await;
    assert_eq!(detail["counts"]["without_reviewer"], 2);
    let kinds: Vec<Value> = sqlx::query_scalar("SELECT metadata FROM audit_events WHERE action='access_review.reviewer_assigned' ORDER BY created_at")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(
        kinds,
        vec![json!({"kind":"assigned"}), json!({"kind":"assigned"})]
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn repeated_and_stale_decisions_change_nothing(pool: PgPool) {
    let f = fixture(&pool).await;
    let c = campaign(&f).await;
    let member_item = item(&pool, &c, f.member.user_id, Some(f.team)).await;
    for _ in 0..3 {
        assert_eq!(
            decide(&f, &f.owner, member_item, "keep").await.0,
            StatusCode::OK
        );
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM access_review_decisions")
        .fetch_one(&pool)
        .await
        .unwrap();
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='access_review.decided'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((rows, audited), (1, 1));
    // The database refuses a decision once the access is no longer in effect.
    revoke_membership(&pool, f.team, f.member.user_id).await;
    let r = sqlx::query("INSERT INTO access_review_decisions(id,item_id,decision,decided_by) VALUES($1,$2,'keep',$3)")
        .bind(Uuid::new_v4()).bind(member_item).bind(f.owner.user_id).execute(&pool).await;
    assert_eq!(sqlstate(r).as_deref(), Some("23514"));
    // A reviewer who loses platform access decides nothing.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT omg_scope_lock_exclusive(72419512, omg_scope_key($1))")
        .bind(f.owner.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(f.owner.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let project_item = item(&pool, &c, f.owner.user_id, Some(f.project)).await;
    assert_ne!(
        decide(&f, &f.owner, project_item, "keep").await.0,
        StatusCode::OK
    );
    assert_ne!(
        call(&f.s, &f.owner, "GET", INBOX, Value::Null).await.0,
        StatusCode::OK
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_decision_waits_for_a_concurrent_close_and_is_refused(pool: PgPool) {
    let f = fixture(&pool).await;
    let c = campaign(&f).await;
    let member_item = item(&pool, &c, f.member.user_id, Some(f.team)).await;
    // Closing holds the campaign row; the decision waits for it, then sees
    // the campaign closed.
    let mut closing = pool.begin().await.unwrap();
    sqlx::query("UPDATE access_review_campaigns SET closed_by=$2, closed_at=now() WHERE id=$1")
        .bind(id(&c))
        .bind(f.admin.user_id)
        .execute(&mut *closing)
        .await
        .unwrap();
    let release = async {
        loop {
            let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE NOT granted AND locktype IN ('transactionid','tuple')")
                .fetch_one(&pool).await.unwrap();
            if waiting > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        closing.commit().await.unwrap();
    };
    let ((status, _), ()) = tokio::join!(decide(&f, &f.owner, member_item, "keep"), release);
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM access_review_decisions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn the_inbox_matches_the_rule_and_pages(pool: PgPool) {
    let f = fixture(&pool).await;
    let second = user(&pool, "second-admin", "admin").await;
    member(&pool, f.team, f.outsider.user_id, "admin").await;
    group_member(&pool, f.project, f.auditor.user_id).await;
    let c = campaign(&f).await;
    let member_item = item(&pool, &c, f.member.user_id, Some(f.team)).await;
    assert_eq!(
        assign(&f, &f.admin, &c, member_item, Some(second.user_id))
            .await
            .0,
        StatusCode::OK
    );
    for who in [
        &f.admin,
        &second,
        &f.auditor,
        &f.owner,
        &f.member,
        &f.outsider,
    ] {
        let got: std::collections::BTreeSet<String> = inbox(&f, who)
            .await
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_owned())
            .collect();
        let want: std::collections::BTreeSet<String> = sqlx::query_scalar::<_, Uuid>("SELECT id FROM access_review_items WHERE access_review_may_decide(id,$1) AND access_review_item_active(id)")
            .bind(who.user_id).fetch_all(&pool).await.unwrap().into_iter().map(|u| u.to_string()).collect();
        assert_eq!(got, want, "{}", who.email);
    }
    let all = inbox(&f, &f.owner).await;
    assert!(all.len() >= 2);
    let mut paged = Vec::new();
    for offset in 0..all.len() {
        let (_, page) = call(
            &f.s,
            &f.owner,
            "GET",
            &format!("{INBOX}?limit=1&offset={offset}"),
            Value::Null,
        )
        .await;
        paged.extend(
            page["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["id"].clone()),
        );
    }
    assert_eq!(
        paged,
        all.iter().map(|i| i["id"].clone()).collect::<Vec<_>>()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn the_inbox_stays_fast_at_twenty_campaigns_of_five_thousand(pool: PgPool) {
    let f = fixture(&pool).await;
    sqlx::query("WITH u AS (INSERT INTO users(id,email) SELECT gen_random_uuid(),'bulk'||n||'@example.test' FROM generate_series(1,4997) n RETURNING id) INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) SELECT $1,id,'member','manual' FROM u")
        .bind(f.team).execute(&pool).await.unwrap();
    for n in 0..20 {
        let (status, c) = call(&f.s, &f.admin, "POST", REVIEWS, json!({"name":format!("Load {n}"),"subjects":["workspace_memberships"],"workspace_ids":[f.team]})).await;
        assert_eq!(status, StatusCode::OK, "{c}");
    }
    let timed = async |who: &BrowserPrincipal| {
        let started = std::time::Instant::now();
        let (status, _) = call(&f.s, who, "GET", &format!("{INBOX}?limit=50"), Value::Null).await;
        (
            status,
            started.elapsed() < std::time::Duration::from_secs(5),
        )
    };
    assert_eq!(timed(&f.member).await, (StatusCode::OK, true), "a member");
    assert_eq!(
        timed(&f.owner).await,
        (StatusCode::OK, true),
        "the owner of every item"
    );
    assert_eq!(
        timed(&f.admin).await,
        (StatusCode::OK, true),
        "a Platform Admin"
    );
}

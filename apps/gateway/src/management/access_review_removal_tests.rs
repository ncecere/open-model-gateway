//! #90 slice 3: a reviewer's "remove" decision removes the access at once,
//! through the same removal paths and protections as the direct routes
//! (last owner, last Platform Admin, keys revoked once no membership
//! remains), or is refused and nothing is recorded. Group-provided access is
//! never removed here (flag_idp instead).
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
async fn item(pool: &PgPool, campaign: &Value, user: Uuid, ws: Option<Uuid>) -> Uuid {
    sqlx::query_scalar("SELECT id FROM access_review_items WHERE campaign_id=$1 AND user_id=$2 AND workspace_id IS NOT DISTINCT FROM $3")
        .bind(id(campaign)).bind(user).bind(ws).fetch_one(pool).await.unwrap()
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
fn reason(body: &Value) -> Option<&str> {
    body["error"]["reason"].as_str()
}
async fn decisions(pool: &PgPool, item: Uuid) -> Vec<String> {
    sqlx::query_scalar("SELECT decision FROM access_review_decisions WHERE item_id=$1 ORDER BY seq")
        .bind(item)
        .fetch_all(pool)
        .await
        .unwrap()
}
async fn live_grant(pool: &PgPool, ws: Uuid, user: Uuid) -> bool {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND revoked_at IS NULL)")
        .bind(ws).bind(user).fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn an_owner_removes_a_member_through_the_membership_path(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, key) = key(&f, &f.member, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{key}");
    let c = campaign(&f).await;
    let member_item = item(&pool, &c, f.member.user_id, Some(f.team)).await;
    let (status, d) = decide(&f, &f.owner, member_item, "remove").await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(
        (d["decision"].as_str(), d["active"].as_bool()),
        (Some("remove"), Some(false))
    );
    assert!(
        !live_grant(&pool, f.team, f.member.user_id).await,
        "membership revoked"
    );
    let key_revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM api_keys WHERE id=$1")
            .bind(id(&key))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(key_revoked, "their key in the workspace revoked");
    // Both the review decision and the membership change are audited.
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events WHERE action IN ('access_review.decided','workspace.manual_membership_revoked') AND workspace_id=$1 ORDER BY created_at, action")
        .bind(f.team).fetch_all(&pool).await.unwrap();
    assert_eq!(
        actions,
        [
            "access_review.decided",
            "workspace.manual_membership_revoked"
        ]
    );
    // Final: nothing more to decide; the item leaves the inbox.
    let (status, body) = decide(&f, &f.owner, member_item, "keep").await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::CONFLICT, Some("access_review_item_inactive"))
    );
    let (_, inbox) = call(&f.s, &f.owner, "GET", INBOX, Value::Null).await;
    assert!(
        inbox["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["id"] != member_item.to_string())
    );
    assert_eq!(decisions(&pool, member_item).await, ["remove"]);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn removal_keeps_the_direct_routes_protections(pool: PgPool) {
    let f = fixture(&pool).await;
    // A Team admin who is not an owner, and a second owner.
    member(&pool, f.team, f.outsider.user_id, "admin").await;
    let second_owner = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'second-owner@example.test')")
        .bind(second_owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'user','manual')")
        .bind(second_owner)
        .execute(&pool)
        .await
        .unwrap();
    member(&pool, f.team, second_owner, "owner").await;
    let c = campaign(&f).await;
    let owner_item = item(&pool, &c, f.owner.user_id, Some(f.team)).await;
    let second_item = item(&pool, &c, second_owner, Some(f.team)).await;
    // Only an owner (or a Platform Admin) removes an owner, as on the members route.
    let (status, body) = decide(&f, &f.outsider, owner_item, "remove").await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::CONFLICT, Some("access_review_owner_removal"))
    );
    // The owner removes the second owner; then the last owner cannot be removed.
    let (status, d) = decide(&f, &f.owner, second_item, "remove").await;
    assert_eq!(status, StatusCode::OK, "{d}");
    let project_item = item(&pool, &c, f.owner.user_id, Some(f.project)).await;
    sqlx::query(
        "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'admin','manual')",
    )
    .bind(f.outsider.user_id)
    .execute(&pool)
    .await
    .unwrap();
    let admin_reviewer = BrowserPrincipal {
        platform_admin: true,
        platform_auditor: true,
        ..f.outsider.clone()
    };
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/items/{project_item}/reviewer", id(&c)),
        json!({"user_id":f.outsider.user_id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = decide(&f, &admin_reviewer, project_item, "remove").await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::CONFLICT, Some("access_review_removal_refused")),
        "{body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("last workspace owner")
    );
    assert!(live_grant(&pool, f.project, f.owner.user_id).await);
    assert!(
        decisions(&pool, project_item).await.is_empty(),
        "a refused removal records nothing"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn group_access_is_flagged_never_removed(pool: PgPool) {
    let f = fixture(&pool).await;
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source,mapping_id) VALUES($1,$2,'member','group',$3)")
        .bind(f.project).bind(f.outsider.user_id).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    let c = campaign(&f).await;
    let group_item = item(&pool, &c, f.outsider.user_id, Some(f.project)).await;
    let (status, body) = decide(&f, &f.owner, group_item, "remove").await;
    assert_eq!(
        (status, reason(&body)),
        (
            StatusCode::CONFLICT,
            Some("access_review_remove_requires_manual")
        )
    );
    assert!(live_grant(&pool, f.project, f.outsider.user_id).await);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_roles_are_removed_through_the_role_path(pool: PgPool) {
    let f = fixture(&pool).await;
    // The auditor also holds a manual User grant, so removing Auditor leaves them entitled.
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'user','manual')")
        .bind(f.auditor.user_id)
        .execute(&pool)
        .await
        .unwrap();
    // Another person holds only an Auditor grant: removing it removes their platform access,
    // exactly as revoking the role directly does (entitlement loss).
    let lone = user(&pool, "lone-auditor", "auditor").await;
    let c = campaign(&f).await;
    let auditor_item = item(&pool, &c, f.auditor.user_id, None).await;
    let lone_item = item(&pool, &c, lone.user_id, None).await;
    let (status, d) = decide(&f, &f.admin, auditor_item, "remove").await;
    assert_eq!(status, StatusCode::OK, "{d}");
    let roles: Vec<String> = sqlx::query_scalar("SELECT role FROM platform_role_grants WHERE user_id=$1 AND revoked_at IS NULL ORDER BY role")
        .bind(f.auditor.user_id).fetch_all(&pool).await.unwrap();
    assert_eq!(roles, ["user"]);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='role.revoked' AND resource_id=$1",
    )
    .bind(f.auditor.user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);
    let (status, d) = decide(&f, &f.admin, lone_item, "remove").await;
    assert_eq!(status, StatusCode::OK, "{d}");
    let reason: Option<String> = sqlx::query_scalar("SELECT disable_reason FROM users WHERE id=$1")
        .bind(lone.user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reason.as_deref(), Some("entitlement_loss"));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn admins_review_each_other_and_never_themselves(pool: PgPool) {
    let f = fixture(&pool).await;
    let second = user(&pool, "second-admin", "admin").await;
    let c = campaign(&f).await;
    let admin_item = item(&pool, &c, f.admin.user_id, None).await;
    let second_item = item(&pool, &c, second.user_id, None).await;
    // A reviewer of a platform role is always another current admin, so the
    // last-admin protection of the role path is never what stops a review:
    // the removed admin simply decides nothing afterwards.
    assert_eq!(
        decide(&f, &second, second_item, "remove").await.0,
        StatusCode::NOT_FOUND,
        "own role"
    );
    let (status, d) = decide(&f, &second, admin_item, "remove").await;
    assert_eq!(status, StatusCode::OK, "{d}");
    let former = BrowserPrincipal {
        platform_admin: false,
        platform_auditor: false,
        ..f.admin.clone()
    };
    assert_eq!(
        decide(&f, &former, second_item, "remove").await.0,
        StatusCode::NOT_FOUND,
        "a former admin"
    );
    assert!(decisions(&pool, second_item).await.is_empty());
    let admins: i64 =
        sqlx::query_scalar("SELECT count(*) FROM effective_platform_roles WHERE role='admin'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(admins, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_remove_decision_is_never_recorded_without_the_removal(pool: PgPool) {
    let f = fixture(&pool).await;
    let c = campaign(&f).await;
    let member_item = item(&pool, &c, f.member.user_id, Some(f.team)).await;
    // A remove decision inserted directly, the access left in place: refused at commit.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO access_review_decisions(id,item_id,decision,decided_by) VALUES($1,$2,'remove',$3)")
        .bind(Uuid::new_v4()).bind(member_item).bind(f.owner.user_id).execute(&mut *tx).await.unwrap();
    let committed = tx.commit().await;
    let code = committed.err().and_then(|e| {
        e.as_database_error()
            .and_then(|d| d.code())
            .map(|c| c.into_owned())
    });
    assert_eq!(code.as_deref(), Some("23514"), "remove without removal");
    assert!(live_grant(&pool, f.team, f.member.user_id).await);
}

fn sqlstate(r: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> Option<String> {
    r.err().map(|e| {
        e.as_database_error()
            .and_then(|d| d.code())
            .map(|c| c.into_owned())
            .unwrap_or_else(|| format!("{e}"))
    })
}
async fn assign(f: &Fixture, campaign: &Value, item: Uuid, reviewer: Uuid) {
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/items/{item}/reviewer", id(campaign)),
        json!({"user_id":reviewer}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn someone_who_may_not_decide_takes_no_locks_and_gets_404(pool: PgPool) {
    let f = fixture(&pool).await;
    let c = campaign(&f).await;
    let auditor_item = item(&pool, &c, f.auditor.user_id, None).await;
    let owner_item = item(&pool, &c, f.owner.user_id, Some(f.team)).await;
    let member_item = item(&pool, &c, f.member.user_id, Some(f.team)).await;
    // Another transaction holds the catalog lock shared (as admission does):
    // a removal's exclusive catalog lock would wait for it.
    let mut admission = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
        .execute(&mut *admission)
        .await
        .unwrap();
    let quick = async |who: &BrowserPrincipal, item: Uuid| {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            decide(&f, who, item, "remove"),
        )
        .await
        .map(|r| r.0)
    };
    assert_eq!(
        quick(&f.owner, auditor_item).await.ok(),
        Some(StatusCode::NOT_FOUND),
        "owner on a platform item"
    );
    assert_eq!(
        quick(&f.member, auditor_item).await.ok(),
        Some(StatusCode::NOT_FOUND),
        "member on a platform item"
    );
    assert_eq!(
        quick(&f.auditor, member_item).await.ok(),
        Some(StatusCode::NOT_FOUND),
        "auditor on a membership"
    );
    assert_eq!(
        quick(&f.member, owner_item).await.ok(),
        Some(StatusCode::NOT_FOUND),
        "member on the owner"
    );
    admission.rollback().await.unwrap();
    for item in [auditor_item, owner_item, member_item] {
        assert!(decisions(&pool, item).await.is_empty());
    }
    // A closed campaign: 404 for everyone.
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/close", id(&c)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        decide(&f, &f.owner, member_item, "remove").await.0,
        StatusCode::NOT_FOUND
    );
    assert!(live_grant(&pool, f.team, f.member.user_id).await);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_membership_removal_keeps_group_access_and_other_workspaces_keys(pool: PgPool) {
    let f = fixture(&pool).await;
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source,mapping_id) VALUES($1,$2,'member','group',$3)")
        .bind(f.team).bind(f.member.user_id).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    member(&pool, f.project, f.member.user_id, "member").await;
    let (status, here) = key(&f, &f.member, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let (status, there) = key(&f, &f.member, f.project, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let c = campaign(&f).await;
    let manual = sqlx::query_scalar::<_, Uuid>("SELECT id FROM access_review_items WHERE campaign_id=$1 AND user_id=$2 AND workspace_id=$3 AND source='manual'")
        .bind(id(&c)).bind(f.member.user_id).bind(f.team).fetch_one(&pool).await.unwrap();
    let (status, d) = decide(&f, &f.owner, manual, "remove").await;
    assert_eq!(status, StatusCode::OK, "{d}");
    let sources: Vec<String> = sqlx::query_scalar("SELECT source FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND revoked_at IS NULL")
        .bind(f.team).bind(f.member.user_id).fetch_all(&pool).await.unwrap();
    assert_eq!(sources, ["group"], "the group membership stays");
    for (key, label) in [
        (id(&here), "a remaining membership keeps the key"),
        (id(&there), "other workspaces untouched"),
    ] {
        let revoked: bool =
            sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM api_keys WHERE id=$1")
                .bind(key)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!revoked, "{label}");
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_platform_role_removal_revokes_only_the_items_grant(pool: PgPool) {
    let f = fixture(&pool).await;
    let second = user(&pool, "second-admin", "admin").await;
    sqlx::query(
        "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'admin','bootstrap')",
    )
    .bind(second.user_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source,mapping_id) VALUES($1,'auditor','group',$2)").bind(f.auditor.user_id).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    let c = campaign(&f).await;
    let item_of = async |user: Uuid, source: &str| -> Uuid {
        sqlx::query_scalar("SELECT id FROM access_review_items WHERE campaign_id=$1 AND user_id=$2 AND workspace_id IS NULL AND source=$3")
            .bind(id(&c)).bind(user).bind(source).fetch_one(&pool).await.unwrap()
    };
    let (manual, bootstrap) = (
        item_of(second.user_id, "manual").await,
        item_of(second.user_id, "bootstrap").await,
    );
    assert_eq!(
        decide(&f, &f.admin, bootstrap, "keep").await.0,
        StatusCode::OK
    );
    let (status, d) = decide(&f, &f.admin, manual, "remove").await;
    assert_eq!(status, StatusCode::OK, "{d}");
    let sources: Vec<String> = sqlx::query_scalar("SELECT source FROM platform_role_grants WHERE user_id=$1 AND role='admin' AND revoked_at IS NULL")
        .bind(second.user_id).fetch_all(&pool).await.unwrap();
    assert_eq!(sources, ["bootstrap"], "the kept grant stays");
    let (_, items) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{REVIEWS}/{}/items?limit=200", id(&c)),
        Value::Null,
    )
    .await;
    let kept = items["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == bootstrap.to_string())
        .unwrap()
        .clone();
    assert_eq!(
        (kept["active"].as_bool(), kept["decision"].as_str()),
        (Some(true), Some("keep"))
    );
    // A manual Auditor grant beside a group one: the group grant stays, the person stays active.
    let (status, d) = decide(
        &f,
        &f.admin,
        item_of(f.auditor.user_id, "manual").await,
        "remove",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{d}");
    let left: Vec<String> = sqlx::query_scalar(
        "SELECT source FROM platform_role_grants WHERE user_id=$1 AND revoked_at IS NULL",
    )
    .bind(f.auditor.user_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(left, ["group"]);
    let disabled: bool =
        sqlx::query_scalar("SELECT disabled_at IS NOT NULL FROM users WHERE id=$1")
            .bind(f.auditor.user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!disabled);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_refused_role_removal_records_nothing(pool: PgPool) {
    let f = fixture(&pool).await;
    // An Auditor with no other role who is the only owner of a Team: removing
    // the role would strand the Team (the role path refuses).
    let lone = user(&pool, "lone-owner", "auditor").await;
    let team = workspace(&pool, "Lone team", "team", lone.user_id).await;
    let c = campaign(&f).await;
    let lone_item = item(&pool, &c, lone.user_id, None).await;
    let (status, body) = decide(&f, &f.admin, lone_item, "remove").await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::CONFLICT, Some("access_review_removal_refused")),
        "{body}"
    );
    assert!(decisions(&pool, lone_item).await.is_empty());
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform_role_grants WHERE user_id=$1 AND revoked_at IS NULL",
    )
    .bind(lone.user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(live, 1);
    assert!(live_grant(&pool, team, lone.user_id).await);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_platform_admin_assigned_removes_a_non_last_owner(pool: PgPool) {
    let f = fixture(&pool).await;
    let second_owner = user(&pool, "second-owner", "user").await;
    member(&pool, f.team, second_owner.user_id, "owner").await;
    let c = campaign(&f).await;
    let target = item(&pool, &c, second_owner.user_id, Some(f.team)).await;
    assign(&f, &c, target, f.admin.user_id).await;
    let (status, d) = decide(&f, &f.admin, target, "remove").await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert!(!live_grant(&pool, f.team, second_owner.user_id).await);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn the_guard_refuses_removals_the_routes_would_refuse(pool: PgPool) {
    let f = fixture(&pool).await;
    member(&pool, f.team, f.outsider.user_id, "admin").await;
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source,mapping_id) VALUES($1,$2,'member','group',$3)")
        .bind(f.project).bind(f.member.user_id).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    let c = campaign(&f).await;
    let owner_item = item(&pool, &c, f.owner.user_id, Some(f.team)).await;
    let group_item = item(&pool, &c, f.member.user_id, Some(f.project)).await;
    let insert = "INSERT INTO access_review_decisions(id,item_id,decision,decided_by) VALUES($1,$2,'remove',$3)";
    // Refused by the guard at the statement itself (not only at commit).
    let mut tx = pool.begin().await.unwrap();
    let r = sqlx::query(insert)
        .bind(Uuid::new_v4())
        .bind(group_item)
        .bind(f.owner.user_id)
        .execute(&mut *tx)
        .await;
    assert_eq!(sqlstate(r).as_deref(), Some("23514"), "group access");
    tx.rollback().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let r = sqlx::query(insert)
        .bind(Uuid::new_v4())
        .bind(owner_item)
        .bind(f.outsider.user_id)
        .execute(&mut *tx)
        .await;
    assert_eq!(
        sqlstate(r).as_deref(),
        Some("23514"),
        "an owner by a workspace admin"
    );
    tx.rollback().await.unwrap();
    // The commit check is about the item's own grant: disabling the workspace
    // instead of revoking the grant does not satisfy it.
    let member_item = item(&pool, &c, f.member.user_id, Some(f.team)).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(insert)
        .bind(Uuid::new_v4())
        .bind(member_item)
        .bind(f.owner.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT omg_scope_lock_exclusive(72419511, omg_scope_key($1))")
        .bind(f.team)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(f.team)
        .execute(&mut *tx)
        .await
        .unwrap();
    let code = tx.commit().await.err().and_then(|e| {
        e.as_database_error()
            .and_then(|d| d.code())
            .map(|c| c.into_owned())
    });
    assert_eq!(
        code.as_deref(),
        Some("23514"),
        "disabled instead of revoked"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_removals_and_closing_serialize(pool: PgPool) {
    let f = fixture(&pool).await;
    member(&pool, f.team, f.outsider.user_id, "admin").await;
    let c = campaign(&f).await;
    let member_item = item(&pool, &c, f.member.user_id, Some(f.team)).await;
    // Two reviewers remove the same item at once: one removes it, the other finds it gone.
    let (a, b) = tokio::join!(
        decide(&f, &f.owner, member_item, "remove"),
        decide(&f, &f.outsider, member_item, "remove")
    );
    let mut got = [
        (a.0, reason(&a.1).map(str::to_owned)),
        (b.0, reason(&b.1).map(str::to_owned)),
    ];
    got.sort();
    assert_eq!(
        got,
        [
            (StatusCode::OK, None),
            (
                StatusCode::CONFLICT,
                Some("access_review_item_inactive".to_owned())
            )
        ]
    );
    assert_eq!(decisions(&pool, member_item).await, ["remove"]);
    // A removal racing a close: the close holds the installation row; the
    // removal waits for it, then finds the campaign closed.
    let second = user(&pool, "second-member", "user").await;
    member(&pool, f.team, second.user_id, "member").await;
    let c2 = campaign(&f).await;
    let second_item = item(&pool, &c2, second.user_id, Some(f.team)).await;
    let mut closing = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM installation WHERE singleton FOR NO KEY UPDATE")
        .execute(&mut *closing)
        .await
        .unwrap();
    sqlx::query("UPDATE access_review_campaigns SET closed_by=$2, closed_at=now() WHERE id=$1")
        .bind(id(&c2))
        .bind(f.admin.user_id)
        .execute(&mut *closing)
        .await
        .unwrap();
    let release = async {
        loop {
            let waiting: i64 =
                sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE NOT granted")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            if waiting > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        closing.commit().await.unwrap();
    };
    let ((status, _), ()) = tokio::join!(decide(&f, &f.owner, second_item, "remove"), release);
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(live_grant(&pool, f.team, second.user_id).await);
    // A removal racing the direct member route: the member is removed once, and
    // a remove decision exists only if its removal happened.
    let third = user(&pool, "third-member", "user").await;
    member(&pool, f.team, third.user_id, "member").await;
    let c3 = campaign(&f).await;
    let third_item = item(&pool, &c3, third.user_id, Some(f.team)).await;
    let direct_path = format!("/api/v1/workspaces/{}/members/{}", f.team, third.user_id);
    let (review, direct) = tokio::join!(
        decide(&f, &f.owner, third_item, "remove"),
        call(&f.s, &f.owner, "DELETE", &direct_path, Value::Null)
    );
    assert!(
        matches!(review.0, StatusCode::OK | StatusCode::CONFLICT),
        "review: unexpected status"
    );
    assert_eq!(direct.0, StatusCode::OK);
    assert!(!live_grant(&pool, f.team, third.user_id).await);
    assert_eq!(
        decisions(&pool, third_item).await.len(),
        usize::from(review.0 == StatusCode::OK)
    );
}

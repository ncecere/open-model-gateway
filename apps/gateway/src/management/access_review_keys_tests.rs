//! #90 slice 6: keys of shared workspaces as access review subjects. One item
//! per live key lineage of an enabled Team or Project (human and service
//! keys; never personal workspaces), reviewed by the workspace's owners and
//! admins (never the key's own holder; Platform Admins have no key authority
//! in a workspace they do not administer, so they are not key reviewers).
//! "remove" revokes the lineage through the direct revoke path. Platform
//! views never name keys or holders, as key safety's platform scope.
use super::*;

const REVIEWS: &str = "/api/v1/platform/access-reviews";
const INBOX: &str = "/api/v1/me/access-reviews";

async fn start(f: &Fixture, body: Value) -> (StatusCode, Value) {
    call(&f.s, &f.admin, "POST", REVIEWS, body).await
}
async fn key_campaign(f: &Fixture) -> Value {
    let (status, c) = start(f, json!({"name":"Keys","subjects":["workspace_keys"]})).await;
    assert_eq!(status, StatusCode::OK, "{c}");
    c
}
async fn named_key(f: &Fixture, who: &BrowserPrincipal, ws: Uuid, body: Value) -> Uuid {
    let (status, k) = call(
        &f.s,
        who,
        "POST",
        &format!("/api/v1/workspaces/{ws}/keys"),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{k}");
    id(&k)
}
/// A member's human key and a service account's key in the Team.
async fn team_keys(f: &Fixture) -> (Uuid, Uuid) {
    let human = named_key(
        f,
        &f.member,
        f.team,
        json!({"name":"zebra-human-key","expires_in_days":30}),
    )
    .await;
    let (status, account) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/service-accounts", f.team),
        json!({"name":"zebra-robot"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{account}");
    let service = named_key(
        f,
        &f.owner,
        f.team,
        json!({"name":"zebra-service-key","expires_in_days":30,"service_account_id":id(&account)}),
    )
    .await;
    (human, service)
}
async fn key_item(pool: &PgPool, campaign: &Value, key: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT id FROM access_review_items WHERE campaign_id=$1 AND api_key_id=$2")
        .bind(id(campaign))
        .bind(key)
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
async fn inbox(f: &Fixture, who: &BrowserPrincipal) -> Vec<Value> {
    let (status, body) = call(&f.s, who, "GET", INBOX, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].as_array().unwrap().clone()
}
fn reason(body: &Value) -> Option<&str> {
    body["error"]["reason"].as_str()
}
/// The raw CSV export as text.
async fn export(f: &Fixture, who: &BrowserPrincipal, campaign: Uuid) -> (StatusCode, String) {
    let response = routes()
        .layer(Extension(ManagementPrincipal::Browser(who.clone())))
        .layer(Extension(who.clone()))
        .with_state(f.s.clone())
        .oneshot(
            Request::builder()
                .uri(format!("{REVIEWS}/{campaign}/export"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}
async fn revoked(pool: &PgPool, key: Uuid) -> bool {
    sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM api_keys WHERE id=$1")
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn live_keys_of_shared_workspaces_are_snapshotted_by_lineage(pool: PgPool) {
    let f = fixture(&pool).await;
    let (human, service) = team_keys(&f).await;
    // Not reviewed: a personal workspace's key, a revoked key, an expired key.
    let personal = named_key(
        &f,
        &f.owner,
        f.personal,
        json!({"name":"personal","expires_in_days":30}),
    )
    .await;
    let gone = named_key(
        &f,
        &f.owner,
        f.team,
        json!({"name":"gone","expires_in_days":30}),
    )
    .await;
    let (status, _) = call(
        &f.s,
        &f.owner,
        "DELETE",
        &format!("/api/v1/workspaces/{}/keys/{gone}", f.team),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let expired = named_key(
        &f,
        &f.owner,
        f.project,
        json!({"name":"expired","expires_in_days":30}),
    )
    .await;
    sqlx::query("UPDATE api_keys SET expires_at=now()-interval '1 minute' WHERE id=$1")
        .bind(expired)
        .execute(&pool)
        .await
        .unwrap();
    let c = key_campaign(&f).await;
    let keys: std::collections::BTreeSet<Uuid> =
        sqlx::query_scalar("SELECT api_key_id FROM access_review_items WHERE campaign_id=$1")
            .bind(id(&c))
            .fetch_all(&pool)
            .await
            .unwrap()
            .into_iter()
            .collect();
    assert_eq!(keys, [human, service].into_iter().collect());
    for k in [personal, gone, expired] {
        assert!(!keys.contains(&k), "not a review subject");
    }
    assert_eq!(c["counts"]["by_kind"]["workspace_key"], 2, "{c}");
    assert_eq!(c["counts"]["by_kind"]["workspace_membership"], 0, "{c}");
    // Items name the key, its lineage and holder, derived by the database.
    let (holder, account, lineage, role, source): (
        Option<Uuid>,
        Option<Uuid>,
        Uuid,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as("SELECT user_id, service_account_id, key_lineage_id, role, source FROM access_review_items WHERE campaign_id=$1 AND api_key_id=$2")
        .bind(id(&c))
        .bind(service)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(holder.is_none() && account.is_some() && role.is_none() && source.is_none());
    assert_eq!(lineage, service);
    // The workspace filter narrows keys too; a Project without keys is empty.
    let (status, body) = start(
        &f,
        json!({"name":"Project keys","subjects":["workspace_keys"],"workspace_ids":[f.project]}),
    )
    .await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::CONFLICT, Some("access_review_empty"))
    );
    // All three subjects together.
    let (status, all) = start(
        &f,
        json!({"name":"All","subjects":["workspace_memberships","platform_roles","workspace_keys"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert_eq!(all["counts"]["by_kind"]["workspace_key"], 2, "{all}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn the_database_snapshots_only_live_shared_keys(pool: PgPool) {
    let f = fixture(&pool).await;
    let personal = named_key(
        &f,
        &f.owner,
        f.personal,
        json!({"name":"personal","expires_in_days":30}),
    )
    .await;
    // A campaign without the keys subject, then one with it: the guard
    // refuses a personal key, and a key named by a membership campaign.
    let mut tx = pool.begin().await.unwrap();
    let c = Uuid::now_v7();
    sqlx::query("INSERT INTO access_review_campaigns(id,name,subjects,created_by,due_at) VALUES($1,'Keys','{workspace_keys}',$2,now()+interval '2 days')")
        .bind(c)
        .bind(f.admin.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let refused = sqlx::query("INSERT INTO access_review_items(id,campaign_id,subject_kind,api_key_id) VALUES(gen_random_uuid(),$1,'workspace_key',$2)")
        .bind(c)
        .bind(personal)
        .execute(&mut *tx)
        .await;
    assert!(refused.is_err(), "a personal key is never a review subject");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_views_never_name_keys_or_holders(pool: PgPool) {
    let f = fixture(&pool).await;
    let (human, service) = team_keys(&f).await;
    let c = key_campaign(&f).await;
    for who in [&f.admin, &f.auditor] {
        let (status, items) = call(
            &f.s,
            who,
            "GET",
            &format!("{REVIEWS}/{}/items", id(&c)),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{items}");
        let rows = items["data"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(row["subject_kind"], "workspace_key");
            for field in [
                "key_name",
                "user_id",
                "email",
                "display_name",
                "service_account_id",
                "service_account_name",
            ] {
                assert!(row[field].is_null(), "platform readers never see {field}");
            }
            assert!(row["api_key_id"].is_string() && row["expires_at"].is_string());
            assert!(row.as_object().unwrap().contains_key("last_used_at"));
        }
        let holders: std::collections::BTreeSet<&str> = rows
            .iter()
            .map(|r| r["holder_kind"].as_str().unwrap())
            .collect();
        assert_eq!(holders, ["member", "service_account"].into_iter().collect());
        let text = items.to_string();
        assert!(!text.contains("zebra"), "no key or account name");
        assert!(
            !text.contains(&f.member.user_id.to_string()),
            "no holder id"
        );
        let (status, csv) = export(&f, who, id(&c)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            csv.lines()
                .next()
                .unwrap()
                .ends_with(",api_key_id,holder_kind,expires_at,last_used_at")
        );
        assert!(csv.contains(&human.to_string()) && csv.contains(&service.to_string()));
        assert!(!csv.contains("zebra"), "the export names no key or account");
        assert!(
            !csv.contains("member@example.test"),
            "the export names no holder"
        );
    }
    // Assigning (a platform response) is masked the same way.
    let item = key_item(&pool, &c, service).await;
    member(&pool, f.team, f.outsider.user_id, "admin").await;
    let (status, row) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/items/{item}/reviewer", id(&c)),
        json!({"user_id":f.outsider.user_id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert!(row["key_name"].is_null() && row["service_account_name"].is_null());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn owners_and_admins_review_keys_but_never_their_own(pool: PgPool) {
    let f = fixture(&pool).await;
    let (human, service) = team_keys(&f).await;
    let c = key_campaign(&f).await;
    let human_item = key_item(&pool, &c, human).await;
    let service_item = key_item(&pool, &c, service).await;
    // The owner sees both, named as the workspace key list names them.
    let rows = inbox(&f, &f.owner).await;
    let mine: Vec<&Value> = rows
        .iter()
        .filter(|r| r["campaign_id"] == c["id"])
        .collect();
    assert_eq!(mine.len(), 2);
    let human_row = mine
        .iter()
        .find(|r| r["api_key_id"] == human.to_string())
        .unwrap();
    assert_eq!(human_row["key_name"], "zebra-human-key");
    assert_eq!(human_row["holder_kind"], "member");
    assert_eq!(human_row["email"], "member@example.test");
    assert_eq!(human_row["reviewable"], true);
    let service_row = mine
        .iter()
        .find(|r| r["api_key_id"] == service.to_string())
        .unwrap();
    assert_eq!(service_row["service_account_name"], "zebra-robot");
    assert!(service_row["user_id"].is_null());
    // The holder, made an admin, still never decides their own key.
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2")
        .bind(f.team)
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    member(&pool, f.team, f.member.user_id, "admin").await;
    let (status, _) = decide(&f, &f.member, human_item, "keep").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "own key");
    let (status, d) = decide(&f, &f.member, service_item, "keep").await;
    assert_eq!(status, StatusCode::OK, "{d}");
    // A Platform Admin who does not administer the Team is not a key reviewer,
    // and cannot be assigned one.
    let (status, _) = decide(&f, &f.admin, human_item, "keep").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let second = user(&pool, "second-admin", "admin").await;
    let (status, body) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/items/{human_item}/reviewer", id(&c)),
        json!({"user_id":second.user_id}),
    )
    .await;
    assert_eq!(
        (status, reason(&body)),
        (
            StatusCode::CONFLICT,
            Some("access_review_reviewer_ineligible")
        )
    );
    // Keys are not group-provided: nothing to flag.
    let (status, body) = decide(&f, &f.owner, human_item, "flag_idp").await;
    assert_eq!(
        (status, reason(&body)),
        (
            StatusCode::CONFLICT,
            Some("access_review_flag_requires_group")
        )
    );
    // The reminder feed counts key items.
    let (status, feed) = call(
        &f.s,
        &f.owner,
        "GET",
        "/api/v1/me/notifications",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        feed["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["source"] == "access_review" && n["id"] == c["id"])
    );
    // The inbox matches the database rule for every viewer.
    for who in [
        &f.admin,
        &f.auditor,
        &f.owner,
        &f.member,
        &f.outsider,
        &second,
    ] {
        let got: std::collections::BTreeSet<String> = inbox(&f, who)
            .await
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_owned())
            .collect();
        let want: std::collections::BTreeSet<String> = sqlx::query_scalar::<_, Uuid>("SELECT id FROM access_review_items WHERE access_review_may_decide(id,$1) AND access_review_item_active(id)")
            .bind(who.user_id)
            .fetch_all(&pool)
            .await
            .unwrap()
            .into_iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(got, want, "inbox equals the rule");
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn remove_revokes_the_whole_lineage_and_it_never_returns(pool: PgPool) {
    let f = fixture(&pool).await;
    let (human, service) = team_keys(&f).await;
    let c = key_campaign(&f).await;
    let item = key_item(&pool, &c, human).await;
    // A rotation keeps the lineage: the item stays in effect.
    let (status, rotated) = call(
        &f.s,
        &f.member,
        "POST",
        &format!("/api/v1/workspaces/{}/keys/{human}/rotate", f.team),
        json!({"expires_in_days":30}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    let rotated = id(&rotated);
    let active: bool = sqlx::query_scalar("SELECT access_review_item_active($1)")
        .bind(item)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(active, "rotating does not escape the review");
    let (status, d) = decide(&f, &f.owner, item, "remove").await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(
        (d["decision"].as_str(), d["active"].as_bool()),
        (Some("remove"), Some(false))
    );
    assert!(revoked(&pool, human).await && revoked(&pool, rotated).await);
    assert!(!revoked(&pool, service).await, "only the item's lineage");
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events WHERE workspace_id=$1 AND action IN ('access_review.decided','key.revoked') ORDER BY created_at, action")
        .bind(f.team)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(actions, ["access_review.decided", "key.revoked"]);
    // Final; and the holder cannot bring it back.
    let (status, body) = decide(&f, &f.owner, item, "keep").await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::CONFLICT, Some("access_review_item_inactive"))
    );
    let (status, _) = call(
        &f.s,
        &f.member,
        "PATCH",
        &format!("/api/v1/workspaces/{}/keys/{rotated}", f.team),
        json!({"disabled":false}),
    )
    .await;
    assert_ne!(status, StatusCode::OK, "a revoked key never re-enables");
    // A key revoked or expired some other way leaves the inbox.
    let service_item = key_item(&pool, &c, service).await;
    sqlx::query("UPDATE api_keys SET expires_at=now()-interval '1 minute' WHERE id=$1")
        .bind(service)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        inbox(&f, &f.owner)
            .await
            .iter()
            .all(|i| i["id"] != service_item.to_string())
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_key_remove_decision_needs_the_lineage_revoked(pool: PgPool) {
    let f = fixture(&pool).await;
    let (human, _) = team_keys(&f).await;
    let c = key_campaign(&f).await;
    let item = key_item(&pool, &c, human).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO access_review_decisions(id,item_id,decision,decided_by) VALUES($1,$2,'remove',$3)")
        .bind(Uuid::now_v7())
        .bind(item)
        .bind(f.owner.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        tx.commit().await.is_err(),
        "refused at commit without the revocation"
    );
    // The holder's own decision is refused outright.
    let mut tx = pool.begin().await.unwrap();
    let own = sqlx::query("INSERT INTO access_review_decisions(id,item_id,decision,decided_by) VALUES($1,$2,'keep',$3)")
        .bind(Uuid::now_v7())
        .bind(item)
        .bind(f.member.user_id)
        .execute(&mut *tx)
        .await;
    assert!(own.is_err());
}

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
    // A disabled key can be enabled again: it is reviewed.
    let (status, _) = call(
        &f.s,
        &f.member,
        "PATCH",
        &format!("/api/v1/workspaces/{}/keys/{human}", f.team),
        json!({"disabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Not reviewed: a suspended holder's key, a disabled workspace's key.
    let suspended = user(&pool, "suspended", "user").await;
    member(&pool, f.team, suspended.user_id, "member").await;
    let suspended_key = named_key(
        &f,
        &suspended,
        f.team,
        json!({"name":"suspended","expires_in_days":30}),
    )
    .await;
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(suspended.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let closed_team = workspace(&pool, "Closed team", "team", f.owner.user_id).await;
    let closed_key = named_key(
        &f,
        &f.owner,
        closed_team,
        json!({"name":"closed","expires_in_days":30}),
    )
    .await;
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(closed_team)
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
    for k in [personal, gone, expired, suspended_key, closed_key] {
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
    // The workspace filter narrows keys too: the Team's keys only; a Project
    // without keys is empty.
    let (status, team) = start(
        &f,
        json!({"name":"Team keys","subjects":["workspace_keys"],"workspace_ids":[f.team]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{team}");
    assert_eq!(team["counts"]["by_kind"]["workspace_key"], 2, "{team}");
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

/// The SQLSTATE of inserting one item into a new campaign (in its own
/// transaction, rolled back): `None` when accepted.
async fn guard_code(
    pool: &PgPool,
    admin: Uuid,
    subjects: &str,
    filter: &[Uuid],
    kind: &str,
    key: Option<Uuid>,
) -> Option<String> {
    let mut tx = pool.begin().await.unwrap();
    let c = Uuid::now_v7();
    sqlx::query("INSERT INTO access_review_campaigns(id,name,subjects,workspace_ids,created_by,due_at) VALUES($1,'Keys',$2::text[],$3,$4,now()+interval '2 days')")
        .bind(c)
        .bind(subjects)
        .bind(filter)
        .bind(admin)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO access_review_items(id,campaign_id,subject_kind,api_key_id) VALUES(gen_random_uuid(),$1,$2,$3)")
        .bind(c)
        .bind(kind)
        .bind(key)
        .execute(&mut *tx)
        .await
        .err()
        .and_then(|e| e.as_database_error().and_then(|d| d.code()).map(|c| c.into_owned()))
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn the_database_snapshots_only_live_shared_keys(pool: PgPool) {
    let f = fixture(&pool).await;
    let admin = f.admin.user_id;
    let (human, service) = team_keys(&f).await;
    // Positive controls: a live human key and a service key are accepted.
    for key in [human, service] {
        assert_eq!(
            guard_code(
                &pool,
                admin,
                "{workspace_keys}",
                &[],
                "workspace_key",
                Some(key)
            )
            .await,
            None
        );
    }
    let new_key = async |who: &BrowserPrincipal, ws: Uuid| {
        named_key(&f, who, ws, json!({"name":"probe","expires_in_days":30})).await
    };
    let personal = new_key(&f.owner, f.personal).await;
    let revoked_key = new_key(&f.owner, f.team).await;
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(revoked_key)
        .execute(&pool)
        .await
        .unwrap();
    let expired = new_key(&f.owner, f.team).await;
    sqlx::query("UPDATE api_keys SET expires_at=now()-interval '1 minute' WHERE id=$1")
        .bind(expired)
        .execute(&pool)
        .await
        .unwrap();
    let project_key = new_key(&f.owner, f.project).await;
    let suspended = user(&pool, "suspended", "user").await;
    member(&pool, f.team, suspended.user_id, "member").await;
    let suspended_key = new_key(&suspended, f.team).await;
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(suspended.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let closed_team = workspace(&pool, "Closed team", "team", f.owner.user_id).await;
    let closed_key = new_key(&f.owner, closed_team).await;
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(closed_team)
        .execute(&pool)
        .await
        .unwrap();
    // Each refused by the guard (23514), whatever the runtime sends.
    for (subjects, filter, kind, key, what) in [
        (
            "{workspace_keys}",
            vec![],
            "workspace_key",
            Some(personal),
            "a personal key",
        ),
        (
            "{workspace_keys}",
            vec![],
            "workspace_key",
            Some(revoked_key),
            "a revoked key",
        ),
        (
            "{workspace_keys}",
            vec![],
            "workspace_key",
            Some(expired),
            "an expired key",
        ),
        (
            "{workspace_keys}",
            vec![],
            "workspace_key",
            Some(suspended_key),
            "a suspended holder's key",
        ),
        (
            "{workspace_keys}",
            vec![],
            "workspace_key",
            Some(closed_key),
            "a disabled workspace's key",
        ),
        (
            "{workspace_keys}",
            vec![f.team],
            "workspace_key",
            Some(project_key),
            "a key outside the filter",
        ),
        (
            "{workspace_memberships}",
            vec![],
            "workspace_key",
            Some(human),
            "a key in a memberships review",
        ),
        (
            "{workspace_keys}",
            vec![],
            "workspace_key",
            None,
            "a key item without a key",
        ),
        (
            "{workspace_keys}",
            vec![],
            "workspace_membership",
            Some(human),
            "a key named as a membership",
        ),
    ] {
        assert_eq!(
            guard_code(&pool, admin, subjects, &filter, kind, key)
                .await
                .as_deref(),
            Some("23514"),
            "{what}"
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_views_never_name_keys_or_holders(pool: PgPool) {
    let f = fixture(&pool).await;
    let (human, service) = team_keys(&f).await;
    // Five more holders, so an order by holder could not pass as item order
    // by chance.
    for n in 0..5 {
        let holder = user(&pool, &format!("holder-{n}"), "user").await;
        member(&pool, f.team, holder.user_id, "member").await;
        named_key(
            &f,
            &holder,
            f.team,
            json!({"name":format!("zebra-extra-{n}"),"expires_in_days":30}),
        )
        .await;
    }
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
        assert_eq!(rows.len(), 7);
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
        // Key items are ordered by item id within a workspace, never by
        // holder.
        let ids: Vec<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "key items in item order");
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
            !csv.contains("member@example.test") && !csv.contains("holder-"),
            "the export names no holder"
        );
    }
    // Keys are never assigned: every reviewer is refused alike (an eligible
    // Team admin, the holder), so the answer names no holder; clearing
    // returns the item, masked the same way.
    let item = key_item(&pool, &c, human).await;
    member(&pool, f.team, f.outsider.user_id, "admin").await;
    for reviewer in [f.outsider.user_id, f.member.user_id] {
        let (status, body) = call(
            &f.s,
            &f.admin,
            "POST",
            &format!("{REVIEWS}/{}/items/{item}/reviewer", id(&c)),
            json!({"user_id":reviewer}),
        )
        .await;
        assert_eq!(
            (status, body["error"]["reason"].as_str()),
            (
                StatusCode::CONFLICT,
                Some("access_review_reviewer_ineligible")
            )
        );
    }
    let (status, row) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/items/{item}/reviewer", id(&c)),
        json!({"user_id":null}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert!(row["key_name"].is_null() && row["user_id"].is_null() && row["email"].is_null());
    // The export's key cells: id, holder kind, expiry, last use (none yet).
    let (_, csv) = export(&f, &f.auditor, id(&c)).await;
    let line = csv
        .split("\r\n")
        .find(|l| l.starts_with(&format!("\"{item}\"")))
        .unwrap();
    let expires: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT expires_at FROM api_keys WHERE id=$1")
            .bind(human)
            .fetch_one(&pool)
            .await
            .unwrap();
    let (_, items) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{REVIEWS}/{}/items", id(&c)),
        Value::Null,
    )
    .await;
    let row = items["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == item.to_string())
        .unwrap();
    assert_eq!(
        row["expires_at"]
            .as_str()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.timestamp_micros()),
        Some(expires.timestamp_micros())
    );
    assert!(
        line.ends_with(&format!(
            "\"{human}\",\"member\",\"{}\",\"\"",
            row["expires_at"].as_str().unwrap()
        )),
        "key cells"
    );
    // The holder as the Team's only admin: still reviewable to platform
    // readers (whether a key has a reviewer never depends on its holder).
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id<>$2")
        .bind(f.team)
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2")
        .bind(f.team)
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    member(&pool, f.team, f.member.user_id, "admin").await;
    let (_, detail) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{REVIEWS}/{}", id(&c)),
        Value::Null,
    )
    .await;
    assert_eq!(detail["counts"]["without_reviewer"], 0, "{detail}");
    let reviewable = async || -> Value {
        let (_, items) = call(
            &f.s,
            &f.auditor,
            "GET",
            &format!("{REVIEWS}/{}/items", id(&c)),
            Value::Null,
        )
        .await;
        items["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == item.to_string())
            .unwrap()["reviewable"]
            .clone()
    };
    assert_eq!(reviewable().await, true, "a workspace admin exists");
    // No owner or admin at all: nobody may review the Team's keys.
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND revoked_at IS NULL")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    let (_, detail) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{REVIEWS}/{}", id(&c)),
        Value::Null,
    )
    .await;
    assert_eq!(detail["counts"]["without_reviewer"], 7, "{detail}");
    assert_eq!(reviewable().await, false);
    // A closed review answers closed before anything about keys.
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/close", id(&c)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/items/{item}/reviewer", id(&c)),
        json!({"user_id":f.outsider.user_id}),
    )
    .await;
    assert_eq!(
        (status, body["error"]["reason"].as_str()),
        (StatusCode::CONFLICT, Some("access_review_closed"))
    );
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
    // Nobody who may not decide a key removes it: the holder, a Platform
    // Admin outside the Team, an outsider. 404, and the key stays.
    for who in [&f.member, &f.admin, &f.outsider] {
        let (status, _) = decide(&f, who, human_item, "remove").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    assert!(!revoked(&pool, human).await, "nothing revoked");
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
    // The item reports the lineage's latest use and its live key's expiry.
    key_used(&pool, f.team, rotated).await;
    let row = inbox(&f, &f.owner)
        .await
        .into_iter()
        .find(|r| r["id"] == item.to_string())
        .unwrap();
    assert!(row["last_used_at"].is_string(), "{row}");
    let expires: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT expires_at FROM api_keys WHERE id=$1")
            .bind(rotated)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        row["expires_at"]
            .as_str()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.timestamp_micros()),
        Some(expires.timestamp_micros())
    );
    // A holder who left the workspace (their key lives on): reviewers see
    // the key, not the person's name or email, as the key list shows it.
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2")
        .bind(f.team)
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let row = inbox(&f, &f.owner)
        .await
        .into_iter()
        .find(|r| r["id"] == item.to_string())
        .unwrap();
    assert!(
        row["email"].is_null() && row["display_name"].is_null(),
        "{row}"
    );
    assert_eq!(row["user_id"], f.member.user_id.to_string());
    assert_eq!(row["key_name"], "zebra-human-key");
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

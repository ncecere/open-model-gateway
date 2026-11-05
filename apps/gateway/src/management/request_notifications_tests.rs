//! #83 slice B3: access-request events in the in-app notification feed, on
//! real PostgreSQL. Derived live from the requests (visibility is rechecked
//! on every read), never copied into alert delivery; per-user read state.
use super::*;

const FEED: &str = "/api/v1/me/notifications";

async fn eligible_model(pool: &PgPool, name: &str) -> Uuid {
    let m = model(pool, name).await;
    let catalog = Uuid::new_v4();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,$2)")
        .bind(catalog)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) VALUES($1,$2)")
        .bind(catalog)
        .bind(m)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES('team',$1)")
        .bind(catalog)
        .execute(pool)
        .await
        .unwrap();
    m
}
/// The feed's access-request items for this viewer: (id, audience, state, read).
async fn request_items(
    f: &Fixture,
    who: &BrowserPrincipal,
    query: &str,
) -> Vec<(String, String, String, bool)> {
    let (status, feed) = call(&f.s, who, "GET", &format!("{FEED}{query}"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{feed}");
    feed["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["source"] == "access_request")
        .map(|n| {
            (
                n["id"].as_str().unwrap().to_owned(),
                n["audience"].as_str().unwrap().to_owned(),
                n["state"].as_str().unwrap().to_owned(),
                n["read"].as_bool().unwrap(),
            )
        })
        .collect()
}
async fn unread(f: &Fixture, who: &BrowserPrincipal) -> i64 {
    let (_, summary) = call(&f.s, who, "GET", &format!("{FEED}/summary"), Value::Null).await;
    summary["unread"].as_i64().unwrap()
}
fn rid(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn approvers_and_requesters_are_notified_live(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = eligible_model(&pool, "company/notify").await;
    // A model request by a member, and a membership request by an outsider.
    let (status, model_req) = call(
        &f.s,
        &f.member,
        "POST",
        "/api/v1/me/access-requests",
        json!({"workspace_id":f.team,"model_id":m,"idempotency_key":"n-model"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{model_req}");
    sqlx::query("UPDATE workspaces SET discoverable=true WHERE id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    let (status, join_req) = call(
        &f.s,
        &f.outsider,
        "POST",
        "/api/v1/me/access-requests",
        json!({"workspace_id":f.team,"idempotency_key":"n-join"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{join_req}");
    // The owner (an approver of both) sees both, unread; the requester does
    // not see its own pending request; a plain member and the auditor see none.
    let owner = request_items(&f, &f.owner, "").await;
    assert_eq!(owner.len(), 2, "{owner:?}");
    assert!(
        owner
            .iter()
            .all(|(_, audience, state, read)| audience == "approver"
                && state == "pending"
                && !read)
    );
    assert_eq!(unread(&f, &f.owner).await, 2);
    assert!(request_items(&f, &f.member, "").await.is_empty());
    assert!(request_items(&f, &f.auditor, "").await.is_empty());
    // A Platform Admin who is not a member decides membership requests only.
    let admin = request_items(&f, &f.admin, "").await;
    assert_eq!(
        admin.iter().map(|i| i.0.clone()).collect::<Vec<_>>(),
        vec![rid(&join_req)]
    );
    // Marked read by id: still listed, read, and gone from the unread view.
    let (status, marked) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("{FEED}/read"),
        json!({"ids":[rid(&model_req)]}),
    )
    .await;
    assert_eq!(
        (status, marked["marked"].clone()),
        (StatusCode::OK, json!(1))
    );
    assert_eq!(unread(&f, &f.owner).await, 1);
    let still = request_items(&f, &f.owner, "").await;
    assert!(still.iter().any(|i| i.0 == rid(&model_req) && i.3));
    assert_eq!(request_items(&f, &f.owner, "?status=unread").await.len(), 1);
    // Decided: the approvers' item goes, the requester's arrives unread.
    let (status, _) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!(
            "/api/v1/workspaces/{}/access-requests/{}/approve",
            f.team,
            rid(&model_req)
        ),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !request_items(&f, &f.owner, "")
            .await
            .iter()
            .any(|i| i.0 == rid(&model_req))
    );
    let mine = request_items(&f, &f.member, "").await;
    assert_eq!(
        mine,
        vec![(
            rid(&model_req),
            "requester".into(),
            "approved".into(),
            false
        )]
    );
    assert_eq!(unread(&f, &f.member).await, 1);
    // Mark all read covers request items.
    let (status, marked) = call(
        &f.s,
        &f.member,
        "POST",
        &format!("{FEED}/read"),
        json!({"all":true}),
    )
    .await;
    assert_eq!(
        (status, marked["marked"].clone()),
        (StatusCode::OK, json!(1))
    );
    assert_eq!(unread(&f, &f.member).await, 0);
    // Another user's request id is ignored, never disclosed.
    let (_, marked) = call(
        &f.s,
        &f.outsider,
        "POST",
        &format!("{FEED}/read"),
        json!({"ids":[rid(&model_req)]}),
    )
    .await;
    assert_eq!(marked["marked"], 0);
    // Denied: the outsider is told, then the admin's item is gone.
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("/api/v1/platform/access-requests/{}/deny", rid(&join_req)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        request_items(&f, &f.outsider, "").await,
        vec![(rid(&join_req), "requester".into(), "denied".into(), false)]
    );
    assert!(request_items(&f, &f.admin, "").await.is_empty());
    // Items carry the frozen references only.
    let (_, feed) = call(&f.s, &f.member, "GET", FEED, Value::Null).await;
    let item = feed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["source"] == "access_request")
        .unwrap()
        .clone();
    assert_eq!(item["model_name_at_request"], "company/notify");
    assert_eq!(item["target_kind"], "model_selection");
    assert!(item.get("reason").is_none() && item.get("requester").is_none());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn lost_authority_hides_request_notifications_at_once(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = eligible_model(&pool, "company/hidden").await;
    let approver = user(&pool, "notify-admin", "user").await;
    member(&pool, f.team, approver.user_id, "admin").await;
    let (_, req) = call(
        &f.s,
        &f.member,
        "POST",
        "/api/v1/me/access-requests",
        json!({"workspace_id":f.team,"model_id":m,"idempotency_key":"hide"}),
    )
    .await;
    assert_eq!(request_items(&f, &approver, "").await.len(), 1);
    assert_eq!(unread(&f, &approver).await, 1);
    sqlx::query(
        "UPDATE workspace_membership_grants SET role='member' WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(f.team)
    .bind(approver.user_id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(request_items(&f, &approver, "").await.is_empty());
    assert_eq!(unread(&f, &approver).await, 0);
    // An invisible request cannot be marked read either.
    let (_, marked) = call(
        &f.s,
        &approver,
        "POST",
        &format!("{FEED}/read"),
        json!({"ids":[rid(&req)]}),
    )
    .await;
    assert_eq!(marked["marked"], 0);
    // Expired pending requests stop notifying approvers.
    sqlx::raw_sql("ALTER TABLE access_requests DISABLE TRIGGER access_requests_guard")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE access_requests SET created_at=clock_timestamp()-interval '8 days',expires_at=clock_timestamp()-interval '1 day'").execute(&pool).await.unwrap();
    sqlx::raw_sql("ALTER TABLE access_requests ENABLE TRIGGER access_requests_guard")
        .execute(&pool)
        .await
        .unwrap();
    assert!(request_items(&f, &f.owner, "").await.is_empty());
    // Nothing was ever read by the demoted approver.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM access_request_notification_reads")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn request_items_merge_with_incidents_and_keep_their_windows(pool: PgPool) {
    let f = fixture(&pool).await;
    // A workspace incident the owner sees (a workspace budget rule), fired
    // between two requests.
    let (status, r) = call(&f.s, &f.owner, "POST", &format!("/api/v1/workspaces/{}/alerts/rules", f.team), json!({"name":"Mixed","kind":"budget_threshold","budget_layers":["local"],"thresholds":[100],"notify_workspace_admins":true})).await;
    assert_eq!(status, StatusCode::OK, "{r}");
    let rule = Uuid::parse_str(r["id"].as_str().unwrap()).unwrap();
    let m1 = eligible_model(&pool, "company/first").await;
    let m2 = eligible_model(&pool, "company/second").await;
    let (_, first) = call(
        &f.s,
        &f.member,
        "POST",
        "/api/v1/me/access-requests",
        json!({"workspace_id":f.team,"model_id":m1,"idempotency_key":"mix-1"}),
    )
    .await;
    let event = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary,details) VALUES($1,$2,'budget_threshold','mix',100,'critical',$3,'Mixed incident',jsonb_build_object('layer','local','period','month','threshold_percent',100,'used_percent','100','used_microusd','100','budget_microusd','100','unknown_cost_requests',0,'window_start','2026-10-01T00:00:00Z','window_end','2026-11-01T00:00:00Z'))")
        .bind(event).bind(rule).bind(f.team).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')",
    )
    .bind(event)
    .execute(&pool)
    .await
    .unwrap();
    let (_, second) = call(
        &f.s,
        &f.member,
        "POST",
        "/api/v1/me/access-requests",
        json!({"workspace_id":f.team,"model_id":m2,"idempotency_key":"mix-2"}),
    )
    .await;
    // Newest first across both sources, paged with has_more.
    let ids = |v: &Value| {
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let (_, all) = call(&f.s, &f.owner, "GET", FEED, Value::Null).await;
    assert_eq!(
        ids(&all),
        vec![rid(&second), event.to_string(), rid(&first)]
    );
    assert_eq!(all["data"][1]["summary"], "Mixed incident");
    let (_, page1) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("{FEED}?limit=2"),
        Value::Null,
    )
    .await;
    assert_eq!(
        (ids(&page1), page1["has_more"].clone()),
        (vec![rid(&second), event.to_string()], json!(true))
    );
    let (_, page2) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("{FEED}?limit=2&offset=2"),
        Value::Null,
    )
    .await;
    assert_eq!(
        (ids(&page2), page2["has_more"].clone()),
        (vec![rid(&first)], json!(false))
    );
    // Marking everything read covers both; the incident's read state is its own.
    assert_eq!(unread(&f, &f.owner).await, 3);
    let (_, marked) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("{FEED}/read"),
        json!({"all":true}),
    )
    .await;
    assert_eq!(marked["marked"], 3);
    let (_, unread_page) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("{FEED}?status=unread"),
        Value::Null,
    )
    .await;
    assert!(unread_page["data"].as_array().unwrap().is_empty());
    // A Platform Admin who also owns the workspace sees one row per request.
    member(&pool, f.team, f.admin.user_id, "owner").await;
    sqlx::query("UPDATE workspaces SET discoverable=true WHERE id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    let (_, join) = call(
        &f.s,
        &f.outsider,
        "POST",
        "/api/v1/me/access-requests",
        json!({"workspace_id":f.team,"idempotency_key":"mix-join"}),
    )
    .await;
    let admin_items = request_items(&f, &f.admin, "").await;
    assert_eq!(admin_items.iter().filter(|i| i.0 == rid(&join)).count(), 1);
    assert_eq!(admin_items.len(), 3);
    // Withdrawn requests never notify anyone; the requester's expired and
    // older-than-30-days decisions follow the window.
    let (status, _) = call(
        &f.s,
        &f.outsider,
        "POST",
        &format!("/api/v1/me/access-requests/{}/withdraw", rid(&join)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(request_items(&f, &f.outsider, "").await.is_empty());
    assert!(
        !request_items(&f, &f.admin, "")
            .await
            .iter()
            .any(|i| i.0 == rid(&join))
    );
    sqlx::raw_sql("ALTER TABLE access_requests DISABLE TRIGGER access_requests_guard")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE access_requests SET created_at=clock_timestamp()-interval '8 days',expires_at=clock_timestamp()-interval '1 day' WHERE id=$1::uuid").bind(rid(&first)).execute(&pool).await.unwrap();
    sqlx::raw_sql("ALTER TABLE access_requests ENABLE TRIGGER access_requests_guard")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        crate::lifecycle::expire_access_requests_fenced(&f.s, None)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        request_items(&f, &f.member, "").await,
        vec![(rid(&first), "requester".into(), "expired".into(), false)]
    );
    sqlx::raw_sql("ALTER TABLE access_requests DISABLE TRIGGER access_requests_guard")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE access_requests SET decided_at=clock_timestamp()-interval '31 days',created_at=clock_timestamp()-interval '40 days',expires_at=clock_timestamp()-interval '33 days' WHERE id=$1::uuid").bind(rid(&first)).execute(&pool).await.unwrap();
    sqlx::raw_sql("ALTER TABLE access_requests ENABLE TRIGGER access_requests_guard")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        request_items(&f, &f.member, "").await.is_empty(),
        "older than 30 days"
    );
    // A disabled workspace's pending requests stop notifying its admins.
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    assert!(request_items(&f, &f.owner, "").await.is_empty());
}

//! #90 slice 4: access review reminders in the in-app notification feed, on
//! real PostgreSQL. One notification per open campaign in which the viewer
//! has items they may decide that nobody has decided yet, derived live on
//! every read; its state is open, due_soon (within 24 hours of the due date)
//! or overdue; read marks are per campaign and state. Never emailed.
use super::*;

const FEED: &str = "/api/v1/me/notifications";
const REVIEWS: &str = "/api/v1/platform/access-reviews";

async fn start(f: &Fixture, days: i64) -> Value {
    let (status, c) = call(&f.s, &f.admin, "POST", REVIEWS, json!({"name":"Q4 review","subjects":["workspace_memberships","platform_roles"],"due_in_days":days})).await;
    assert_eq!(status, StatusCode::OK, "{c}");
    c
}
/// The viewer's access-review notifications: (campaign id, state, read).
async fn reminders(
    f: &Fixture,
    who: &BrowserPrincipal,
    query: &str,
) -> Vec<(String, String, bool)> {
    let (status, feed) = call(&f.s, who, "GET", &format!("{FEED}{query}"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{feed}");
    feed["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["source"] == "access_review")
        .map(|n| {
            (
                n["id"].as_str().unwrap().to_owned(),
                n["state"].as_str().unwrap().to_owned(),
                n["read"].as_bool().unwrap(),
            )
        })
        .collect()
}
async fn unread(f: &Fixture, who: &BrowserPrincipal) -> i64 {
    let (status, s) = call(&f.s, who, "GET", &format!("{FEED}/summary"), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    s["unread"].as_i64().unwrap()
}
async fn item(pool: &PgPool, campaign: &Value, user: Uuid, ws: Option<Uuid>) -> Uuid {
    sqlx::query_scalar("SELECT id FROM access_review_items WHERE campaign_id=$1 AND user_id=$2 AND workspace_id IS NOT DISTINCT FROM $3")
        .bind(id(campaign)).bind(user).bind(ws).fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn reviewers_are_reminded_until_they_have_decided(pool: PgPool) {
    let f = fixture(&pool).await;
    let c = start(&f, 14).await;
    let cid = id(&c).to_string();
    // The Team owner reviews the Team member; the Platform Admin the Auditor.
    assert_eq!(
        reminders(&f, &f.owner, "").await,
        [(cid.clone(), "open".to_owned(), false)]
    );
    assert_eq!(
        reminders(&f, &f.admin, "").await,
        [(cid.clone(), "open".to_owned(), false)]
    );
    // Nothing for people with nothing to decide.
    for who in [&f.member, &f.auditor, &f.outsider] {
        assert!(reminders(&f, who, "").await.is_empty(), "nothing to decide");
    }
    // What a reminder shows: the campaign's name and due date, never its items.
    let (_, feed) = call(&f.s, &f.owner, "GET", FEED, Value::Null).await;
    let n = feed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["source"] == "access_review")
        .unwrap()
        .clone();
    let keys: std::collections::BTreeSet<&str> =
        n.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "at",
            "campaign_name",
            "due_at",
            "id",
            "read",
            "source",
            "state"
        ]
        .into()
    );
    assert_eq!(n["campaign_name"], "Q4 review");
    // It counts as unread, and is marked read by id.
    let before = unread(&f, &f.owner).await;
    let (status, marked) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("{FEED}/read"),
        json!({"ids":[cid]}),
    )
    .await;
    assert_eq!(
        (status, marked["marked"].as_i64()),
        (StatusCode::OK, Some(1))
    );
    assert_eq!(unread(&f, &f.owner).await, before - 1);
    assert_eq!(
        reminders(&f, &f.owner, "").await,
        [(cid.clone(), "open".to_owned(), true)]
    );
    assert!(reminders(&f, &f.owner, "?status=unread").await.is_empty());
    // Once every item they may decide has a decision, the reminder goes.
    let member_item = item(&pool, &c, f.member.user_id, Some(f.team)).await;
    let (status, _) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/me/access-reviews/{member_item}/decision"),
        json!({"decision":"keep"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(reminders(&f, &f.owner, "").await.is_empty());
    // An item assigned away no longer reminds its automatic reviewer.
    let auditor_item = item(&pool, &c, f.auditor.user_id, None).await;
    let second = user(&pool, "second-admin", "admin").await;
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/items/{auditor_item}/reviewer", id(&c)),
        json!({"user_id":second.user_id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(reminders(&f, &f.admin, "").await.is_empty());
    assert_eq!(reminders(&f, &second, "").await.len(), 1);
    // A closed campaign reminds nobody.
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/close", id(&c)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(reminders(&f, &second, "").await.is_empty());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_reminder_is_unread_again_when_the_due_date_approaches_and_passes(pool: PgPool) {
    let f = fixture(&pool).await;
    // Due in one day: already within 24 hours.
    let c = start(&f, 1).await;
    let cid = id(&c).to_string();
    assert_eq!(
        reminders(&f, &f.owner, "").await,
        [(cid.clone(), "due_soon".to_owned(), false)]
    );
    let (status, _) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("{FEED}/read"),
        json!({"all":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        reminders(&f, &f.owner, "").await,
        [(cid.clone(), "due_soon".to_owned(), true)]
    );
    // Past due (the clock moved): overdue, unread again.
    sqlx::query(
        "ALTER TABLE access_review_campaigns DISABLE TRIGGER access_review_campaigns_guard",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE access_review_campaigns SET created_at=created_at-interval '2 days', due_at=due_at-interval '2 days' WHERE id=$1").bind(id(&c)).execute(&pool).await.unwrap();
    sqlx::query("ALTER TABLE access_review_campaigns ENABLE TRIGGER access_review_campaigns_guard")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        reminders(&f, &f.owner, "").await,
        [(cid.clone(), "overdue".to_owned(), false)]
    );
    // Read marks are the viewer's own: another reviewer's stay unread.
    assert_eq!(
        reminders(&f, &f.admin, "").await,
        [(cid, "overdue".to_owned(), false)]
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn read_marks_are_insert_only_and_bound_to_the_reader(pool: PgPool) {
    let f = fixture(&pool).await;
    let c = start(&f, 14).await;
    let (status, _) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("{FEED}/read"),
        json!({"all":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let marks: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT user_id,state FROM access_review_notification_reads WHERE campaign_id=$1",
    )
    .bind(id(&c))
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(marks, vec![(f.owner.user_id, "open".to_owned())]);
    // Marking someone else's ids marks nothing for them.
    let (status, marked) = call(
        &f.s,
        &f.member,
        "POST",
        &format!("{FEED}/read"),
        json!({"ids":[id(&c)]}),
    )
    .await;
    assert_eq!(
        (status, marked["marked"].as_i64()),
        (StatusCode::OK, Some(0))
    );
    for sql in [
        "UPDATE access_review_notification_reads SET state='due_soon'",
        "DELETE FROM access_review_notification_reads",
    ] {
        let r = sqlx::query(sql).execute(&pool).await;
        let code = r.err().and_then(|e| {
            e.as_database_error()
                .and_then(|d| d.code())
                .map(|c| c.into_owned())
        });
        assert_eq!(code.as_deref(), Some("P0001"), "immutable");
    }
}

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

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn reminders_follow_the_reviewer_rule_and_the_items_state(pool: PgPool) {
    let f = fixture(&pool).await;
    // A second Team member and a Team admin.
    let second = user(&pool, "second-member", "user").await;
    member(&pool, f.team, second.user_id, "member").await;
    member(&pool, f.team, f.outsider.user_id, "admin").await;
    let c = start(&f, 14).await;
    let other = start(&f, 30).await;
    // Two open campaigns: two reminders, counted in the summary.
    assert_eq!(reminders(&f, &f.owner, "").await.len(), 2);
    assert_eq!(unread(&f, &f.owner).await, 2);
    // Ids that are not this viewer's reminders (a request's, an incident's,
    // anything) mark none of them.
    let (_, marked) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("{FEED}/read"),
        json!({"ids":[Uuid::new_v4()]}),
    )
    .await;
    assert_eq!(marked["marked"], 0);
    // The "at" of an open reminder is the campaign's start.
    let (_, feed) = call(&f.s, &f.owner, "GET", FEED, Value::Null).await;
    let n = feed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == id(&c).to_string())
        .unwrap()
        .clone();
    assert_eq!(n["at"], c["created_at"]);
    // Deciding one of two items keeps the reminder; the second clears it.
    let (status, _) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!(
            "/api/v1/me/access-reviews/{}/decision",
            item(&pool, &c, f.member.user_id, Some(f.team)).await
        ),
        json!({"decision":"keep"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mine: Vec<String> = reminders(&f, &f.owner, "")
        .await
        .into_iter()
        .map(|r| r.0)
        .collect();
    assert!(mine.contains(&id(&c).to_string()));
    // The Team admin's own membership is the owner's to review too.
    let (status, _) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!(
            "/api/v1/me/access-reviews/{}/decision",
            item(&pool, &c, f.outsider.user_id, Some(f.team)).await
        ),
        json!({"decision":"keep"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // An item no longer in effect is nothing to decide: the second member leaves the Team.
    revoke_membership(&pool, f.team, second.user_id).await;
    let mine: Vec<String> = reminders(&f, &f.owner, "")
        .await
        .into_iter()
        .map(|r| r.0)
        .collect();
    assert!(
        !mine.contains(&id(&c).to_string()),
        "only an inactive item was left"
    );
    // An assignee who no longer qualifies: the automatic reviewers are reminded again.
    let target = item(&pool, &other, f.member.user_id, Some(f.team)).await;
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/items/{target}/reviewer", id(&other)),
        json!({"user_id":f.outsider.user_id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!(
            "/api/v1/me/access-reviews/{}/decision",
            item(&pool, &other, f.outsider.user_id, Some(f.team)).await
        ),
        json!({"decision":"keep"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let owner_other =
        |v: Vec<(String, String, bool)>| v.into_iter().any(|r| r.0 == id(&other).to_string());
    assert!(
        !owner_other(reminders(&f, &f.owner, "").await),
        "the assignee reviews it"
    );
    assert!(owner_other(reminders(&f, &f.outsider, "").await));
    revoke_membership(&pool, f.team, f.outsider.user_id).await;
    assert!(
        owner_other(reminders(&f, &f.owner, "").await),
        "back to the owner"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn an_overdue_reminder_is_dated_by_its_due_date(pool: PgPool) {
    let f = fixture(&pool).await;
    let c = start(&f, 1).await;
    sqlx::query(
        "ALTER TABLE access_review_campaigns DISABLE TRIGGER access_review_campaigns_guard",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE access_review_campaigns SET created_at=created_at-interval '3 days', due_at=due_at-interval '3 days' WHERE id=$1").bind(id(&c)).execute(&pool).await.unwrap();
    sqlx::query("ALTER TABLE access_review_campaigns ENABLE TRIGGER access_review_campaigns_guard")
        .execute(&pool)
        .await
        .unwrap();
    let due: String = sqlx::query_scalar(
        "SELECT to_jsonb(due_at)#>>'{}' FROM access_review_campaigns WHERE id=$1",
    )
    .bind(id(&c))
    .fetch_one(&pool)
    .await
    .unwrap();
    let (_, feed) = call(&f.s, &f.owner, "GET", FEED, Value::Null).await;
    let n = feed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["source"] == "access_review")
        .unwrap()
        .clone();
    assert_eq!(
        (n["state"].as_str(), n["at"].as_str()),
        (Some("overdue"), Some(due.as_str()))
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn reminders_stay_cheap_for_everyone_at_twenty_campaigns_of_five_thousand(pool: PgPool) {
    let f = fixture(&pool).await;
    sqlx::query("WITH u AS (INSERT INTO users(id,email) SELECT gen_random_uuid(),'bulk'||n||'@example.test' FROM generate_series(1,4997) n RETURNING id) INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) SELECT $1,id,'member','manual' FROM u")
        .bind(f.team).execute(&pool).await.unwrap();
    for n in 0..20 {
        let (status, c) = call(&f.s, &f.admin, "POST", REVIEWS, json!({"name":format!("Load {n}"),"subjects":["workspace_memberships"],"workspace_ids":[f.team]})).await;
        assert_eq!(status, StatusCode::OK, "{c}");
    }
    sqlx::query("ANALYZE").execute(&pool).await.unwrap();
    // Someone who reviews nothing never scans the campaigns' items.
    let sql = format!(
        "EXPLAIN (FORMAT JSON) SELECT count(*) FROM ({}) n",
        crate::management::access_reviews::reminder_items("$1", "$2")
    );
    let plan: Value = sqlx::query_scalar(&sql)
        .bind(f.member.user_id)
        .bind(false)
        .fetch_one(&pool)
        .await
        .unwrap();
    fn scans(node: &Value, relation: &str) -> bool {
        (node["Node Type"] == "Seq Scan" && node["Relation Name"] == relation)
            || node["Plans"]
                .as_array()
                .is_some_and(|p| p.iter().any(|n| scans(n, relation)))
            || node["Plan"].is_object() && scans(&node["Plan"], relation)
            || node
                .as_array()
                .is_some_and(|p| p.iter().any(|n| scans(n, relation)))
    }
    assert!(
        !scans(&plan, "access_review_items"),
        "a member's poll scans every item"
    );
    let timed = async |who: &BrowserPrincipal| {
        let started = std::time::Instant::now();
        let (a, _) = call(&f.s, who, "GET", &format!("{FEED}/summary"), Value::Null).await;
        let (b, _) = call(&f.s, who, "GET", FEED, Value::Null).await;
        (a, b, started.elapsed() < std::time::Duration::from_secs(3))
    };
    assert_eq!(
        timed(&f.member).await,
        (StatusCode::OK, StatusCode::OK, true),
        "a member"
    );
    assert_eq!(
        timed(&f.admin).await,
        (StatusCode::OK, StatusCode::OK, true),
        "a Platform Admin"
    );
    assert_eq!(
        timed(&f.owner).await,
        (StatusCode::OK, StatusCode::OK, true),
        "the owner of every item"
    );
    assert_eq!(reminders(&f, &f.owner, "?limit=50").await.len(), 20);
}

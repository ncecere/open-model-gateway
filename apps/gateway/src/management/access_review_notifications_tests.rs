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
    // Never an alert delivery.
    let deliveries: i64 = sqlx::query_scalar("SELECT count(*) FROM alert_deliveries")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(deliveries, 0);
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

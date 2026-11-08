//! #90 slice 5b: a review's CSV export and each item's `reviewable` flag
//! (#469), on real PostgreSQL. Platform Admins and Auditors export; cells
//! that a spreadsheet would run as a formula are neutralized; audited.
use super::*;

const REVIEWS: &str = "/api/v1/platform/access-reviews";

async fn start(f: &Fixture) -> Value {
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
/// The raw export response: (status, headers, body).
async fn export(
    f: &Fixture,
    who: &BrowserPrincipal,
    campaign: Uuid,
) -> (StatusCode, axum::http::HeaderMap, String) {
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
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_readers_export_every_item_with_its_decision(pool: PgPool) {
    let f = fixture(&pool).await;
    // A display name a spreadsheet would run as a formula.
    sqlx::query("UPDATE users SET display_name='=HYPERLINK(\"http://x\")' WHERE id=$1")
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let c = start(&f).await;
    let member_item: Uuid = sqlx::query_scalar("SELECT id FROM access_review_items WHERE campaign_id=$1 AND user_id=$2 AND workspace_id=$3")
        .bind(id(&c)).bind(f.member.user_id).bind(f.team).fetch_one(&pool).await.unwrap();
    let (status, _) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/me/access-reviews/{member_item}/decision"),
        json!({"decision":"keep"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for who in [&f.admin, &f.auditor] {
        let (status, headers, body) = export(&f, who, id(&c)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(headers["content-type"], "text/csv; charset=utf-8");
        assert!(
            headers["content-disposition"]
                .to_str()
                .unwrap()
                .starts_with("attachment; filename=\"access-review-")
        );
        assert_eq!(headers["cache-control"], "no-store");
        let lines: Vec<&str> = body.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines[0],
            "item_id,subject_kind,workspace_id,workspace_name,user_id,email,display_name,role,source,granted_at,active,reviewable,decision,decided_by,decided_by_email,decided_at,reviewer_user_id,reviewer_email"
        );
        assert_eq!(lines.len() - 1, c["item_count"].as_u64().unwrap() as usize);
        let member = lines
            .iter()
            .find(|l| l.starts_with(&member_item.to_string()))
            .unwrap();
        assert!(member.contains(",keep,"), "{member}");
        assert!(member.contains("owner@example.test"), "the decider's email");
        assert!(
            member.contains("\"'=HYPERLINK(\"\"http://x\"\")\""),
            "formula neutralized: {member}"
        );
    }
    let audited: Vec<Value> = sqlx::query_scalar("SELECT metadata FROM audit_events WHERE action='access_review.exported' AND resource_id=$1 ORDER BY created_at")
        .bind(id(&c)).fetch_all(&pool).await.unwrap();
    assert_eq!(
        audited,
        vec![
            json!({"count": c["item_count"]}),
            json!({"count": c["item_count"]})
        ]
    );
    for who in [&f.owner, &f.member, &f.outsider] {
        assert_eq!(export(&f, who, id(&c)).await.0, StatusCode::FORBIDDEN);
    }
    assert_eq!(
        export(&f, &f.admin, Uuid::new_v4()).await.0,
        StatusCode::NOT_FOUND
    );
    let token =
        crate::route_policy::token_policy("GET", "/api/v1/platform/access-reviews/{id}/export")
            .expect("declared");
    assert!(matches!(token, crate::route_policy::TokenPolicy::Denied));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn each_item_says_whether_anyone_may_review_it(pool: PgPool) {
    let f = fixture(&pool).await;
    let c = start(&f).await;
    let (status, page) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{REVIEWS}/{}/items?limit=200", id(&c)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let reviewable = |user: Uuid, ws: Option<Uuid>| {
        page["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["user_id"] == user.to_string() && i["workspace_id"] == json!(ws))
            .unwrap()["reviewable"]
            .as_bool()
            .unwrap()
    };
    // The Team owner reviews the member; nobody reviews the sole owner's own
    // memberships or the only Platform Admin's role; another admin reviews the Auditor.
    assert!(reviewable(f.member.user_id, Some(f.team)));
    assert!(!reviewable(f.owner.user_id, Some(f.team)));
    assert!(!reviewable(f.owner.user_id, Some(f.project)));
    assert!(!reviewable(f.admin.user_id, None));
    assert!(reviewable(f.auditor.user_id, None));
    // The count agrees with the items.
    let (_, detail) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{REVIEWS}/{}", id(&c)),
        Value::Null,
    )
    .await;
    let unreviewable = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["active"] == true && i["reviewable"] == false)
        .count();
    assert_eq!(detail["counts"]["without_reviewer"], json!(unreviewable));
    // An assignment makes the owner's item reviewable.
    let owner_item = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["user_id"] == f.owner.user_id.to_string() && i["workspace_id"] == json!(f.team))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, a) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/items/{owner_item}/reviewer", id(&c)),
        json!({"user_id":f.admin.user_id}),
    )
    .await;
    assert_eq!(
        (status, a["reviewable"].as_bool()),
        (StatusCode::OK, Some(true))
    );
}

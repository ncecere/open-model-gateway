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
        assert_eq!(
            headers["content-disposition"],
            "attachment; filename=\"access-review.csv\""
        );
        assert_eq!(headers["cache-control"], "no-store");
        let lines: Vec<&str> = body.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines[0],
            "item_id,subject_kind,workspace_id,workspace_name,user_id,email,display_name,role,source,granted_at,active,reviewable,decision,decided_by,decided_by_email,decided_at,reviewer_user_id,reviewer_email,api_key_id,holder_kind,expires_at,last_used_at"
        );
        assert_eq!(lines.len() - 1, c["item_count"].as_u64().unwrap() as usize);
        let member = lines
            .iter()
            .find(|l| l.starts_with(&format!("\"{member_item}\"")))
            .unwrap();
        assert!(member.contains(",\"keep\","), "{member}");
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
async fn reviewable_agrees_with_who_may_decide(pool: PgPool) {
    let f = fixture(&pool).await;
    let second = user(&pool, "second-member", "user").await;
    member(&pool, f.team, second.user_id, "member").await;
    member(&pool, f.team, f.outsider.user_id, "admin").await;
    let open = start(&f).await;
    let closed = start(&f).await;
    let item_of = async |c: &Value, user: Uuid, ws: Option<Uuid>| -> String {
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM access_review_items WHERE campaign_id=$1 AND user_id=$2 AND workspace_id IS NOT DISTINCT FROM $3")
            .bind(id(c)).bind(user).bind(ws).fetch_one(&pool).await.unwrap().to_string()
    };
    // An assignee who still qualifies (a Platform Admin), and one who stops
    // qualifying (the Team admin, who then loses the role).
    let owner_item = item_of(&open, f.owner.user_id, Some(f.team)).await;
    let member_item = item_of(&open, f.member.user_id, Some(f.team)).await;
    for (item, reviewer) in [
        (&owner_item, f.admin.user_id),
        (&member_item, f.outsider.user_id),
    ] {
        let (status, _) = call(
            &f.s,
            &f.admin,
            "POST",
            &format!("{REVIEWS}/{}/items/{item}/reviewer", id(&open)),
            json!({"user_id":reviewer}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    revoke_membership(&pool, f.team, f.outsider.user_id).await;
    // An item no longer in effect.
    revoke_membership(&pool, f.team, second.user_id).await;
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/close", id(&closed)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut compared = 0;
    for c in [&open, &closed] {
        let (_, page) = call(
            &f.s,
            &f.auditor,
            "GET",
            &format!("{REVIEWS}/{}/items?limit=200", id(c)),
            Value::Null,
        )
        .await;
        for item in page["data"].as_array().unwrap() {
            let truth: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM users r WHERE access_review_may_decide($1, r.id))",
            )
            .bind(Uuid::parse_str(item["id"].as_str().unwrap()).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                item["reviewable"].as_bool().unwrap(),
                item["active"] == true && truth,
                "reviewable disagrees with who may decide"
            );
            compared += 1;
        }
    }
    assert!(compared >= 10);
    // The inbox only holds items its viewer may decide: all reviewable.
    let (_, inbox) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/me/access-reviews?limit=200",
        Value::Null,
    )
    .await;
    assert!(!inbox["data"].as_array().unwrap().is_empty());
    assert!(
        inbox["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["reviewable"] == true)
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn export_cells_are_safe_and_complete(pool: PgPool) {
    let f = fixture(&pool).await;
    // A workspace name and an email a spreadsheet would run, a name with a quote and a line break.
    sqlx::query("UPDATE workspaces SET name='+SUM(A1)' WHERE id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE users SET email='@evil@example.test', display_name='Quote \"x\" here' WHERE id=$1",
    )
    .bind(f.member.user_id)
    .execute(&pool)
    .await
    .unwrap();
    let c = start(&f).await;
    let auditor_item: Uuid = sqlx::query_scalar(
        "SELECT id FROM access_review_items WHERE campaign_id=$1 AND user_id=$2",
    )
    .bind(id(&c))
    .bind(f.auditor.user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
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
    // A person whose data is removed after the snapshot: empty cells, never stale values.
    sqlx::query("UPDATE users SET email=NULL, display_name=NULL WHERE id=$1")
        .bind(f.outsider.user_id)
        .execute(&pool)
        .await
        .ok();
    let (status, headers, body) = export(&f, &f.auditor, id(&c)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert!(body.contains("\"'+SUM(A1)\""), "workspace name neutralized");
    assert!(
        body.contains("\"'@evil@example.test\""),
        "email neutralized"
    );
    assert!(
        body.contains("\"Quote \"\"x\"\" here\""),
        "quotes doubled inside the cell"
    );
    let assigned = body
        .split("\r\n")
        .find(|l| l.starts_with(&format!("\"{auditor_item}\"")))
        .unwrap();
    assert!(
        assigned.ends_with(&format!(
            "\"{}\",\"second-admin@example.test\",\"\",\"\",\"\",\"\"",
            second.user_id
        )),
        "the assigned reviewer, then empty key columns"
    );
    // A closed review exports too; HEAD exports and records nothing.
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/close", id(&c)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(export(&f, &f.auditor, id(&c)).await.0, StatusCode::OK);
    let head = routes()
        .layer(Extension(ManagementPrincipal::Browser(f.auditor.clone())))
        .layer(Extension(f.auditor.clone()))
        .with_state(f.s.clone())
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(format!("{REVIEWS}/{}/export", id(&c)))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::METHOD_NOT_ALLOWED);
    let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='access_review.exported' AND resource_id=$1").bind(id(&c)).fetch_one(&pool).await.unwrap();
    assert_eq!(audited, 2);
}

use super::*;

async fn event(pool: &PgPool, actor: Uuid, ws: Option<Uuid>, action: &str) {
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,workspace_id,action,resource_type,metadata) VALUES($1,$2,$3,$4,'test','{}')")
        .bind(Uuid::new_v4())
        .bind(actor)
        .bind(ws)
        .bind(action)
        .execute(pool)
        .await
        .unwrap();
}
fn actions(body: &Value) -> Vec<String> {
    let mut out: Vec<String> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap().to_owned())
        .collect();
    out.sort();
    out
}

/// A user's activity is the platform audit filtered by actor: the same
/// Admin/Auditor authority and the same exclusion of every personal-workspace
/// event, whoever the actor or owner is.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_audit_filters_by_actor_and_never_reveals_personal_events(pool: PgPool) {
    let f = fixture(&pool).await;
    let admin_personal = workspace(&pool, "Admin personal", "personal", f.admin.user_id).await;
    let owner = f.owner.user_id;
    event(&pool, owner, Some(f.team), "workspace.updated").await;
    event(&pool, owner, None, "identity.groups_synchronized").await;
    event(&pool, owner, None, "identity.rebound").await;
    event(&pool, owner, None, "mapping.created").await;
    event(&pool, owner, Some(f.personal), "key.created").await;
    event(&pool, owner, Some(admin_personal), "key.revoked").await;
    event(&pool, f.admin.user_id, Some(f.team), "workspace.created").await;
    event(&pool, f.admin.user_id, Some(admin_personal), "key.rotated").await;
    let base = format!("/api/v1/platform/audit?actor_user_id={owner}");
    for actor in [&f.admin, &f.auditor] {
        let (status, body) = call(&f.s, actor, "GET", &base, json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            actions(&body),
            [
                "identity.groups_synchronized",
                "identity.rebound",
                "mapping.created",
                "workspace.updated"
            ]
        );
        assert_eq!(body["has_more"], false);
        for e in body["data"].as_array().unwrap() {
            assert_eq!(e["actor_user_id"], owner.to_string());
            assert_ne!(e["workspace_id"], f.personal.to_string());
            assert_ne!(e["workspace_id"], admin_personal.to_string());
        }
        let (_, hidden) = call(
            &f.s,
            actor,
            "GET",
            &format!("{base}&hide_sign_ins=true"),
            json!({}),
        )
        .await;
        assert_eq!(actions(&hidden), ["mapping.created", "workspace.updated"]);
        let (_, shown) = call(
            &f.s,
            actor,
            "GET",
            &format!("{base}&hide_sign_ins=false&exclude_actions=mapping.created,identity.rebound"),
            json!({}),
        )
        .await;
        assert_eq!(
            actions(&shown),
            ["identity.groups_synchronized", "workspace.updated"]
        );
    }
    // The admin's own personal-workspace events stay out of their own activity too.
    let (_, own) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("/api/v1/platform/audit?actor_user_id={}", f.admin.user_id),
        json!({}),
    )
    .await;
    assert_eq!(actions(&own), ["workspace.created"]);
    let (_, page) = call(&f.s, &f.admin, "GET", &format!("{base}&limit=1"), json!({})).await;
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(page["has_more"], true);
    let (_, last) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("{base}&limit=1&offset=3"),
        json!({}),
    )
    .await;
    assert_eq!(last["has_more"], false);
    // A filter never grants read access.
    assert_eq!(
        call(&f.s, &f.member, "GET", &base, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f.s, &f.owner, "GET", &base, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_audit_filters_are_strictly_validated(pool: PgPool) {
    let f = fixture(&pool).await;
    let many = vec!["test.code"; 21].join(",");
    for query in [
        "actor_user_id=not-a-uuid".to_owned(),
        "actor_user_id=".to_owned(),
        "hide_sign_ins=yes".to_owned(),
        "hide_sign_ins=1".to_owned(),
        "exclude_actions=".to_owned(),
        "exclude_actions=key.created,,key.revoked".to_owned(),
        "exclude_actions=Key.Created".to_owned(),
        "exclude_actions=key%20created".to_owned(),
        format!("exclude_actions={}", "a".repeat(65)),
        format!("exclude_actions={many}"),
        "actor=me".to_owned(),
        format!("actor_user_id={0}&actor_user_id={0}", f.owner.user_id),
        "limit=0".to_owned(),
    ] {
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "GET",
                &format!("/api/v1/platform/audit?{query}"),
                json!({})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST,
            "{query}"
        );
    }
    let ok = format!(
        "/api/v1/platform/audit?actor_user_id={}&hide_sign_ins=true&exclude_actions={}",
        Uuid::new_v4(),
        vec!["test.code"; 20].join(",")
    );
    let (status, body) = call(&f.s, &f.auditor, "GET", &ok, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!([]));
}

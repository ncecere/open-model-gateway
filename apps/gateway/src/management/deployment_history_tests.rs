//! #86: run against real PostgreSQL, never mocked history/revisions.
use super::*;

fn history(id: Uuid) -> String {
    format!("/api/v1/platform/deployments/{id}/configuration/history")
}

async fn create(f: &Fixture, pool: &PgPool) -> Uuid {
    let m = model(pool, "history-model").await;
    let p = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'History','openai','env:UNUSED')").bind(p).execute(pool).await.unwrap();
    let (status, body) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/deployments",
        json!({"model_id":m,"provider_connection_id":p,"upstream_model":"history","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deployment_history_writers_revisions_and_immutable_snapshots(pool: PgPool) {
    let f = fixture(&pool).await;
    let d = create(&f, &pool).await;
    for (method, enabled) in [("PATCH", false), ("PATCH", true), ("DELETE", false)] {
        let (status, body) = call(
            &f.s,
            &f.admin,
            method,
            &format!("/api/v1/platform/deployments/{d}"),
            json!({"enabled":enabled}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    // A no-op must not invent a state transition or reset the revision.
    call(
        &f.s,
        &f.admin,
        "PATCH",
        &format!("/api/v1/platform/deployments/{d}"),
        json!({"enabled":false}),
    )
    .await;
    let rows: Vec<(i64, Option<bool>, bool, Option<Uuid>)> = sqlx::query_as("SELECT c.revision,c.before_enabled,c.after_enabled,a.actor_user_id FROM configuration_changes c JOIN audit_events a ON (a.id,a.created_at)=(c.audit_event_id,c.audit_created_at) WHERE c.deployment_id=$1 ORDER BY c.revision").bind(d).fetch_all(&pool).await.unwrap();
    assert_eq!(
        rows,
        vec![
            (1, None, true, Some(f.admin.user_id)),
            (2, Some(true), false, Some(f.admin.user_id)),
            (3, Some(false), true, Some(f.admin.user_id)),
            (4, Some(true), false, Some(f.admin.user_id))
        ]
    );
    // The database catches every writer, including SQL without handler bookkeeping.
    sqlx::query("UPDATE deployments SET enabled=true WHERE id=$1")
        .bind(d)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT revision FROM deployments WHERE id=$1")
            .bind(d)
            .fetch_one(&pool)
            .await
            .unwrap(),
        5
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM configuration_changes WHERE deployment_id=$1"
        )
        .bind(d)
        .fetch_one(&pool)
        .await
        .unwrap(),
        5
    );
    let actor: Option<Uuid> = sqlx::query_scalar("SELECT a.actor_user_id FROM configuration_changes c JOIN audit_events a ON (a.id,a.created_at)=(c.audit_event_id,c.audit_created_at) WHERE c.deployment_id=$1 AND c.revision=5").bind(d).fetch_one(&pool).await.unwrap();
    assert!(
        actor.is_none(),
        "transaction-local actor must not leak into SQL writers"
    );
    let (_, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("/api/v1/platform/deployments/{d}/configuration/preview"),
        json!({"enabled":false}),
    )
    .await;
    assert_eq!(preview["expected_revision"], "5");
    for sql in [
        "UPDATE configuration_changes SET after_enabled=false",
        "DELETE FROM configuration_changes",
        "TRUNCATE configuration_changes",
        "UPDATE deployments SET revision=0",
    ] {
        assert!(
            sqlx::query(sql).execute(&pool).await.is_err(),
            "accepted {sql}"
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deployment_history_authorization_pagination_legacy_and_detail(pool: PgPool) {
    let f = fixture(&pool).await;
    let d = create(&f, &pool).await;
    sqlx::query("UPDATE users SET display_name='History operator' WHERE id=$1")
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let legacy = Uuid::new_v4();
    // Same target-shaped metadata must never pull personal audit into platform history.
    let personal_event = Uuid::new_v4();
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,workspace_id,action,resource_type,resource_id) VALUES($1,$2,$3,'configuration.updated','deployments',$4)").bind(personal_event).bind(f.owner.user_id).bind(f.personal).bind(d).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id,metadata,created_at) VALUES($1,$2,'configuration.updated','deployments',$3,'{}',now()-interval '1 day')").bind(legacy).bind(f.admin.user_id).bind(d).execute(&pool).await.unwrap();
    for actor in [&f.admin, &f.auditor] {
        let (status, first) = call(
            &f.s,
            actor,
            "GET",
            &format!("{}?limit=1", history(d)),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["data"].as_array().unwrap().len(), 1);
        assert_eq!(first["data"][0]["after"]["enabled"], true);
        assert_eq!(first["data"][0]["actor"]["label"], "History operator");
        assert_eq!(first["data"][0]["actor"]["id"], f.admin.user_id.to_string());
        let next = first["next_cursor"].as_str().unwrap();
        let (_, second) = call(
            &f.s,
            actor,
            "GET",
            &format!("{}?limit=1&cursor={next}", history(d)),
            Value::Null,
        )
        .await;
        assert_eq!(second["data"][0]["id"], legacy.to_string());
        assert_eq!(second["data"][0]["values_available"], false);
        assert!(second["data"][0]["before"].is_null());
        assert!(second["next_cursor"].is_null());
        let (status, detail) = call(
            &f.s,
            actor,
            "GET",
            &format!("{}/{legacy}", history(d)),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["id"], legacy.to_string());
    }
    for actor in [&f.owner, &f.outsider] {
        for suffix in [String::new(), format!("/{legacy}")] {
            assert_eq!(
                call(
                    &f.s,
                    actor,
                    "GET",
                    &format!("{}{suffix}", history(d)),
                    Value::Null
                )
                .await
                .0,
                StatusCode::FORBIDDEN
            );
        }
    }
    for suffix in ["?limit=0", "?limit=101", "?cursor=bad"] {
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "GET",
                &format!("{}{suffix}", history(d)),
                Value::Null
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "GET",
            &format!("{}/{legacy}", history(Uuid::new_v4())),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // Missing target names/rows must not lose the immutable target identity.
    let (_, list) = call(&f.s, &f.admin, "GET", &history(d), Value::Null).await;
    assert_eq!(
        list["data"][0]["target"],
        json!({"kind":"deployment","id":d})
    );
    assert!(!list.to_string().contains(&personal_event.to_string()));
    assert!(!list.to_string().contains(&f.personal.to_string()));
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "GET",
            &format!("{}/{personal_event}", history(d)),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deployment_history_same_named_routes_never_share_identity(pool: PgPool) {
    let f = fixture(&pool).await;
    let original = create(&f, &pool).await;
    let same_name = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) SELECT $1,model_id,provider_connection_id,upstream_model,enabled FROM deployments WHERE id=$2")
        .bind(same_name).bind(original).execute(&pool).await.unwrap();
    let (status, body) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &format!("/api/v1/platform/deployments/{original}"),
        json!({"enabled":false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, first) = call(&f.s, &f.auditor, "GET", &history(original), Value::Null).await;
    let (_, second) = call(&f.s, &f.auditor, "GET", &history(same_name), Value::Null).await;
    assert_eq!(first["data"].as_array().unwrap().len(), 2);
    assert_eq!(second["data"].as_array().unwrap().len(), 1);
    assert_eq!(first["data"][0]["target"]["id"], original.to_string());
    assert_eq!(second["data"][0]["target"]["id"], same_name.to_string());
    assert_eq!(second["data"][0]["after"]["enabled"], true);
    let change = first["data"][0]["id"].as_str().unwrap();
    assert_eq!(
        call(
            &f.s,
            &f.auditor,
            "GET",
            &format!("{}/{change}", history(same_name)),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deployment_history_setup_records_one_creation(pool: PgPool) {
    let f = fixture(&pool).await;
    let p = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Setup history','openai','env:UNUSED')").bind(p).execute(&pool).await.unwrap();
    let (status, body) = call(&f.s, &f.admin, "POST", "/api/v1/platform/model-setup", json!({"model":{"public_name":"history-setup","display_name":"History setup","enabled":false},"route":{"provider_connection_id":p,"upstream_model":"history","enabled":true}})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM configuration_changes c JOIN deployments d ON d.id=c.deployment_id JOIN models m ON m.id=d.model_id WHERE m.public_name='history-setup' AND c.revision=1 AND c.before_enabled IS NULL AND c.after_enabled").fetch_one(&pool).await.unwrap();
    assert_eq!(count, 1);
}

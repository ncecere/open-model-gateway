//! #85: previews are observations, never rolled-back writes.
use super::*;

async fn route(pool: &PgPool, model: Uuid, enabled: bool) -> Uuid {
    let provider = Uuid::new_v4();
    let deployment = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Preview connection','openai','env:NOT_RESOLVED',true)")
        .bind(provider).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'preview-upstream',$4)")
        .bind(deployment).bind(model).bind(provider).bind(enabled).execute(pool).await.unwrap();
    deployment
}
fn path(id: Uuid) -> String {
    format!("/api/v1/platform/deployments/{id}/configuration/preview")
}
async fn preview(f: &Fixture, id: Uuid, enabled: bool) -> Value {
    let (status, body) = call(
        &f.s,
        &f.admin,
        "POST",
        &path(id),
        json!({"enabled":enabled}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}
async fn state(pool: &PgPool) -> Value {
    // Include every public table, including audit, configuration versions,
    // notifications, quota and ledger. This detects updates as well as inserts.
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables WHERE schemaname='public' ORDER BY tablename",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut result = serde_json::Map::new();
    for table in tables {
        let rows: Value = sqlx::query_scalar(&format!("SELECT coalesce(jsonb_agg(v ORDER BY v::text),'[]'::jsonb) FROM (SELECT to_jsonb(t) v FROM public.\"{table}\" t) rows"))
            .fetch_one(pool).await.unwrap();
        result.insert(table, rows);
    }
    Value::Object(result)
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deployment_preview_last_route_multi_catalog_visibility_and_no_writes(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "preview-model").await;
    let other = model(&pool, "unaffected-model").await;
    let d = route(&pool, m, true).await;
    route(&pool, other, true).await;
    for name in ["One", "Two"] {
        let catalog = Uuid::new_v4();
        sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,$2)")
            .bind(catalog)
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) VALUES($1,$2),($1,$3)")
            .bind(catalog)
            .bind(m)
            .bind(other)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES('team',$1),('personal',$1)").bind(catalog).execute(&pool).await.unwrap();
    }
    for ws in [f.team, f.project, f.personal] {
        sqlx::query(
            "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,$3)",
        )
        .bind(ws)
        .bind(m)
        .bind(if ws == f.project { "direct" } else { "catalog" })
        .execute(&pool)
        .await
        .unwrap();
    }
    // Catalog availability without a selection is not an affected workspace.
    workspace(&pool, "Unselected", "team", f.owner.user_id).await;
    let mut listener = sqlx::postgres::PgListener::connect_with(&pool)
        .await
        .unwrap();
    listener.listen("omg_config").await.unwrap();
    let before = state(&pool).await;
    let p = preview(&f, d, false).await;
    assert_eq!(p["target"], json!({"kind":"deployment","id":d}));
    assert_eq!(
        p["diff"],
        json!([{"field":"enabled","before":true,"after":false}])
    );
    assert_eq!(p["serving_routes_lost"], json!([d]));
    assert_eq!(p["serving_routes_gained"], json!([]));
    assert_eq!(
        p["models_losing_last_serving_route"],
        json!([{"id":m,"public_name":"preview-model"}])
    );
    assert_eq!(p["affected_workspaces"]["total"], 2);
    let details = p["affected_workspaces"]["data"].as_array().unwrap();
    assert_eq!(details.len(), 2);
    assert!(details.iter().any(|w| w["id"] == f.team.to_string()));
    assert!(details.iter().any(|w| w["id"] == f.project.to_string()));
    let text = p.to_string();
    for hidden in [
        f.personal.to_string(),
        f.owner.user_id.to_string(),
        f.owner.email.clone(),
        "credential_ref".into(),
        "NOT_RESOLVED".into(),
    ] {
        // Never echo the hidden value (an email) into test output.
        assert!(!text.contains(&hidden), "the preview leaked a hidden value");
    }
    assert!(
        p["expected_revision"]
            .as_str()
            .unwrap()
            .starts_with("catalog:")
    );
    assert!(p["observed_at"].is_string());
    assert_eq!(p["dependency_versions"].as_object().unwrap().len(), 2);
    assert!(p["dependency_versions"]["keys"].is_null());
    assert_eq!(before, state(&pool).await);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), listener.recv())
            .await
            .is_err()
    );
    let unchanged = preview(&f, d, true).await;
    assert_eq!(unchanged["diff"], json!([]));
    assert_eq!(unchanged["affected_workspaces"]["total"], 0);
    assert_eq!(before, state(&pool).await);
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deployment_preview_gained_alternative_routes_and_apply_behavior(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "preview-enable").await;
    let d = route(&pool, m, false).await;
    assert_eq!(
        preview(&f, d, true).await["serving_routes_gained"],
        json!([d])
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/deployments/{d}"),
            json!({"enabled":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    let audit: Value = sqlx::query_scalar(
        "SELECT metadata FROM audit_events WHERE resource_id=$1 AND action='configuration.updated'",
    )
    .bind(d)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit, json!({"enabled":true}));
    route(&pool, m, true).await;
    assert_eq!(
        preview(&f, d, false).await["models_losing_last_serving_route"],
        json!([])
    );
    sqlx::query("UPDATE provider_connections SET enabled=false WHERE id=(SELECT provider_connection_id FROM deployments WHERE id=$1)").bind(d).execute(&pool).await.unwrap();
    assert_eq!(
        preview(&f, d, false).await["serving_routes_lost"],
        json!([])
    );
    sqlx::query("UPDATE provider_connections SET provider='vllm' WHERE id=(SELECT provider_connection_id FROM deployments WHERE id=$1)").bind(d).execute(&pool).await.unwrap();
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['responses'] WHERE id=$1")
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    for endpoint in [path(d), format!("/api/v1/platform/deployments/{d}")] {
        let method = if endpoint.ends_with("preview") {
            "POST"
        } else {
            "PATCH"
        };
        let (status, body) = call(&f.s, &f.admin, method, &endpoint, json!({"enabled":true})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"]["reason"], "route_unsupported_capability");
    }
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("/api/v1/platform/deployments/{d}"),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &path(Uuid::new_v4()),
            json!({"enabled":false})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for actor in [&f.auditor, &f.owner, &f.outsider] {
        assert_eq!(
            call(&f.s, actor, "POST", &path(d), json!({"enabled":false}))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deployment_preview_live_replacement_and_disabled_model_effects(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "preview-replacement").await;
    let d = route(&pool, m, false).await;
    let catalog = Uuid::new_v4();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,'Default')")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) VALUES($1,$2)")
        .bind(catalog)
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES('team',$1)")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    for ws in [f.team, f.project] {
        sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'catalog')").bind(ws).bind(m).execute(&pool).await.unwrap();
    }
    // Project's stale catalog selection is not eligible; team inherits.
    assert_eq!(
        preview(&f, d, true).await["affected_workspaces"]["total"],
        1
    );
    // Empty replacement removes the team's eligibility without touching grants.
    sqlx::query("INSERT INTO workspace_catalog_overrides(workspace_id) VALUES($1)")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        preview(&f, d, true).await["affected_workspaces"]["total"],
        0
    );
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')",
    )
    .bind(f.team)
    .bind(m)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        preview(&f, d, true).await["affected_workspaces"]["total"],
        1
    );
    sqlx::query("UPDATE models SET enabled=false WHERE id=$1")
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    let p = preview(&f, d, true).await;
    assert_eq!(p["serving_routes_gained"], json!([]));
    assert_eq!(p["affected_workspaces"]["total"], 0);
    // Existing apply accepts enabling a route on a disabled model/connection.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/deployments/{d}"),
            json!({"enabled":true})
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deployment_preview_does_not_wait_for_management_locks(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "preview-lock-free").await;
    let d = route(&pool, m, true).await;
    let lock = resources::catalog_tx(&f.s, &f.admin, true).await.unwrap();
    let p = tokio::time::timeout(std::time::Duration::from_secs(2), preview(&f, d, false))
        .await
        .expect("preview must not acquire catalog/installation locks");
    assert_eq!(p["serving_routes_lost"], json!([d]));
    lock.rollback().await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deployment_preview_bounds_after_visibility_filter(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "preview-bounded").await;
    let d = route(&pool, m, true).await;
    sqlx::query("INSERT INTO workspaces(id,name,kind) SELECT gen_random_uuid(),'Shared '||n,'team' FROM generate_series(1,103) n").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) SELECT id,$1,'direct' FROM workspaces WHERE name LIKE 'Shared %' OR kind='personal'").bind(m).execute(&pool).await.unwrap();
    let p = preview(&f, d, false).await;
    assert_eq!(p["affected_workspaces"]["total"], 103);
    assert_eq!(
        p["affected_workspaces"]["data"].as_array().unwrap().len(),
        100
    );
    assert_eq!(p["affected_workspaces"]["truncated"], true);
    assert!(!p.to_string().contains(&f.personal.to_string()));
}

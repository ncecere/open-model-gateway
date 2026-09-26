use super::*;
use crate::{bootstrap, config::Environment};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use sqlx::PgPool;
use tower::ServiceExt;

struct Fixture {
    store: Store,
    org: Uuid,
    ws: Uuid,
    personal: Uuid,
    personal_token: String,
    team_token: String,
    admin: BrowserPrincipal,
    operator: BrowserPrincipal,
}
async fn fixture(pool: &PgPool) -> Fixture {
    let store = Store::new(pool.clone());
    let seed = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    let admin_id = sqlx::query_scalar("SELECT issued_to_user_id FROM api_keys WHERE id=$1")
        .bind(seed.personal_key.id)
        .fetch_one(pool)
        .await
        .unwrap();
    let admin = BrowserPrincipal {
        user_id: admin_id,
        email: "admin@invalid".into(),
        platform_admin: false,
    };
    let operator = BrowserPrincipal {
        user_id: Uuid::new_v4(),
        email: "operator@invalid".into(),
        platform_admin: true,
    };
    sqlx::query("INSERT INTO users(id,email,platform_admin) VALUES($1,$2,true)")
        .bind(operator.user_id)
        .bind(&operator.email)
        .execute(pool)
        .await
        .unwrap();
    Fixture {
        store,
        org: seed.organization_id,
        ws: seed.team_workspace_id,
        personal: seed.personal_workspace_id,
        personal_token: seed.personal_key.token,
        team_token: seed.team_key.token,
        admin,
        operator,
    }
}
async fn call(
    f: &Fixture,
    u: &BrowserPrincipal,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = super::super::routes()
        .layer(Extension(u.clone()))
        .with_state(f.store.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
async fn model(f: &Fixture) -> Uuid {
    let (status, value) = call(
        f,
        &f.operator,
        "POST",
        "/api/v1/platform/models",
        json!({"public_name":"global/model","display_name":"Global","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value["id"].as_str().unwrap().parse().unwrap()
}
#[sqlx::test]
async fn global_catalog_requires_live_operator_and_has_no_implicit_assignment(pool: PgPool) {
    let f = fixture(&pool).await;
    for path in ["providers", "models", "deployments", "audit"] {
        assert_eq!(
            call(
                &f,
                &f.admin,
                "GET",
                &format!("/api/v1/platform/{path}"),
                json!({})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    for path in ["providers", "deployments"] {
        assert_eq!(
            call(
                &f,
                &f.admin,
                "GET",
                &format!("/api/v1/orgs/{}/{path}", f.org),
                json!({})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    for path in ["providers", "models", "deployments"] {
        assert_eq!(
            call(
                &f,
                &f.admin,
                "POST",
                &format!("/api/v1/orgs/{}/{path}", f.org),
                json!({})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let id = model(&f).await;
    let provenance: Option<Uuid> =
        sqlx::query_scalar("SELECT organization_id FROM models WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(provenance.is_none());
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM organization_model_grants WHERE model_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    let catalog = call(
        &f,
        &f.admin,
        "GET",
        &format!("/api/v1/orgs/{}/models", f.org),
        json!({}),
    )
    .await;
    assert_eq!(catalog.0, StatusCode::OK);
    assert!(!catalog.1.to_string().contains("global/model"));
    assert_eq!(
        call(
            &f,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/models/{id}"),
            json!({"enabled":false})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f,
            &f.admin,
            "PATCH",
            &format!("/api/v1/orgs/{}/models/{id}", f.org),
            json!({"enabled":false})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE users SET platform_admin=false WHERE id=$1")
        .bind(f.operator.user_id)
        .execute(&pool)
        .await
        .unwrap();
    for method in ["GET", "POST"] {
        assert_eq!(
            call(
                &f,
                &f.operator,
                method,
                "/api/v1/platform/models",
                json!({"public_name":"stale","display_name":"Stale","enabled":true})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call(
            &f,
            &f.operator,
            "PATCH",
            &format!("/api/v1/platform/models/{id}"),
            json!({"enabled":false})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
#[sqlx::test]
async fn global_provider_deployment_validation_and_atomic_audits(pool: PgPool) {
    let f = fixture(&pool).await;
    let id = model(&f).await;
    let valid = json!({"name":"AWS","provider":"bedrock","credential_ref":"aws:default","region":"us-east-1","endpoint":null,"enabled":true});
    assert_eq!(
        call(
            &f,
            &f.admin,
            "POST",
            "/api/v1/platform/providers",
            valid.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for (field, value) in [
        ("endpoint", json!("http://127.0.0.1:8080")),
        ("credential_ref", json!("aws:arbitrary")),
        ("region", json!("../secret")),
        ("organization_id", json!(f.org)),
    ] {
        let mut bad = valid.clone();
        bad[field] = value;
        assert!(
            call(&f, &f.operator, "POST", "/api/v1/platform/providers", bad)
                .await
                .0
                .is_client_error()
        );
    }
    let (status, provider) =
        call(&f, &f.operator, "POST", "/api/v1/platform/providers", valid).await;
    assert_eq!(status, StatusCode::OK);
    let provider = provider["id"].as_str().unwrap();
    let body = json!({"model_id":id,"provider_connection_id":provider,"upstream_model":"test-model","enabled":true});
    assert_eq!(
        call(
            &f,
            &f.admin,
            "POST",
            "/api/v1/platform/deployments",
            body.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, deployment) = call(
        &f,
        &f.operator,
        "POST",
        "/api/v1/platform/deployments",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let deployment = deployment["id"].as_str().unwrap();
    for (resource, id) in [("providers", provider), ("deployments", deployment)] {
        assert_eq!(
            call(
                &f,
                &f.admin,
                "PATCH",
                &format!("/api/v1/platform/{resource}/{id}"),
                json!({"enabled":false})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &f,
                &f.operator,
                "PATCH",
                &format!("/api/v1/platform/{resource}/{id}"),
                json!({"enabled":false})
            )
            .await
            .0,
            StatusCode::OK
        );
        let listed = call(
            &f,
            &f.operator,
            "GET",
            &format!("/api/v1/platform/{resource}"),
            json!({}),
        )
        .await;
        assert_eq!(listed.0, StatusCode::OK);
        assert!(!listed.1.to_string().contains("credential_ref"));
    }
    assert_eq!(
        call(
            &f,
            &f.operator,
            "PATCH",
            &format!("/api/v1/platform/providers/{provider}"),
            json!({"enabled":true,"credential_ref":"env:NOT_ALLOWLISTED"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    sqlx::raw_sql("CREATE FUNCTION fail_resource_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'audit failed'; END $$; CREATE TRIGGER fail_resource_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION fail_resource_audit();").execute(&pool).await.unwrap();
    assert_eq!(
        call(
            &f,
            &f.operator,
            "PATCH",
            &format!("/api/v1/platform/models/{id}"),
            json!({"enabled":false})
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let enabled: bool = sqlx::query_scalar("SELECT enabled FROM models WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(enabled, "failed audit must roll back catalog change");
}

#[sqlx::test]
async fn assignments_aliases_user_grants_and_revocation_are_scoped(pool: PgPool) {
    let f = fixture(&pool).await;
    let id = model(&f).await;
    let assigned = format!("/api/v1/platform/orgs/{}/models/{id}", f.org);
    let ws = format!("/api/v1/workspaces/{}/grants", f.ws);
    let users = format!("/api/v1/orgs/{}/users/{}/grants", f.org, f.admin.user_id);
    assert_eq!(
        call(&f, &f.admin, "POST", &ws, json!({"model_id":id}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&f, &f.admin, "POST", &users, json!({"model_id":id}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&f, &f.admin, "PUT", &assigned, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f,
            &f.operator,
            "PUT",
            &assigned,
            json!({"public_name":"org/alias"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let grants: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workspace_model_grants WHERE model_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(grants, 0);
    assert_eq!(
        call(&f, &f.admin, "POST", &ws, json!({"model_id":id}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.admin, "POST", &users, json!({"model_id":id}))
            .await
            .0,
        StatusCode::OK
    );
    let personal_grants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workspace_model_grants WHERE workspace_id=$1 AND model_id=$2",
    )
    .bind(f.personal)
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        personal_grants, 0,
        "individual access must not create or enumerate personal workspaces"
    );
    let (status, data) = call(&f, &f.admin, "GET", &users, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        data,
        json!({"data":[{"model_id":id,"public_name":"org/alias","display_name":"Global"}]})
    );
    assert_eq!(
        call(&f, &f.admin, "DELETE", &format!("{users}/{id}"), json!({}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.admin, "POST", &users, json!({"model_id":id}))
            .await
            .0,
        StatusCode::OK
    );
    let personal = format!("/api/v1/orgs/{}/models/{id}/personal-access", f.org);
    assert_eq!(
        call(&f, &f.admin, "PUT", &personal, json!({"enabled":true}))
            .await
            .0,
        StatusCode::OK
    );
    let enabled: bool = sqlx::query_scalar("SELECT personal_enabled FROM models WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!enabled, "org policy must not change global model");
    assert_eq!(
        call(&f, &f.operator, "DELETE", &assigned, json!({}))
            .await
            .0,
        StatusCode::OK
    );
    for table in [
        "workspace_model_grants",
        "user_model_grants",
        "organization_model_grants",
    ] {
        let count: i64 =
            sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE model_id=$1"))
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }
    assert_eq!(
        call(&f, &f.operator, "PUT", &assigned, json!({})).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f, &f.admin, "GET", &users, json!({})).await.1,
        json!({"data":[]})
    );
    let enabled:bool=sqlx::query_scalar("SELECT personal_enabled FROM organization_model_grants WHERE organization_id=$1 AND model_id=$2").bind(f.org).bind(id).fetch_one(&pool).await.unwrap();
    assert!(!enabled);
    sqlx::query("UPDATE organization_memberships SET disabled_at=now() WHERE organization_id=$1 AND user_id=$2").bind(f.org).bind(f.admin.user_id).execute(&pool).await.unwrap();
    assert_eq!(
        call(&f, &f.operator, "POST", &users, json!({"model_id":id}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&f, &f.admin, "PUT", &personal, json!({"enabled":true}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}
async fn inference_catalog(f: &Fixture, token: &str) -> Value {
    let response = crate::http::router(f.store.clone())
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}
#[sqlx::test]
async fn individual_grants_apply_only_to_own_personal_keys_and_revoke_immediately(pool: PgPool) {
    let f = fixture(&pool).await;
    let (deployment, model, provider): (Uuid, Uuid, Uuid) =
        sqlx::query_as("SELECT id,model_id,provider_connection_id FROM deployments")
            .fetch_one(&pool)
            .await
            .unwrap();
    for (resource, id) in [("providers", provider), ("deployments", deployment)] {
        assert_eq!(
            call(
                &f,
                &f.operator,
                "PATCH",
                &format!("/api/v1/platform/{resource}/{id}"),
                json!({"enabled":true})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    for ws in [f.ws, f.personal] {
        assert_eq!(
            call(
                &f,
                &f.admin,
                "DELETE",
                &format!("/api/v1/workspaces/{ws}/grants/{model}"),
                json!({})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let grants = format!("/api/v1/orgs/{}/users/{}/grants", f.org, f.admin.user_id);
    assert_eq!(
        call(&f, &f.admin, "POST", &grants, json!({"model_id":model}))
            .await
            .0,
        StatusCode::OK
    );
    assert!(
        inference_catalog(&f, &f.personal_token)
            .await
            .to_string()
            .contains("company/smart")
    );
    assert!(
        !inference_catalog(&f, &f.team_token)
            .await
            .to_string()
            .contains("company/smart")
    );
    let personal_path = format!("/api/v1/workspaces/{}/grants", f.personal);
    let own = call(&f, &f.admin, "GET", &personal_path, json!({})).await;
    assert_eq!(own.0, StatusCode::OK);
    assert_eq!(own.1["data"][0]["model_id"], json!(model));
    assert_eq!(own.1["data"][0]["individual_granted"], true);
    assert_eq!(own.1["data"][0]["workspace_granted"], false);
    assert_eq!(
        call(&f, &f.operator, "GET", &personal_path, json!({}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f,
            &f.admin,
            "GET",
            &format!("/api/v1/workspaces/{}/grants", f.ws),
            json!({})
        )
        .await
        .1["data"],
        json!([])
    );
    let assigned = format!("/api/v1/platform/orgs/{}/models/{model}", f.org);
    assert_eq!(
        call(
            &f,
            &f.operator,
            "PUT",
            &assigned,
            json!({"public_name":"private/alias"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let catalog = inference_catalog(&f, &f.personal_token).await;
    assert!(catalog.to_string().contains("private/alias"));
    assert!(!catalog.to_string().contains("company/smart"));
    assert_eq!(
        call(
            &f,
            &f.admin,
            "POST",
            &personal_path,
            json!({"model_id":model})
        )
        .await
        .0,
        StatusCode::OK
    );
    let both = call(&f, &f.admin, "GET", &personal_path, json!({})).await.1;
    assert_eq!(
        both["data"].as_array().unwrap().len(),
        1,
        "sources are deduplicated"
    );
    assert_eq!(both["data"][0]["individual_granted"], true);
    assert_eq!(both["data"][0]["workspace_granted"], true);
    assert_eq!(
        call(
            &f,
            &f.admin,
            "DELETE",
            &format!("{personal_path}/{model}"),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f,
            &f.admin,
            "DELETE",
            &format!("{grants}/{model}"),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        !inference_catalog(&f, &f.personal_token)
            .await
            .to_string()
            .contains("private/alias")
    );
    assert_eq!(
        call(&f, &f.admin, "GET", &personal_path, json!({})).await.1["data"],
        json!([])
    );
}

#[sqlx::test]
async fn catalog_authority_is_rechecked_after_advisory_lock_and_audit_is_private(pool: PgPool) {
    let f = fixture(&pool).await;
    let private = Uuid::new_v4();
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,organization_id,workspace_id,action,target_id) VALUES($1,$2,$3,$4,'private.sentinel',$4)").bind(private).bind(f.admin.user_id).bind(f.org).bind(f.personal).execute(&pool).await.unwrap();
    let (status, audit) = call(&f, &f.operator, "GET", "/api/v1/platform/audit", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!audit.to_string().contains("private.sentinel"));
    let mut tx = pool.begin().await.unwrap();
    catalog_lock(&mut tx, true).await.unwrap();
    let store = f.store.clone();
    let stale = f.operator.clone();
    let pending = tokio::spawn(async move {
        let body = ModelInput {
            public_name: "blocked".into(),
            display_name: "Blocked".into(),
            enabled: true,
        };
        platform_create_model(State(store), Extension(stale), Json(body)).await
    });
    sqlx::query("UPDATE users SET platform_admin=false WHERE id=$1")
        .bind(f.operator.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(pending.await.unwrap().unwrap_err().0, StatusCode::FORBIDDEN);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM models WHERE public_name='blocked'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

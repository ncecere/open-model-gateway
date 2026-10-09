#![cfg(feature = "integration-tests")]
//! Explicitly opt in only on the dedicated disposable PostgreSQL test cluster.
use sqlx::PgPool;
#[sqlx::test(migrations = "./enterprise_migrations")]
#[ignore = "role/maintenance ACL probe: run only on a disposable test PostgreSQL cluster"]
async fn enterprise_runtime_allowlist_and_rollback_probes(pool: PgPool) {
    let mut connection = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(72419505)")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::raw_sql("DO $$ BEGIN IF NOT EXISTS(SELECT FROM pg_roles WHERE rolname='gateway_runtime') THEN CREATE ROLE gateway_runtime NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS; END IF; END $$; REVOKE CONNECT ON DATABASE postgres FROM PUBLIC,gateway_runtime; REVOKE CONNECT ON DATABASE template1 FROM PUBLIC,gateway_runtime;")
        .execute(&mut *connection).await.unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    let database = database.replace('"', "\"\"");
    sqlx::raw_sql(&format!("REVOKE ALL ON DATABASE \"{database}\" FROM PUBLIC,gateway_runtime; GRANT CONNECT ON DATABASE \"{database}\" TO gateway_runtime"))
        .execute(&mut *connection).await.unwrap();
    sqlx::raw_sql(include_str!("../../../deploy/staging/runtime-grants.sql"))
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../../../deploy/staging/verify-privileges.sql"
    ))
    .execute(&mut *connection)
    .await
    .unwrap();
    for table in [
        "users",
        "api_keys",
        "inference_executions",
        "monetary_ledger",
        "audit_events",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&mut *connection)
                .await
                .unwrap(),
            0,
            "rollback-only probe persisted {table}"
        );
    }
    alert_evaluation_runs_as_runtime(&pool).await;
    scim_provisioning_runs_as_runtime(&pool).await;
    sqlx::query("SELECT pg_advisory_unlock(72419505)")
        .execute(&mut *connection)
        .await
        .unwrap();
}

async fn runtime_pool(pool: &PgPool) -> PgPool {
    let options = (*pool.connect_options()).clone();
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::Executor::execute(c, "SET ROLE gateway_runtime").await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .unwrap()
}

/// SCIM provisioning, deactivation, group-provenance sync and account cleanup need
/// nothing beyond the reviewed grants.
async fn scim_provisioning_runs_as_runtime(pool: &PgPool) {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    const TOKEN: &str = "runtime-scim-token-0123456789abcdefghij";
    const ISSUER: &str = "https://issuer.example.invalid";
    sqlx::raw_sql(&format!("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES(gen_random_uuid(),'{ISSUER}','Runtime engineers','platform','user')"))
        .execute(pool)
        .await
        .unwrap();
    let runtime = runtime_pool(pool).await;
    let store = open_model_gateway::store::Store::new(runtime.clone());
    let config = open_model_gateway::scim::ScimConfig::from_lookup(|name| match name {
        "GATEWAY_SCIM_TOKEN_ENV" => Some("RUNTIME_SCIM_TOKEN".into()),
        "RUNTIME_SCIM_TOKEN" => Some(TOKEN.into()),
        _ => None,
    })
    .unwrap()
    .unwrap();
    let app = open_model_gateway::scim::standalone_router(
        store.clone(),
        config,
        ISSUER,
        "https://gateway.example.invalid/scim/v2",
    );
    let call = |method: &str, uri: String, body: Option<serde_json::Value>| {
        let app = app.clone();
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/scim+json")
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        async move {
            let response = app.oneshot(request).await.unwrap();
            let status = response.status().as_u16();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&bytes)
                    .unwrap_or(serde_json::Value::Null),
            )
        }
    };
    let (status, user) = call(
        "POST",
        "/scim/v2/Users".into(),
        Some(serde_json::json!({"userName":"runtime-scim@example.invalid","externalId":"00u-runtime","name":{"givenName":"Run"}})),
    )
    .await;
    assert_eq!(status, 201, "{user}");
    let id = user["id"].as_str().unwrap().to_owned();
    let (status, group) = call(
        "POST",
        "/scim/v2/Groups".into(),
        Some(serde_json::json!({"displayName":"Runtime engineers","members":[{"value":id}]})),
    )
    .await;
    assert_eq!(status, 201, "{group}");
    let group = group["id"].as_str().unwrap().to_owned();
    for (method, uri, body, expected) in [
        (
            "GET",
            "/scim/v2/Users?filter=userName%20eq%20%22RUNTIME-SCIM%40example.invalid%22".to_owned(),
            None,
            200,
        ),
        (
            "GET",
            "/scim/v2/Groups?excludedAttributes=members".to_owned(),
            None,
            200,
        ),
        (
            "PATCH",
            format!("/scim/v2/Users/{id}"),
            Some(
                serde_json::json!({"Operations":[{"op":"replace","value":{"active":false,"displayName":"Runtime"}}]}),
            ),
            200,
        ),
        (
            "PATCH",
            format!("/scim/v2/Users/{id}"),
            Some(
                serde_json::json!({"Operations":[{"op":"Replace","path":"active","value":"True"}]}),
            ),
            200,
        ),
        (
            "PUT",
            format!("/scim/v2/Users/{id}"),
            Some(
                serde_json::json!({"userName":"runtime-scim@example.invalid","emails":[{"value":"runtime-scim2@example.invalid"}]}),
            ),
            200,
        ),
        (
            "PATCH",
            format!("/scim/v2/Groups/{group}"),
            Some(
                serde_json::json!({"Operations":[{"op":"remove","path":format!("members[value eq \"{id}\"]")}]}),
            ),
            204,
        ),
        (
            "PATCH",
            format!("/scim/v2/Groups/{group}"),
            Some(
                serde_json::json!({"Operations":[{"op":"add","path":"members","value":[{"value":id}]},{"op":"replace","path":"externalId","value":"grp-runtime"}]}),
            ),
            204,
        ),
        (
            "PUT",
            format!("/scim/v2/Groups/{group}"),
            Some(serde_json::json!({"displayName":"Runtime engineers 2","members":[{"value":id}]})),
            200,
        ),
        ("DELETE", format!("/scim/v2/Groups/{group}"), None, 204),
        ("DELETE", format!("/scim/v2/Users/{id}"), None, 204),
    ] {
        let (status, body) = call(method, uri.clone(), body).await;
        assert_eq!(status, expected, "{method} {uri}: {body}");
    }
    // Grace expiry is simulated as owner; cleanup itself runs as runtime.
    sqlx::query("UPDATE users SET cleanup_due_at=now()-interval '1 second' WHERE email='runtime-scim2@example.invalid'")
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(
        open_model_gateway::lifecycle::cleanup_inactive_accounts(&store)
            .await
            .unwrap(),
        1
    );
    let (status, _) = call("GET", format!("/scim/v2/Users/{id}"), None).await;
    assert_eq!(status, 404);
    runtime.close().await;
}

/// The alert evaluator and email delivery need nothing beyond the reviewed
/// grants: seed as owner, then evaluate through a pool that runs as runtime.
async fn alert_evaluation_runs_as_runtime(pool: &PgPool) {
    sqlx::raw_sql(r#"DO $$ DECLARE u uuid:=gen_random_uuid(); ws uuid:=gen_random_uuid(); personal uuid:=gen_random_uuid(); k uuid:=gen_random_uuid(); pk uuid:=gen_random_uuid(); m uuid:=gen_random_uuid(); pc uuid:=gen_random_uuid(); d uuid:=gen_random_uuid(); e uuid; i integer; BEGIN
 INSERT INTO users(id,email) VALUES(u,'alerts-'||u||'@example.invalid');
 INSERT INTO platform_role_grants(user_id,role,source) VALUES(u,'admin','manual');
 INSERT INTO workspaces(id,name,kind) VALUES(ws,'Alerts project','project');
 INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES(personal,'Personal','personal',u);
 INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES(ws,u,'owner','manual'),(personal,u,'owner','manual');
 INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES(k,ws,u,'Alerts',decode(repeat('02',32),'hex')),(pk,personal,u,'Mine',decode(repeat('03',32),'hex'));
 INSERT INTO models(id,public_name) VALUES(m,'alerts-'||m);
 INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES(pc,'Alerts local','openai_compatible','none','http://127.0.0.1:19091/v1',true);
 INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES(d,m,pc,'x',true);
 FOR i IN 1..4 LOOP
  e:=gen_random_uuid();
  INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,error_code,root_request_id,completed_at) VALUES(e,CASE WHEN i=4 THEN personal ELSE ws END,CASE WHEN i=4 THEN pk ELSE k END,d,'alerts','openai_compatible',false,'failed','upstream_unavailable',e,now());
  INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,input_tokens,output_tokens) VALUES(e,CASE WHEN i=4 THEN personal ELSE ws END,CASE WHEN i=4 THEN pk ELSE k END,d,now(),date_trunc('minute',now()),date_trunc('month',now()),now(),'settled',2000000,1,1);
 END LOOP;
 INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) VALUES('local',ws,'month',5000000),('local',personal,'month',1000000);
 INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','day',10000000);
 INSERT INTO alert_rules(id,scope,kind,name,budget_layers,thresholds,notify_platform_admins) VALUES(gen_random_uuid(),'installation','budget_threshold','Budgets',ARRAY['installation','local','key'],ARRAY[50,80,100],true);
 INSERT INTO alert_rules(id,scope,kind,name,spike_factor_percent,min_spend_microusd) VALUES(gen_random_uuid(),'installation','spend_spike','Spike',300,1);
 INSERT INTO alert_rules(id,scope,kind,name,window_minutes,error_rate_percent,min_requests) VALUES(gen_random_uuid(),'installation','error_rate','Errors',15,50,2);
 INSERT INTO alert_rules(id,scope,kind,name,window_minutes,consecutive_failures) VALUES(gen_random_uuid(),'installation','provider_failing','Upstream',15,3);
 INSERT INTO alert_rules(id,scope,workspace_id,kind,name,budget_layers,thresholds,notify_workspace_admins,notify_emails) VALUES(gen_random_uuid(),'workspace',ws,'budget_threshold','Project budgets',ARRAY['local'],ARRAY[80],true,ARRAY['ops@example.invalid']);
 END $$"#)
        .execute(pool)
        .await
        .unwrap();
    let options = (*pool.connect_options()).clone();
    let runtime = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::Executor::execute(c, "SET ROLE gateway_runtime").await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .unwrap();
    let store = open_model_gateway::store::Store::new(runtime.clone());
    let report = open_model_gateway::alerts::evaluate_once(&store)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.failed_rules, 0, "{report:?}");
    // Installation day 60%, project local 120%, spike, error rate, connection, project rule, personal built-in.
    assert_eq!(report.fired, 7, "{report:?}");
    assert_eq!(
        open_model_gateway::alerts::evaluate_once(&store)
            .await
            .unwrap()
            .unwrap()
            .fired,
        0
    );
    // No relay configured: deliveries are recorded as such, as runtime.
    assert_eq!(
        open_model_gateway::alerts::deliver_pending(&store, 100).await,
        7
    );
    // Readiness and scrape-time metrics collectors need no extra grants.
    assert_eq!(
        store.readiness().await,
        open_model_gateway::store::Readiness {
            database: true,
            schema: true
        }
    );
    let exposition = open_model_gateway::metrics::METRICS
        .render(Some(&store))
        .await;
    assert!(exposition.contains(r#"gateway_reservations_held{state="pending"} 0"#));
    assert!(!exposition.contains(r#"gateway_metrics_collection_errors_total{"#));
    runtime.close().await;
}

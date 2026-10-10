#![cfg(feature = "integration-tests")]
// Test probes may begin transactions directly (see `src/db.rs`).
#![allow(clippy::disallowed_methods)]
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
        "budget_totals",
        "rate_minute_counters",
        "inflight_counters",
        "realtime_responses",
        "async_jobs",
        "async_job_files",
        "stored_files",
        "storage_usage_hours",
        "batch_lines",
        "batch_segments",
        "deployment_batch_scheduling",
        "deployment_batch_signals",
        "batch_route_waits",
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
    realtime_accounting_runs_as_runtime(&pool).await;
    scim_provisioning_runs_as_runtime(&pool).await;
    budget_totals_maintained_as_runtime(&pool).await;
    read_snapshots_run_as_runtime(&pool).await;
    file_store_runs_as_runtime(&pool).await;
    files_api_runs_as_runtime(&pool).await;
    // Last: its unknown batch hold would change the installation totals above.
    async_jobs_run_as_runtime(&pool).await;
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

/// Scale plan P1: lock-free read snapshots and the reporting pool (including
/// its replay-lag check) work with the runtime role alone; a missing grant
/// would otherwise silently fall back to the primary.
async fn read_snapshots_run_as_runtime(pool: &PgPool) {
    let runtime = runtime_pool(pool).await;
    let options = (*pool.connect_options())
        .clone()
        .application_name("omg_runtime_reporting");
    let reporting = sqlx::postgres::PgPoolOptions::new()
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
    let store = open_model_gateway::store::Store::new(runtime)
        .with_reporting(Some(reporting), std::time::Duration::from_secs(30));
    let authorized = store.snapshot().await.unwrap();
    let mut tx = store.reporting(authorized).await.unwrap();
    let (name, role, read_only): (String, String, String) = sqlx::query_as(
        "SELECT current_setting('application_name'),current_user::text,current_setting('transaction_read_only')",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        (name.as_str(), role.as_str(), read_only.as_str()),
        ("omg_runtime_reporting", "gateway_runtime", "on")
    );
    // The standby replay-lag functions are executable (null on a primary).
    sqlx::query("SELECT pg_is_in_recovery(),pg_last_wal_receive_lsn()::text,pg_last_wal_replay_lsn()::text,pg_last_xact_replay_timestamp()")
        .execute(&mut *tx)
        .await
        .unwrap();
    // The report/usage/log/me tables are readable in the snapshot.
    for table in [
        "inference_executions",
        "governance_reservations",
        "api_keys",
        "workspaces",
        "effective_workspace_memberships",
        "effective_platform_roles",
        "installation_settings",
    ] {
        sqlx::query(&format!("SELECT count(*) FROM {table}"))
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("{table}: {e}"));
    }
    tx.commit().await.unwrap();
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
    // Last-admin refusal (0018): rollback, audit and the installation alert as runtime.
    let admins: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT user_id FROM effective_platform_roles WHERE role='admin'")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(admins.len(), 1, "the alert probe seeds exactly one admin");
    let (status, body) = call("DELETE", format!("/scim/v2/Users/{}", admins[0]), None).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["scimType"], "mutability");
    let (audits, alerts): (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM audit_events WHERE action='scim.last_admin_protected'),(SELECT count(*) FROM alert_events WHERE builtin='scim_last_admin' AND resolved_at IS NULL)")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!((audits, alerts), (1, 1));
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

/// Files API (0020): quota reservations (advisory locks + reserved_bytes),
/// listing, quota reads, the upload pipeline and hourly usage recording run
/// with the reviewed grants; usage history cannot be rewritten.
async fn files_api_runs_as_runtime(pool: &PgPool) {
    use futures::StreamExt;
    use open_model_gateway::filestore::{
        FileStoreRuntime, NewFile, Purpose,
        files::{FileError, FileList, FileStorage},
        upload::{Uploader, receive},
        usage::record_hours,
    };
    let (ws, user, key) = (
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
    );
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(user)
        .bind(format!("files-api-{user}@example.test"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Files API probe','team')")
        .bind(ws)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'files',decode(repeat('00',32),'hex'))").bind(key).bind(ws).bind(user).execute(pool).await.unwrap();
    let runtime = runtime_pool(pool).await;
    let store = open_model_gateway::store::Store::new(runtime.clone());
    // Storage quota is edited like the other workspace limits.
    sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id,storage_bytes) VALUES($1,64) ON CONFLICT(workspace_id) DO UPDATE SET storage_bytes=excluded.storage_bytes").bind(ws).execute(&runtime).await.unwrap();
    sqlx::query("INSERT INTO workspace_local_policies(workspace_id,storage_bytes) VALUES($1,48) ON CONFLICT(workspace_id) DO UPDATE SET storage_bytes=excluded.storage_bytes").bind(ws).execute(&runtime).await.unwrap();
    sqlx::query("UPDATE installation_settings SET file_user_files_enabled=true,file_batch_enabled=true WHERE singleton").execute(&runtime).await.unwrap();
    let files = FileStorage::new(store.clone(), FileStoreRuntime::memory());
    let boundary = "probe";
    let form = |data: &str| {
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nuser_data\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\n{data}\r\n--{boundary}--\r\n"
        )
    };
    let who = Uploader {
        workspace_id: ws,
        api_key_id: Some(key),
        user_id: Some(user),
    };
    let ct = format!("multipart/form-data; boundary={boundary}");
    let stream =
        |s: String| futures::stream::iter([Ok::<_, std::io::Error>(bytes::Bytes::from(s))]);
    let stored = receive(
        &files,
        who,
        Some(&ct),
        stream(form(&"a".repeat(40))),
        1 << 20,
    )
    .await
    .unwrap();
    assert_eq!(stored.api_purpose.as_deref(), Some("user_data"));
    assert!(
        receive(
            &files,
            who,
            Some(&ct),
            stream(form(&"b".repeat(20))),
            1 << 20
        )
        .await
        .is_err()
    );
    let usage = files.workspace_storage(ws).await.unwrap();
    assert_eq!((usage.used_bytes, usage.quota_bytes), (40, Some(48)));
    let out = files
        .create(
            NewFile::new(Purpose::BatchOutput, Some(ws)),
            futures::stream::iter([Ok(bytes::Bytes::from_static(b"12345678901"))]).boxed(),
        )
        .await;
    assert_eq!(out.unwrap_err(), FileError::QuotaExceeded);
    let (page, more) = files
        .list(
            ws,
            &FileList {
                limit: 10,
                ..FileList::default()
            },
        )
        .await
        .unwrap();
    assert_eq!((page.len(), more), (1, false));
    // Hourly usage: backdate the progress mark and the file (as owner), record as runtime.
    sqlx::query("ALTER TABLE storage_usage_progress DISABLE TRIGGER storage_usage_progress_guard")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE storage_usage_progress SET recorded_through=date_trunc('hour', now(), 'UTC') - interval '3 hours'").execute(pool).await.unwrap();
    sqlx::query("ALTER TABLE storage_usage_progress ENABLE TRIGGER storage_usage_progress_guard")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE stored_files DISABLE TRIGGER stored_files_guard")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE stored_files SET committed_at=now()-interval '3 hours' WHERE id=$1")
        .bind(stored.id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE stored_files ENABLE TRIGGER stored_files_guard")
        .execute(pool)
        .await
        .unwrap();
    assert!(record_hours(&store, 48).await.unwrap() >= 2);
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM storage_usage_hours WHERE workspace_id=$1")
            .bind(ws)
            .fetch_one(&runtime)
            .await
            .unwrap();
    assert!(rows >= 2);
    assert!(
        sqlx::query("UPDATE storage_usage_hours SET byte_seconds=1")
            .execute(&runtime)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM storage_usage_hours")
            .execute(&runtime)
            .await
            .is_err()
    );
    assert!(files.delete(stored.id, Some(ws)).await.unwrap());
    runtime.close().await;
}

/// File store (0019): metadata writes, scoped reads, deletion, retention sweeps,
/// verification and the Admin › Settings › Storage statements need nothing
/// beyond the reviewed grants; rows are never deleted.
async fn file_store_runs_as_runtime(pool: &PgPool) {
    use futures::StreamExt;
    use open_model_gateway::filestore::{
        FileStoreRuntime, NewFile, Purpose,
        files::FileStorage,
        sweep::{sweep_once, verify},
    };
    let (ws, user, key) = (
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
    );
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(user)
        .bind(format!("files-{user}@example.test"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Files probe','project')")
        .bind(ws)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'files',decode(repeat('00',32),'hex'))").bind(key).bind(ws).bind(user).execute(pool).await.unwrap();
    let runtime = runtime_pool(pool).await;
    let store = open_model_gateway::store::Store::new(runtime.clone());
    // Settings › Storage writes (toggles, retention, health result) and reads.
    sqlx::query("UPDATE installation_settings SET file_batch_enabled=true,file_batch_retention_days=7,file_video_enabled=false,file_video_retention_days=7,file_user_files_enabled=false,file_user_files_retention_days=30,file_export_retention_days=1,updated_at=now(),updated_by=$1 WHERE singleton").bind(user).execute(&runtime).await.unwrap();
    sqlx::query("UPDATE installation_settings SET file_store_last_check_at=now(),file_store_last_check_ok=true,file_store_last_check_error=NULL,file_store_last_check_target=$1 WHERE singleton").bind("0".repeat(64)).execute(&runtime).await.unwrap();
    sqlx::query("SELECT purpose,count(*),coalesce(sum(size_bytes),0)::bigint FROM stored_files WHERE deleted_at IS NULL AND committed_at IS NOT NULL GROUP BY purpose").fetch_all(&runtime).await.unwrap();
    let files = FileStorage::new(store.clone(), FileStoreRuntime::memory());
    assert!(files.accepts(Purpose::BatchInput).await.unwrap());
    let mut new = NewFile::new(Purpose::BatchInput, Some(ws));
    new.created_by_user_id = Some(user);
    new.created_by_api_key_id = Some(key);
    new.filename = Some("input.jsonl".into());
    let body = futures::stream::iter([Ok(bytes::Bytes::from_static(b"{}\n"))]).boxed();
    let file = files.create(new, body).await.unwrap();
    let (_, mut stream) = files.open(file.id, Some(ws)).await.unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), b"{}\n");
    assert_eq!(files.workspace_stored_bytes(ws).await.unwrap(), 3);
    assert!(
        verify(&store, files.runtime(), 100)
            .await
            .unwrap()
            .consistent()
    );
    let export = files
        .create(
            NewFile::new(Purpose::Export, None),
            futures::stream::iter([Ok(bytes::Bytes::from_static(b"a,b"))]).boxed(),
        )
        .await
        .unwrap();
    assert!(files.delete(file.id, Some(ws)).await.unwrap());
    // Expire the export (as owner: created_at is not runtime-writable) and sweep it.
    sqlx::query("UPDATE stored_files SET created_at=now()-interval '2 days' WHERE id=$1")
        .bind(export.id)
        .execute(pool)
        .await
        .unwrap();
    let report = sweep_once(&store, files.runtime(), 10).await.unwrap();
    assert_eq!((report.deleted, report.failed), (1, 0));
    assert!(
        sqlx::query("DELETE FROM stored_files")
            .execute(&runtime)
            .await
            .is_err()
    );
    let deleted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM stored_files WHERE deleted_at IS NOT NULL AND filename IS NULL",
    )
    .fetch_one(&runtime)
    .await
    .unwrap();
    assert_eq!(deleted, 2);
    // Installation logo (0023): the upload, replace and remove statements and
    // the public endpoint need nothing beyond the reviewed grants.
    let image: &'static [u8] = b"\x89PNG\r\n\x1a\nlogo-probe";
    let logo = files
        .create(
            NewFile {
                created_by_user_id: Some(user),
                filename: Some("logo.png".into()),
                content_type: Some("image/png".into()),
                ..NewFile::new(Purpose::Branding, None)
            },
            futures::stream::iter([Ok(bytes::Bytes::from_static(image))]).boxed(),
        )
        .await
        .unwrap();
    let previous: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT branding_logo_file_id FROM installation_settings WHERE singleton",
    )
    .fetch_one(&runtime)
    .await
    .unwrap();
    assert_eq!(previous, None);
    sqlx::query("UPDATE installation_settings SET branding_logo_file_id=$1,branding_logo_updated_at=now(),branding_logo_width=$2,branding_logo_height=$3,logo_url=NULL,updated_at=now(),updated_by=$4 WHERE singleton")
        .bind(logo.id)
        .bind(64)
        .bind(64)
        .bind(user)
        .execute(&runtime)
        .await
        .unwrap();
    let app = open_model_gateway::http::router(store.clone())
        .layer(axum::Extension(files.runtime().clone()));
    let response = tower::ServiceExt::oneshot(
        app,
        axum::http::Request::builder()
            .uri("/api/v1/branding/logo")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "image/png");
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), image);
    sqlx::query("UPDATE installation_settings SET branding_logo_file_id=NULL,branding_logo_updated_at=NULL,branding_logo_width=NULL,branding_logo_height=NULL,updated_at=now(),updated_by=$1 WHERE singleton")
        .bind(user)
        .execute(&runtime)
        .await
        .unwrap();
    assert!(files.delete(logo.id, None).await.unwrap());
    runtime.close().await;
}

/// Expiry reconciliation (a trigger-maintained reservation write) and the
/// `budget verify` consistency check need nothing beyond the reviewed grants.
async fn budget_totals_maintained_as_runtime(pool: &PgPool) {
    // Seed an expired pending attempt as owner, like an admission whose lease ran out.
    sqlx::raw_sql(r#"DO $$ DECLARE e uuid:=gen_random_uuid(); r record; BEGIN
      SELECT workspace_id,api_key_id,deployment_id INTO r FROM governance_reservations ORDER BY execution_id LIMIT 1;
      INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id) VALUES(e,r.workspace_id,r.api_key_id,r.deployment_id,'alerts','openai_compatible',false,'started',e);
      INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd) VALUES(e,r.workspace_id,r.api_key_id,r.deployment_id,now(),date_trunc('minute',now()),date_trunc('month',now()),now()-interval '1 second','pending',500);
    END $$"#)
        .execute(pool)
        .await
        .unwrap();
    let runtime = runtime_pool(pool).await;
    let store = open_model_gateway::store::Store::new(runtime.clone());
    assert_eq!(
        open_model_gateway::governance::reconcile_expired(&store, 10)
            .await
            .unwrap(),
        1
    );
    // Pruning old minute counters (0024) runs as runtime; retained ones stay.
    open_model_gateway::governance::rates::prune(&store, 100)
        .await
        .unwrap();
    let report = open_model_gateway::governance::totals::verify(&store)
        .await
        .unwrap();
    assert!(report.consistent(), "{report:?}");
    assert!(report.buckets > 0);
    assert!(report.rate_buckets > 0);
    // The unknown attempt keeps its 500 micro-USD hold in every period.
    let held: Vec<String> = sqlx::query_scalar(
        "SELECT held_microusd::text FROM budget_totals WHERE scope_kind='installation' ORDER BY period",
    )
    .fetch_all(&runtime)
    .await
    .unwrap();
    assert_eq!(held, ["500", "500", "500", "500"]);
    runtime.close().await;
}

mod jobs_fake {
    use open_model_gateway::{
        inference::{
            error::InferenceError,
            types::{
                ApiProtocol, Capabilities, ChatRequest, ChatResponse, Deployment, FinishReason,
                ProviderOutput, Usage,
            },
        },
        jobs::types::*,
        providers::ProviderAdapter,
    };
    use std::sync::Mutex;
    /// Scripted provider: a queued then completed video, a batch that is
    /// cancelled; uploads are drained.
    #[derive(Default)]
    pub struct Fake(pub Mutex<Vec<&'static str>>);
    fn video(state: JobState) -> UpstreamVideo {
        UpstreamVideo {
            id: UpstreamId::parse("video_runtime").unwrap(),
            state,
            progress: None,
            seconds: Some(4),
            size: None,
            completed_at: None,
            expires_at: None,
            error: None,
        }
    }
    fn answer() -> ChatResponse {
        ChatResponse {
            content: Some("probe".into()),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: Usage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                ..Usage::default()
            },
        }
    }
    fn batch(status: BatchStatus) -> UpstreamBatch {
        UpstreamBatch {
            id: UpstreamId::parse("batch_runtime").unwrap(),
            status,
            input_file: None,
            output_file: None,
            error_file: Some(UpstreamId::parse("file-err-runtime").unwrap()),
            counts: None,
            usage: None,
            created_at: None,
            in_progress_at: None,
            finalizing_at: None,
            completed_at: None,
            failed_at: None,
            expired_at: None,
            cancelling_at: None,
            cancelled_at: None,
            expires_at: None,
            metadata: None,
        }
    }
    #[async_trait::async_trait]
    impl ProviderAdapter for Fake {
        fn id(&self) -> &'static str {
            "openai"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                text_chat: true,
                streaming: false,
                tools: false,
            }
        }
        fn supports_protocol(&self, p: ApiProtocol) -> bool {
            matches!(
                p,
                ApiProtocol::Videos | ApiProtocol::Batches | ApiProtocol::ChatCompletions
            )
        }
        fn supports_video_request(&self, _: &Deployment, _: &VideoRequest) -> bool {
            true
        }
        async fn execute(
            &self,
            _: &Deployment,
            _: ChatRequest,
        ) -> Result<ProviderOutput, InferenceError> {
            self.0.lock().unwrap().push("line");
            Ok(ProviderOutput::Complete(answer()))
        }
        async fn create_video(
            &self,
            _: &Deployment,
            _: VideoRequest,
        ) -> Result<UpstreamVideo, InferenceError> {
            Ok(video(JobState::Queued))
        }
        async fn retrieve_video(
            &self,
            _: &Deployment,
            _: &UpstreamId,
        ) -> Result<UpstreamVideo, InferenceError> {
            Ok(video(JobState::Completed))
        }
        fn native_batch(&self, _: &Deployment, _: BatchEndpoint) -> bool {
            true
        }
        fn encode_native_line(
            &self,
            _: &Deployment,
            _: BatchEndpoint,
            custom_id: &str,
            _: &BatchRequest,
        ) -> Result<Vec<u8>, InferenceError> {
            Ok(format!("{{\"custom_id\":\"{custom_id}\"}}").into_bytes())
        }
        async fn submit_native_batch(
            &self,
            _: &Deployment,
            _: BatchEndpoint,
            mut records: ByteStream,
        ) -> Result<UpstreamBatch, InferenceError> {
            use futures_util::StreamExt;
            while let Some(record) = records.next().await {
                record?;
            }
            self.0.lock().unwrap().push("submit");
            Ok(batch(BatchStatus::InProgress))
        }
        async fn retrieve_batch(
            &self,
            _: &Deployment,
            _: &UpstreamId,
        ) -> Result<UpstreamBatch, InferenceError> {
            Ok(batch(BatchStatus::Completed))
        }
        async fn native_batch_results(
            &self,
            _: &Deployment,
            _: &UpstreamBatch,
        ) -> Result<ByteStream, InferenceError> {
            Ok(Box::pin(futures_util::stream::once(async {
                Ok(axum::body::Bytes::from_static(b"{\"custom_id\":\"l0\"}\n"))
            })))
        }
        fn decode_native_result(
            &self,
            _: &Deployment,
            _: BatchEndpoint,
            line: &[u8],
        ) -> Result<NativeResult, InferenceError> {
            let v: serde_json::Value =
                serde_json::from_slice(line).map_err(|_| InferenceError::InvalidUpstream)?;
            Ok(NativeResult {
                custom_id: v["custom_id"].as_str().unwrap_or_default().to_owned(),
                outcome: NativeOutcome::Succeeded(Box::new(BatchResponse::Chat(answer()))),
            })
        }
        async fn delete_native_batch(
            &self,
            _: &Deployment,
            _: &UpstreamBatch,
        ) -> Result<(), InferenceError> {
            self.0.lock().unwrap().push("delete");
            Ok(())
        }
        async fn cancel_batch(
            &self,
            _: &Deployment,
            _: &UpstreamId,
        ) -> Result<UpstreamBatch, InferenceError> {
            self.0.lock().unwrap().push("cancel");
            Ok(batch(BatchStatus::Cancelled))
        }
    }
}

/// Video and batch jobs (admission, lease extension, job/file rows, polling,
/// settlement, cancel) need nothing beyond the reviewed grants.
async fn async_jobs_run_as_runtime(pool: &PgPool) {
    use open_model_gateway::{
        auth::Principal,
        jobs::{JobLimits, Jobs, types::*},
        providers::ProviderRegistry,
    };
    let (user, ws, key, video_model, batch_model) = (
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
    );
    sqlx::raw_sql(&format!(r#"DO $$ DECLARE pc uuid:=gen_random_uuid(); dv uuid:=gen_random_uuid(); db uuid:=gen_random_uuid(); BEGIN
 INSERT INTO users(id,email) VALUES('{user}','jobs-{user}@example.invalid');
 INSERT INTO platform_role_grants(user_id,role,source) VALUES('{user}','user','manual');
 INSERT INTO workspaces(id,name,kind) VALUES('{ws}','Jobs project','project');
 INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES('{ws}','{user}','owner','manual');
 INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES('{key}','{ws}','{user}','Jobs',decode(repeat('06',32),'hex'));
 INSERT INTO models(id,public_name,supported_protocols) VALUES('{video_model}','video-{video_model}',ARRAY['videos']),('{batch_model}','batch-{batch_model}',ARRAY['chat_completions']);
 INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES(pc,'Jobs','openai','env:UNUSED',true);
 INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES(dv,'{video_model}',pc,'sora-2',true),(db,'{batch_model}',pc,'gpt-x',true);
 INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES('{ws}','{video_model}','direct'),('{ws}','{batch_model}','direct');
 INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES(gen_random_uuid(),dv,0,1,3,
  '[{{"meter":"input_tokens","not_applicable":true}},{{"meter":"output_tokens","not_applicable":true}},{{"meter":"cache_read_tokens","not_applicable":true}},{{"meter":"cache_write_tokens","not_applicable":true}},{{"meter":"cache_write_5m_tokens","not_applicable":true}},{{"meter":"cache_write_1h_tokens","not_applicable":true}},{{"meter":"output_images","not_applicable":true}},{{"meter":"input_characters","not_applicable":true}},{{"meter":"input_audio_seconds_ms","not_applicable":true}},{{"meter":"output_audio_seconds_ms","not_applicable":true}},{{"meter":"search_units","not_applicable":true}},{{"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"}},{{"meter":"output_video_seconds_ms","microusd_per_batch":"100000","batch":1000,"unit_label":"/second","sku_label":"Video"}}]','{{}}');
 INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES(gen_random_uuid(),db,1000000,1000000,100,50,1);
 END $$"#))
    .execute(pool)
    .await
    .unwrap();
    let store = open_model_gateway::store::Store::new(runtime_pool(pool).await);
    let fake = std::sync::Arc::new(jobs_fake::Fake::default());
    let mut registry = ProviderRegistry::default();
    registry.register(fake.clone()).unwrap();
    let jobs = Jobs::with(store, registry, JobLimits::default());
    let principal = Principal {
        key_id: key,
        workspace_id: ws,
        user_id: Some(user),
    };
    let video = uuid::Uuid::new_v4();
    jobs.create_video(
        principal,
        VideoRequest {
            model: format!("video-{video_model}"),
            prompt: "probe".into(),
            seconds: 4,
            size: VideoSize::DEFAULT,
        },
        video,
    )
    .await
    .unwrap();
    // Jobs at once (0018): the accepted video holds the key's only job slot.
    sqlx::query(
        "INSERT INTO key_policies(workspace_id,governance_key_id,concurrent_jobs) VALUES($1,$2,1)",
    )
    .bind(ws)
    .bind(key)
    .execute(pool)
    .await
    .unwrap();
    let denied = jobs
        .create_video(
            principal,
            VideoRequest {
                model: format!("video-{video_model}"),
                prompt: "probe".into(),
                seconds: 4,
                size: VideoSize::DEFAULT,
            },
            uuid::Uuid::new_v4(),
        )
        .await;
    assert!(matches!(
        denied,
        Err(open_model_gateway::jobs::JobError::Inference(
            open_model_gateway::inference::error::InferenceError::JobLimitExceeded(
                open_model_gateway::inference::error::LimitScope::ApiKey
            )
        ))
    ));
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(jobs.poll_once().await.unwrap(), 1);
    let (state, actual): (String, Option<i64>) = sqlx::query_as(
        "SELECT state,actual_microusd FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(video)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!((state.as_str(), actual), ("settled", Some(400_000)));
    // Batches (0021): a gateway input file, native admission at the pinned
    // price, cancel before submission (released at zero), a native
    // submit/poll/collect, and a gateway-run batch through the engine.
    use open_model_gateway::{
        filestore::{FileStoreRuntime, NewFile, Purpose, QuotaMode, files::FileStorage},
        jobs::{BatchMode, batch::CreateBatch, runner::Runner},
    };
    sqlx::query("UPDATE installation_settings SET file_batch_enabled=true WHERE singleton")
        .execute(pool)
        .await
        .unwrap();
    let store = jobs_store(pool).await;
    let files_runtime = FileStoreRuntime::memory();
    let files = FileStorage::new(store.clone(), files_runtime.clone());
    let mut registry = ProviderRegistry::default();
    registry.register(fake.clone()).unwrap();
    let engine = open_model_gateway::inference::Engine::new(
        std::sync::Arc::new(store.clone()),
        registry.clone(),
        open_model_gateway::inference::EngineLimits::default(),
    )
    .unwrap();
    let jobs = Jobs::with(store, registry, JobLimits::default())
        .with_files(Some(files_runtime))
        .with_engine(engine);
    let line = format!(
        "{{\"custom_id\":\"a\",\"method\":\"POST\",\"url\":\"/v1/chat/completions\",\"body\":{{\"model\":\"batch-{batch_model}\",\"messages\":[{{\"role\":\"user\",\"content\":\"probe\"}}],\"max_tokens\":10}}}}\n"
    );
    let mut inputs = Vec::new();
    for _ in 0..3 {
        let body = line.clone().into_bytes();
        let stored = files
            .create(
                NewFile {
                    created_by_api_key_id: Some(key),
                    quota: QuotaMode::CountOnly,
                    ..NewFile::new(Purpose::BatchInput, Some(ws))
                },
                Box::pin(futures_util::stream::once(async move {
                    Ok(axum::body::Bytes::from(body))
                })),
            )
            .await
            .unwrap();
        inputs.push(open_model_gateway::filestore::files::public_id(stored.id));
    }
    let create = |file: String, gateway: bool| CreateBatch {
        input_file_id: file,
        endpoint: BatchEndpoint::ChatCompletions,
        metadata: gateway.then(|| {
            serde_json::json!({"omg_mode":"gateway"})
                .as_object()
                .cloned()
                .unwrap()
        }),
        // 0022: a longer completion window (gateway-run) is stored once.
        completion_window_hours: gateway.then_some(48),
    };
    let first = uuid::Uuid::new_v4();
    let job = jobs
        .create_batch(principal, first, create(inputs[0].clone(), false))
        .await
        .unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Native));
    let cancelled = jobs
        .cancel_batch(&principal, &client_id("batch_", job.id))
        .await
        .unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    let (state, actual): (String, Option<i64>) = sqlx::query_as(
        "SELECT state,actual_microusd FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(first)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!((state.as_str(), actual), ("settled", Some(0)));
    let native = uuid::Uuid::new_v4();
    jobs.create_batch(principal, native, create(inputs[1].clone(), false))
        .await
        .unwrap();
    for _ in 0..2 {
        sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
            .execute(pool)
            .await
            .unwrap();
        jobs.poll_once().await.unwrap();
    }
    let (state, actual): (String, Option<i64>) = sqlx::query_as(
        "SELECT state,actual_microusd FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(native)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!((state.as_str(), actual), ("settled", Some(15)));
    let gateway = jobs
        .create_batch(
            principal,
            uuid::Uuid::new_v4(),
            create(inputs[2].clone(), true),
        )
        .await
        .unwrap();
    // Batch scheduling (0022): route settings written as runtime; the runner
    // gates, claims under the route lock, publishes demand and clears it.
    {
        use open_model_gateway::jobs::schedule;
        let deployment: uuid::Uuid =
            sqlx::query_scalar("SELECT deployment_id FROM async_jobs WHERE id=$1")
                .bind(gateway.id)
                .fetch_one(pool)
                .await
                .unwrap();
        let runtime = runtime_pool(pool).await;
        let mut tx = runtime.begin().await.unwrap();
        schedule::save_settings(
            &mut tx,
            deployment,
            &schedule::RouteSettings {
                max_concurrency: 1,
                yield_live_threshold: Some(5),
                ..schedule::RouteSettings::default()
            },
            user,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            schedule::load_settings(&runtime, deployment)
                .await
                .unwrap()
                .max_concurrency,
            1
        );
    }
    assert!(Runner::new(jobs.clone()).run_next().await.unwrap());
    let done: (String, Option<i32>, Option<i16>) = sqlx::query_as(
        "SELECT state,request_completed,completion_window_hours FROM async_jobs WHERE id=$1",
    )
    .bind(gateway.id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        (done.0.as_str(), done.1, done.2),
        ("completed", Some(1), Some(48))
    );
    {
        use open_model_gateway::jobs::schedule;
        let runtime = runtime_pool(pool).await;
        let store = open_model_gateway::store::Store::new(runtime.clone());
        let (deployment, routed): (uuid::Uuid, Option<uuid::Uuid>) = sqlx::query_as("SELECT j.deployment_id,l.deployment_id FROM async_jobs j JOIN batch_lines l ON l.job_id=j.id WHERE j.id=$1")
            .bind(gateway.id)
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(
            routed,
            Some(deployment),
            "the line was claimed on its route"
        );
        let status = schedule::route_status(&runtime, deployment).await.unwrap();
        assert_eq!(status["running_lines"], 0);
        assert_eq!(status["live_in_flight"], 0);
        assert!(
            schedule::batch_waits(&runtime, gateway.id)
                .await
                .unwrap()
                .is_empty()
        );
        schedule::observe_metrics(&store).await.unwrap();
    }
    assert_eq!(
        *fake.0.lock().unwrap(),
        ["submit", "delete", "line"],
        "no cancelled batch reached the provider"
    );
    let report = open_model_gateway::governance::totals::verify(&jobs_store(pool).await)
        .await
        .unwrap();
    assert!(report.consistent(), "{report:?}");
}
async fn jobs_store(pool: &PgPool) -> open_model_gateway::store::Store {
    open_model_gateway::store::Store::new(runtime_pool(pool).await)
}

/// A realtime session's admission, window extension, per-response settlement
/// and finish need nothing beyond the reviewed grants (including the composed
/// 0017 validators and their bases).
async fn realtime_accounting_runs_as_runtime(pool: &PgPool) {
    use open_model_gateway::{
        auth::Principal,
        billing::MeterUsage,
        inference::{
            realtime::{RealtimeFinish, RealtimeUsage, ResponseBound, ResponseStatus},
            repository::{AttemptTelemetry, ExecutionStart, InferenceRepository, Outcome},
            types::WorkloadKind,
            workload::{OutputReservation, WorkloadAdmission},
        },
    };
    let (user, ws, key, model) = (
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
    );
    sqlx::raw_sql(&format!(r#"DO $$ DECLARE pc uuid:=gen_random_uuid(); d uuid:=gen_random_uuid(); BEGIN
 INSERT INTO users(id,email) VALUES('{user}','realtime-{user}@example.invalid');
 INSERT INTO platform_role_grants(user_id,role,source) VALUES('{user}','user','manual');
 INSERT INTO workspaces(id,name,kind) VALUES('{ws}','Realtime project','project');
 INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES('{ws}','{user}','owner','manual');
 INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES('{key}','{ws}','{user}','Realtime',decode(repeat('05',32),'hex'));
 INSERT INTO models(id,public_name,supported_protocols) VALUES('{model}','realtime-{model}',ARRAY['realtime']);
 INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES(pc,'Realtime','openai','env:UNUSED',true);
 INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES(d,'{model}',pc,'gpt-realtime',true);
 INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES('{ws}','{model}','direct');
 INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES(gen_random_uuid(),d,1000,100,3,
  '[{{"meter":"input_tokens","microusd_per_batch":"4000000","batch":1000000,"unit_label":"/M tokens","sku_label":"Text"}},{{"meter":"cache_read_tokens","not_applicable":true}},{{"meter":"output_tokens","microusd_per_batch":"16000000","batch":1000000,"unit_label":"/M tokens","sku_label":"Text out"}},{{"meter":"input_audio_tokens","microusd_per_batch":"32000000","batch":1000000,"unit_label":"/M tokens","sku_label":"Audio"}},{{"meter":"cache_read_audio_tokens","not_applicable":true}},{{"meter":"output_audio_tokens","microusd_per_batch":"64000000","batch":1000000,"unit_label":"/M tokens","sku_label":"Audio out"}},{{"meter":"cache_write_tokens","not_applicable":true}},{{"meter":"cache_write_5m_tokens","not_applicable":true}},{{"meter":"cache_write_1h_tokens","not_applicable":true}},{{"meter":"output_images","not_applicable":true}},{{"meter":"input_characters","not_applicable":true}},{{"meter":"input_audio_seconds_ms","not_applicable":true}},{{"meter":"output_audio_seconds_ms","not_applicable":true}},{{"meter":"search_units","not_applicable":true}},{{"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"}}]','{{}}');
 END $$"#))
    .execute(pool)
    .await
    .unwrap();
    let store = open_model_gateway::store::Store::new(runtime_pool(pool).await);
    let principal = Principal {
        key_id: key,
        workspace_id: ws,
        user_id: Some(user),
    };
    let name = format!("realtime-{model}");
    let deployment = store
        .deployments(&principal, &name)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        store
            .realtime_window_output(deployment.id, 4096)
            .await
            .unwrap(),
        100
    );
    let id = uuid::Uuid::new_v4();
    store
        .admit_workload(
            &ExecutionStart {
                id,
                root_request_id: id,
                attempt_number: 1,
                principal,
                deployment_id: deployment.id,
                provider: "openai".into(),
                model: name.clone(),
                streamed: true,
                upstream_model: Some("gpt-realtime".into()),
                client: Default::default(),
            },
            &WorkloadAdmission {
                kind: WorkloadKind::Realtime,
                output: OutputReservation::Requested(Some(100)),
                unit_ceilings: MeterUsage {
                    requests: Some(1),
                    ..MeterUsage::default()
                },
            },
            960,
            &deployment,
        )
        .await
        .unwrap();
    let usage = RealtimeUsage {
        input_text_tokens: 10,
        cached_text_tokens: 0,
        input_audio_tokens: 20,
        cached_audio_tokens: 0,
        output_text_tokens: 10,
        output_audio_tokens: 40,
    };
    // Resize the admission window to the context, then add one more window.
    let window = ResponseBound {
        text_input: Some(400),
        audio_input: Some(100),
        output: 100,
    };
    store
        .realtime_reserve_window(
            &principal,
            id,
            &name,
            window,
            Some(ResponseBound::admission(100)),
        )
        .await
        .unwrap();
    store.realtime_open_response(id, 1, window).await.unwrap();
    store
        .realtime_settle_response(id, 1, Some(ResponseStatus::Completed), Some(usage), window)
        .await
        .unwrap();
    store
        .realtime_reserve_window(&principal, id, &name, ResponseBound::unknown(100), None)
        .await
        .unwrap();
    store
        .realtime_finish(&RealtimeFinish {
            id,
            outcome: Outcome::Succeeded,
            error: None,
            elapsed_ms: 1,
            telemetry: AttemptTelemetry::default(),
            unopened_request: false,
            window: ResponseBound::unknown(100),
        })
        .await
        .unwrap();
    // 10×4 + 10×16 + 20×32 + 40×64 = 3,400 µUSD; the unused window is released.
    let (state, actual): (String, Option<i64>) = sqlx::query_as(
        "SELECT state,actual_microusd FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!((state.as_str(), actual), ("settled", Some(3_400)));
}

use super::provider_cost_tests::{mapping, upload};
use super::*;

async fn snapshot(f: &Fixture, import: Uuid, mapping: Uuid) -> Value {
    let (status, created) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/provider-reconciliations",
        json!({"import_id":import,"mapping_id":mapping}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let (status, value) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("/api/v1/platform/provider-reconciliations/{}", id(&created)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}
async fn create(f: &Fixture, path: &str, body: Value) -> Uuid {
    let (status, value) = call(&f.s, &f.admin, "POST", path, body).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    id(&value)
}
async fn estimate(pool: &PgPool, f: &Fixture, ws: Uuid, deployment: Uuid, amount: i64) -> Uuid {
    let (_, k) = key(f, &f.owner, ws, Value::Null).await;
    let e = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,upstream_model,streamed,state,root_request_id,started_at,input_tokens,output_tokens) VALUES($1,$2,$3,$4,'not-the-upstream-model','anthropic','claude-placeholder',false,'succeeded',$1,'2025-08-01T12:00:00Z',1,1)")
        .bind(e).bind(ws).bind(id(&k)).bind(deployment).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,actual_microusd,input_tokens,output_tokens) VALUES($1,$2,$3,$4,'2025-08-01T12:00:00Z','2025-08-01T12:00:00Z','2025-08-01T00:00:00Z',now(),'settled',999,$5,1,1)")
        .bind(e).bind(ws).bind(id(&k)).bind(deployment).bind(amount).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,admitted_at) VALUES($1,$2,'settlement',$3,'2025-08-01T12:00:00Z')")
        .bind(Uuid::new_v4()).bind(e).bind(amount).execute(pool).await.unwrap();
    e
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn provider_cost_matching_exclusive_variance_shared_no_proration_and_archived_coverage(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let c = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Historical','anthropic','env:PLACEHOLDER')").bind(c).execute(&pool).await.unwrap();
    let m = model(&pool, "current-public-alias-is-not-evidence").await;
    let d = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'not-historical')").bind(d).bind(m).bind(c).execute(&pool).await.unwrap();
    let execution = estimate(&pool, &f, f.personal, d, 2500).await;
    let mut body = upload();
    body["pages"][0]["data"][0]["results"][0]["amount"] = json!("1");
    let imp = create(&f, "/api/v1/platform/provider-cost-imports", body).await;
    let mut map = mapping(c, Some(f.personal));
    let first_mapping = create(&f, "/api/v1/platform/provider-cost-mappings", map.clone()).await;
    let exclusive = snapshot(&f, imp, first_mapping).await;
    assert_eq!(
        exclusive["snapshot"]["rows"][0]["variance_microusd"],
        "7500"
    );
    assert_eq!(
        exclusive["snapshot"]["rows"][0]["workspace_id"],
        json!(f.personal)
    );
    assert_eq!(
        exclusive["snapshot"]["estimates"]["held_floor_microusd"], "0",
        "settled reservation's stale floor is not an active hold"
    );
    assert!(!exclusive.to_string().contains(&execution.to_string()));
    estimate(&pool, &f, f.team, d, 500).await;
    let contradictory = snapshot(&f, imp, first_mapping).await;
    let row = &contradictory["snapshot"]["rows"][0];
    assert!(row["workspace_id"].is_null());
    assert!(row["variance_microusd"].is_null());
    assert!(row.to_string().contains("ambiguous_scope"));
    assert_eq!(row["imported_microusd"], "10000");
    assert_eq!(row["workspace_estimates"].as_array().unwrap().len(), 2);
    map["supersedes"] = json!(first_mapping);
    map["bindings"][0]["exclusive"] = json!(false);
    map["bindings"][0]["workspace_id"] = Value::Null;
    let shared_mapping = create(&f, "/api/v1/platform/provider-cost-mappings", map).await;
    let shared = snapshot(&f, imp, shared_mapping).await;
    assert_eq!(shared["snapshot"]["imported_microusd"], "10000");
    assert_eq!(
        shared["snapshot"]["estimates"]["settled_estimate_microusd"],
        "3000"
    );
    assert!(
        shared["snapshot"]["rows"][0]
            .to_string()
            .contains("shared_unallocated")
    );
    sqlx::query("INSERT INTO archived_partitions(id,archive_group,month,lower_bound,upper_bound,partitions,manifest_sha256,disposition) VALUES($1,'history','2025-08','2025-08-01','2025-09-01','[]',repeat('a',64),'detached')").bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    let archived = snapshot(&f, imp, shared_mapping).await;
    assert_eq!(archived["snapshot"]["archived_detail_missing"], true);
    assert!(
        archived["snapshot"]["rows"][0]
            .to_string()
            .contains("incomplete_estimate")
    );
    assert_eq!(
        snapshot_read(&f, id(&exclusive)).await,
        exclusive,
        "new usage or archival never recomputes an old snapshot"
    );
}
async fn snapshot_read(f: &Fixture, id: Uuid) -> Value {
    call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("/api/v1/platform/provider-reconciliations/{id}"),
        Value::Null,
    )
    .await
    .1
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn provider_cost_default_scope_unsupported_categories_and_account_lineage_refusals(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let mut body = upload();
    let row = &mut body["pages"][0]["data"][0]["results"][0];
    row["workspace_id"] = Value::Null;
    row["cost_type"] = json!("credit");
    row["amount"] = json!("-1.5");
    let imp = create(&f, "/api/v1/platform/provider-cost-imports", body).await;
    let c = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Default','anthropic','env:PLACEHOLDER')").bind(c).execute(&pool).await.unwrap();
    let mut map = mapping(c, None);
    map["bindings"][0]["provider_workspace_id"] = Value::Null;
    let map_id = create(&f, "/api/v1/platform/provider-cost-mappings", map.clone()).await;
    let rec = snapshot(&f, imp, map_id).await;
    assert_eq!(
        rec["snapshot"]["rows"][0]["provider_workspace_id"],
        Value::Null
    );
    assert_eq!(rec["snapshot"]["rows"][0]["imported_microusd"], "-15000");
    assert_eq!(rec["snapshot"]["rows"][0]["category"], "credit");
    assert!(rec.to_string().contains("unsupported_category"));
    map["account_id"] = json!("different-account");
    let other_map = create(&f, "/api/v1/platform/provider-cost-mappings", map).await;
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            "/api/v1/platform/provider-reconciliations",
            json!({"import_id":imp,"mapping_id":other_map})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    for table in [
        "provider_cost_mappings",
        "provider_cost_bindings",
        "provider_reconciliations",
    ] {
        for statement in [
            format!(
                "UPDATE {table} SET {}={}",
                if table == "provider_cost_bindings" {
                    "evidence_reference"
                } else {
                    "id"
                },
                if table == "provider_cost_bindings" {
                    "evidence_reference"
                } else {
                    "id"
                }
            ),
            format!("DELETE FROM {table}"),
            format!("TRUNCATE {table} CASCADE"),
        ] {
            assert!(
                sqlx::query(&statement).execute(&pool).await.is_err(),
                "{statement}"
            );
        }
    }
}

#![cfg(feature = "integration-tests")]

use open_model_gateway::{
    auth::NewApiKey,
    bootstrap,
    config::Environment,
    governance,
    inference::{
        repository::{ExecutionStart, InferenceRepository},
        types::ChatRequest,
    },
    store::Store,
};
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test(migrations = false)]
async fn catalog_migration_preserves_aliases_ids_and_immutable_accounting(pool: PgPool) {
    for migration in [
        include_str!("../migrations/0001_foundation.sql"),
        include_str!("../migrations/0002_inference_engine.sql"),
        include_str!("../migrations/0003_control_plane.sql"),
        include_str!("../migrations/0004_governance.sql"),
        include_str!("../migrations/0005_routing.sql"),
        include_str!("../migrations/0006_execution_retention.sql"),
    ] {
        sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    }
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    let principal = store
        .authenticate(&keys.personal_key.token)
        .await
        .unwrap()
        .unwrap();
    let (deployment, model): (Uuid, Uuid) =
        sqlx::query_as("SELECT id,model_id FROM deployments LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let org2 = Uuid::new_v4();
    let model2 = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'other')")
        .bind(org2)
        .bind(org2.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO models (id,organization_id,public_name,display_name,personal_enabled) VALUES ($1,$2,'company/smart','duplicate',true)").bind(model2).bind(org2).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO governance_policies (id,organization_id,scope,requests_per_minute,monthly_budget_microusd) VALUES ($1,$2,'organization',10,1000)").bind(Uuid::new_v4()).bind(principal.organization_id).execute(&pool).await.unwrap();
    let price = Uuid::new_v4();
    sqlx::query("INSERT INTO deployment_prices (id,organization_id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit) VALUES ($1,$2,$3,100,200,1000,100)").bind(price).bind(principal.organization_id).bind(deployment).execute(&pool).await.unwrap();
    let execution = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions (id,organization_id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id) VALUES ($1,$2,$3,$4,$5,'company/smart','openai',false,'started',$1)").bind(execution).bind(principal.organization_id).bind(principal.workspace_id).bind(principal.key_id).bind(deployment).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO governance_reservations (execution_id,organization_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd) VALUES ($1,$2,$3,$4,$5,$6,now(),date_trunc('minute',now()),date_trunc('month',now()),now()+interval '1 minute','pending',1100,1)").bind(execution).bind(principal.organization_id).bind(principal.workspace_id).bind(principal.key_id).bind(deployment).bind(price).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO monetary_ledger (id,organization_id,execution_id,kind,amount_microusd) VALUES ($1,$2,$3,'hold',1)").bind(Uuid::new_v4()).bind(principal.organization_id).bind(execution).execute(&pool).await.unwrap();
    async fn history(pool: &PgPool) -> Vec<serde_json::Value> {
        let mut rows = Vec::new();
        for table in [
            "deployment_prices",
            "inference_executions",
            "governance_reservations",
            "monetary_ledger",
        ] {
            rows.push(
                sqlx::query_scalar::<_, serde_json::Value>(&format!(
                    "SELECT coalesce(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM {table} t"
                ))
                .fetch_one(pool)
                .await
                .unwrap(),
            );
        }
        rows
    }
    let before = history(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql(include_str!("../migrations/0007_platform_catalog.sql"))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(history(&pool).await, before);
    let aliases: Vec<(Uuid, Uuid, String, bool)> = sqlx::query_as("SELECT organization_id,model_id,public_name,personal_enabled FROM organization_model_grants ORDER BY model_id").fetch_all(&pool).await.unwrap();
    assert_eq!(aliases.len(), 2);
    assert!(
        aliases
            .iter()
            .any(|(o, m, a, _)| *o == principal.organization_id
                && *m == model
                && a == "company/smart")
    );
    assert!(
        aliases
            .iter()
            .any(|(o, m, a, p)| *o == org2 && *m == model2 && a == "company/smart" && *p)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(DISTINCT public_name) FROM models")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT requests_per_minute FROM platform_organization_policies WHERE organization_id=$1").bind(principal.organization_id).fetch_one(&pool).await.unwrap(), 10);
    for statement in [
        "UPDATE deployment_prices SET input_token_limit=1",
        "DELETE FROM deployment_prices",
        "UPDATE monetary_ledger SET amount_microusd=0",
        "DELETE FROM monetary_ledger",
    ] {
        assert!(sqlx::query(statement).execute(&pool).await.is_err());
    }
    assert_eq!(history(&pool).await, before);
}

#[sqlx::test]
async fn shared_global_deployment_requires_each_consumer_entitlement_and_uses_global_price(
    pool: PgPool,
) {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    let first = store
        .authenticate(&keys.personal_key.token)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled=true")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE provider_connections SET enabled=true")
        .execute(&pool)
        .await
        .unwrap();
    let (deployment, model): (Uuid, Uuid) =
        sqlx::query_as("SELECT id,model_id FROM deployments LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let org = Uuid::new_v4();
    let workspace = Uuid::new_v4();
    let user = first.user_id.unwrap();
    let key = NewApiKey::generate();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'consumer')")
        .bind(org)
        .bind(org.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO organization_memberships (organization_id,user_id,role) VALUES ($1,$2,'member')").bind(org).bind(user).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO workspaces (id,organization_id,name,kind,owner_user_id) VALUES ($1,$2,'personal','personal',$3)").bind(workspace).bind(org).bind(user).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO api_keys (id,organization_id,workspace_id,issued_to_user_id,name,secret_hash) VALUES ($1,$2,$3,$4,'test',$5)").bind(key.id).bind(org).bind(workspace).bind(user).bind(key.digest.as_slice()).execute(&pool).await.unwrap();
    let second = store.authenticate(&key.token).await.unwrap().unwrap();
    assert!(
        store
            .deployments(&second, "company/smart")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(sqlx::query("INSERT INTO workspace_model_grants (organization_id,workspace_id,model_id) VALUES ($1,$2,$3)").bind(org).bind(workspace).bind(model).execute(&pool).await.is_err());
    sqlx::query("INSERT INTO organization_model_grants (organization_id,model_id,public_name) VALUES ($1,$2,'shared-alias')").bind(org).bind(model).execute(&pool).await.unwrap();
    assert!(
        store
            .deployments(&second, "shared-alias")
            .await
            .unwrap()
            .is_empty()
    );
    sqlx::query("INSERT INTO workspace_model_grants (organization_id,workspace_id,model_id) VALUES ($1,$2,$3)").bind(org).bind(workspace).bind(model).execute(&pool).await.unwrap();
    let target = store
        .deployments(&second, "shared-alias")
        .await
        .unwrap()
        .remove(0);
    assert_eq!(target.id, deployment);
    assert!(
        store
            .deployments(&second, "company/smart")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .deployments(&first, "shared-alias")
            .await
            .unwrap()
            .is_empty()
    );
    let price = Uuid::new_v4();
    sqlx::query("INSERT INTO deployment_prices (id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit) VALUES ($1,$2,1000000,1000000,100,50)").bind(price).bind(deployment).execute(&pool).await.unwrap();
    let id = Uuid::new_v4();
    let record = ExecutionStart {
        id,
        root_request_id: id,
        attempt_number: 1,
        principal: second,
        deployment_id: deployment,
        provider: target.provider.clone(),
        model: "shared-alias".into(),
        streamed: false,
    };
    let request = ChatRequest {
        model: "shared-alias".into(),
        messages: vec![],
        tools: vec![],
        tool_choice: None,
        temperature: None,
        max_output_tokens: Some(10),
        stream: false,
    };
    governance::admit_for_deployment(&store, &record, &request, 30, &target)
        .await
        .unwrap();
    let reservation: (Uuid, Uuid, i64) = sqlx::query_as("SELECT organization_id,price_id,held_microusd FROM governance_reservations WHERE execution_id=$1").bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(reservation, (org, price, 110));
    governance::finish(
        &store,
        &open_model_gateway::inference::repository::ExecutionFinish {
            id,
            outcome: open_model_gateway::inference::repository::Outcome::Succeeded,
            error: None,
            usage: open_model_gateway::inference::types::Usage {
                input_tokens: Some(2),
                output_tokens: Some(3),
            },
            elapsed_ms: 1,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT actual_microusd FROM governance_reservations WHERE execution_id=$1"
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        5
    );
}

#[sqlx::test]
async fn fresh_global_models_have_no_implicit_access_but_legacy_insert_preserves_alias(
    pool: PgPool,
) {
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'consumer')")
        .bind(org)
        .bind(org.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let global = Uuid::new_v4();
    sqlx::query("INSERT INTO models (id,public_name,display_name,personal_enabled) VALUES ($1,'same','global',true)").bind(global).execute(&pool).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM organization_model_grants")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let legacy = Uuid::new_v4();
    sqlx::query("INSERT INTO models (id,organization_id,public_name,display_name,personal_enabled) VALUES ($1,$2,'same','legacy',true)").bind(legacy).bind(org).execute(&pool).await.unwrap();
    let grant: (Uuid,String,bool) = sqlx::query_as("SELECT model_id,public_name,personal_enabled FROM organization_model_grants WHERE organization_id=$1").bind(org).fetch_one(&pool).await.unwrap();
    assert_eq!(grant, (legacy, "same".into(), true));
    assert_ne!(
        sqlx::query_scalar::<_, String>("SELECT public_name FROM models WHERE id=$1")
            .bind(legacy)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "same"
    );
    // Updating deprecated bootstrap input must neither revive nor expand access.
    sqlx::query("DELETE FROM organization_model_grants WHERE organization_id=$1")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE models SET personal_enabled=true WHERE id=$1")
        .bind(legacy)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM organization_model_grants")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

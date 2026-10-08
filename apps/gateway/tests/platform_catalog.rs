#![cfg(feature = "integration-tests")]

mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{BrowserSession, management_app};
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
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

async fn fixture(pool: &PgPool) -> (Store, open_model_gateway::bootstrap::DevelopmentKeys) {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    for table in ["provider_connections", "deployments"] {
        sqlx::query(&format!("UPDATE {table} SET enabled=true"))
            .execute(pool)
            .await
            .unwrap();
    }
    (store, keys)
}

#[sqlx::test(migrations = false)]
async fn legacy_catalog_lineage_is_rejected_without_rewriting_aliases_or_history(pool: PgPool) {
    // Old migrations are evidence only. Never bootstrap enterprise data into them.
    for migration in [
        include_str!("../migrations/0001_foundation.sql"),
        include_str!("../migrations/0002_inference_engine.sql"),
        include_str!("../migrations/0003_control_plane.sql"),
        include_str!("../migrations/0004_governance.sql"),
        include_str!("../migrations/0005_routing.sql"),
        include_str!("../migrations/0006_execution_retention.sql"),
        include_str!("../migrations/0007_platform_catalog.sql"),
        include_str!("../migrations/0008_projects.sql"),
        include_str!("../migrations/0009_key_model_restrictions.sql"),
    ] {
        sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    }
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations(id,slug,name) VALUES($1,'historic','Untouched legacy')")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(r#"DO $$
    DECLARE o uuid; u uuid:=gen_random_uuid(); w uuid:=gen_random_uuid();
      k uuid:=gen_random_uuid(); p uuid:=gen_random_uuid(); m uuid:=gen_random_uuid();
      d uuid:=gen_random_uuid(); price uuid:=gen_random_uuid(); e uuid:=gen_random_uuid();
    BEGIN
      SELECT id INTO o FROM organizations WHERE slug='historic';
      INSERT INTO users(id,email) VALUES(u,'legacy@example.invalid');
      INSERT INTO organization_memberships(organization_id,user_id,role) VALUES(o,u,'owner');
      INSERT INTO workspaces(id,organization_id,name,kind) VALUES(w,o,'Historical team','team');
      INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES(o,w,u,'owner');
      INSERT INTO api_keys(id,organization_id,workspace_id,issued_to_user_id,name,secret_hash) VALUES(k,o,w,u,'Legacy digest only',decode(repeat('ab',32),'hex'));
      INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES(p,'Historical connection','openai','env:UNUSED_LEGACY_KEY');
      INSERT INTO models(id,organization_id,public_name,display_name) VALUES(m,o,'historical-alias','Historical model');
      INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES(d,m,p,'never-called');
      INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit) VALUES(price,d,1000000,2000000,100,10);
      INSERT INTO inference_executions(id,organization_id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id) VALUES(e,o,w,k,d,'historical-alias','openai',false,'started',e);
      INSERT INTO governance_reservations(execution_id,organization_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd) VALUES(e,o,w,k,d,price,now(),date_trunc('minute',now()),date_trunc('month',now()),now()+interval '1 minute','pending',110,120);
      INSERT INTO monetary_ledger(id,organization_id,execution_id,kind,amount_microusd) VALUES(gen_random_uuid(),o,e,'hold',120);
    END $$"#).execute(&pool).await.unwrap();
    async fn history(pool: &PgPool) -> Vec<Value> {
        let mut history = Vec::new();
        for table in [
            "organizations",
            "organization_model_grants",
            "models",
            "deployment_prices",
            "inference_executions",
            "governance_reservations",
            "monetary_ledger",
        ] {
            history.push(
                sqlx::query_scalar(&format!(
                    "SELECT coalesce(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM {table} t"
                ))
                .fetch_one(pool)
                .await
                .unwrap(),
            );
        }
        history
    }
    let history_before = history(&pool).await;
    let objects_before: Vec<(String,String)> = sqlx::query_as("SELECT table_name,column_name FROM information_schema.columns WHERE table_schema='public' ORDER BY table_name,ordinal_position").fetch_all(&pool).await.unwrap();
    let rows_before: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(o)) FROM organizations o")
            .fetch_one(&pool)
            .await
            .unwrap();
    let store = Store::new(pool.clone());
    assert!(store.preflight_enterprise().await.is_err());
    assert!(store.migrate_enterprise().await.is_err());
    assert!(!store.is_ready().await);
    let objects_after: Vec<(String,String)> = sqlx::query_as("SELECT table_name,column_name FROM information_schema.columns WHERE table_schema='public' ORDER BY table_name,ordinal_position").fetch_all(&pool).await.unwrap();
    let rows_after: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(o)) FROM organizations o")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(objects_before, objects_after);
    assert_eq!(rows_before, rows_after);
    assert_eq!(history_before, history(&pool).await);
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT to_regclass('public.installation') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn shared_global_deployment_requires_each_workspace_grant_and_uses_pinned_global_price(
    pool: PgPool,
) {
    let (store, keys) = fixture(&pool).await;
    let first = store
        .authenticate(&keys.personal_key.token)
        .await
        .unwrap()
        .unwrap();
    let (deployment, model): (Uuid, Uuid) =
        sqlx::query_as("SELECT id,model_id FROM deployments LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let workspace = Uuid::new_v4();
    let user = first.user_id.unwrap();
    let key = NewApiKey::generate();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Consumer','project')")
        .bind(workspace)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'member','manual')").bind(workspace).bind(user).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'test',$4)").bind(key.id).bind(workspace).bind(user).bind(key.digest.as_slice()).execute(&pool).await.unwrap();
    let second = store.authenticate(&key.token).await.unwrap().unwrap();
    assert!(
        store
            .deployments(&second, "company/smart")
            .await
            .unwrap()
            .is_empty()
    );
    // Platform infrastructure is global, authorization is never inferred from model presence.
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')",
    )
    .bind(workspace)
    .bind(model)
    .execute(&pool)
    .await
    .unwrap();
    let target = store
        .deployments(&second, "company/smart")
        .await
        .unwrap()
        .remove(0);
    assert_eq!(target.id, deployment);
    assert_eq!(
        store.deployments(&first, "company/smart").await.unwrap()[0].id,
        deployment
    );
    let price = Uuid::new_v4();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,1000000,100,50,1)").bind(price).bind(deployment).execute(&pool).await.unwrap();
    let id = Uuid::new_v4();
    let record = ExecutionStart {
        id,
        root_request_id: id,
        attempt_number: 1,
        principal: second,
        deployment_id: deployment,
        provider: target.provider.clone(),
        model: "company/smart".into(),
        streamed: false,
        upstream_model: None,
        client: Default::default(),
    };
    let request = ChatRequest {
        model: "company/smart".into(),
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
    let reservation: (Uuid,Uuid,i64) = sqlx::query_as("SELECT workspace_id,price_id,held_microusd FROM governance_reservations WHERE execution_id=$1").bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(reservation, (workspace, price, 110));
    governance::finish(
        &store,
        &open_model_gateway::inference::repository::ExecutionFinish {
            id,
            outcome: open_model_gateway::inference::repository::Outcome::Succeeded,
            error: None,
            usage: open_model_gateway::inference::types::Usage {
                input_tokens: Some(2),
                output_tokens: Some(3),
                billing: None,
                ..Default::default()
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
    for statement in [
        "UPDATE deployment_prices SET input_token_limit=1",
        "DELETE FROM deployment_prices",
        "UPDATE monetary_ledger SET amount_microusd=0",
        "DELETE FROM monetary_ledger",
    ] {
        assert!(sqlx::query(statement).execute(&pool).await.is_err());
    }
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

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn global_models_have_no_implicit_access_and_public_names_are_unique(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    let principal = store
        .authenticate(&keys.personal_key.token)
        .await
        .unwrap()
        .unwrap();
    let model = Uuid::new_v4();
    sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,'global-unassigned',ARRAY['chat_completions'])").bind(model).execute(&pool).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM workspace_model_grants WHERE model_id=$1"
        )
        .bind(model)
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert!(
        !store
            .visible_models(&principal)
            .await
            .unwrap()
            .iter()
            .any(|m| m.id == "global-unassigned")
    );
    assert!(
        sqlx::query("INSERT INTO models(id,public_name) VALUES($1,'global-unassigned')")
            .bind(Uuid::new_v4())
            .execute(&pool)
            .await
            .is_err()
    );
    sqlx::query("UPDATE models SET enabled=false WHERE id=$1")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE models SET enabled=true WHERE id=$1")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM workspace_model_grants WHERE model_id=$1"
        )
        .bind(model)
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn catalog_defaults_are_live_and_empty_workspace_override_replaces_them(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    let principal = store
        .authenticate(&keys.personal_key.token)
        .await
        .unwrap()
        .unwrap();
    let model: Uuid = sqlx::query_scalar("SELECT id FROM models")
        .fetch_one(&pool)
        .await
        .unwrap();
    let admin = BrowserSession::new(&pool, principal.user_id.unwrap()).await;
    let app = management_app(store.clone()).await;
    let catalog = Uuid::new_v4();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,'Approved')")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) VALUES($1,$2)")
        .bind(catalog)
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1")
        .bind(keys.personal_workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    // SQL writes configure the fixture; GETs use the production authenticated management router.
    // No unconfigured-OIDC write bypass is invented or advertised as end-to-end sign-in.
    let available = format!(
        "/api/v1/workspaces/{}/available-models",
        keys.personal_workspace_id
    );
    assert_eq!(
        admin.get(&app, &available).await,
        (StatusCode::OK, json!({"data":[]}))
    );
    sqlx::query("INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES('personal',$1)")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    let (status, value) = admin.get(&app, &available).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["data"].as_array().unwrap().len(), 1);
    assert!(store.visible_models(&principal).await.unwrap().is_empty());
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'catalog')",
    )
    .bind(keys.personal_workspace_id)
    .bind(model)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(store.visible_models(&principal).await.unwrap().len(), 1);
    sqlx::query("INSERT INTO workspace_catalog_overrides(workspace_id) VALUES($1)")
        .bind(keys.personal_workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.visible_models(&principal).await.unwrap().is_empty());
    let defaults = format!(
        "/api/v1/platform/workspaces/{}/catalogs",
        keys.personal_workspace_id
    );
    let (status, value) = admin.get(&app, &defaults).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["mode"], "replace");
    assert_eq!(value["effective_catalog_ids"], json!([]));
    sqlx::query(
        "INSERT INTO workspace_catalog_override_items(workspace_id,catalog_id) VALUES($1,$2)",
    )
    .bind(keys.personal_workspace_id)
    .bind(catalog)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(store.visible_models(&principal).await.unwrap().len(), 1);
    sqlx::query("DELETE FROM workspace_type_catalogs WHERE kind='personal'")
        .execute(&pool)
        .await
        .unwrap();
    // An override is independent of its type defaults.
    assert_eq!(store.visible_models(&principal).await.unwrap().len(), 1);
    sqlx::query("DELETE FROM workspace_catalog_overrides WHERE workspace_id=$1")
        .bind(keys.personal_workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.visible_models(&principal).await.unwrap().is_empty());
    let (_, value) = admin.get(&app, &defaults).await;
    assert_eq!(value["mode"], "inherit");
    assert_eq!(value["effective_catalog_ids"], json!([]));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn catalog_access_and_key_lineage_are_live_intersections_not_permission_unions(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    let principal = store
        .authenticate(&keys.personal_key.token)
        .await
        .unwrap()
        .unwrap();
    let model: Uuid = sqlx::query_scalar("SELECT id FROM models")
        .fetch_one(&pool)
        .await
        .unwrap();
    let catalog = Uuid::new_v4();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,'Local')")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) VALUES($1,$2)")
        .bind(catalog)
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES('personal',$1)")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'catalog')",
    )
    .bind(principal.workspace_id)
    .bind(model)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1 AND source='direct'")
        .bind(principal.workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO key_model_restrictions(workspace_id,governance_key_id) VALUES($1,$2)")
        .bind(principal.workspace_id)
        .bind(principal.key_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.visible_models(&principal).await.unwrap().is_empty());
    sqlx::query("INSERT INTO key_model_selections(workspace_id,governance_key_id,model_id) VALUES($1,$2,$3)").bind(principal.workspace_id).bind(principal.key_id).bind(model).execute(&pool).await.unwrap();
    assert_eq!(store.visible_models(&principal).await.unwrap().len(), 1);
    let rotated = NewApiKey::generate();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,governance_key_id) VALUES($1,$2,$3,'rotated',$4,$5)").bind(rotated.id).bind(principal.workspace_id).bind(principal.user_id).bind(rotated.digest.as_slice()).bind(principal.key_id).execute(&pool).await.unwrap();
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(principal.key_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        store
            .authenticate(&keys.personal_key.token)
            .await
            .unwrap()
            .is_none()
    );
    let successor = store.authenticate(&rotated.token).await.unwrap().unwrap();
    assert_eq!(store.visible_models(&successor).await.unwrap().len(), 1);
    sqlx::query("DELETE FROM workspace_type_catalogs WHERE kind='personal'")
        .execute(&pool)
        .await
        .unwrap();
    // A retained catalog grant or key selection must not bypass current catalog availability.
    assert!(store.visible_models(&successor).await.unwrap().is_empty());
    assert!(
        store
            .deployments(&successor, "company/smart")
            .await
            .unwrap()
            .is_empty()
    );
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')",
    )
    .bind(successor.workspace_id)
    .bind(model)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(store.visible_models(&successor).await.unwrap().len(), 1);
    sqlx::query("DELETE FROM key_model_selections WHERE workspace_id=$1")
        .bind(successor.workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.visible_models(&successor).await.unwrap().is_empty());
    sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1 AND source='direct'")
        .bind(successor.workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES('personal',$1)")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    // Retired selection headers survive a catalog's return; inheritance is never resurrected.
    assert!(store.visible_models(&successor).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn management_rejects_inference_credentials_and_unentitled_sessions(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    let principal = store
        .authenticate(&keys.personal_key.token)
        .await
        .unwrap()
        .unwrap();
    let session = BrowserSession::new(&pool, principal.user_id.unwrap()).await;
    let app = management_app(store).await;
    assert_eq!(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/platform/catalogs")
                    .header(
                        "authorization",
                        format!("Bearer {}", keys.personal_key.token)
                    )
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        session.get(&app, "/api/v1/platform/catalogs").await.0,
        StatusCode::OK
    );
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        session.get(&app, "/api/v1/platform/catalogs").await.0,
        StatusCode::UNAUTHORIZED
    );
}

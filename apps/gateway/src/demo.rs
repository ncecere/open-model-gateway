//! Explicit, loopback-only fixtures for a fresh enterprise demo.
//! No upstream is enabled, no inference secret is printed and no usage is fabricated.
use crate::{config::Environment, store::Store};
use anyhow::{Context, Result, bail};
use sqlx::PgPool;
use uuid::Uuid;

const ISSUER: &str = "http://127.0.0.1:18084";
pub const DEMO_DATABASE: &str = "gateway_enterprise_demo";
fn id(n: u128) -> Uuid {
    Uuid::from_u128(0x8e210000_0000_4000_8000_000000000000_u128 + n)
}

pub async fn seed(store: &Store, environment: Environment) -> Result<bool> {
    if environment != Environment::Development {
        bail!("bootstrap-demo requires development mode");
    }
    store.preflight_enterprise().await?;
    ensure_database(&store.pool).await?;
    seed_data(store).await
}
async fn seed_data(store: &Store) -> Result<bool> {
    let mut tx = store.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(72419502)")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT lock_installation()")
        .execute(&mut *tx)
        .await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_events WHERE action='installation.bootstrap_demo') AND EXISTS(SELECT 1 FROM users WHERE lower(email)='operator@demo.invalid')").fetch_one(&mut *tx).await?;
    if exists {
        tx.commit().await?;
        return Ok(false);
    }
    let occupied: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users) OR EXISTS(SELECT 1 FROM workspaces) OR EXISTS(SELECT 1 FROM inference_executions)").fetch_one(&mut *tx).await?;
    anyhow::ensure!(
        !occupied,
        "Demo bootstrap requires an empty dedicated enterprise demo; existing identities/history are never replaced"
    );
    sqlx::query("UPDATE installation SET name='Open Model Gateway' WHERE singleton")
        .execute(&mut *tx)
        .await?;
    let users = [
        (id(1), "operator@demo.invalid"),
        (id(2), "auditor@demo.invalid"),
        (id(3), "alex@demo.invalid"),
        (id(4), "blair@demo.invalid"),
    ];
    for (user, email) in users {
        sqlx::query("INSERT INTO users(id,email,oidc_link_allowed) VALUES($1,$2,true)")
            .bind(user)
            .bind(email)
            .execute(&mut *tx)
            .await?;
    }
    let team = id(20);
    let project = id(21);
    for (ws, name, kind) in [(team, "Product", "team"), (project, "Research", "project")] {
        sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,$2,$3)")
            .bind(ws)
            .bind(name)
            .bind(kind)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO workspace_membership_grants(id,workspace_id,user_id,role,source) VALUES($1,$2,$3,'owner','manual')").bind(Uuid::new_v4()).bind(ws).bind(id(3)).execute(&mut *tx).await?;
    }
    // Signed ID-token groups confer explicit platform entitlement. A matching
    // team-only group can never substitute for a platform-role grant.
    for (mapping, group, role) in [
        (id(30), "omg/platform-admin", "admin"),
        (id(31), "omg/platform-auditor", "auditor"),
        (id(32), "omg/platform-user", "user"),
    ] {
        sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,$2,$3,'platform',$4)").bind(mapping).bind(ISSUER).bind(group).bind(role).execute(&mut *tx).await?;
    }
    for (mapping, group, ws, role) in [
        (id(33), "omg/product-admin", team, "admin"),
        (id(34), "omg/product-member", team, "member"),
        (id(35), "omg/research-member", project, "member"),
    ] {
        sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,workspace_id,workspace_role) VALUES($1,$2,$3,'workspace',$4,$5)").bind(mapping).bind(ISSUER).bind(group).bind(ws).bind(role).execute(&mut *tx).await?;
    }
    let local = id(40);
    let cloud = id(41);
    let advanced = id(42);
    for (cat, name, description) in [
        (
            local,
            "Local datacenter",
            "Local model examples; operators configure their actual servers",
        ),
        (
            cloud,
            "Approved cloud",
            "Illustrative self-service cloud catalog",
        ),
        (
            advanced,
            "Advanced cloud",
            "Restricted model examples for explicit catalog availability or direct assignment",
        ),
    ] {
        sqlx::query("INSERT INTO catalogs(id,name,description) VALUES($1,$2,$3)")
            .bind(cat)
            .bind(name)
            .bind(description)
            .execute(&mut *tx)
            .await?;
    }
    for (kind, catalogs) in [
        ("personal", vec![local]),
        ("team", vec![local, cloud]),
        ("project", vec![local, cloud]),
    ] {
        for cat in catalogs {
            sqlx::query("INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES($1,$2)")
                .bind(kind)
                .bind(cat)
                .execute(&mut *tx)
                .await?;
        }
    }
    sqlx::query("INSERT INTO workspace_catalog_overrides(workspace_id) VALUES($1)")
        .bind(project)
        .execute(&mut *tx)
        .await?;
    for cat in [local, advanced] {
        sqlx::query(
            "INSERT INTO workspace_catalog_override_items(workspace_id,catalog_id) VALUES($1,$2)",
        )
        .bind(project)
        .bind(cat)
        .execute(&mut *tx)
        .await?;
    }
    for (kind, budget) in [
        ("personal", 25_000_000_i64),
        ("team", 100_000_000),
        ("project", 100_000_000),
    ] {
        sqlx::query("INSERT INTO workspace_type_policies(kind,requests_per_minute,tokens_per_minute,concurrent_requests) VALUES($1,60,100000,8) ON CONFLICT(kind) DO UPDATE SET requests_per_minute=EXCLUDED.requests_per_minute,tokens_per_minute=EXCLUDED.tokens_per_minute,concurrent_requests=EXCLUDED.concurrent_requests").bind(kind).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO policy_budgets(layer,kind,period,amount_microusd) VALUES('type',$1,'month',$2)").bind(kind).bind(budget).execute(&mut *tx).await?;
    }
    // No installation-wide budget is enabled implicitly.
    sqlx::query("INSERT INTO installation_policy(singleton) VALUES(true)")
        .execute(&mut *tx)
        .await?;
    let local_provider = id(50);
    let cloud_provider = id(51);
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,'Local compatible — disabled example','openai_compatible','none','http://127.0.0.1:19091/v1',false)").bind(local_provider).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'OpenAI — disabled example','openai','env:OPENAI_API_KEY',false)").bind(cloud_provider).execute(&mut *tx).await?;
    let pricing = serde_json::json!({"read":{"status":"priced","microusd_per_million":"100000"},"write":{"status":"priced","microusd_per_million":"1250000"},"write_5m":{"status":"priced","microusd_per_million":"1250000"},"write_1h":{"status":"priced","microusd_per_million":"2000000"}});
    for (model, alias, protocols, provider, upstream, cat, output) in [
        (
            id(60),
            "demo/local-chat",
            vec!["chat_completions"],
            local_provider,
            "configure-your-chat-model",
            local,
            1024_i64,
        ),
        (
            id(61),
            "demo/local-embedding",
            vec!["embeddings"],
            local_provider,
            "configure-your-embedding-model",
            local,
            0,
        ),
        (
            id(62),
            "demo/cloud-smart",
            vec!["chat_completions", "responses"],
            cloud_provider,
            "configure-your-cloud-model",
            cloud,
            1024,
        ),
        (
            id(63),
            "demo/cloud-restricted",
            vec!["chat_completions", "responses"],
            cloud_provider,
            "configure-your-restricted-model",
            advanced,
            1024,
        ),
    ] {
        sqlx::query("INSERT INTO models(id,public_name,display_name,description,supported_protocols) VALUES($1,$2,$2,'Example configuration and prices, not vendor rates',$3)").bind(model).bind(alias).bind(protocols).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) VALUES($1,$2)")
            .bind(cat)
            .bind(model)
            .execute(&mut *tx)
            .await?;
        let deployment = Uuid::new_v4();
        sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,$4,false)").bind(deployment).bind(model).bind(provider).bind(upstream).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,cache_pricing) VALUES($1,$2,1000000,3000000,8192,$3,2,$4)").bind(Uuid::new_v4()).bind(deployment).bind(output).bind(&pricing).execute(&mut *tx).await?;
    }
    for (ws, model) in [
        (team, id(60)),
        (team, id(61)),
        (team, id(62)),
        (project, id(60)),
        (project, id(63)),
    ] {
        sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'catalog')").bind(ws).bind(model).execute(&mut *tx).await?;
    }
    // Optional attribution: Product has a cost center; Research stays Unallocated.
    sqlx::query("INSERT INTO cost_centers(id,name,code) VALUES($1,'Product department','PRODUCT')")
        .bind(id(70))
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE workspaces SET cost_center_id=$1 WHERE id=$2")
        .bind(id(70))
        .bind(team)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'Product automation')",
    )
    .bind(id(80))
    .bind(team)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO audit_events(id,action,resource_type,resource_id,metadata) SELECT $1,'installation.bootstrap_demo','installation',id,'{\"sample_providers_disabled\":true,\"usage_fabricated\":false}'::jsonb FROM installation WHERE singleton").bind(Uuid::new_v4()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}
async fn ensure_database(pool: &PgPool) -> Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(pool)
        .await
        .context("Cannot verify demo database")?;
    anyhow::ensure!(
        database == DEMO_DATABASE,
        "Demo bootstrap requires the dedicated gateway_enterprise_demo database; existing databases are never reset"
    );
    Ok(())
}
// Kept only as an explicit CLI error, never an upgrade of a legacy demo.
pub async fn add_missing_personas(_store: &Store, _environment: Environment) -> Result<bool> {
    bail!("Legacy persona upgrades are unsupported; initialize a fresh enterprise demo instead")
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "demo/tests.rs"]
mod tests;

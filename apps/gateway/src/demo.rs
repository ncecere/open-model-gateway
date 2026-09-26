//! Explicit, local-only demo provisioning. Login still uses the normal OIDC flow.
//! No identity, session, execution, usage, or audit history is fabricated here.

use anyhow::{Context, Result, ensure};
use uuid::Uuid;

use crate::{auth::NewApiKey, config::Environment, store::Store};

fn validate_target(environment: Environment, database: &str) -> Result<()> {
    ensure!(
        environment == Environment::Development,
        "bootstrap-demo requires GATEWAY_ENV=development"
    );
    ensure!(
        database == "gateway_demo",
        "bootstrap-demo requires the database name gateway_demo exactly"
    );
    Ok(())
}

async fn validate_store(store: &Store, environment: Environment) -> Result<()> {
    // Reject production before opening a connection, including on repeated runs.
    ensure!(
        environment == Environment::Development,
        "bootstrap-demo requires GATEWAY_ENV=development"
    );
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&store.pool)
        .await?;
    validate_target(environment, &database)
}

/// Seed once, without resetting later edits or rotating/reviving keys.
/// The CLI must additionally require a loopback database host.
/// Returns true when created, false when the demo organization already exists.
pub async fn seed(store: &Store, environment: Environment) -> Result<bool> {
    validate_store(store, environment).await?;
    seed_inner(store).await
}

// Private so only SQLx tests can bypass the exact database-name guard.
async fn seed_inner(store: &Store) -> Result<bool> {
    let mut tx = store.pool.begin().await?;
    // Share the development bootstrap lock and serialize concurrent demo runs.
    sqlx::query("SELECT pg_advisory_xact_lock(72419501)")
        .execute(&mut *tx)
        .await?;
    // Also exclude unrelated provisioning between the empty-database check and inserts.
    sqlx::query("LOCK TABLE organizations, users IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM organizations WHERE slug = 'gateway-demo')",
    )
    .fetch_one(&mut *tx)
    .await?;
    if exists {
        tx.commit().await?;
        return Ok(false);
    }
    let occupied: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM organizations) OR EXISTS (SELECT 1 FROM users)",
    )
    .fetch_one(&mut *tx)
    .await?;
    // Never adopt existing accounts, including accounts using these demo email addresses.
    ensure!(
        !occupied,
        "bootstrap-demo requires an empty database with no organizations or users on first seed"
    );

    let organization = Uuid::new_v4();
    let product = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations(id,slug,name) VALUES($1,'gateway-demo','Gateway demo')")
        .bind(organization)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO workspaces(id,organization_id,name,kind) VALUES($1,$2,'Product','team')",
    )
    .bind(product)
    .bind(organization)
    .execute(&mut *tx)
    .await?;

    let mut personal_workspaces = Vec::new();
    for persona in [
        ("operator@demo.invalid", "Operator", true, "owner", "owner"),
        ORGADMIN,
        ("alex@demo.invalid", "Alex", false, "member", "admin"),
        ("blair@demo.invalid", "Blair", false, "member", "member"),
    ] {
        personal_workspaces
            .push(insert_persona(&mut tx, organization, Some(product), persona).await?);
    }
    sqlx::query("INSERT INTO service_accounts(id,organization_id,workspace_id,name) VALUES($1,$2,$3,'Product demo automation (no key)')")
        .bind(Uuid::new_v4()).bind(organization).bind(product).execute(&mut *tx).await?;

    let openai = Uuid::new_v4();
    let anthropic = Uuid::new_v4();
    // Fixed, syntactically valid environment references, never credentials or custom URLs.
    // Disabled configurations do not resolve secrets or require API keys to be present.
    // Actual inference still requires the normal secret allowlist and operator configuration.
    for (id, provider, reference, name) in [
        (
            openai,
            "openai",
            "env:OPENAI_API_KEY",
            "Demo OpenAI (configured example prices)",
        ),
        (
            anthropic,
            "anthropic",
            "env:ANTHROPIC_API_KEY",
            "Demo Anthropic (configured example prices)",
        ),
    ] {
        sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,$2,$3,$4,false)")
            .bind(id).bind(name).bind(provider).bind(reference)
            .execute(&mut *tx).await?;
    }

    for (alias, display, openai_model, anthropic_model, input_price, output_price) in [
        (
            "demo/smart",
            "Demo smart — example prices, not vendor rates",
            "gpt-4.1",
            "claude-sonnet-4-20250514",
            1_000_000_i64,
            3_000_000_i64,
        ),
        (
            "demo/fast",
            "Demo fast — example prices, not vendor rates",
            "gpt-4.1-mini",
            "claude-3-5-haiku-20241022",
            250_000_i64,
            1_000_000_i64,
        ),
    ] {
        let model = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO models(id,public_name,display_name,enabled) VALUES($1,$2,$3,true)",
        )
        .bind(model)
        .bind(alias)
        .bind(display)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO organization_model_grants(organization_id,model_id,public_name,personal_enabled) VALUES($1,$2,$3,true)")
            .bind(organization).bind(model).bind(alias).execute(&mut *tx).await?;
        for workspace in personal_workspaces.iter().copied().chain([product]) {
            sqlx::query("INSERT INTO workspace_model_grants(organization_id,workspace_id,model_id) VALUES($1,$2,$3)")
                .bind(organization).bind(workspace).bind(model).execute(&mut *tx).await?;
        }
        for (provider, upstream) in [(openai, openai_model), (anthropic, anthropic_model)] {
            let deployment = Uuid::new_v4();
            sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,$4,false)")
                .bind(deployment).bind(model).bind(provider).bind(upstream)
                .execute(&mut *tx).await?;
            // Immutable configured examples only. These numbers are NOT vendor pricing,
            // model capability claims, historical spend, or measured usage.
            sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit) VALUES($1,$2,$3,$4,8192,1024)")
                .bind(Uuid::new_v4()).bind(deployment)
                .bind(input_price).bind(output_price).execute(&mut *tx).await?;
        }
    }

    // Platform ceilings cannot be removed by the organization administrator.
    sqlx::query("INSERT INTO platform_organization_policies(organization_id,requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd) VALUES($1,120,120000,8,50000000)")
        .bind(organization).execute(&mut *tx).await?;
    // Conservative illustrative limits: $50 organization, $20 Product, $5 personal.
    // These are configured caps, not credits or incurred costs.
    sqlx::query("INSERT INTO governance_policies(id,organization_id,scope,requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd) VALUES($1,$2,'organization',120,120000,8,50000000)")
        .bind(Uuid::new_v4()).bind(organization).execute(&mut *tx).await?;
    for workspace in personal_workspaces.iter().copied().chain([product]) {
        let budget = if workspace == product {
            20_000_000_i64
        } else {
            5_000_000_i64
        };
        sqlx::query("INSERT INTO governance_policies(id,organization_id,scope,workspace_id,requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd) VALUES($1,$2,'workspace',$3,30,30000,2,$4)")
            .bind(Uuid::new_v4()).bind(organization).bind(workspace).bind(budget)
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(true)
}

// Email, display name, platform privilege, organization role, team role.
type Persona = (&'static str, &'static str, bool, &'static str, &'static str);
const ORGADMIN: Persona = (
    "orgadmin@demo.invalid",
    "Organization Admin",
    false,
    "admin",
    "member",
);

// Only inserts fresh identities; callers hold the provisioning and users-table locks.
async fn insert_persona(
    tx: &mut sqlx::PgConnection,
    organization: Uuid,
    team: Option<Uuid>,
    (email, name, platform_admin, organization_role, team_role): Persona,
) -> Result<Uuid> {
    let user = Uuid::new_v4();
    let personal = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users(id,email,platform_admin,oidc_link_allowed) VALUES($1,$2,$3,true)",
    )
    .bind(user)
    .bind(email)
    .bind(platform_admin)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,$3)",
    )
    .bind(organization)
    .bind(user)
    .bind(organization_role)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO workspaces(id,organization_id,name,kind,owner_user_id) VALUES($1,$2,$3,'personal',$4)")
        .bind(personal).bind(organization).bind(format!("{name}'s personal workspace"))
        .bind(user).execute(&mut *tx).await?;
    // Personal ownership alone grants access; never add other users as members.
    if let Some(team) = team {
        sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,$4)")
            .bind(organization).bind(team).bind(user).bind(team_role).execute(&mut *tx).await?;
    }
    for (workspace, key_name) in [(personal, "Demo personal key (secret discarded)")]
        .into_iter()
        .chain(team.map(|id| (id, "Demo team key (secret discarded)")))
    {
        // Only the digest is persisted. The plaintext is neither returned nor printed.
        let NewApiKey {
            id,
            digest,
            token: _,
        } = NewApiKey::generate();
        sqlx::query("INSERT INTO api_keys(id,organization_id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(id).bind(organization).bind(workspace).bind(user).bind(key_name)
            .bind(digest.as_slice()).execute(&mut *tx).await?;
    }
    Ok(personal)
}

/// Explicit additive upgrade only; never adopt, revive, relink or promote an existing email.
/// Requires an active gateway-demo organization and the same local-only CLI guards as seed.
/// Returns false if the email already exists (case-insensitive), regardless of its state.
pub async fn add_missing_personas(store: &Store, environment: Environment) -> Result<bool> {
    validate_store(store, environment).await?;
    add_missing_personas_inner(store).await
}

// Private so only SQLx tests can bypass the exact database-name guard.
async fn add_missing_personas_inner(store: &Store) -> Result<bool> {
    let mut tx = store.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(72419501)")
        .execute(&mut *tx)
        .await?;
    // Same order as seed. Serialize email checks against ordinary user provisioning too.
    sqlx::query("LOCK TABLE organizations, users IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let organization: Uuid = sqlx::query_scalar(
        "SELECT id FROM organizations WHERE slug='gateway-demo' AND disabled_at IS NULL FOR UPDATE",
    )
    .fetch_optional(&mut *tx)
    .await?
    .context(
        "adding demo personas requires an active gateway-demo organization; seed first if missing",
    )?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE lower(email)=lower($1))")
            .bind(ORGADMIN.0)
            .fetch_one(&mut *tx)
            .await?;
    if exists {
        tx.commit().await?;
        return Ok(false);
    }
    // Prefer Product, but respect renames and disabled teams. Never modify existing teams.
    let team: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM workspaces WHERE organization_id=$1 AND kind='team' AND disabled_at IS NULL ORDER BY (name='Product') DESC,created_at,id LIMIT 1 FOR UPDATE",
    ).bind(organization).fetch_optional(&mut *tx).await?;
    let personal = insert_persona(&mut tx, organization, team, ORGADMIN).await?;
    // Only the new private workspace gets grants/policy. Existing catalog and grants stay intact.
    sqlx::query("INSERT INTO workspace_model_grants(organization_id,workspace_id,model_id) SELECT $1,$2,g.model_id FROM organization_model_grants g JOIN models m ON m.id=g.model_id WHERE g.organization_id=$1 AND m.enabled AND g.personal_enabled")
        .bind(organization).bind(personal).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO governance_policies(id,organization_id,scope,workspace_id,requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd) VALUES($1,$2,'workspace',$3,30,30000,2,5000000)")
        .bind(Uuid::new_v4()).bind(organization).bind(personal).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_and_inexact_database_names_are_rejected() {
        assert!(validate_target(Environment::Development, "gateway_demo").is_ok());
        assert!(validate_target(Environment::Production, "gateway_demo").is_err());
        for name in [
            "",
            "gateway",
            "postgres",
            "gateway_demo_test",
            "Gateway_demo",
            "gateway_demo ",
            "_sqlx_test",
        ] {
            assert!(validate_target(Environment::Development, name).is_err());
        }
    }
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "demo/tests.rs"]
mod integration_tests;

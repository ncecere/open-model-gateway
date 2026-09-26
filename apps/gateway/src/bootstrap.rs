use anyhow::{Result, bail};
use uuid::Uuid;

use crate::{auth::NewApiKey, config::Environment, store::Store};

/// Printed once by the explicit development CLI; never sent to structured logs.
pub struct DevelopmentKeys {
    pub organization_id: Uuid,
    pub personal_workspace_id: Uuid,
    pub team_workspace_id: Uuid,
    pub personal_key: NewApiKey,
    pub team_key: NewApiKey,
}

/// Trusted operator CLI only. Never expose email linking or operator grants to an unauthenticated API.
pub async fn provision_user(store: &Store, email: &str, platform_admin: bool) -> Result<Uuid> {
    let email = email.trim().to_lowercase();
    anyhow::ensure!(
        email.len() <= 320
            && email.contains('@')
            && !email.chars().any(|c| c.is_whitespace() || c.is_control()),
        "invalid email"
    );
    let id=sqlx::query_scalar("INSERT INTO users(id,email,platform_admin,oidc_link_allowed) VALUES($1,$2,$3,true) ON CONFLICT(lower(email)) DO UPDATE SET platform_admin=users.platform_admin OR EXCLUDED.platform_admin, oidc_link_allowed=NOT EXISTS(SELECT 1 FROM oidc_identities i WHERE i.user_id=users.id) RETURNING id")
        .bind(Uuid::new_v4()).bind(email).bind(platform_admin).fetch_one(&store.pool).await?;
    Ok(id)
}

pub async fn seed(store: &Store, environment: Environment) -> Result<Option<DevelopmentKeys>> {
    if environment != Environment::Development {
        bail!("bootstrap-dev requires GATEWAY_ENV=development");
    }
    let mut tx = store.pool.begin().await?;
    // Serialize repeated/concurrent bootstrap invocations without rotating existing keys.
    sqlx::query("SELECT pg_advisory_xact_lock(72419501)")
        .execute(&mut *tx)
        .await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM organizations WHERE slug = 'local-dev')")
            .fetch_one(&mut *tx)
            .await?;
    if exists {
        tx.commit().await?;
        return Ok(None);
    }
    let organization_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let personal_workspace_id = Uuid::new_v4();
    let team_workspace_id = Uuid::new_v4();
    let provider_id = Uuid::new_v4();
    let model_id = Uuid::new_v4();
    let personal_key = NewApiKey::generate();
    let team_key = NewApiKey::generate();

    sqlx::query(
        "INSERT INTO organizations (id, slug, name) VALUES ($1, 'local-dev', 'Local development')",
    )
    .bind(organization_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, 'developer@local.invalid')")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO organization_memberships (organization_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(organization_id).bind(user_id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO workspaces (id, organization_id, name, kind, owner_user_id) VALUES ($1, $2, 'Personal', 'personal', $3)")
        .bind(personal_workspace_id).bind(organization_id).bind(user_id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO workspaces (id, organization_id, name, kind) VALUES ($1, $2, 'Engineering', 'team')")
        .bind(team_workspace_id).bind(organization_id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO workspace_memberships (organization_id, workspace_id, user_id, role) VALUES ($1, $2, $3, 'owner')")
        .bind(organization_id).bind(team_workspace_id).bind(user_id).execute(&mut *tx).await?;
    for (workspace, key, name) in [
        (personal_workspace_id, &personal_key, "Local personal key"),
        (team_workspace_id, &team_key, "Local team key"),
    ] {
        sqlx::query("INSERT INTO api_keys (id, organization_id, workspace_id, issued_to_user_id, name, secret_hash) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(key.id).bind(organization_id).bind(workspace).bind(user_id).bind(name)
            .bind(key.digest.as_slice()).execute(&mut *tx).await?;
    }

    // A disabled configuration example, not an operational provider.
    sqlx::query("INSERT INTO provider_connections (id, organization_id, name, provider, credential_ref) VALUES ($1, $2, 'Example OpenAI', 'openai', 'env:OPENAI_API_KEY')")
        .bind(provider_id).bind(organization_id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO models (id, organization_id, public_name, display_name, enabled) VALUES ($1, $2, 'company/smart', 'Smart model', true)")
        .bind(model_id).bind(organization_id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO deployments (id, organization_id, model_id, provider_connection_id, upstream_model) VALUES ($1, $2, $3, $4, 'gpt-4.1')")
        .bind(Uuid::new_v4()).bind(organization_id).bind(model_id).bind(provider_id).execute(&mut *tx).await?;
    for workspace in [personal_workspace_id, team_workspace_id] {
        sqlx::query("INSERT INTO workspace_model_grants (organization_id, workspace_id, model_id) VALUES ($1, $2, $3)")
            .bind(organization_id).bind(workspace).bind(model_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Some(DevelopmentKeys {
        organization_id,
        personal_workspace_id,
        team_workspace_id,
        personal_key,
        team_key,
    }))
}

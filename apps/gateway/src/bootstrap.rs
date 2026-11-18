use crate::{auth::NewApiKey, config::Environment, lifecycle, store::Store};
use anyhow::Result;
use uuid::Uuid;

/// Printed once by the explicit development CLI; never sent to structured logs.
pub struct DevelopmentKeys {
    pub installation_id: Uuid,
    pub personal_workspace_id: Uuid,
    pub team_workspace_id: Uuid,
    pub personal_key: NewApiKey,
    pub team_key: NewApiKey,
}

/// Explicit trusted operator bootstrap/recovery. Grants are auditable; email linking is one-use.
pub async fn provision_user(store: &Store, email: &str, platform_admin: bool) -> Result<Uuid> {
    let email = email.trim().to_lowercase();
    anyhow::ensure!(
        crate::text::chars_at_most(&email, 320)
            && email.contains('@')
            && !email.chars().any(|c| c.is_whitespace() || c.is_control()),
        "invalid email"
    );
    let mut tx = crate::db::begin(&store.pool).await?;
    lifecycle::lock(&mut tx).await?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO users(id,email,oidc_link_allowed) VALUES($1,$2,true) ON CONFLICT(lower(email)) DO UPDATE SET oidc_link_allowed=NOT EXISTS(SELECT 1 FROM oidc_identities i WHERE i.user_id=users.id) RETURNING id")
        .bind(Uuid::new_v4()).bind(email).fetch_one(&mut *tx).await?;
    let active: bool = sqlx::query_scalar(
        "SELECT disabled_at IS NULL AND cleaned_at IS NULL FROM users WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(
        active,
        "Account is suspended; reactivate through audited platform management first"
    );
    let role = if platform_admin { "admin" } else { "user" };
    let inserted = sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,$3,'bootstrap') ON CONFLICT DO NOTHING")
        .bind(Uuid::new_v4()).bind(id).bind(role).execute(&mut *tx).await?;
    if inserted.rows_affected() > 0 {
        sqlx::query("INSERT INTO audit_events(id,action,resource_type,resource_id,metadata) VALUES($1,'user.bootstrap_role','user',$2,jsonb_build_object('role',$3::text))")
            .bind(Uuid::new_v4()).bind(id).bind(role).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(id)
}

/// Development fixtures require an explicitly local database, before any connection.
pub fn ensure_dev_config(
    environment: Environment,
    options: &sqlx::postgres::PgConnectOptions,
) -> Result<()> {
    anyhow::ensure!(
        environment == Environment::Development,
        "bootstrap-dev requires GATEWAY_ENV=development"
    );
    anyhow::ensure!(
        options.get_socket().is_none()
            && matches!(options.get_host(), "127.0.0.1" | "::1" | "[::1]"),
        "bootstrap-dev requires a loopback database (127.0.0.1 or ::1)"
    );
    Ok(())
}

pub(crate) async fn ensure_empty(tx: &mut sqlx::PgConnection) -> Result<()> {
    let occupied: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users) OR EXISTS(SELECT 1 FROM workspaces) OR EXISTS(SELECT 1 FROM inference_executions)").fetch_one(&mut *tx).await?;
    anyhow::ensure!(
        !occupied,
        "Local bootstrap requires an empty dedicated installation; existing identities/history are never replaced"
    );
    Ok(())
}

pub async fn seed(store: &Store, environment: Environment) -> Result<Option<DevelopmentKeys>> {
    ensure_dev_config(environment, &store.pool.connect_options())?;
    let mut tx = crate::db::begin(&store.pool).await?;
    // Seeds the global catalog (providers, models, deployments): the
    // exclusive catalog lock, then the installation row.
    sqlx::query("SELECT pg_advisory_xact_lock(72419502)")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT lock_installation()")
        .execute(&mut *tx)
        .await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM audit_events WHERE action='installation.bootstrap_dev') AND EXISTS(SELECT 1 FROM users WHERE email='developer@local.invalid')",
    )
    .fetch_one(&mut *tx)
    .await?;
    if exists {
        tx.commit().await?;
        return Ok(None);
    }
    ensure_empty(&mut tx).await?;
    let installation_id: Uuid = sqlx::query_scalar("SELECT id FROM installation WHERE singleton")
        .fetch_one(&mut *tx)
        .await?;
    let user_id = Uuid::new_v4();
    let personal_workspace_id = Uuid::new_v4();
    let team_workspace_id = Uuid::new_v4();
    let provider_id = Uuid::new_v4();
    let model_id = Uuid::new_v4();
    let personal_key = NewApiKey::generate()?;
    let team_key = NewApiKey::generate()?;
    sqlx::query(
        "INSERT INTO users(id,email,oidc_link_allowed) VALUES($1,'developer@local.invalid',true)",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'admin','bootstrap')")
        .bind(Uuid::new_v4()).bind(user_id).execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,'Personal','personal',$2)",
    )
    .bind(personal_workspace_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Engineering','team')")
        .bind(team_workspace_id)
        .execute(&mut *tx)
        .await?;
    for ws in [personal_workspace_id, team_workspace_id] {
        sqlx::query("INSERT INTO workspace_membership_grants(id,workspace_id,user_id,role,source) VALUES($1,$2,$3,'owner','manual')")
            .bind(Uuid::new_v4()).bind(ws).bind(user_id).execute(&mut *tx).await?;
    }
    for (ws, key, name) in [
        (personal_workspace_id, &personal_key, "Local personal key"),
        (team_workspace_id, &team_key, "Local team key"),
    ] {
        sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,$4,$5)")
            .bind(key.id).bind(ws).bind(user_id).bind(name).bind(key.digest.as_slice()).execute(&mut *tx).await?;
    }
    // Disabled example only: bootstrap never makes an upstream call.
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Example OpenAI','openai','env:OPENAI_API_KEY')")
        .bind(provider_id).execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO models(id,public_name,description) VALUES($1,'company/smart','Smart model')",
    )
    .bind(model_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'gpt-4.1')")
        .bind(Uuid::new_v4()).bind(model_id).bind(provider_id).execute(&mut *tx).await?;
    for ws in [personal_workspace_id, team_workspace_id] {
        sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')")
            .bind(ws).bind(model_id).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id) VALUES($1,$2,'installation.bootstrap_dev','installation',$3)")
        .bind(Uuid::new_v4()).bind(user_id).bind(installation_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(DevelopmentKeys {
        installation_id,
        personal_workspace_id,
        team_workspace_id,
        personal_key,
        team_key,
    }))
}

#[cfg(test)]
mod config_tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn development_bootstrap_accepts_only_explicit_loopback_options() {
        for url in ["postgres://user@127.0.0.1/db", "postgres://user@[::1]/db"] {
            let options = sqlx::postgres::PgConnectOptions::from_str(url).unwrap();
            assert!(ensure_dev_config(Environment::Development, &options).is_ok());
            assert!(ensure_dev_config(Environment::Production, &options).is_err());
        }
        for url in [
            "postgres://user@example.invalid/db",
            "postgres://user@localhost/db",
            "postgres://user@127.0.0.1/db?host=/unused-socket-directory",
            "postgres://user@127.0.0.1/db?host=example.invalid",
            "postgres://user@127.0.0.1/db?hostaddr=192.0.2.1",
        ] {
            let options = sqlx::postgres::PgConnectOptions::from_str(url).unwrap();
            assert!(ensure_dev_config(Environment::Development, &options).is_err());
        }
    }
}

#[cfg(all(test, feature = "integration-tests"))]
mod tests {
    use super::*;
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn operator_grants_are_explicit_audited_idempotent_and_do_not_create_personal_workspaces(
        pool: sqlx::PgPool,
    ) {
        let store = Store::new(pool.clone());
        let id = provision_user(&store, "  Operator@Example.test ", false)
            .await
            .unwrap();
        assert_eq!(
            id,
            provision_user(&store, "operator@example.test", false)
                .await
                .unwrap()
        );
        let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM platform_role_grants),(SELECT count(*) FROM audit_events),(SELECT count(*) FROM workspaces)").fetch_one(&pool).await.unwrap();
        assert_eq!(counts, (1, 1, 0));
        assert_eq!(
            id,
            provision_user(&store, "operator@example.test", true)
                .await
                .unwrap()
        );
        let role: String =
            sqlx::query_scalar("SELECT role FROM effective_platform_roles WHERE user_id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(role, "admin");
        sqlx::query(
            "UPDATE users SET disabled_at=now(),disable_reason='admin_suspension' WHERE id=$1",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            provision_user(&store, "operator@example.test", true)
                .await
                .is_err()
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn dev_bootstrap_never_adopts_existing_developer_email(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let email = "developer@local.invalid";
        sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
            .bind(Uuid::new_v4())
            .bind(email)
            .execute(&pool)
            .await
            .unwrap();
        let error = seed(&store, Environment::Development).await.err().unwrap();
        assert!(error.to_string().contains("requires an empty"));
        let counts: (i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM api_keys),(SELECT count(*) FROM platform_role_grants),(SELECT count(*) FROM audit_events)").fetch_one(&pool).await.unwrap();
        assert_eq!(counts, (0, 0, 0));
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn dev_bootstrap_is_environment_gated_and_never_rotates_existing_credentials(
        pool: sqlx::PgPool,
    ) {
        let store = Store::new(pool.clone());
        assert!(seed(&store, Environment::Production).await.is_err());
        let initial = seed(&store, Environment::Development)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .authenticate(&initial.personal_key.token)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .authenticate(&initial.team_key.token)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            seed(&store, Environment::Development)
                .await
                .unwrap()
                .is_none()
        );
        let keys: i64 = sqlx::query_scalar("SELECT count(*) FROM api_keys")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(keys, 2);
        let enabled: i64 =
            sqlx::query_scalar("SELECT count(*) FROM provider_connections WHERE enabled")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(enabled, 0);
    }
}

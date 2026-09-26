use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use open_model_gateway::{
    bootstrap,
    config::Config,
    http,
    identity::{IdentityConfig, IdentityState},
    inference::Engine,
    providers::{
        ProviderRegistry, anthropic::AnthropicAdapter, bedrock::BedrockAdapter,
        openai::OpenAiAdapter, secrets::EnvSecrets,
    },
    store::{MIGRATOR, Store},
    web::WebAssets,
};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "Multi-tenant model gateway")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the gateway HTTP server (default).
    Serve,
    /// Apply database migrations explicitly, before starting replicas.
    Migrate,
    /// Provision local development workspaces and print new keys once.
    BootstrapDev,
    /// Seed fixed local-demo personas and disabled sample configuration in gateway_demo only.
    BootstrapDemo {
        /// Add only missing new personas to an active demo; never reset existing identities.
        #[arg(long)]
        add_missing_personas: bool,
    },
    /// Close a bounded batch of expired executions without refunding unknown cost.
    ReconcileExecutions {
        #[arg(long, default_value_t = 100)]
        limit: i64,
    },
    /// Compact settled execution display metadata; retain pricing, usage and ledger.
    CompactHistory {
        #[arg(long)]
        older_than_days: i32,
        #[arg(long, default_value_t = 1000)]
        limit: i64,
    },
    /// Explicitly provision an email for first OIDC linking; requires trusted database access.
    ProvisionUser {
        #[arg(long)]
        email: String,
        #[arg(long)]
        platform_admin: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("open_model_gateway=info")),
        )
        .init();
    let config = Config::from_env()?;
    if matches!(cli.command, Some(Command::BootstrapDemo { .. }))
        || std::env::var("GATEWAY_OIDC_ISSUER").as_deref() == Ok("http://127.0.0.1:18084")
    {
        ensure_demo_config(&config)?;
    }
    let pool = config.connect().await?;
    let store = Store::new(pool.clone());
    match cli.command.unwrap_or(Command::Serve) {
        Command::Migrate => {
            MIGRATOR
                .run(&pool)
                .await
                .context("database migration failed")?;
            println!("Migrations applied.");
        }
        Command::BootstrapDev => match bootstrap::seed(&store, config.environment).await? {
            Some(keys) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "notice": "Development keys: save these now. They cannot be retrieved again.",
                        "organization_id": keys.organization_id,
                        "personal_workspace_id": keys.personal_workspace_id,
                        "team_workspace_id": keys.team_workspace_id,
                        "personal_api_key": keys.personal_key.token,
                        "team_api_key": keys.team_key.token,
                    }))?
                );
            }
            None => println!(
                "Local development organization already exists. No changes or key rotation performed."
            ),
        },
        Command::BootstrapDemo {
            add_missing_personas,
        } => {
            if add_missing_personas {
                let created =
                    open_model_gateway::demo::add_missing_personas(&store, config.environment)
                        .await?;
                println!(
                    "{}",
                    if created {
                        "Missing Organization Admin persona added; existing data unchanged. No inference tokens were printed."
                    } else {
                        "Organization Admin email already exists; no privileges, linking state, or other data changed."
                    }
                );
            } else {
                let created = open_model_gateway::demo::seed(&store, config.environment).await?;
                println!(
                    "{}",
                    if created {
                        "Local demo created. Sample provider connections are disabled; no inference tokens were printed."
                    } else {
                        "Local demo already exists; no changes made. Use --add-missing-personas for an explicit additive upgrade."
                    }
                );
            }
        }
        Command::ReconcileExecutions { limit } => {
            let n = open_model_gateway::governance::reconcile_expired(&store, limit).await?;
            println!("Reconciled {n} expired executions; unknown cost holds retained.");
        }
        Command::CompactHistory {
            older_than_days,
            limit,
        } => {
            let n =
                open_model_gateway::maintenance::compact_history(&store, older_than_days, limit)
                    .await?;
            println!("Compacted {n} settled execution records; financial history retained.");
        }
        Command::ProvisionUser {
            email,
            platform_admin,
        } => {
            let id = bootstrap::provision_user(&store, &email, platform_admin).await?;
            println!(
                "Provisioned user {id}. OIDC must provide the same verified email on first sign-in."
            );
        }
        Command::Serve => {
            anyhow::ensure!(
                store.is_ready().await,
                "database schema is not ready; run `open-model-gateway migrate` with the matching release"
            );
            let web = config
                .web_directory
                .as_deref()
                .map(WebAssets::load)
                .transpose()?;
            let mut registry = ProviderRegistry::default();
            let secrets = Arc::new(EnvSecrets::new(config.secret_env_allowlist));
            registry.register(Arc::new(OpenAiAdapter::new(secrets.clone())?))?;
            registry.register(Arc::new(AnthropicAdapter::new(secrets)?))?;
            registry.register(Arc::new(BedrockAdapter::new()?))?;
            let engine = Engine::new(Arc::new(store.clone()), registry, config.inference_limits)?;
            let identity = IdentityState::new(store.clone(), IdentityConfig::from_env()?).await?;
            let retention = open_model_gateway::maintenance::retention_from_env()?;
            let listener = tokio::net::TcpListener::bind(config.listen).await?;
            tracing::info!(address = %listener.local_addr()?, serving_web = web.is_some(), "gateway listening");
            let maintenance = open_model_gateway::maintenance::start(store.clone(), retention);
            let served = axum::serve(
                listener,
                http::router_with_identity(store, web, engine, identity),
            )
            .with_graceful_shutdown(shutdown_signal())
            .await;
            maintenance.abort();
            let _ = maintenance.await;
            served?;
        }
    }
    pool.close().await;
    Ok(())
}

fn ensure_demo_config(config: &Config) -> Result<()> {
    use std::str::FromStr;
    let options = sqlx::postgres::PgConnectOptions::from_str(&config.database_url)
        .context("invalid demo database configuration")?;
    anyhow::ensure!(
        config.environment == open_model_gateway::config::Environment::Development
            && config.listen.ip().is_loopback()
            && matches!(options.get_host(), "127.0.0.1" | "::1")
            && options.get_database() == Some("gateway_demo"),
        "local demo requires development mode, loopback binding/database, and the dedicated gateway_demo database"
    );
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("could not register Ctrl-C handler")
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("could not register SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    tracing::info!("shutdown requested");
}

#[cfg(test)]
mod demo {
    use super::*;

    #[test]
    fn upgrade_is_an_explicit_cli_option() {
        assert!(matches!(
            Cli::try_parse_from(["gateway", "bootstrap-demo"])
                .unwrap()
                .command,
            Some(Command::BootstrapDemo {
                add_missing_personas: false
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["gateway", "bootstrap-demo", "--add-missing-personas"])
                .unwrap()
                .command,
            Some(Command::BootstrapDemo {
                add_missing_personas: true
            })
        ));
        assert!(Cli::try_parse_from(["gateway", "serve", "--add-missing-personas"]).is_err());
    }

    #[test]
    fn local_demo_guard_rejects_real_databases_and_nonlocal_bindings() {
        let mut config = Config {
            database_url: "postgres://gateway:gateway@127.0.0.1:54329/gateway_demo".into(),
            listen: "127.0.0.1:3000".parse().unwrap(),
            environment: open_model_gateway::config::Environment::Development,
            web_directory: None,
            secret_env_allowlist: vec![],
            inference_limits: Default::default(),
        };
        assert!(ensure_demo_config(&config).is_ok());
        for url in [
            "postgres://gateway:gateway@127.0.0.1:54329/gateway",
            "postgres://gateway:gateway@database.example/gateway_demo",
            "postgres://gateway:gateway@127.0.0.1:54329/gateway_demo?host=database.example",
        ] {
            config.database_url = url.into();
            assert!(ensure_demo_config(&config).is_err());
        }
        config.database_url = "postgres://gateway:gateway@127.0.0.1:54329/gateway_demo".into();
        config.listen = "0.0.0.0:3000".parse().unwrap();
        assert!(ensure_demo_config(&config).is_err());
        config.listen = "127.0.0.1:3000".parse().unwrap();
        config.environment = open_model_gateway::config::Environment::Production;
        assert!(ensure_demo_config(&config).is_err());
    }
}

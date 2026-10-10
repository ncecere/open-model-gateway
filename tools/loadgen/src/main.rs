//! `loadgen`: seed a throwaway load-test database and drive open-loop load
//! against gateway replicas. See docs/operations.md "Capacity baseline".
use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use omg_loadgen::{
    run::{RunConfig, run},
    seed::{SeedConfig, seed},
};

#[derive(Parser)]
#[command(
    version,
    about = "Open Model Gateway load generator, seeder and settlement verifier (test only)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Seed users, workspaces, keys, the mock catalog, policies and optional
    /// history into a THROWAWAY omg_loadtest* database (connect as the owner).
    Seed {
        /// Owner/migrator connection URL of an omg_loadtest* database.
        #[arg(long, env = "LOADGEN_SEED_DATABASE_URL", hide_env_values = true)]
        database_url: String,
        /// Key derivation seed shared with `run --key-seed`.
        #[arg(long, env = "LOADGEN_KEY_SEED", default_value = "omg-loadtest")]
        key_seed: String,
        #[arg(long, default_value_t = 5000)]
        users: u32,
        #[arg(long, default_value_t = 2000)]
        shared_workspaces: u32,
        #[arg(long, default_value_t = 20000)]
        keys: u64,
        #[arg(long, default_value = "loadtest/chat")]
        model: String,
        /// Approved local endpoint of the mock upstream (GATEWAY_LOCAL_UPSTREAMS).
        #[arg(long, default_value = "http://mock-upstream:8000/v1")]
        endpoint: String,
        /// Skip the installation rate policy and monthly budgets.
        #[arg(long)]
        no_policies: bool,
        /// Settled historical attempts to pre-seed.
        #[arg(long, default_value_t = 0)]
        history: u64,
        #[arg(long, default_value_t = 120)]
        history_days: u32,
        /// Of --history, attempts left unknown (hold retained) last month.
        #[arg(long, default_value_t = 0)]
        history_unknown: u64,
        #[arg(long, default_value_t = 250_000)]
        chunk: u64,
    },
    /// Drive open-loop load and report latency, throughput, gateway phase
    /// histograms and settlement verification as JSON.
    Run {
        /// Gateway base URL; repeat (or comma-separate) for round-robin replicas.
        #[arg(long = "target", required = true, value_delimiter = ',')]
        targets: Vec<String>,
        /// Arrival rate in requests per second (open loop).
        #[arg(long)]
        rate: f64,
        #[arg(long, default_value_t = 30.0)]
        duration: f64,
        /// Leading seconds excluded from latency statistics.
        #[arg(long, default_value_t = 0.0)]
        warmup: f64,
        #[arg(long, default_value_t = 0.5)]
        stream_ratio: f64,
        #[arg(long, env = "LOADGEN_KEY_SEED", default_value = "omg-loadtest")]
        key_seed: String,
        /// Number of seeded keys to spread requests over (the hot set).
        #[arg(long, default_value_t = 2000)]
        keys: u64,
        #[arg(long, default_value_t = 0)]
        key_offset: u64,
        #[arg(long, default_value = "loadtest/chat")]
        model: String,
        #[arg(long, default_value_t = 16)]
        max_tokens: u32,
        #[arg(long, default_value_t = 60.0)]
        timeout: f64,
        #[arg(long, default_value_t = 4096)]
        max_in_flight: usize,
        #[arg(long, default_value_t = 1)]
        rng_seed: u64,
        /// Mock upstream base URL, for nonce matching and gateway overhead.
        #[arg(long)]
        mock_url: Option<String>,
        /// Gateway metrics URL (repeat or comma-separate, one per replica).
        #[arg(long = "metrics-url", value_delimiter = ',')]
        metrics_urls: Vec<String>,
        /// Read-only verification connection (runtime role is enough).
        #[arg(long, env = "LOADGEN_VERIFY_DATABASE_URL", hide_env_values = true)]
        database_url: Option<String>,
        /// Free-form label recorded in the report.
        #[arg(long)]
        label: Option<String>,
        /// Write the JSON report here as well as to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Seed {
            database_url,
            key_seed,
            users,
            shared_workspaces,
            keys,
            model,
            endpoint,
            no_policies,
            history,
            history_days,
            history_unknown,
            chunk,
        } => {
            let report = seed(
                &database_url,
                SeedConfig {
                    key_seed,
                    users,
                    shared_workspaces,
                    keys,
                    model,
                    endpoint,
                    policies: !no_policies,
                    history,
                    history_days,
                    history_unknown,
                    chunk,
                },
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Run {
            targets,
            rate,
            duration,
            warmup,
            stream_ratio,
            key_seed,
            keys,
            key_offset,
            model,
            max_tokens,
            timeout,
            max_in_flight,
            rng_seed,
            mock_url,
            metrics_urls,
            database_url,
            label,
            out,
        } => {
            let report = run(RunConfig {
                targets,
                rate,
                duration_s: duration,
                warmup_s: warmup,
                stream_ratio,
                key_seed,
                keys,
                key_offset,
                model,
                max_tokens,
                timeout_s: timeout,
                max_in_flight,
                rng_seed,
                mock_url,
                metrics_urls,
                database_url,
                label,
            })
            .await?;
            eprintln!("{}", report.headline());
            let json = serde_json::to_string_pretty(&report)?;
            if let Some(path) = out {
                std::fs::write(&path, &json).with_context(|| format!("writing {path:?}"))?;
            }
            println!("{json}");
            anyhow::ensure!(
                report.violations.is_empty(),
                "{} invariant violation(s)",
                report.violations.len()
            );
        }
    }
    Ok(())
}

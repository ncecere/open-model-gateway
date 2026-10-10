//! `mock-upstream`: deterministic OpenAI-compatible upstream for load tests.
//! TEST ONLY; see the library documentation and docs/operations.md.
use clap::Parser;
use omg_mock_upstream::{Config, Mock};

#[derive(Parser)]
#[command(
    version,
    about = "Deterministic OpenAI-compatible mock upstream for load tests (never forwards anything)"
)]
struct Args {
    /// Listen address.
    #[arg(long, env = "MOCK_LISTEN", default_value = "127.0.0.1:8000")]
    listen: std::net::SocketAddr,
    /// Complete responses: milliseconds until the whole response is sent.
    #[arg(long, env = "MOCK_LATENCY_MS", default_value_t = 20)]
    latency_ms: u64,
    /// Streams: milliseconds until the first content delta.
    #[arg(long, env = "MOCK_TTFT_MS", default_value_t = 10)]
    ttft_ms: u64,
    /// Streams: milliseconds between content deltas.
    #[arg(long, env = "MOCK_INTER_TOKEN_MS", default_value_t = 5)]
    inter_token_ms: u64,
    /// Completion tokens per response (capped by the request's max tokens).
    #[arg(long, env = "MOCK_COMPLETION_TOKENS", default_value_t = 4)]
    completion_tokens: u32,
    /// Reported prompt tokens (independent of the prompt).
    #[arg(long, env = "MOCK_PROMPT_TOKENS", default_value_t = 12)]
    prompt_tokens: u32,
    /// Deterministic fraction of calls that fail with --error-status.
    #[arg(long, env = "MOCK_ERROR_RATE", default_value_t = 0.0)]
    error_rate: f64,
    #[arg(long, env = "MOCK_ERROR_STATUS", default_value_t = 500)]
    error_status: u16,
    /// Seed for the deterministic error decision.
    #[arg(long, env = "MOCK_SEED", default_value_t = 1)]
    seed: u64,
    /// Calls kept for /__mock/calls (later calls are only counted).
    #[arg(long, env = "MOCK_MAX_RECORDS", default_value_t = 5_000_000)]
    max_records: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mock = Mock::new(Config {
        latency_ms: args.latency_ms,
        ttft_ms: args.ttft_ms,
        inter_token_ms: args.inter_token_ms,
        completion_tokens: args.completion_tokens,
        prompt_tokens: args.prompt_tokens,
        error_rate: args.error_rate,
        error_status: args.error_status,
        seed: args.seed,
        max_records: args.max_records,
    })?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    eprintln!(
        "mock-upstream listening on {} with {}",
        listener.local_addr()?,
        serde_json::to_string(mock.config())?
    );
    omg_mock_upstream::serve(listener, mock, shutdown()).await?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

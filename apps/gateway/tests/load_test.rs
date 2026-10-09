#![cfg(feature = "integration-tests")]
//! Reproducible admission + settlement load harness (docs/operations.md).
//!
//! Ignored by default. It never contacts a provider: an in-process mock
//! adapter stands in for the upstream, while authentication, routing,
//! durable admission (installation lock, policies, budgets), settlement and
//! the ledger run for real against PostgreSQL over a real TCP listener.
//!
//! ```sh
//! DATABASE_URL=postgres://gateway:gateway@127.0.0.1:54339/gateway \
//!   cargo test --release -p open-model-gateway --features integration-tests \
//!   --test load_test -- --ignored --nocapture
//! ```
//!
//! `DATABASE_URL` selects the *server*: the harness creates its own
//! `omg_load_<random>` database there, migrates it, and drops it afterwards.
//! Loopback servers only. Tunables (defaults): `LOAD_TEST_REQUESTS` (1000 per
//! scenario), `LOAD_TEST_CONCURRENCY` (`50,200`), `LOAD_TEST_UPSTREAM_MS` (20),
//! `LOAD_TEST_POOL` (10, the production default), `LOAD_TEST_MAX_CONCURRENT` (256),
//! `LOAD_TEST_PRESEED` (0): settled history rows inserted into a separate
//! workspace earlier this month, to measure how admission scales with history.
use std::{
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use open_model_gateway::{
    bootstrap,
    config::Environment,
    http,
    inference::{Engine, EngineLimits, error::InferenceError, types::*},
    providers::{ProviderAdapter, ProviderRegistry},
    store::Store,
};
use serde_json::json;
use sqlx::{
    ConnectOptions,
    postgres::{PgConnectOptions, PgPoolOptions},
};

/// Price v1: 1 µUSD/input token, 2 µUSD/output token; 7 in + 2 out = 11 µUSD.
const ACTUAL_PER_REQUEST: i64 = 11;
/// Hold: 100-token input ceiling + 10 reserved output tokens = 120 µUSD.
const HOLD_PER_REQUEST: i64 = 120;

fn env<T: FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct MockUpstream {
    calls: AtomicUsize,
    delay: Duration,
}
#[async_trait]
impl ProviderAdapter for MockUpstream {
    fn id(&self) -> &'static str {
        "load_mock"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: true,
            tools: false,
        }
    }
    async fn execute(
        &self,
        _target: &Deployment,
        request: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let usage = Usage {
            input_tokens: Some(7),
            output_tokens: Some(2),
            ..Default::default()
        };
        if request.stream {
            let step = self.delay / 4;
            Ok(ProviderOutput::Stream(Box::pin(async_stream::stream! {
                for word in ["Load ", "test ", "stream ", "ok"] {
                    tokio::time::sleep(step).await;
                    yield Ok(ChatEvent::Delta { text: Some(word.into()), tool_calls: vec![] });
                }
                yield Ok(ChatEvent::Finish(FinishReason::Stop));
                yield Ok(ChatEvent::Usage(usage));
                yield Ok(ChatEvent::Done);
            })))
        } else {
            tokio::time::sleep(self.delay).await;
            Ok(ProviderOutput::Complete(ChatResponse {
                content: Some("Load test ok".into()),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage,
            }))
        }
    }
}

#[derive(Default)]
struct Outcome {
    latencies: Vec<Duration>,
    statuses: std::collections::BTreeMap<u16, usize>,
    incomplete: usize,
}
impl Outcome {
    fn ok(&self) -> usize {
        self.statuses.get(&200).copied().unwrap_or(0) - self.incomplete
    }
    fn percentile(&self, p: f64) -> f64 {
        let mut sorted = self.latencies.clone();
        sorted.sort();
        if sorted.is_empty() {
            return 0.0;
        }
        let index = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
        sorted[index].as_secs_f64() * 1000.0
    }
}

async fn drive(
    base: &str,
    key: &str,
    total: usize,
    concurrency: usize,
    stream_every: usize,
) -> (Outcome, Duration) {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(concurrency)
        .build()
        .unwrap();
    let next = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let workers: Vec<_> = (0..concurrency)
        .map(|_| {
            let (client, next) = (client.clone(), next.clone());
            let url = format!("{base}/v1/chat/completions");
            let key = key.to_owned();
            tokio::spawn(async move {
                let mut outcome = Outcome::default();
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= total {
                        break;
                    }
                    let stream = stream_every > 0 && i % stream_every == 0;
                    let body = json!({"model":"company/smart","max_completion_tokens":10,"stream":stream,
                        "messages":[{"role":"user","content":"synthetic load"}]});
                    let began = Instant::now();
                    let response = client.post(&url).bearer_auth(&key).json(&body).send().await;
                    let (status, complete) = match response {
                        Ok(response) => {
                            let status = response.status().as_u16();
                            let text = response.text().await.unwrap_or_default();
                            let complete = status != 200
                                || if stream {
                                    text.contains("data: [DONE]")
                                } else {
                                    text.contains("Load test ok")
                                };
                            (status, complete)
                        }
                        Err(_) => (0, true),
                    };
                    outcome.latencies.push(began.elapsed());
                    *outcome.statuses.entry(status).or_default() += 1;
                    if !complete {
                        outcome.incomplete += 1;
                    }
                }
                outcome
            })
        })
        .collect();
    let mut merged = Outcome::default();
    for worker in workers {
        let part = worker.await.unwrap();
        merged.latencies.extend(part.latencies);
        merged.incomplete += part.incomplete;
        for (status, n) in part.statuses {
            *merged.statuses.entry(status).or_default() += n;
        }
    }
    (merged, started.elapsed())
}

fn report(name: &str, outcome: &Outcome, elapsed: Duration) {
    println!(
        "| {name} | {} | {:.0} | {:.1} | {:.1} | {:.1} | {:.1} | {:?} | {} |",
        outcome.latencies.len(),
        outcome.latencies.len() as f64 / elapsed.as_secs_f64(),
        outcome.percentile(50.0),
        outcome.percentile(95.0),
        outcome.percentile(99.0),
        outcome.percentile(100.0),
        outcome.statuses,
        outcome.incomplete,
    );
}

struct Ledger {
    executions: i64,
    succeeded: i64,
    pending: i64,
    unknown: i64,
    settled: i64,
    actual_sum: i64,
    holds: i64,
    hold_sum: i64,
    settlements: i64,
    settlement_sum: i64,
    orphans: i64,
}

async fn ledger(pool: &sqlx::PgPool, workspace: uuid::Uuid) -> Ledger {
    let row: (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        r#"SELECT
          (SELECT count(*) FROM inference_executions WHERE workspace_id=$1),
          (SELECT count(*) FROM inference_executions WHERE workspace_id=$1 AND state='succeeded'),
          (SELECT count(*) FROM governance_reservations WHERE workspace_id=$1 AND state='pending'),
          (SELECT count(*) FROM governance_reservations WHERE workspace_id=$1 AND state='unknown'),
          (SELECT count(*) FROM governance_reservations WHERE workspace_id=$1 AND state='settled'),
          (SELECT coalesce(sum(actual_microusd),0)::bigint FROM governance_reservations WHERE workspace_id=$1),
          (SELECT count(*) FROM monetary_ledger l JOIN inference_executions e ON e.id=l.execution_id WHERE e.workspace_id=$1 AND l.kind='hold'),
          (SELECT coalesce(sum(amount_microusd),0)::bigint FROM monetary_ledger l JOIN inference_executions e ON e.id=l.execution_id WHERE e.workspace_id=$1 AND l.kind='hold'),
          (SELECT count(*) FROM monetary_ledger l JOIN inference_executions e ON e.id=l.execution_id WHERE e.workspace_id=$1 AND l.kind='settlement'),
          (SELECT coalesce(sum(amount_microusd),0)::bigint FROM monetary_ledger l JOIN inference_executions e ON e.id=l.execution_id WHERE e.workspace_id=$1 AND l.kind='settlement'),
          (SELECT count(*) FROM inference_executions e WHERE e.workspace_id=$1 AND NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=e.id))"#,
    )
    .bind(workspace)
    .fetch_one(pool)
    .await
    .unwrap();
    Ledger {
        executions: row.0,
        succeeded: row.1,
        pending: row.2,
        unknown: row.3,
        settled: row.4,
        actual_sum: row.5,
        holds: row.6,
        hold_sum: row.7,
        settlements: row.8,
        settlement_sum: row.9,
        orphans: row.10,
    }
}

/// Exact accounting for `ok` successful, fully settled requests and nothing else.
fn assert_exact(name: &str, l: &Ledger, ok: i64) {
    assert_eq!(l.orphans, 0, "{name}: executions without reservation");
    assert_eq!(l.pending, 0, "{name}: lost (still pending) reservations");
    assert_eq!(l.unknown, 0, "{name}: unknown settlements");
    assert_eq!(l.executions, ok, "{name}: executions vs successes");
    assert_eq!(l.succeeded, ok, "{name}");
    assert_eq!(l.settled, ok, "{name}");
    assert_eq!(l.holds, ok, "{name}: one hold per attempt");
    assert_eq!(l.settlements, ok, "{name}: one settlement per attempt");
    assert_eq!(l.hold_sum, ok * HOLD_PER_REQUEST, "{name}: hold sum");
    assert_eq!(l.actual_sum, ok * ACTUAL_PER_REQUEST, "{name}: actual sum");
    assert_eq!(
        l.settlement_sum, l.actual_sum,
        "{name}: ledger vs reservations"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "load harness: run explicitly against a disposable loopback PostgreSQL server"]
async fn admission_and_settlement_under_concurrency() {
    let server = PgConnectOptions::from_str(
        &std::env::var("DATABASE_URL").expect("DATABASE_URL selects the test server"),
    )
    .unwrap();
    assert!(
        matches!(server.get_host(), "127.0.0.1" | "::1" | "localhost"),
        "load test only runs against a loopback PostgreSQL server"
    );
    let name = format!("omg_load_{}", uuid::Uuid::new_v4().simple());
    let mut admin = server.connect().await.unwrap();
    sqlx::raw_sql(&format!("CREATE DATABASE \"{name}\""))
        .execute(&mut admin)
        .await
        .unwrap();
    let options = server.clone().database(&name);
    let outcome =
        futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(run(options))).await;
    sqlx::raw_sql(&format!("DROP DATABASE \"{name}\" WITH (FORCE)"))
        .execute(&mut admin)
        .await
        .unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn run(options: PgConnectOptions) {
    let pool_size: u32 = env("LOAD_TEST_POOL", 10);
    let total: usize = env("LOAD_TEST_REQUESTS", 1000);
    let delay = Duration::from_millis(env("LOAD_TEST_UPSTREAM_MS", 20));
    let concurrency: Vec<usize> = std::env::var("LOAD_TEST_CONCURRENCY")
        .unwrap_or_else(|_| "50,200".into())
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    // Same pool shape as `Config::connect`, so results reflect production defaults.
    let pool = PgPoolOptions::new()
        .max_connections(pool_size)
        .acquire_timeout(Duration::from_secs(3))
        .connect_with(options)
        .await
        .unwrap();
    let store = Store::new(pool.clone());
    store.migrate_enterprise().await.unwrap();
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    sqlx::raw_sql(
        "UPDATE provider_connections SET provider='load_mock', enabled=true;
         UPDATE deployments SET enabled=true, upstream_model='mock-upstream';
         UPDATE models SET supported_protocols=ARRAY['chat_completions'];
         INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version)
           SELECT gen_random_uuid(),id,1000000,2000000,100,10,1 FROM deployments;
         INSERT INTO installation_policy(singleton,requests_per_minute,tokens_per_minute,concurrent_requests) VALUES(true,1000000,1000000000,100000);
         INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','month',1000000000000);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let preseed: i64 = env("LOAD_TEST_PRESEED", 0);
    if preseed > 0 {
        let started = Instant::now();
        sqlx::raw_sql(&format!(r#"DO $$ DECLARE u uuid; ws uuid:=gen_random_uuid(); k uuid:=gen_random_uuid(); d uuid; BEGIN
          SELECT owner_user_id INTO u FROM workspaces WHERE kind='personal' LIMIT 1;
          SELECT id INTO d FROM deployments LIMIT 1;
          INSERT INTO workspaces(id,name,kind) VALUES(ws,'Load history','project');
          INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES(ws,u,'owner','manual');
          INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES(k,ws,u,'History',decode(repeat('04',32),'hex'));
          CREATE TEMP TABLE history AS SELECT gen_random_uuid() id, date_trunc('month',now(),'UTC')+greatest(interval '0',now()-interval '5 minutes'-date_trunc('month',now(),'UTC'))*random() ts FROM generate_series(1,{preseed});
          INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at,completed_at,input_tokens,output_tokens)
            SELECT id,ws,k,d,'company/smart','load_mock',false,'succeeded',id,ts,ts,7,2 FROM history;
          INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd,actual_microusd,input_tokens,output_tokens)
            SELECT id,ws,k,d,ts,date_trunc('minute',ts,'UTC'),date_trunc('month',ts,'UTC'),ts,'settled',110,120,11,7,2 FROM history;
          DROP TABLE history;
        END $$; ANALYZE inference_executions; ANALYZE governance_reservations;"#))
        .execute(&pool)
        .await
        .unwrap();
        println!(
            "Preseeded {preseed} settled history rows in {:?}.",
            started.elapsed()
        );
    }
    let upstream = Arc::new(MockUpstream {
        calls: AtomicUsize::new(0),
        delay,
    });
    let mut registry = ProviderRegistry::default();
    registry.register(upstream.clone()).unwrap();
    let limits = EngineLimits {
        max_concurrent: env("LOAD_TEST_MAX_CONCURRENT", 256),
        ..EngineLimits::default()
    };
    let engine = Engine::new(Arc::new(store.clone()), registry, limits).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            http::router_with_engine(store.clone(), None, engine),
        )
        .await
        .unwrap();
    });

    println!(
        "\nLoad test: {total} requests/scenario, mock upstream {delay:?}, DB pool {pool_size}, gateway capacity {}, history rows {preseed}\n",
        limits.max_concurrent
    );
    println!(
        "| scenario | requests | req/s | p50 ms | p95 ms | p99 ms | max ms | statuses | incomplete |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    let mut expected_ok = 0i64;
    for &c in &concurrency {
        // Half streaming, half non-streaming (personal workspace).
        let (outcome, elapsed) = drive(&base, &keys.personal_key.token, total, c, 2).await;
        report(&format!("mixed c={c}"), &outcome, elapsed);
        assert_eq!(outcome.ok(), total, "every request should succeed");
        expected_ok += outcome.ok() as i64;
        let l = ledger(&pool, keys.personal_workspace_id).await;
        assert_exact(&format!("mixed c={c}"), &l, expected_ok);
    }

    // Budget contention: a tight team-workspace budget under the highest
    // concurrency must never be exceeded, and every denial must be clean.
    // About half the requests fit; the rest must be denied cleanly.
    let budget = (total as i64 * ACTUAL_PER_REQUEST / 2).max(2 * HOLD_PER_REQUEST);
    sqlx::query("INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) VALUES('local',$1,'month',$2)")
        .bind(keys.team_workspace_id)
        .bind(budget)
        .execute(&pool)
        .await
        .unwrap();
    let c = concurrency.iter().copied().max().unwrap_or(200);
    let (outcome, elapsed) = drive(&base, &keys.team_key.token, total, c, 2).await;
    report(&format!("budget c={c}"), &outcome, elapsed);
    let ok = outcome.ok() as i64;
    let denied = outcome.statuses.get(&429).copied().unwrap_or(0);
    assert_eq!(outcome.incomplete, 0);
    assert_eq!(ok as usize + denied, total, "only 200/429 expected");
    let l = ledger(&pool, keys.team_workspace_id).await;
    assert_exact("budget", &l, ok);
    assert!(l.actual_sum <= budget, "budget exceeded under contention");
    // Conservative holds (120 µUSD each while in flight) legitimately deny
    // requests before settled spend reaches the budget; utilization is reported.
    assert!(denied > 0, "the budget scenario should deny some requests");
    let executions: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions WHERE api_key_id IN (SELECT id FROM api_keys WHERE name<>'History')")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        executions,
        expected_ok + ok,
        "denied requests never executed"
    );
    assert_eq!(
        upstream.calls.load(Ordering::SeqCst) as i64,
        expected_ok + ok,
        "upstream calls equal admitted attempts"
    );
    println!(
        "\nBudget scenario: {ok} admitted, {denied} denied, settled {} of {budget} µUSD.",
        l.actual_sum
    );

    let exposition = open_model_gateway::metrics::METRICS
        .render(Some(&Store::new(pool.clone())))
        .await;
    let settled = format!(
        "gateway_settlements_total{{outcome=\"settled\"}} {}",
        expected_ok + ok
    );
    assert!(exposition.contains(&settled), "{settled}");
    assert!(exposition.contains(r#"gateway_reservations_held{state="pending"} 0"#));
    assert!(!exposition.contains(r#"gateway_settlements_total{outcome="held"}"#));
    // Maintained budget totals (0015) still equal a full scan after the run.
    let verified = std::time::Instant::now();
    let report = open_model_gateway::governance::totals::verify(&Store::new(pool.clone()))
        .await
        .unwrap();
    assert!(report.consistent(), "{report:?}");
    println!(
        "Budget totals verified: {} buckets consistent with a full scan in {:?}.",
        report.buckets,
        verified.elapsed()
    );
    server.abort();
    pool.close().await;
}

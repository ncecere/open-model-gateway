//! Controlled row-lock stall, NOT a reproduction or resolution of the old
//! 100–150 rps CNPG/WAL incident (#132). No provider traffic, retries, product
//! tuning, ACL exceptions, or performance SLO: 30s is only a recovery guard.
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::FutureExt;
use open_model_gateway::{
    bootstrap,
    config::Environment,
    governance, history, http,
    inference::{Engine, EngineLimits, error::InferenceError, types::*},
    providers::{ProviderAdapter, ProviderRegistry},
    store::Store,
};
use serde_json::json;
use sqlx::{ConnectOptions, Connection, PgPool, postgres::PgPoolOptions};
use tokio::{sync::Semaphore, task::JoinSet};
use uuid::Uuid;

const GUARD: Duration = Duration::from_secs(30);
const APP: &str = "omg_controlled_stall_runtime";
const POOL_SIZE: u32 = 10; // Config::connect default; do not shrink gates artificially.
const BURST: usize = 4;
const HOLD: i64 = 120;
const ACTUAL: i64 = 11;

struct MockAdapter {
    calls: AtomicUsize,
    completions: Arc<Semaphore>,
    cancelled: Arc<AtomicUsize>,
}
impl Default for MockAdapter {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            completions: Arc::new(Semaphore::new(0)),
            cancelled: Arc::new(AtomicUsize::new(0)),
        }
    }
}
struct StreamDrop {
    cancelled: Arc<AtomicUsize>,
    complete: bool,
}
impl Drop for StreamDrop {
    fn drop(&mut self) {
        if !self.complete {
            self.cancelled.fetch_add(1, Ordering::SeqCst);
        }
    }
}
#[async_trait::async_trait]
impl ProviderAdapter for MockAdapter {
    fn id(&self) -> &'static str {
        "controlled_stall_mock"
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
        _: &Deployment,
        request: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        let initial = self.calls.fetch_add(1, Ordering::SeqCst) < 3;
        let usage = Usage {
            input_tokens: Some(7),
            output_tokens: Some(2),
            ..Usage::default()
        };
        if request.stream {
            // Capture outside the generator: cancellation even before first poll
            // must drop upstream. The cancelled fixture never reports usage.
            let drop_guard = StreamDrop {
                cancelled: self.cancelled.clone(),
                complete: false,
            };
            let gate = self.completions.clone();
            Ok(ProviderOutput::Stream(Box::pin(async_stream::stream! {
                let mut drop_guard = drop_guard;
                yield Ok(ChatEvent::Delta { text: Some("controlled fixture".into()), tool_calls: vec![] });
                if initial {
                    gate.acquire().await.unwrap().forget();
                }
                drop_guard.complete = true;
                yield Ok(ChatEvent::Finish(FinishReason::Stop));
                yield Ok(ChatEvent::Usage(usage));
                yield Ok(ChatEvent::Done);
            })))
        } else {
            if initial {
                self.completions.acquire().await.unwrap().forget();
            }
            Ok(ProviderOutput::Complete(ChatResponse {
                messages_content: None,
                content: Some("controlled fixture".into()),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage,
            }))
        }
    }
}

// Poll only explicit coordination facts, never retry an inference request.
async fn until<F, Fut>(mut ready: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    tokio::time::timeout(GUARD, async {
        while !ready().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("controlled-stall coordination guard expired");
}

struct Activity {
    locks: i64,
    direct: i64,
    transactions: i64,
}
async fn activity(owner: &PgPool, blocker: i32) -> Activity {
    // Deliberately never select query text, identities, addresses, or tokens.
    let (locks, direct, transactions) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE wait_event_type='Lock'),
          count(*) FILTER (WHERE $1=ANY(pg_blocking_pids(pid))),
          count(*) FILTER (WHERE xact_start IS NOT NULL)
         FROM pg_stat_activity WHERE datname=current_database() AND application_name=$2",
    )
    .bind(blocker)
    .bind(APP)
    .fetch_one(owner)
    .await
    .unwrap();
    Activity {
        locks,
        direct,
        transactions,
    }
}
fn measure(phase: &str, began: Instant, pool: &PgPool, a: &Activity) {
    eprintln!(
        "controlled_lock_stall phase={phase} elapsed_ms={} pool_size={} pool_busy={} lock_waiters={} direct_blocker_waiters={} transactions={}",
        began.elapsed().as_millis(),
        pool.size(),
        (pool.size() as usize).saturating_sub(pool.num_idle()),
        a.locks,
        a.direct,
        a.transactions,
    );
}

struct Reply {
    status: u16,
    accounting_unavailable: bool,
    done: bool,
    transport_incomplete: bool,
    successful_response: bool,
}
impl Reply {
    async fn read(mut response: reqwest::Response) -> Self {
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        let mut transport_incomplete = false;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(_) => {
                    transport_incomplete = true;
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&bytes);
        let done = text.contains("data: [DONE]");
        let successful_response = done
            || serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .is_some_and(|v| v["choices"][0]["message"]["content"] == "controlled fixture");
        Self {
            status,
            accounting_unavailable: text.contains("accounting_unavailable"),
            done,
            transport_incomplete,
            successful_response,
        }
    }
    fn failed_accounting(&self, streamed: bool) {
        assert_eq!(self.status, if streamed { 200 } else { 503 });
        assert!(
            self.accounting_unavailable,
            "must explicitly refuse unaccounted success"
        );
        assert!(
            !self.done,
            "failed settlement fabricated a successful stream terminator"
        );
    }
    fn succeeded(&self) {
        assert_eq!(self.status, 200);
        assert!(!self.accounting_unavailable && !self.transport_incomplete);
        assert!(
            self.successful_response,
            "missing successful fixture completion"
        );
    }
}
fn request(
    client: &reqwest::Client,
    url: &str,
    key: &str,
    stream: bool,
) -> reqwest::RequestBuilder {
    client.post(url).bearer_auth(key).json(&json!({
        "model":"company/smart", "max_completion_tokens":10, "stream":stream,
        "messages":[{"role":"user","content":"controlled fixture"}]
    }))
}

async fn first_delta(response: &mut reqwest::Response) {
    assert_eq!(response.status(), 200);
    tokio::time::timeout(GUARD, async {
        let mut prefix = Vec::new();
        loop {
            let chunk = response
                .chunk()
                .await
                .unwrap()
                .expect("stream ended before first delta");
            prefix.extend_from_slice(&chunk);
            if String::from_utf8_lossy(&prefix).contains("controlled fixture") {
                break;
            }
        }
    })
    .await
    .expect("stream did not deliver first delta");
}

pub async fn run(pool: PgPool) {
    // This second sqlx::test owns a fresh database and independently takes the
    // old probe's session lock before touching the cluster-wide role/ACLs.
    // Never return this locked session to a pool, including on panic.
    let mut role_lock = pool.acquire().await.unwrap();
    role_lock.close_on_drop();
    sqlx::query("SELECT pg_advisory_lock(72419505)")
        .execute(&mut *role_lock)
        .await
        .unwrap();
    sqlx::raw_sql("DO $$ BEGIN IF NOT EXISTS(SELECT FROM pg_roles WHERE rolname='gateway_runtime') THEN CREATE ROLE gateway_runtime NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS; END IF; END $$; REVOKE CONNECT ON DATABASE postgres FROM PUBLIC,gateway_runtime; REVOKE CONNECT ON DATABASE template1 FROM PUBLIC,gateway_runtime;")
        .execute(&mut *role_lock).await.unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&mut *role_lock)
        .await
        .unwrap();
    let database = database.replace('"', "\"\"");
    sqlx::raw_sql(&format!("REVOKE ALL ON DATABASE \"{database}\" FROM PUBLIC,gateway_runtime; GRANT CONNECT ON DATABASE \"{database}\" TO gateway_runtime"))
        .execute(&mut *role_lock).await.unwrap();
    let owner_store = Store::new(pool.clone());
    owner_store.preflight_enterprise().await.unwrap();
    owner_store.apply_runtime_grants().await.unwrap();
    // Full, unmodified verifier (including rollback probes), before any HTTP.
    owner_store.verify_runtime_grants().await.unwrap();
    let keys = bootstrap::seed(&owner_store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    sqlx::raw_sql(
        "UPDATE provider_connections SET provider='controlled_stall_mock',enabled=true;
         UPDATE deployments SET enabled=true,upstream_model='controlled-fixture';
         UPDATE models SET supported_protocols=ARRAY['chat_completions'];
         INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version)
           SELECT gen_random_uuid(),id,1000000,2000000,100,10,1 FROM deployments;
         UPDATE workspace_type_policies SET requests_per_minute=64,tokens_per_minute=6400,concurrent_requests=16;
         INSERT INTO policy_budgets(layer,kind,period,amount_microusd)
           SELECT 'type',kind,'month',100000 FROM workspace_type_policies;"
    ).execute(&pool).await.unwrap();
    let options = (*pool.connect_options()).clone();
    let observer = PgPoolOptions::new()
        .max_connections(3)
        .connect_with(
            options
                .clone()
                .application_name("omg_controlled_stall_observer"),
        )
        .await
        .unwrap();
    let runtime = PgPoolOptions::new()
        .max_connections(POOL_SIZE)
        .acquire_timeout(Duration::from_secs(3))
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::Executor::execute(c, "SET ROLE gateway_runtime").await?;
                Ok(())
            })
        })
        .connect_with(options.application_name(APP))
        .await
        .unwrap();
    // Catch only to close pools after RAII has aborted request/server tasks and
    // dropped the dedicated blocker socket (which releases SQL locks on panic).
    let outcome = AssertUnwindSafe(scenario(&observer, &runtime, keys))
        .catch_unwind()
        .await;
    tokio::time::timeout(GUARD, runtime.close())
        .await
        .expect("runtime pool did not close");
    observer.close().await;
    sqlx::query("SELECT pg_advisory_unlock(72419505)")
        .execute(&mut *role_lock)
        .await
        .unwrap();
    drop(role_lock);
    drop(owner_store);
    pool.close().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn scenario(owner: &PgPool, runtime: &PgPool, keys: bootstrap::DevelopmentKeys) {
    let store = Store::new(runtime.clone());
    assert_eq!(
        store.admission_mode(),
        governance::locks::AdmissionMode::Scoped,
        "this controlled-stall fixture measures the default scoped protocol"
    );
    let sql_deadlines: (String, String) = sqlx::query_as(
        "SELECT current_setting('lock_timeout'),current_setting('statement_timeout')",
    )
    .fetch_one(runtime)
    .await
    .unwrap();
    assert_eq!(
        sql_deadlines,
        ("0".into(), "0".into()),
        "the fixture must observe the gateway finish deadline, not a PostgreSQL timeout"
    );
    let mock = Arc::new(MockAdapter::default());
    let mut registry = ProviderRegistry::default();
    registry.register(mock.clone()).unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let app = http::router_with_engine(store.clone(), None, engine);
    let mut server = JoinSet::new();
    server.spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder()
        .no_proxy()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .pool_max_idle_per_host(0)
        .timeout(GUARD)
        .build()
        .unwrap();
    let mut initial = JoinSet::new();
    let complete = request(&client, &url, &keys.team_key.token, false);
    initial.spawn(async move { (false, Reply::read(complete.send().await.unwrap()).await) });
    until(|| async { mock.calls.load(Ordering::SeqCst) == 1 }).await;
    let mut streamed = request(&client, &url, &keys.team_key.token, true)
        .send()
        .await
        .unwrap();
    first_delta(&mut streamed).await;
    let mut cancelled = request(&client, &url, &keys.team_key.token, true)
        .send()
        .await
        .unwrap();
    first_delta(&mut cancelled).await;
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT execution_id FROM governance_reservations WHERE workspace_id=$1 ORDER BY execution_id")
        .bind(keys.team_workspace_id).fetch_all(owner).await.unwrap();
    assert_eq!(
        ids.len(),
        3,
        "all three attempts must durably admit before blocking"
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 3);
    assert_held(owner, &ids, "pending").await;

    // Dedicated owner connection, never the runtime pool. Its socket is dropped
    // on unwind, so even a panic while a query is waiting unblocks PostgreSQL.
    let mut blocker = (*owner.connect_options())
        .clone()
        .application_name("omg_controlled_stall_blocker")
        .connect()
        .await
        .unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut blocker)
        .await
        .unwrap();
    let mut tx = blocker.begin().await.unwrap();
    let locked: Uuid = sqlx::query_scalar("SELECT scope_id FROM budget_totals WHERE scope_kind='workspace' AND scope_id=$1 AND period='lifetime' FOR UPDATE")
        .bind(keys.team_workspace_id).fetch_one(&mut *tx).await.unwrap();
    assert_eq!(locked, keys.team_workspace_id);
    let stall = Instant::now();
    initial.spawn(async move { (true, Reply::read(streamed).await) });
    drop(cancelled);
    until(|| async { mock.cancelled.load(Ordering::SeqCst) == 1 }).await;
    let finish = Instant::now();
    mock.completions.add_permits(2);
    until(|| async {
        let a = activity(owner, pid).await;
        a.locks == 3 && a.direct >= 1
    })
    .await;
    // Before admissions launch, these can ONLY be real settlement lock waits.
    measure(
        "settlement_blocked",
        stall,
        runtime,
        &activity(owner, pid).await,
    );

    let mut traffic = JoinSet::new();
    for _ in 0..BURST {
        let req = request(&client, &url, &keys.team_key.token, false);
        traffic.spawn(async move { Reply::read(req.send().await.unwrap()).await });
    }
    until(|| async { activity(owner, pid).await.locks > 3 }).await;
    let probe = request(&client, &url, &keys.personal_key.token, false);
    traffic.spawn(async move { Reply::read(probe.send().await.unwrap()).await });
    // Unrelated admission/dispatch must have pool and engine capacity while the
    // same-scope admissions are stalled. Its finalization can queue or time out.
    until(|| async { mock.calls.load(Ordering::SeqCst) == 4 }).await;
    measure(
        "burst_and_unrelated",
        stall,
        runtime,
        &activity(owner, pid).await,
    );
    tokio::time::timeout(GUARD, async {
        while let Some(result) = initial.join_next().await {
            let (stream, reply) = result.unwrap();
            reply.failed_accounting(stream);
        }
    })
    .await
    .expect("initial finalization did not fail within recovery guard");
    assert!(
        finish.elapsed() >= Duration::from_secs(3),
        "did not exercise real finish deadline"
    );
    measure(
        "finish_deadline_observed",
        stall,
        runtime,
        &activity(owner, pid).await,
    );
    // Do not release early on client timeout: HTTP accounting refusal above is
    // the evidence that the gateway's actual finalization deadline fired.
    tx.rollback().await.unwrap();
    blocker.close().await.unwrap();
    let recovery = Instant::now();
    let recover = async {
        while let Some(result) = traffic.join_next().await {
            let reply = result.unwrap();
            if reply.status == 200 {
                reply.succeeded();
            } else {
                reply.failed_accounting(false);
            }
        }
        until(|| async { activity(owner, pid).await.transactions == 0 }).await;
        measure(
            "unblocked_and_drained",
            recovery,
            runtime,
            &activity(owner, pid).await,
        );
        assert_held(owner, &ids, "pending").await;

        // Backdate ONLY this fresh database's bounded fixture pending leases,
        // never immutable prices/ledger/history. Include an unrelated probe if its
        // finalization also timed out behind the occupied settlement gate.
        let pending: Vec<Uuid> = sqlx::query_scalar("SELECT execution_id FROM governance_reservations WHERE workspace_id=ANY($1) AND state='pending'")
        .bind([keys.team_workspace_id, keys.personal_workspace_id].as_slice())
        .fetch_all(owner).await.unwrap();
        assert!(pending.len() <= 3 + BURST + 1);
        assert!(ids.iter().all(|id| pending.contains(id)));
        sqlx::query("UPDATE governance_reservations SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE execution_id=ANY($1) AND state='pending'")
        .bind(&pending).execute(owner).await.unwrap();
        assert_eq!(
            governance::reconcile_expired(&store, 64).await.unwrap(),
            pending.len() as u64
        );
        assert_eq!(governance::reconcile_expired(&store, 64).await.unwrap(), 0);
        assert_held(owner, &ids, "unknown").await;

        // Fresh concurrent requests are new attempts, not retries. Both scopes and
        // all real admission/settlement gates must work again with retained holds.
        for key in [&keys.team_key.token, &keys.personal_key.token] {
            for i in 0..3 {
                let req = request(&client, &url, key, i == 0);
                traffic.spawn(async move { Reply::read(req.send().await.unwrap()).await });
            }
        }
        while let Some(reply) = traffic.join_next().await {
            reply.unwrap().succeeded();
        }
        until(|| async { activity(owner, pid).await.transactions == 0 }).await;
        // Reacquire the entire production-shaped pool at once. A SQLx response
        // drain/queued rollback must not leave connections or transactions behind.
        let mut connections = Vec::new();
        for _ in 0..POOL_SIZE {
            connections.push(runtime.acquire().await.unwrap());
        }
        for c in &mut connections {
            let role: String = sqlx::query_scalar("SELECT current_user")
                .fetch_one(&mut **c)
                .await
                .unwrap();
            assert_eq!(role, "gateway_runtime");
        }
        drop(connections);
        until(|| async { runtime.num_idle() == runtime.size() as usize }).await;
        assert_eq!(activity(owner, pid).await.transactions, 0);
        let inflight: i64 = sqlx::query_scalar(
            "SELECT coalesce(sum(requests+jobs),0)::bigint FROM inflight_counters",
        )
        .fetch_one(owner)
        .await
        .unwrap();
        assert_eq!(inflight, 0);
        let dispatches = mock.calls.load(Ordering::SeqCst);
        assert_eq!(dispatches, 3 + BURST + 1 + 6, "no implicit request replay");
        assert_accounting(owner, dispatches as i64).await;
        let totals = governance::totals::verify(&store).await.unwrap();
        assert!(
            totals.consistent(),
            "budget/rate/inflight verification failed"
        );
        let history = history::verify(runtime, None).await.unwrap();
        assert!(
            history.consistent(),
            "history parent/orphan verification failed"
        );
        assert_eq!(
            mock.cancelled.load(Ordering::SeqCst),
            1,
            "exactly one upstream was client-cancelled"
        );
        measure(
            "recovered_and_verified",
            recovery,
            runtime,
            &activity(owner, pid).await,
        );
    };
    tokio::time::timeout(GUARD, recover)
        .await
        .expect("controlled lock-stall recovery guard expired after blocker release");
    server.abort_all();
    while let Some(result) = server.join_next().await {
        if let Err(error) = result {
            assert!(error.is_cancelled(), "HTTP listener task failed");
        }
    }
}

async fn assert_held(owner: &PgPool, ids: &[Uuid], state: &str) {
    let valid: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations WHERE execution_id=ANY($1) AND state=$2 AND held_microusd=$3 AND actual_microusd IS NULL AND input_tokens IS NULL AND output_tokens IS NULL")
        .bind(ids).bind(state).bind(HOLD).fetch_one(owner).await.unwrap();
    assert_eq!(
        valid,
        ids.len() as i64,
        "unknown/failed settlement must retain nonzero holds, not actual zero"
    );
}

async fn assert_accounting(owner: &PgPool, dispatches: i64) {
    let executions: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
        .fetch_one(owner)
        .await
        .unwrap();
    assert_eq!(
        executions, dispatches,
        "one dispatch per durable attempt, no implicit retries"
    );
    let reservations: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations")
        .fetch_one(owner)
        .await
        .unwrap();
    assert_eq!(reservations, executions, "one reservation per execution");
    let bad: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM inference_executions e
         LEFT JOIN governance_reservations r ON r.execution_id=e.id AND r.admitted_at=e.started_at
         WHERE r.execution_id IS NULL OR e.state='started' OR r.state='pending'
           OR e.attempt_number<>1 OR e.root_request_id<>e.id
           OR (SELECT count(*) FROM monetary_ledger l WHERE l.execution_id=e.id AND l.kind='hold')<>1
           OR EXISTS(SELECT 1 FROM monetary_ledger l WHERE l.execution_id=e.id AND l.kind='hold' AND l.amount_microusd IS DISTINCT FROM $2)
           OR (SELECT count(*) FROM monetary_ledger l WHERE l.execution_id=e.id AND l.kind IN ('settlement','unknown'))<>1
           OR (r.state='settled' AND (r.actual_microusd IS DISTINCT FROM $1 OR e.state<>'succeeded'
               OR (SELECT sum(amount_microusd) FROM monetary_ledger l WHERE l.execution_id=e.id AND l.kind='settlement') IS DISTINCT FROM r.actual_microusd))
           OR (r.state='unknown' AND (r.actual_microusd IS NOT NULL OR r.held_microusd IS DISTINCT FROM $2
               OR EXISTS(SELECT 1 FROM monetary_ledger l WHERE l.execution_id=e.id AND (l.kind='settlement' OR (l.kind='unknown' AND l.amount_microusd IS NOT NULL)))))"
    ).bind(ACTUAL).bind(HOLD).fetch_one(owner).await.unwrap();
    assert_eq!(
        bad, 0,
        "orphan, duplicate ledger entry, lost hold, or incorrect charge"
    );
}

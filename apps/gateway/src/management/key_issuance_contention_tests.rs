//! #278: measurement only; no production lock, quota, or authority changes.
//! Deliberately private: reuse real browser-session and identity test fixtures.
use super::*;
use futures_util::{StreamExt, stream};
use sqlx::{ConnectOptions, postgres::PgPoolOptions};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const GUARD: Duration = Duration::from_secs(30);
const HOLDERS: usize = 8;
const OPS_PER_HOLDER: usize = 2;
const REPETITIONS: usize = 2;
const APP: &str = "omg_key_issuance_measurement";

// Never include transport/database errors, SQL text, response bodies, cookies,
// CSRF values or issued tokens in panic diagnostics (including on refusal).
#[track_caller]
fn checked<T, E>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("key contention harness operation failed (details suppressed)"),
    }
}

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Http {
    client: reqwest::Client,
    base: String,
}
enum Credential<'a> {
    Anonymous,
    Browser(&'a Session),
    Token(&'a str),
}
// No Debug implementation: the only retained response fields are allowlisted.
struct Outcome {
    status: u16,
    retry: Option<u64>,
    quota: bool,
    id: Option<Uuid>,
    elapsed: Duration,
}
impl Http {
    async fn send(
        &self,
        credential: Credential<'_>,
        method: reqwest::Method,
        path: &str,
        body: Value,
    ) -> Outcome {
        checked(
            tokio::time::timeout(GUARD, async {
                let start = Instant::now();
                let mut request = self
                    .client
                    .request(method, format!("{}{path}", self.base))
                    .header("origin", ORIGIN)
                    .json(&body);
                match credential {
                    Credential::Anonymous => {}
                    Credential::Browser(s) => {
                        request = request
                            .header("cookie", format!("omg_session={}", s.cookie))
                            .header("x-csrf-token", &s.csrf);
                    }
                    Credential::Token(token) => request = request.bearer_auth(token),
                }
                let response = checked(request.send().await);
                let status = response.status().as_u16();
                let retry = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok());
                let mut bytes = Vec::new();
                let mut chunks = response.bytes_stream();
                while let Some(chunk) = chunks.next().await {
                    let chunk = checked(chunk);
                    assert!(
                        bytes.len() + chunk.len() <= 65_536,
                        "response exceeds harness bound"
                    );
                    bytes.extend_from_slice(&chunk);
                }
                let value: Value = checked(serde_json::from_slice(&bytes));
                let quota =
                    value["error"]["reason"].as_str() == Some("key_issuance_limit_exceeded");
                let id = value["id"].as_str().and_then(|v| Uuid::parse_str(v).ok());
                Outcome {
                    status,
                    retry,
                    quota,
                    id,
                    elapsed: start.elapsed(),
                }
            })
            .await,
        )
    }
    async fn create(&self, s: &Session, ws: Uuid) -> Outcome {
        self.send(
            Credential::Browser(s),
            reqwest::Method::POST,
            &keys_path(ws),
            json!({"name":"Contention measurement", "expires_in_days":1}),
        )
        .await
    }
    async fn rotate(&self, s: &Session, ws: Uuid, key: Uuid) -> Outcome {
        self.send(
            Credential::Browser(s),
            reqwest::Method::POST,
            &format!("{}/{key}/rotate", keys_path(ws)),
            json!({"expires_in_days":1}),
        )
        .await
    }
}
fn keys_path(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/keys")
}
fn issued(outcome: Outcome) -> Uuid {
    assert_eq!(outcome.status, 200, "fixture issuance failed");
    outcome
        .id
        .expect("successful issuance must return a key id")
}

#[derive(Clone, Copy)]
struct Shape {
    kind: &'static str,
    independent: bool,
}
struct Holder {
    workspace: Uuid,
    user: Uuid,
    session: Arc<Session>,
    tip: Option<Uuid>,
}
async fn holders(owner: &PgPool, shape: Shape) -> Vec<Holder> {
    let mut result = Vec::<Holder>::new();
    for _ in 0..HOLDERS {
        let person = user(owner, &format!("contention-{}", Uuid::new_v4()), "user").await;
        let ws = if shape.independent || result.is_empty() {
            workspace(owner, "Contention measurement", shape.kind, person.user_id).await
        } else {
            let ws = result[0].workspace;
            // Match the independent fixtures' owner role to avoid a role-mix confound.
            member(owner, ws, person.user_id, "owner").await;
            ws
        };
        result.push(Holder {
            workspace: ws,
            user: person.user_id,
            session: Arc::new(session(owner, &person).await),
            tip: None,
        });
    }
    result
}

#[derive(Default)]
struct Observations {
    samples: usize,
    active_max: i64,
    blocked_max: i64,
    blocking_edges: i64,
    installation_tuple_samples: i64,
    unattributed_xid_samples: i64,
    other_lock_samples: i64,
    other_wait_samples: i64,
    pool_in_use_max: u32,
    pool_idle_min: usize,
    pool_no_idle_samples: usize,
}
impl Observations {
    fn merge(&mut self, other: Self) {
        self.pool_idle_min = if self.samples == 0 {
            other.pool_idle_min
        } else {
            self.pool_idle_min.min(other.pool_idle_min)
        };
        self.samples += other.samples;
        self.active_max = self.active_max.max(other.active_max);
        self.blocked_max = self.blocked_max.max(other.blocked_max);
        self.blocking_edges += other.blocking_edges;
        self.installation_tuple_samples += other.installation_tuple_samples;
        self.unattributed_xid_samples += other.unattributed_xid_samples;
        self.other_lock_samples += other.other_lock_samples;
        self.other_wait_samples += other.other_wait_samples;
        self.pool_in_use_max = self.pool_in_use_max.max(other.pool_in_use_max);
        self.pool_no_idle_samples += other.pool_no_idle_samples;
    }
}
// Observe only PID/wait metadata, never pg_stat_activity.query. A granted tuple
// lock can identify the first waiter on a row (it then waits on the holder's
// transactionid); later waiters may wait directly for that tuple lock. Count
// either as installation evidence ONLY when this PID has an installation tuple
// lock. Do not guess the row from a ubiquitous relation lock or query text.
// Unattributed transactionid waits are explicitly separate, not mislabeled as
// workspace contention. Sampling is approximate and can miss short waits.
async fn observe(observer: &PgPool, runtime: &PgPool, done: &AtomicBool) -> Observations {
    let mut out = Observations {
        pool_idle_min: usize::MAX,
        ..Default::default()
    };
    while !done.load(Ordering::Acquire) && out.samples < 1_500 {
        let row: (i64, i64, i64, i64, i64, i64, i64) = checked(sqlx::query_as(
            "WITH activity AS MATERIALIZED (SELECT pid,wait_event_type,wait_event,pg_blocking_pids(pid) AS blockers FROM pg_stat_activity WHERE datname=current_database() AND application_name=$1 AND state='active'), classified AS (SELECT a.*,EXISTS(SELECT 1 FROM pg_locks l WHERE l.pid=a.pid AND l.locktype='tuple' AND l.relation='installation'::regclass) AS installation_tuple FROM activity a) SELECT count(*),count(*) FILTER(WHERE cardinality(blockers)>0),coalesce(sum(cardinality(blockers)),0)::bigint,count(*) FILTER(WHERE wait_event_type='Lock' AND installation_tuple),count(*) FILTER(WHERE wait_event_type='Lock' AND NOT installation_tuple AND wait_event='transactionid'),count(*) FILTER(WHERE wait_event_type='Lock' AND NOT installation_tuple AND wait_event<>'transactionid'),count(*) FILTER(WHERE wait_event_type IS NOT NULL AND wait_event_type<>'Lock') FROM classified"
        ).bind(APP).fetch_one(observer).await);
        out.samples += 1;
        out.active_max = out.active_max.max(row.0);
        out.blocked_max = out.blocked_max.max(row.1);
        out.blocking_edges += row.2;
        out.installation_tuple_samples += row.3;
        out.unattributed_xid_samples += row.4;
        out.other_lock_samples += row.5;
        out.other_wait_samples += row.6;
        let idle = runtime.num_idle();
        out.pool_idle_min = out.pool_idle_min.min(idle);
        out.pool_in_use_max = out
            .pool_in_use_max
            .max(runtime.size().saturating_sub(idle as u32));
        out.pool_no_idle_samples += usize::from(idle == 0);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    out
}

struct Measurement {
    outcomes: Vec<Outcome>,
    elapsed: Duration,
    observations: Observations,
}
async fn measure(
    http: &Http,
    observer: &PgPool,
    runtime: &PgPool,
    holders: &mut [Holder],
    rotate: bool,
    concurrency: usize,
) -> Measurement {
    // The rotation seed is outside the measured window. Each holder has exactly
    // one chain; a chain's two operations are sequential, never a same-tip race.
    if rotate {
        for holder in holders.iter_mut() {
            holder.tip = Some(issued(http.create(&holder.session, holder.workspace).await));
        }
    }
    let done = AtomicBool::new(false);
    let work = async {
        let start = Instant::now();
        let chains: Vec<Vec<Outcome>> = stream::iter(holders.iter_mut())
            .map(|holder| async move {
                let mut outcomes = Vec::new();
                for _ in 0..OPS_PER_HOLDER {
                    let outcome = if rotate {
                        http.rotate(
                            &holder.session,
                            holder.workspace,
                            holder.tip.expect("rotation seed must exist"),
                        )
                        .await
                    } else {
                        http.create(&holder.session, holder.workspace).await
                    };
                    if outcome.status == 200 {
                        holder.tip = outcome.id;
                    }
                    outcomes.push(outcome);
                }
                outcomes
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;
        let elapsed = start.elapsed();
        done.store(true, Ordering::Release);
        (chains.into_iter().flatten().collect::<Vec<_>>(), elapsed)
    };
    let ((outcomes, elapsed), observations) = checked(
        tokio::time::timeout(GUARD, async {
            tokio::join!(work, observe(observer, runtime, &done))
        })
        .await,
    );
    Measurement {
        outcomes,
        elapsed,
        observations,
    }
}
fn percentile(sorted: &[Duration], percent: usize) -> f64 {
    sorted[(sorted.len() * percent).div_ceil(100) - 1].as_secs_f64() * 1_000.0
}
fn report(shape: Shape, rotate: bool, concurrency: usize, runs: Vec<Measurement>) {
    let mut times = Vec::new();
    let mut elapsed = Duration::ZERO;
    let mut statuses = BTreeMap::<u16, usize>::new();
    let mut observations = Observations::default();
    for run in runs {
        elapsed += run.elapsed;
        observations.merge(run.observations);
        for outcome in run.outcomes {
            times.push(outcome.elapsed);
            *statuses.entry(outcome.status).or_default() += 1;
        }
    }
    times.sort_unstable();
    let o = observations;
    // Fixed labels and numerical aggregates only. No row IDs or secrets.
    println!(
        "key_contention kind={} workspaces={} operation={} concurrency={} repetitions={} ops={} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} throughput_ops_s={:.3} statuses={:?} observer_samples={} active_max={} blocked_max={} blocking_edge_samples={} installation_tuple_wait_samples={} unattributed_xid_wait_samples={} other_lock_wait_samples={} other_wait_samples={} pool_in_use_max={} pool_idle_min={} pool_no_idle_samples={}",
        shape.kind,
        if shape.independent { 8 } else { 1 },
        if rotate { "rotate" } else { "create" },
        concurrency,
        REPETITIONS,
        times.len(),
        percentile(&times, 50),
        percentile(&times, 95),
        percentile(&times, 99),
        times.len() as f64 / elapsed.as_secs_f64(),
        statuses,
        o.samples,
        o.active_max,
        o.blocked_max,
        o.blocking_edges,
        o.installation_tuple_samples,
        o.unattributed_xid_samples,
        o.other_lock_samples,
        o.other_wait_samples,
        o.pool_in_use_max,
        o.pool_idle_min,
        o.pool_no_idle_samples,
    );
    assert_eq!(times.len(), HOLDERS * OPS_PER_HOLDER * REPETITIONS);
    assert_eq!(
        statuses.get(&200).copied(),
        Some(times.len()),
        "measurement requests must succeed"
    );
}

async fn key_count(owner: &PgPool, workspace: Uuid) -> i64 {
    checked(
        sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE workspace_id=$1")
            .bind(workspace)
            .fetch_one(owner)
            .await,
    )
}
async fn assert_defaults(owner: &PgPool) {
    let policies: Vec<(String, Option<i64>, Option<i64>)> = checked(sqlx::query_as(
        "SELECT kind,key_issuances_per_holder,key_issuances_per_workspace FROM workspace_type_policies ORDER BY kind"
    ).fetch_all(owner).await);
    assert_eq!(policies.len(), 3);
    for (_, holder, workspace) in policies {
        assert_eq!(
            (holder, workspace),
            (Some(50), Some(500)),
            "default caps must remain unchanged"
        );
    }
}
async fn assert_below_caps(owner: &PgPool, holders: &[Holder]) {
    for holder in holders {
        let count: i64 = checked(
            sqlx::query_scalar(
                "SELECT count(*) FROM api_keys WHERE workspace_id=$1 AND issued_to_user_id=$2",
            )
            .bind(holder.workspace)
            .bind(holder.user)
            .fetch_one(owner)
            .await,
        );
        assert!(count < 50, "measurement must remain below the holder cap");
        assert!(
            key_count(owner, holder.workspace).await < 500,
            "measurement must remain below the workspace cap"
        );
    }
}

/// Surround the sample with a default-cap contract through this exact TCP /
/// runtime-role path. Existing `key_issuance_tests` already cover window edges,
/// policy composition, service accounts, and authority changes; do not duplicate
/// their full matrix here. `ux_tests::keys_disable_reversibly_revoked_never_and_stats_follow_lineage`
/// already covers irreversible revocation; here it guards this runtime path.
async fn contracts(owner: &PgPool, http: &Http) {
    let mut actors = holders(
        owner,
        Shape {
            kind: "team",
            independent: false,
        },
    )
    .await;
    let holder = actors.remove(0);
    let ws = holder.workspace;
    // 47 creates + one rotation + one create = 49 issued rows, with both a
    // rotated predecessor and an explicitly revoked key still in the window.
    let mut keys = Vec::new();
    for _ in 0..47 {
        keys.push(issued(http.create(&holder.session, ws).await));
    }
    issued(http.rotate(&holder.session, ws, keys[0]).await);
    let revoked = http
        .send(
            Credential::Browser(&holder.session),
            reqwest::Method::DELETE,
            &format!("{}/{}", keys_path(ws), keys[1]),
            Value::Null,
        )
        .await;
    assert_eq!(revoked.status, 200);
    issued(http.create(&holder.session, ws).await);
    assert_eq!(key_count(owner, ws).await, 49);
    // All eight calls share one fresh holder's last remaining allowance. There
    // are no retries, and refusal diagnostics never include the returned token.
    let outcomes: Vec<Outcome> = stream::iter(0..8)
        .map(|_| http.create(&holder.session, ws))
        .buffer_unordered(8)
        .collect()
        .await;
    assert_eq!(outcomes.iter().filter(|o| o.status == 200).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| o.status == 429 && o.quota && o.retry.is_some_and(|n| n > 0 && n <= 86_400))
            .count(),
        7
    );
    assert_eq!(
        key_count(owner, ws).await,
        50,
        "only one race request may insert"
    );
    let refused = http.create(&holder.session, ws).await;
    assert_eq!(refused.status, 429);
    assert!(refused.quota && refused.retry.is_some());
    assert_eq!(
        key_count(owner, ws).await,
        50,
        "refusal must not insert a key"
    );

    let outsider = user(owner, &format!("outsider-{}", Uuid::new_v4()), "user").await;
    let outsider = session(owner, &outsider).await;
    for (credential, expected) in [
        (Credential::Anonymous, 401),
        (Credential::Browser(&outsider), 403),
    ] {
        let response = http
            .send(
                credential,
                reqwest::Method::POST,
                &keys_path(ws),
                json!({"name":"Refused", "expires_in_days":1}),
            )
            .await;
        assert_eq!(response.status, expected, "authority must precede quota");
        assert!(!response.quota && response.retry.is_none());
        assert_eq!(key_count(owner, ws).await, 50);
    }
    // The existing private mint helper is in a sibling module. Use the same
    // test-only owner transaction and production token API, not a new constructor.
    let admin = user(owner, &format!("admin-{}", Uuid::new_v4()), "admin").await;
    let mut tx = checked(crate::db::begin(owner).await);
    checked(
        crate::governance::locks::exclusive(
            &mut tx,
            [crate::governance::locks::Scope::User(admin.user_id)],
        )
        .await,
    );
    let scopes: Vec<String> = crate::admin_tokens::SCOPES
        .iter()
        .map(|s| s.to_string())
        .collect();
    let token = checked(
        crate::admin_tokens::create_admin_token(
            &mut tx,
            admin.user_id,
            "Contention refusal contract",
            &scopes,
            30,
            None,
        )
        .await,
    );
    checked(tx.commit().await);
    let response = http
        .send(
            Credential::Token(&token.token),
            reqwest::Method::POST,
            &keys_path(ws),
            json!({"name":"Refused", "expires_in_days":1}),
        )
        .await;
    assert_eq!(response.status, 403, "admin tokens may not issue keys");
    assert!(!response.quota && response.retry.is_none());
    assert_eq!(key_count(owner, ws).await, 50);

    for key in &keys[..2] {
        let response = http
            .send(
                Credential::Browser(&holder.session),
                reqwest::Method::PATCH,
                &format!("{}/{key}", keys_path(ws)),
                json!({"disabled":false}),
            )
            .await;
        assert_eq!(
            response.status, 409,
            "revoked credentials cannot reactivate"
        );
        assert_eq!(http.rotate(&holder.session, ws, *key).await.status, 404);
    }
    let revoked_count: i64 = checked(sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE workspace_id=$1 AND id=ANY($2) AND revoked_at IS NOT NULL")
        .bind(ws).bind(&keys[..2]).fetch_one(owner).await);
    assert_eq!(revoked_count, 2);
    assert_eq!(key_count(owner, ws).await, 50);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
#[ignore = "dedicated PostgreSQL runtime-role contention measurement"]
async fn key_issuance_contention_measurement(owner: PgPool) {
    // The supplied sqlx pool owns a fresh test database. Shared role and
    // maintenance ACL changes require the same dedicated-cluster lock as the
    // runtime_privileges suite. Hold its connection until all pools are closed.
    let mut lock = checked(owner.acquire().await);
    checked(
        sqlx::raw_sql("SET statement_timeout='30s'; SET lock_timeout='30s'")
            .execute(&mut *lock)
            .await,
    );
    checked(
        sqlx::query("SELECT pg_advisory_lock(72419505)")
            .execute(&mut *lock)
            .await,
    );
    checked(sqlx::raw_sql("DO $$ BEGIN IF NOT EXISTS(SELECT FROM pg_roles WHERE rolname='gateway_runtime') THEN CREATE ROLE gateway_runtime NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS; END IF; END $$; REVOKE CONNECT ON DATABASE postgres FROM PUBLIC,gateway_runtime; REVOKE CONNECT ON DATABASE template1 FROM PUBLIC,gateway_runtime;")
        .execute(&mut *lock).await);
    let database: String = checked(
        sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&mut *lock)
            .await,
    );
    let database = database.replace('"', "\"\"");
    checked(sqlx::raw_sql(&format!("REVOKE ALL ON DATABASE \"{database}\" FROM PUBLIC,gateway_runtime; GRANT CONNECT ON DATABASE \"{database}\" TO gateway_runtime"))
        .execute(&mut *lock).await);
    let store = Store::new(owner.clone());
    // Full embedded grants/rollback verification, unchanged; never a reduced ACL.
    checked(store.apply_runtime_grants().await);
    checked(store.verify_runtime_grants().await);
    assert_defaults(&owner).await;

    let options = (*owner.connect_options())
        .clone()
        .application_name(APP)
        .disable_statement_logging();
    let runtime = checked(PgPoolOptions::new().max_connections(10).min_connections(10)
        .acquire_timeout(GUARD)
        .after_connect(|connection, _| Box::pin(async move {
            sqlx::raw_sql("SET ROLE gateway_runtime; SET statement_timeout='30s'; SET lock_timeout='30s'")
                .execute(connection).await?;
            Ok(())
        })).connect_with(options).await);
    let role: String = checked(
        sqlx::query_scalar("SELECT current_user")
            .fetch_one(&runtime)
            .await,
    );
    assert_eq!(
        role, "gateway_runtime",
        "requests must never use the owner role"
    );
    let observer = checked(
        PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(GUARD)
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::raw_sql("SET statement_timeout='30s'")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(
                (*owner.connect_options())
                    .clone()
                    .application_name("omg_key_issuance_observer")
                    .disable_statement_logging(),
            )
            .await,
    );
    let runtime_store = Store::new(runtime.clone());
    let mock = MockProvider::start(None).await;
    let identity = mock.state(runtime.clone()).await;
    let engine = checked(crate::inference::Engine::new(
        Arc::new(runtime_store.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    ));
    let app = crate::http::router_with_identity(runtime_store, None, engine, identity);
    let listener = checked(tokio::net::TcpListener::bind("127.0.0.1:0").await);
    let base = format!("http://{}", checked(listener.local_addr()));
    let server = Server(tokio::spawn(async move {
        checked(
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await,
        );
    }));
    let http = Http {
        base,
        client: checked(
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .connect_timeout(GUARD)
                .timeout(GUARD)
                .build(),
        ),
    };
    contracts(&owner, &http).await;
    for kind in ["team", "project"] {
        for independent in [true, false] {
            let shape = Shape { kind, independent };
            for rotate in [false, true] {
                for concurrency in [1, 2, 4, 8] {
                    let mut runs = Vec::new();
                    for _ in 0..REPETITIONS {
                        let mut actors = holders(&owner, shape).await;
                        runs.push(
                            measure(&http, &observer, &runtime, &mut actors, rotate, concurrency)
                                .await,
                        );
                        assert_below_caps(&owner, &actors).await;
                    }
                    report(shape, rotate, concurrency, runs);
                }
            }
        }
    }
    contracts(&owner, &http).await;
    assert_defaults(&owner).await;
    drop(http);
    drop(server);
    drop(mock);
    runtime.close().await;
    observer.close().await;
    checked(
        sqlx::query("SELECT pg_advisory_unlock(72419505)")
            .execute(&mut *lock)
            .await,
    );
}

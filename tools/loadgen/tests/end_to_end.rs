#![cfg(feature = "integration-tests")]
//! Seeder + real gateway + mock upstream + generator + verification, in one
//! process, against a throwaway `omg_loadtest_<random>` database created on
//! the loopback server named by `DATABASE_URL` and dropped afterwards.
//!
//! The gateway runs its real `openai_compatible` adapter against the mock over
//! loopback HTTP (approved through `GATEWAY_LOCAL_UPSTREAMS`, development
//! environment), so this covers authentication with seeded keys, admission,
//! the HTTP upstream, settlement and the ledger. No provider is contacted.
//!
//! Three scenarios share the `harness` below:
//! - the full settlement scenario (120 requests, exact usage/cost/hold,
//!   every management-reader endpoint);
//! - a reader-endpoint-coverage scenario under a controlled, owned-fixture
//!   management-read delay (never applied to inference/provider traffic, and
//!   never a change to production gateway code; issue #273);
//! - a warmup-exclusion scenario (readers disabled) confirming CLI warmup
//!   semantics for inference requests. The reader/warmup interaction is
//!   proven deterministically, without network timing, by
//!   `reader::summarize`'s own unit tests in `src/reader.rs`.
//!
//! `SCENARIO_GUARD` below serializes the three `#[tokio::test]` functions
//! against each other: the standard test harness otherwise runs them
//! concurrently in this process, and the gateway's admission/settlement
//! metrics counters (`metrics::METRICS`, a process-global singleton) would
//! then mix traffic from more than one scenario into the one each test
//! scrapes and asserts on.
use std::{future::Future, str::FromStr, sync::Arc, time::Duration};

use axum::{
    extract::Request,
    http::Method,
    middleware::{self, Next},
    response::Response,
};
use omg_loadgen::{
    keys,
    run::{RunConfig, run},
    seed::{SeedConfig, SeedReport, seed},
};
use omg_mock_upstream::{Config, Mock};
use open_model_gateway::{
    http,
    inference::{Engine, EngineLimits},
    metrics, providers,
    store::Store,
};
use sqlx::{
    ConnectOptions,
    postgres::{PgConnectOptions, PgPoolOptions},
};

/// Serializes the three `#[tokio::test]` scenarios below against each
/// other (see the module doc) so each one's own gateway metrics scrape
/// reflects only its own traffic. Held for the entire scenario, including
/// `with_throwaway_database`'s cleanup, which is why this is an async
/// `tokio::sync::Mutex` (the critical section spans awaits) rather than a
/// `std::sync::Mutex`, which must never be held across an `.await`.
static SCENARIO_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Create a throwaway `omg_loadtest_<prefix>_<random>` database on the
/// loopback server named by `DATABASE_URL`, run `body`, then drop the
/// database (even if `body` panics).
async fn with_throwaway_database<F, Fut>(prefix: &str, body: F)
where
    F: FnOnce(PgConnectOptions) -> Fut,
    Fut: Future<Output = ()>,
{
    let server = PgConnectOptions::from_str(
        &std::env::var("DATABASE_URL").expect("DATABASE_URL selects the test server"),
    )
    .unwrap();
    assert!(
        matches!(server.get_host(), "127.0.0.1" | "::1" | "localhost"),
        "only against a loopback PostgreSQL server"
    );
    let name = format!("omg_loadtest_{prefix}_{}", uuid::Uuid::new_v4().simple());
    let mut admin = server.connect().await.unwrap();
    sqlx::raw_sql(&format!("CREATE DATABASE \"{name}\""))
        .execute(&mut admin)
        .await
        .unwrap();
    let options = server.clone().database(&name);
    let outcome =
        futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(body(options))).await;
    sqlx::raw_sql(&format!("DROP DATABASE \"{name}\" WITH (FORCE)"))
        .execute(&mut admin)
        .await
        .unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Owned test fixture only (never production gateway code): delay every GET
/// on the identity/management router by `delay` (zero is a no-op). Every
/// inference call in this harness is a POST to `/v1/chat/completions`
/// against the mock upstream on a separate router entirely, so this never
/// touches inference/provider latency, cost or upstream call accounting, and
/// never incurs a paid call.
async fn delay_management_reads(delay: Duration, req: Request, next: Next) -> Response {
    if req.method() == Method::GET && !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    next.run(req).await
}

struct Harness {
    pool: sqlx::PgPool,
    store: Store,
    url: String,
    mock_base: String,
    gateway: String,
    metrics_url: String,
}

/// Seed a throwaway database, start the mock upstream, the real gateway
/// (inference plus identity/management routes, for `--readers`) and its
/// metrics endpoint. `management_delay` is the `delay_management_reads` test
/// fixture above (zero disables it).
async fn harness(
    options: PgConnectOptions,
    key_seed: &str,
    users: u32,
    shared_workspaces: u32,
    keys: u64,
    history: u64,
    management_delay: Duration,
) -> (Harness, SeedReport) {
    let url = options.to_url_lossy().to_string();
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(3))
        .connect_with(options.clone())
        .await
        .unwrap();
    let store = Store::new(pool.clone());
    store.migrate_enterprise().await.unwrap();

    // Mock upstream on loopback.
    let mock = Mock::new(Config {
        latency_ms: 10,
        ttft_ms: 5,
        inter_token_ms: 1,
        completion_tokens: 4,
        prompt_tokens: 12,
        ..Config::default()
    })
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_base = format!("http://{}", listener.local_addr().unwrap());
    let served = mock.clone();
    tokio::spawn(async move {
        omg_mock_upstream::serve(listener, served, std::future::pending()).await
    });

    let seeded = seed(
        &url,
        SeedConfig {
            key_seed: key_seed.into(),
            users,
            shared_workspaces,
            keys,
            model: "loadtest/chat".into(),
            endpoint: format!("{mock_base}/v1"),
            policies: true,
            history,
            history_days: 30,
            history_unknown: history.min(10),
            chunk: 700,
        },
    )
    .await
    .unwrap();

    // The real gateway with the local adapters approved for the mock. This
    // harness owns the approval JSON directly and parses it with the same
    // validation `from_env` uses, rather than mutating the process-wide
    // `GATEWAY_LOCAL_UPSTREAMS` environment variable (unsafe to do
    // concurrently in a multi-threaded test binary, and not necessary: the
    // approval parser is public precisely so trusted, owned-fixture callers
    // like this one can pass it configuration directly).
    let engine = {
        let approvals = providers::local::endpoints::ApprovedEndpoints::parse(
            &format!(r#"[{{"endpoint":"{mock_base}/v1","addresses":["127.0.0.1"]}}]"#),
            "development",
        )
        .unwrap();
        let mut registry = providers::ProviderRegistry::default();
        for adapter in providers::local::adapters(
            Arc::new(providers::secrets::EnvSecrets::new(Vec::new())),
            approvals,
        ) {
            registry.register(adapter).unwrap();
        }
        Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = format!("http://{}", listener.local_addr().unwrap());
    // Management routes too (session cookies, no OIDC provider), for --readers.
    let identity = open_model_gateway::identity::IdentityState::new(store.clone(), None)
        .await
        .unwrap();
    let app = http::router_with_identity(store.clone(), None, engine, identity).layer(
        middleware::from_fn(move |req, next| delay_management_reads(management_delay, req, next)),
    );
    tokio::spawn(async move { axum::serve(listener, app).await });
    let metrics_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let metrics_url = format!("http://{}/metrics", metrics_listener.local_addr().unwrap());
    let metrics_app = metrics::router(store.clone());
    tokio::spawn(async move { axum::serve(metrics_listener, metrics_app).await });

    (
        Harness {
            pool,
            store,
            url,
            mock_base,
            gateway,
            metrics_url,
        },
        seeded,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeded_keys_drive_the_real_gateway_and_every_request_settles_exactly() {
    let _guard = SCENARIO_GUARD.lock().await;
    with_throwaway_database("main", |options| async move {
        scenario_main(options).await;
    })
    .await;
}

async fn scenario_main(options: PgConnectOptions) {
    let (h, seeded) = harness(
        options,
        "e2e",
        30,
        6,
        120,
        2_000,
        Duration::ZERO, // no owned-fixture delay in the primary settlement scenario.
    )
    .await;
    assert_eq!(seeded.users, 30);
    assert_eq!(seeded.workspaces, 36);
    assert_eq!(seeded.api_keys, 120);
    assert_eq!(seeded.reservations, 2_000);
    assert_eq!(seeded.ledger_entries, 4_000);
    // Seeding again is refused; other databases are refused before any write.
    assert!(
        seed(
            &h.url,
            SeedConfig {
                history: 0,
                ..seeded.config.clone()
            }
        )
        .await
        .is_err()
    );
    // Every key kind authenticates with its derived token.
    for k in [0, 30, 60, 90, 119] {
        let principal = h
            .store
            .authenticate(&keys::token("e2e", k))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("key {k} does not authenticate"));
        assert_eq!(
            principal.key_id.simple().to_string(),
            keys::key_id_hex("e2e", k)
        );
    }
    assert!(
        h.store
            .authenticate(&keys::token("other", 0))
            .await
            .unwrap()
            .is_none()
    );
    let verified = open_model_gateway::governance::totals::verify(&h.store)
        .await
        .unwrap();
    assert!(verified.consistent(), "{verified:?}");

    let report = run(RunConfig {
        targets: vec![
            h.gateway.clone(),
            h.gateway.replace("127.0.0.1", "localhost"),
        ],
        rate: 60.0,
        duration_s: 2.0,
        // Issue #273: no warmup here. With one reader per endpoint below,
        // every endpoint's first (guaranteed-distinct) request lands inside
        // the measured window, so complete coverage no longer depends on how
        // many closed-loop laps a few readers can fit in a short warmup-cut
        // window on a possibly slow host.
        warmup_s: 0.0,
        stream_ratio: 0.5,
        key_seed: "e2e".into(),
        keys: 120,
        key_offset: 0,
        model: "loadtest/chat".into(),
        max_tokens: 16,
        timeout_s: 30.0,
        max_in_flight: 500,
        rng_seed: 3,
        mock_url: Some(h.mock_base.clone()),
        metrics_urls: vec![h.metrics_url.clone()],
        database_url: Some(h.url.clone()),
        label: Some("e2e".into()),
        readers: 8,
        reader_days: 31,
    })
    .await
    .unwrap();
    eprintln!("{}", report.headline());
    assert!(report.violations.is_empty(), "{:?}", report.violations);
    assert_eq!(report.totals.sent, 120);
    assert_eq!(report.totals.ok, 120, "{:?}", report.totals);
    let upstream = report.upstream.as_ref().unwrap();
    assert_eq!(upstream.ok_requests_matched, 120);
    assert_eq!(upstream.calls, 120);
    let db = report.database.as_ref().unwrap();
    assert_eq!(db.settled, 120);
    assert_eq!(db.exact_usage, 120);
    // 12 prompt tokens x 1 µUSD + 4 completion tokens x 2 µUSD.
    assert_eq!(db.expected_actual_per_request_microusd, Some(20));
    assert_eq!(db.actual_sum_microusd, 120 * 20);
    // Hold: 1000-token input ceiling x 1 µUSD + 16 reserved output x 2 µUSD.
    assert_eq!(db.held_sum_microusd, 120 * 1032);
    let gateway_metrics = report.gateway.as_ref().unwrap();
    let total = &gateway_metrics.admission["phase=total,outcome=admitted"];
    assert!(total.count >= 90, "{total:?}");
    assert!(
        gateway_metrics
            .settlement
            .contains_key("phase=locks,outcome=settled")
    );
    assert!(report.gateway_overhead_ms.as_ref().unwrap().count >= 90);
    // The seeded reader session reads every report/usage/log/me endpoint.
    let readers = report.reader.as_ref().unwrap();
    assert!(readers.ok >= 8, "{readers:?}");
    assert_eq!(readers.ok, readers.requests, "{readers:?}");
    assert_eq!(readers.endpoints.len(), 8, "{readers:?}");
    let verified = open_model_gateway::governance::totals::verify(&h.store)
        .await
        .unwrap();
    assert!(verified.consistent(), "{verified:?}");
    h.pool.close().await;
}

/// Issue #273 RED/GREEN proof, kept as a permanent regression: one
/// concurrent reader per endpoint, no warmup, and a ~300 ms owned-fixture
/// delay on every management GET (never on inference/provider traffic, no
/// paid calls). The run's measured window closes (`until`) well before a
/// delayed response plus the fixed 50 ms pre-start gap could complete a
/// *second* request, so each reader gets exactly one counted sample if and
/// only if that first request is not excluded for starting before the run's
/// common scheduled start.
///
/// Before the fix (`reader::reader` starting its loop at spawn time instead
/// of waiting for the common start), every reader's only sample starts
/// before the measured window and is excluded: `readers.ok == 0`, 0 of 8
/// endpoints covered. After the fix, all 8 samples start exactly at the
/// common start, are counted and (against the real gateway/DB/mock here)
/// succeed: `readers.ok == 8`, all 8 endpoints. Run three times against the
/// same gateway to show this is deterministic, not a lucky rerun.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readers_cover_every_endpoint_despite_slow_management_responses() {
    let _guard = SCENARIO_GUARD.lock().await;
    with_throwaway_database("slow", |options| async move {
        scenario_slow_management(options).await;
    })
    .await;
}

async fn scenario_slow_management(options: PgConnectOptions) {
    let (h, _seeded) = harness(options, "e2eslow", 1, 1, 4, 0, Duration::from_millis(300)).await;
    for attempt in 0..3u64 {
        let report = run(RunConfig {
            targets: vec![h.gateway.clone()],
            rate: 40.0,
            // Common start is 50 ms after scheduling; a ~300 ms delayed
            // response plus a second round trip cannot land inside a
            // 200 ms-after-start window, so coverage depends entirely on
            // each reader's *first* request being counted, not on how many
            // laps fit.
            duration_s: 0.2,
            warmup_s: 0.0,
            stream_ratio: 0.0,
            key_seed: "e2eslow".into(),
            keys: 4,
            key_offset: 0,
            model: "loadtest/chat".into(),
            max_tokens: 8,
            timeout_s: 10.0,
            max_in_flight: 50,
            rng_seed: 5 + attempt,
            mock_url: Some(h.mock_base.clone()),
            metrics_urls: vec![],
            database_url: None,
            label: Some(format!("slow-management-{attempt}")),
            readers: 8,
            reader_days: 7,
        })
        .await
        .unwrap();
        eprintln!("{}", report.headline());
        let readers = report.reader.as_ref().unwrap();
        assert_eq!(readers.endpoints.len(), 8, "attempt {attempt}: {readers:?}");
        assert_eq!(readers.ok, 8, "attempt {attempt}: {readers:?}");
        assert_eq!(
            readers.ok, readers.requests,
            "attempt {attempt}: {readers:?}"
        );
    }
    h.pool.close().await;
}

/// Separate warmup-exclusion coverage (full stack, alongside the fix above):
/// inference samples that start before the warmup boundary are excluded
/// from measured stats exactly as before. Readers are disabled here
/// (`readers: 0`, so `report.reader` is `None`): a prior version of this
/// scenario also asserted on a couple of closed-loop readers reaching every
/// endpoint inside a 0.5 s measured window, which depended on host speed
/// the same way issue #273 did for the main scenario. That coverage is now
/// proven deterministically and without any network timing by
/// `reader::summarize`'s own unit tests in `src/reader.rs`
/// (`summary_excludes_warmup_and_failures_from_latency` and the explicit
/// pre-warmup/boundary/inside-sample test alongside it); nothing about
/// reader/warmup exclusion is weakened, it is just tested orthogonally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warmup_excludes_early_inference_samples() {
    let _guard = SCENARIO_GUARD.lock().await;
    with_throwaway_database("warmup", |options| async move {
        scenario_warmup(options).await;
    })
    .await;
}

async fn scenario_warmup(options: PgConnectOptions) {
    let (h, _seeded) = harness(options, "e2ewarm", 1, 1, 4, 0, Duration::ZERO).await;
    let report = run(RunConfig {
        targets: vec![h.gateway.clone()],
        rate: 20.0,
        duration_s: 1.0,
        warmup_s: 0.5,
        stream_ratio: 0.0,
        key_seed: "e2ewarm".into(),
        keys: 4,
        key_offset: 0,
        model: "loadtest/chat".into(),
        max_tokens: 8,
        timeout_s: 10.0,
        max_in_flight: 50,
        rng_seed: 9,
        mock_url: Some(h.mock_base.clone()),
        metrics_urls: vec![],
        database_url: None,
        label: Some("warmup".into()),
        readers: 0,
        reader_days: 7,
    })
    .await
    .unwrap();
    eprintln!("{}", report.headline());
    assert!(report.violations.is_empty(), "{:?}", report.violations);
    assert_eq!(report.totals.sent, 20);
    // Half the schedule (0.5 of 1.0 s) is warmup: excluded from the measured
    // bucket, not from totals.
    assert_eq!(
        report.totals.measured, 10,
        "warmup excludes only the leading half of inference requests: {:?}",
        report.totals
    );
    // Readers disabled (`readers: 0` above): no reader stats are collected.
    assert!(report.reader.is_none(), "{:?}", report.reader);
    h.pool.close().await;
}

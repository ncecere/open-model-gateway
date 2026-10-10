#![cfg(feature = "integration-tests")]
//! Seeder + real gateway + mock upstream + generator + verification, in one
//! process, against a throwaway `omg_loadtest_<random>` database created on
//! the loopback server named by `DATABASE_URL` and dropped afterwards.
//!
//! The gateway runs its real `openai_compatible` adapter against the mock over
//! loopback HTTP (approved through `GATEWAY_LOCAL_UPSTREAMS`, development
//! environment), so this covers authentication with seeded keys, admission,
//! the HTTP upstream, settlement and the ledger. No provider is contacted.
use std::{str::FromStr, sync::Arc, time::Duration};

use omg_loadgen::{
    keys,
    run::{RunConfig, run},
    seed::{SeedConfig, seed},
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeded_keys_drive_the_real_gateway_and_every_request_settles_exactly() {
    let server = PgConnectOptions::from_str(
        &std::env::var("DATABASE_URL").expect("DATABASE_URL selects the test server"),
    )
    .unwrap();
    assert!(
        matches!(server.get_host(), "127.0.0.1" | "::1" | "localhost"),
        "only against a loopback PostgreSQL server"
    );
    let name = format!("omg_loadtest_{}", uuid::Uuid::new_v4().simple());
    let mut admin = server.connect().await.unwrap();
    sqlx::raw_sql(&format!("CREATE DATABASE \"{name}\""))
        .execute(&mut admin)
        .await
        .unwrap();
    let options = server.clone().database(&name);
    let outcome =
        futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(scenario(options)))
            .await;
    sqlx::raw_sql(&format!("DROP DATABASE \"{name}\" WITH (FORCE)"))
        .execute(&mut admin)
        .await
        .unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn scenario(options: PgConnectOptions) {
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
            key_seed: "e2e".into(),
            users: 30,
            shared_workspaces: 6,
            keys: 120,
            model: "loadtest/chat".into(),
            endpoint: format!("{mock_base}/v1"),
            policies: true,
            history: 2_000,
            history_days: 30,
            history_unknown: 10,
            chunk: 700,
        },
    )
    .await
    .unwrap();
    assert_eq!(seeded.users, 30);
    assert_eq!(seeded.workspaces, 36);
    assert_eq!(seeded.api_keys, 120);
    assert_eq!(seeded.reservations, 2_000);
    assert_eq!(seeded.ledger_entries, 4_000);
    // Seeding again is refused; other databases are refused before any write.
    assert!(
        seed(
            &url,
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
        let principal = store
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
        store
            .authenticate(&keys::token("other", 0))
            .await
            .unwrap()
            .is_none()
    );
    let verified = open_model_gateway::governance::totals::verify(&store)
        .await
        .unwrap();
    assert!(verified.consistent(), "{verified:?}");

    // The real gateway with the local adapters approved for the mock.
    // SAFETY: set before any thread reads it; the value is test-only.
    unsafe {
        std::env::set_var(
            "GATEWAY_LOCAL_UPSTREAMS",
            format!(r#"[{{"endpoint":"{mock_base}/v1","addresses":["127.0.0.1"]}}]"#),
        );
    }
    let approvals =
        providers::local::endpoints::ApprovedEndpoints::from_env("development").unwrap();
    let mut registry = providers::ProviderRegistry::default();
    for adapter in providers::local::adapters(
        Arc::new(providers::secrets::EnvSecrets::new(Vec::new())),
        approvals,
    ) {
        registry.register(adapter).unwrap();
    }
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = format!("http://{}", listener.local_addr().unwrap());
    let app = http::router_with_engine(store.clone(), None, engine);
    tokio::spawn(async move { axum::serve(listener, app).await });
    let metrics_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let metrics_url = format!("http://{}/metrics", metrics_listener.local_addr().unwrap());
    let metrics_app = metrics::router(store.clone());
    tokio::spawn(async move { axum::serve(metrics_listener, metrics_app).await });

    let report = run(RunConfig {
        targets: vec![gateway.clone(), gateway.replace("127.0.0.1", "localhost")],
        rate: 60.0,
        duration_s: 2.0,
        warmup_s: 0.5,
        stream_ratio: 0.5,
        key_seed: "e2e".into(),
        keys: 120,
        key_offset: 0,
        model: "loadtest/chat".into(),
        max_tokens: 16,
        timeout_s: 30.0,
        max_in_flight: 500,
        rng_seed: 3,
        mock_url: Some(mock_base.clone()),
        metrics_urls: vec![metrics_url],
        database_url: Some(url.clone()),
        label: Some("e2e".into()),
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
    let verified = open_model_gateway::governance::totals::verify(&store)
        .await
        .unwrap();
    assert!(verified.consistent(), "{verified:?}");
    pool.close().await;
}

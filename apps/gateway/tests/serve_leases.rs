//! A real `serve` process that fails start-up after it has taken its
//! background-job leases must hand them back before it exits (#527), as
//! graceful shutdown does, so another replica can run those jobs at once
//! instead of after the lease period.
#![cfg(feature = "integration-tests")]
#![allow(clippy::disallowed_methods)]
use sqlx::{ConnectOptions, PgPool};
use std::{process::Stdio, time::Duration};

/// Run `serve` with `extra` settings until it exits; returns its stderr.
async fn serve_until_exit(pool: &PgPool, listen: &str, extra: &[(&str, &str)]) -> String {
    let database = pool.connect_options().to_url_lossy().to_string();
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_open-model-gateway"))
        .arg("serve")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("DATABASE_URL", database)
        .env("GATEWAY_ENV_FILE", "/dev/null")
        .env("GATEWAY_ENV", "development")
        .env("GATEWAY_LISTEN", listen)
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .envs(extra.iter().copied())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("serve exits after a start-up failure")
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    stderr
}

/// The process took leases (it got past `prepare`) and none of them is
/// still current: every term it held has been released, not left to expire.
async fn assert_leases_taken_and_released(pool: &PgPool, stderr: &str) {
    let taken: i64 =
        sqlx::query_scalar("SELECT count(*) FROM work_leases WHERE holder IS NOT NULL")
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(taken > 0, "serve never took a lease: {stderr}");
    let current: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM work_leases WHERE expires_at>clock_timestamp() ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(
        current.is_empty(),
        "leases still held after serve exited: {current:?}\n{stderr}"
    );
}

fn busy_port() -> (std::net::TcpListener, String) {
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = busy.local_addr().unwrap().to_string();
    (busy, address)
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_busy_public_port_releases_the_leases(pool: PgPool) {
    let (_busy, address) = busy_port();
    let stderr = serve_until_exit(&pool, &address, &[]).await;
    assert!(
        stderr.to_lowercase().contains("address already in use"),
        "{stderr}"
    );
    assert_leases_taken_and_released(&pool, &stderr).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_busy_metrics_port_releases_the_leases(pool: PgPool) {
    let (_busy, metrics) = busy_port();
    let stderr = serve_until_exit(
        &pool,
        "127.0.0.1:0",
        &[("GATEWAY_METRICS_ADDR", metrics.as_str())],
    )
    .await;
    assert!(
        stderr.to_lowercase().contains("address already in use"),
        "{stderr}"
    );
    assert_leases_taken_and_released(&pool, &stderr).await;
}

/// A refused partitions setting stops `serve` before it takes any lease.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_refused_partitions_setting_takes_no_lease(pool: PgPool) {
    let stderr = serve_until_exit(
        &pool,
        "127.0.0.1:0",
        &[("GATEWAY_PARTITIONS_AHEAD_MONTHS", "99")],
    )
    .await;
    assert!(
        stderr.contains("GATEWAY_PARTITIONS_AHEAD_MONTHS"),
        "{stderr}"
    );
    let taken: i64 =
        sqlx::query_scalar("SELECT count(*) FROM work_leases WHERE holder IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(taken, 0, "serve took a lease before refusing: {stderr}");
}

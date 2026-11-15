//! Requests lost on a pooled connection the upstream is closing (#483), over
//! real HTTP to a loopback server that closes idle keep-alive connections.
use super::*;
use crate::{
    inference::error::InferenceError, metrics::characterization_tests::fresh_registry,
    providers::framing,
};
use std::{
    sync::atomic::AtomicUsize,
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The server's keep-alive timeout. Every gap and idle timeout below keeps at
/// least a 75 ms margin to it, far above scheduler jitter.
const SERVER_IDLE: Duration = Duration::from_millis(150);
/// A pause between requests that is longer than [`SERVER_IDLE`].
const PAUSE: Duration = Duration::from_millis(300);

const ANSWER: &str = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";

fn loopback() -> Arc<Resolver> {
    resolver_with(Purpose::Provider, Policy::new(true, Networks::default()))
}

async fn read_head(stream: &mut tokio::net::TcpStream) -> String {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// What the server saw.
#[derive(Clone, Default)]
struct Seen {
    connections: Arc<AtomicUsize>,
    served: Arc<AtomicUsize>,
    /// Requests that arrived on a connection idle for longer than
    /// [`SERVER_IDLE`]: the server had already decided to close it, so it
    /// closes without answering (a deterministic stand-in for the race where
    /// the client writes while the upstream's close is in flight).
    dropped: Arc<AtomicUsize>,
}

/// A keep-alive server that closes a connection, without answering, when a
/// request reaches it after `idle` of silence. It never closes proactively,
/// so the client only learns of the close from the lost request.
async fn lazy_close_server(idle: Duration) -> (u16, Seen) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Seen::default();
    let shared = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let seen = shared.clone();
            seen.connections.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut last = Instant::now();
                let mut answered = false;
                loop {
                    if read_head(&mut stream).await.is_empty() {
                        return;
                    }
                    if answered && last.elapsed() > idle {
                        seen.dropped.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                    seen.served.fetch_add(1, Ordering::SeqCst);
                    if stream.write_all(ANSWER.as_bytes()).await.is_err() {
                        return;
                    }
                    answered = true;
                    last = Instant::now();
                }
            });
        }
    });
    (port, seen)
}

/// `rounds` requests, `pause` apart. Returns how many failed.
async fn run(client: &HttpClient, port: u16, rounds: usize, pause: Duration) -> usize {
    let url = format!("http://localhost:{port}/");
    let mut failed = 0;
    for round in 0..rounds {
        if round > 0 {
            tokio::time::sleep(pause).await;
        }
        match client.get(&url).send().await {
            Ok(response) => assert_eq!(response.bytes().await.unwrap().as_ref(), b"ok"),
            Err(_) => failed += 1,
        }
    }
    failed
}

/// A client whose pool keeps an idle connection for `idle`.
fn client_with_idle(idle: Duration) -> HttpClient {
    client_builder(loopback())
        .pool_idle_timeout(idle)
        .build()
        .unwrap()
}

/// What reqwest's own 90 s default (the client before #483) does: the pool
/// keeps the idle connection, so every request after a pause longer than the
/// server's timeout is lost. Every second request here, as measured before
/// the change (6 of 12).
#[tokio::test]
async fn pool_idle_timeout_above_the_servers_loses_the_request_after_each_pause() {
    let (port, seen) = lazy_close_server(SERVER_IDLE).await;
    let client = client_with_idle(Duration::from_secs(90));
    let failed = run(&client, port, 12, PAUSE).await;
    assert_eq!(
        failed, 6,
        "requests 2, 4, ... 12 reuse a closing connection"
    );
    assert_eq!(seen.dropped.load(Ordering::SeqCst), failed);
    assert_eq!(seen.connections.load(Ordering::SeqCst), 6);
}

/// The fix: with the pool's idle timeout below the server's, a connection
/// older than that is dropped at checkout, so no request meets the close.
#[tokio::test]
async fn pool_idle_timeout_below_the_servers_loses_no_request() {
    let (port, seen) = lazy_close_server(SERVER_IDLE).await;
    let client = client_with_idle(Duration::from_millis(75));
    let failed = run(&client, port, 12, PAUSE).await;
    assert_eq!(failed, 0);
    assert_eq!(seen.dropped.load(Ordering::SeqCst), 0);
    assert_eq!(seen.connections.load(Ordering::SeqCst), 12, "one each");
}

/// A shorter idle timeout does not cost reuse while requests keep coming.
#[tokio::test]
async fn steady_load_still_reuses_one_connection() {
    let (port, seen) = lazy_close_server(Duration::from_secs(5)).await;
    let client = client_with_idle(Duration::from_secs(2));
    assert_eq!(run(&client, port, 100, Duration::ZERO).await, 0);
    assert_eq!(seen.connections.load(Ordering::SeqCst), 1);
    assert_eq!(seen.served.load(Ordering::SeqCst), 100);
}

/// The counter counts each lost request once, under the client's label, and
/// the caller's error is the one it was before: `UpstreamUnavailable`.
#[tokio::test]
async fn a_lost_request_is_counted_once_and_keeps_its_error() {
    let (port, _) = lazy_close_server(SERVER_IDLE).await;
    let client = client_with_idle(Duration::from_secs(90));
    let url = format!("http://localhost:{port}/");
    let metrics = fresh_registry();
    let mut lost = 0;
    for round in 0..6 {
        if round > 0 {
            tokio::time::sleep(PAUSE).await;
        }
        let Err(error) = client.get(&url).send().await else {
            continue;
        };
        assert!(UpstreamClient::Local.count_on(metrics, &error));
        lost += 1;
        // The mapping every adapter applies after counting is unchanged.
        assert_eq!(
            framing::transport(error),
            InferenceError::UpstreamUnavailable
        );
    }
    assert_eq!(lost, 3);
    assert_eq!(metrics.upstream_connection_lost_count("local"), 3);
    assert_eq!(metrics.upstream_connection_lost_count("openai"), 0);
}

async fn expect_not_counted(client: &HttpClient, url: &str) {
    let error = match client.get(url).send().await {
        Err(error) => error,
        Ok(response) => response.bytes().await.unwrap_err(),
    };
    assert!(
        !upstream::connection_lost_before_response(&error),
        "{error:?}"
    );
}

/// A server that runs `handler` on every accepted connection.
async fn serve<F, Fut>(handler: F) -> u16
where
    F: Fn(tokio::net::TcpStream) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(handler(stream));
        }
    });
    port
}

async fn expect_counted(client: &HttpClient, url: &str, what: &str) {
    let error = client
        .get(url)
        .send()
        .await
        .expect_err("the request is lost");
    assert!(
        upstream::connection_lost_before_response(&error),
        "{what} is counted: {error:?}"
    );
}

/// Reads the request, then closes with an RST: SO_LINGER 0 is the one way to
/// make the kernel send one.
#[expect(
    deprecated,
    reason = "a test needs a real RST, which only SO_LINGER 0 gives"
)]
async fn reset_after_request(mut stream: tokio::net::TcpStream) {
    read_head(&mut stream).await;
    stream.set_linger(Some(Duration::ZERO)).unwrap();
}

/// Counted: the connection is lost before a complete response head arrived,
/// whatever the upstream had received or done.
#[tokio::test]
async fn a_connection_lost_before_a_complete_head_is_counted() {
    let client = client_with_idle(Duration::from_secs(1));
    // A reset (SO_LINGER 0) after the request: the io error path, not hyper's.
    let reset = serve(reset_after_request).await;
    expect_counted(&client, &format!("http://localhost:{reset}/"), "a reset").await;
    // The upstream closes without reading anything: a fresh connection, no
    // request byte seen.
    let closed = serve(|stream| async move { drop(stream) }).await;
    expect_counted(
        &client,
        &format!("http://localhost:{closed}/"),
        "an accept and close",
    )
    .await;
    // A partial status line, and partial headers, then the close.
    for (name, partial) in [
        ("a partial status line", &b"HTTP/1.1 20"[..]),
        ("partial headers", &b"HTTP/1.1 200 OK\r\ncontent-le"[..]),
    ] {
        let port = serve(move |mut stream| async move {
            read_head(&mut stream).await;
            let _ = stream.write_all(partial).await;
        })
        .await;
        expect_counted(&client, &format!("http://localhost:{port}/"), name).await;
    }
}

/// Not counted: the gateway's own request body failing is the gateway's
/// error, whatever io error kind its stream used (#483 review).
#[tokio::test]
async fn a_request_body_stream_that_fails_is_not_counted() {
    let client = client_with_idle(Duration::from_secs(1));
    let port = serve(|mut stream| async move {
        read_head(&mut stream).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    })
    .await;
    for kind in [
        std::io::ErrorKind::UnexpectedEof,
        std::io::ErrorKind::ConnectionReset,
        std::io::ErrorKind::BrokenPipe,
    ] {
        let failing =
            futures_util::stream::once(
                async move { Err::<Vec<u8>, _>(std::io::Error::from(kind)) },
            );
        let error = client
            .post(format!("http://localhost:{port}/"))
            .body(reqwest::Body::wrap_stream(failing))
            .send()
            .await
            .expect_err("the body fails");
        assert!(
            !upstream::connection_lost_before_response(&error),
            "{kind:?} from the body stream: {error:?}"
        );
    }
}

/// What is not this class: a refused connection, a timeout, a response that
/// breaks after its head, and a response with an error status.
#[tokio::test]
async fn failures_that_are_not_a_lost_request_are_not_counted() {
    let client = client_with_idle(Duration::from_secs(1));
    // Connect failure: nothing listens.
    let free = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed = free.local_addr().unwrap().port();
    drop(free);
    expect_not_counted(&client, &format!("http://localhost:{closed}/")).await;
    // Timeout: the server reads the request and never answers.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_port = silent.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut stream, _)) = silent.accept().await {
            read_head(&mut stream).await;
            held.push(stream);
        }
    });
    let timed = client_builder(loopback())
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    expect_not_counted(&timed, &format!("http://localhost:{silent_port}/")).await;
    // The response head arrives, then the connection dies mid-body: after a
    // response byte, so not this class.
    let cut = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cut_port = cut.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = cut.accept().await {
            read_head(&mut stream).await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nabc")
                .await;
        }
    });
    expect_not_counted(&client, &format!("http://localhost:{cut_port}/")).await;
    // An error status is a response, not an error.
    let (port, _) = server_status().await;
    let response = client.get(format!("http://localhost:{port}/")).send().await;
    assert_eq!(response.unwrap().status(), 503);
}

async fn server_status() -> (u16, ()) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            read_head(&mut stream).await;
            let _ = stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n")
                .await;
        }
    });
    (port, ())
}

/// Each client's idle timeout is positive, below reqwest's 90 s default, and,
/// for the one upstream with a documented default, below it (uvicorn: 5 s).
#[test]
fn every_client_has_a_pool_idle_timeout_below_the_default() {
    let mut labels = std::collections::BTreeSet::new();
    for client in UpstreamClient::ALL {
        let idle = client.pool_idle_timeout();
        assert!(idle >= Duration::from_secs(1) && idle < Duration::from_secs(90));
        assert!(labels.insert(client.as_str()), "labels are unique");
    }
    assert!(UpstreamClient::Local.pool_idle_timeout() < Duration::from_secs(5));
    assert!(UpstreamClient::Openai.pool_idle_timeout() < Duration::from_secs(60));
    assert_eq!(
        labels.into_iter().collect::<Vec<_>>(),
        [
            "anthropic",
            "bedrock",
            "export",
            "local",
            "openai",
            "openrouter",
            "s3"
        ]
    );
}

/// The operator reference names the counter and every label value.
#[test]
fn the_metric_reference_names_the_counter_and_each_client() {
    let reference = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/operations.md"),
    )
    .unwrap();
    let row = reference
        .lines()
        .find(|line| line.starts_with("| `gateway_upstream_connection_lost_total`"))
        .expect("a row in the metric table");
    for client in UpstreamClient::ALL {
        assert!(
            row.contains(&format!("`{}`", client.as_str())),
            "{client:?}"
        );
    }
}

/// Throughput and latency of a reuse-heavy loop, for the default idle timeout
/// and the shortest one in use. Run by hand:
/// `cargo test -p open-model-gateway --lib -- --ignored --nocapture reuse_benchmark`
#[tokio::test]
#[ignore = "benchmark: prints its numbers"]
async fn reuse_benchmark() {
    for (name, idle) in [
        ("idle=90s", Duration::from_secs(90)),
        ("idle=3s", Duration::from_secs(3)),
        ("idle=90s", Duration::from_secs(90)),
        ("idle=3s", Duration::from_secs(3)),
    ] {
        let (port, seen) = lazy_close_server(Duration::from_secs(3600)).await;
        let client = client_with_idle(idle);
        let url = format!("http://localhost:{port}/");
        let mut micros = Vec::new();
        let started = Instant::now();
        for _ in 0..5000 {
            let sent = Instant::now();
            let response = client.get(&url).send().await.unwrap();
            let _ = response.bytes().await.unwrap();
            micros.push(sent.elapsed().as_micros());
        }
        let total = started.elapsed();
        micros.sort_unstable();
        println!(
            "BENCH {name}: {:.0} req/s p50={}us p99={}us connections={}",
            5000.0 / total.as_secs_f64(),
            micros[micros.len() / 2],
            micros[micros.len() * 99 / 100],
            seen.connections.load(Ordering::SeqCst)
        );
        assert_eq!(seen.connections.load(Ordering::SeqCst), 1);
    }
}

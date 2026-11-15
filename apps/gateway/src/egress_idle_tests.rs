//! Requests lost on a pooled connection the upstream is closing (#483), over
//! real HTTP to a loopback server that closes idle keep-alive connections.
use super::*;
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

/// Before #483: the pool keeps an idle connection for reqwest's 90 s default,
/// so every request after a pause longer than the server's timeout is lost.
#[tokio::test]
async fn default_pool_loses_the_request_after_a_pause_longer_than_the_servers_timeout() {
    let (port, seen) = lazy_close_server(SERVER_IDLE).await;
    let client = client_builder(loopback()).build().unwrap();
    let failed = run(&client, port, 12, PAUSE).await;
    println!(
        "BASELINE default pool: failed={failed}/12 connections={} dropped={}",
        seen.connections.load(Ordering::SeqCst),
        seen.dropped.load(Ordering::SeqCst)
    );
    assert_eq!(failed, seen.dropped.load(Ordering::SeqCst));
    assert!(failed >= 5, "expected lost requests, got {failed}");
}

//! Behaviour of the sanctioned client constructor (#509), over real HTTP to
//! loopback test servers reached by the hostname `localhost`, so every
//! request goes through the egress resolver.
use super::*;
use std::sync::atomic::AtomicUsize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Development policy: loopback is allowed, as for a local test upstream.
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

/// A server that answers every request with `answer(request head)` (or,
/// for `None`, closes the connection without a response) and counts the
/// requests it reads.
async fn server(
    answer: impl Fn(&str, u16) -> Option<String> + Send + Sync + 'static,
) -> (u16, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    let answer = Arc::new(answer);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let (counter, answer) = (counter.clone(), answer.clone());
            tokio::spawn(async move {
                let head = read_head(&mut stream).await;
                if head.is_empty() {
                    return;
                }
                counter.fetch_add(1, Ordering::SeqCst);
                if let Some(response) = answer(&head, port) {
                    let _ = stream.write_all(response.as_bytes()).await;
                }
                let _ = stream.shutdown().await;
            });
        }
    });
    (port, requests)
}

const NO_CONTENT: &str =
    "HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

#[tokio::test]
async fn guarded_client_does_not_follow_redirects() {
    let (port, requests) = server(|head, port| {
        Some(if head.starts_with("GET /start ") {
            format!(
                "HTTP/1.1 302 Found\r\nlocation: http://localhost:{port}/landing\r\n\
                 content-length: 0\r\nconnection: close\r\n\r\n"
            )
        } else {
            NO_CONTENT.to_owned()
        })
    })
    .await;
    let url = format!("http://localhost:{port}/start");
    // Control: reqwest's default follows the 302 (two requests).
    let default = reqwest::Client::builder().no_proxy().build().unwrap();
    assert_eq!(default.get(&url).send().await.unwrap().status(), 204);
    assert_eq!(requests.load(Ordering::SeqCst), 2);

    let guarded = client_builder(loopback()).build().unwrap();
    let response = guarded.get(&url).send().await.unwrap();
    assert_eq!(response.status(), 302);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        3,
        "the redirect target was not requested"
    );
}

#[tokio::test]
async fn guarded_client_sends_a_failed_request_once() {
    // Reads the request, then closes the connection without answering.
    let (port, requests) = server(|_, _| None).await;
    let guarded = client_builder(loopback()).build().unwrap();
    let url = format!("http://localhost:{port}/");
    assert!(guarded.post(&url).body("x").send().await.is_err());
    assert!(guarded.get(&url).send().await.is_err());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "one attempt per request"
    );
}

#[tokio::test]
async fn guarded_client_resolves_through_the_egress_policy() {
    let (port, requests) = server(|_, _| Some(NO_CONTENT.to_owned())).await;
    let url = format!("http://localhost:{port}/");
    // Strict (production) policy: loopback is refused before connecting.
    let strict = client_builder(resolver_with(Purpose::Provider, Policy::default()))
        .build()
        .unwrap();
    let error = strict.get(&url).send().await.unwrap_err();
    assert!(error.is_connect(), "{error:?}");
    assert_eq!(requests.load(Ordering::SeqCst), 0, "never connected");
    // The same destination under a policy that allows loopback.
    let allowed = client_builder(loopback()).build().unwrap();
    assert_eq!(allowed.get(&url).send().await.unwrap().status(), 204);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

/// The pinned client of an approved local endpoint (`upstream.invalid` pinned
/// to loopback, development), built through the production approval loader.
fn approved(port: u16) -> crate::providers::local::endpoints::ApprovedEndpoints {
    crate::providers::local::endpoints::ApprovedEndpoints::for_test(&format!(
        "http://upstream.invalid:{port}/v1"
    ))
}

#[tokio::test]
async fn pinned_client_connects_only_to_its_approved_destination() {
    let (port, requests) = server(|_, _| Some(NO_CONTENT.to_owned())).await;
    let metrics = format!("http://upstream.invalid:{port}/metrics");
    // Control: without the pin, `upstream.invalid` never resolves.
    let unpinned = client_builder(loopback()).build().unwrap();
    let error = unpinned.get(&metrics).send().await.unwrap_err();
    assert!(error.is_connect(), "{error:?}");
    assert_eq!(requests.load(Ordering::SeqCst), 0);

    let endpoints = approved(port);
    let (client, url) = endpoints.metrics_endpoint(&metrics).unwrap();
    assert_eq!(client.get(url).send().await.unwrap().status(), 204);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pinned_client_does_not_follow_redirects() {
    let (port, requests) = server(|head, port| {
        Some(if head.starts_with("GET /metrics ") {
            format!(
                "HTTP/1.1 302 Found\r\nlocation: http://upstream.invalid:{port}/landing\r\n\
                 content-length: 0\r\nconnection: close\r\n\r\n"
            )
        } else {
            NO_CONTENT.to_owned()
        })
    })
    .await;
    let endpoints = approved(port);
    let (client, url) = endpoints
        .metrics_endpoint(&format!("http://upstream.invalid:{port}/metrics"))
        .unwrap();
    assert_eq!(client.get(url).send().await.unwrap().status(), 302);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "the redirect target was not requested"
    );
}

/// Set by `guarded_client_ignores_ambient_proxy_variables` for its child.
const PROXY_CHILD: &str = "OMG_TEST_EGRESS_PROXY_CHILD";

/// Proxy variables are read once per process, so this runs the check below
/// in a child process of this test binary with them set to a counting
/// "proxy". The child's control client (reqwest's default) must reach the
/// proxy; the guarded client must go straight to its destination.
#[test]
fn guarded_client_ignores_ambient_proxy_variables() {
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let reached = Arc::new(AtomicUsize::new(0));
    let count = reached.clone();
    std::thread::spawn(move || {
        for stream in proxy.incoming() {
            let Ok(mut stream) = stream else { return };
            count.fetch_add(1, Ordering::SeqCst);
            use std::io::{Read, Write};
            // Read the request head before answering, as a real proxy does.
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream.write_all(
                b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            );
        }
    });
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "egress::client_tests::proxy_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROXY_CHILD, "1")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy");
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        child.env(name, &proxy_url);
    }
    let output = child.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "child failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        reached.load(Ordering::SeqCst),
        1,
        "only the control used the proxy"
    );
}

#[tokio::test]
async fn proxy_child() {
    if std::env::var_os(PROXY_CHILD).is_none() {
        return; // run only by `guarded_client_ignores_ambient_proxy_variables`
    }
    let (port, requests) = server(|_, _| Some(NO_CONTENT.to_owned())).await;
    let url = format!("http://localhost:{port}/");
    // Control: reqwest's default sends the request to the ambient proxy.
    let default = reqwest::Client::builder().build().unwrap();
    assert_eq!(default.get(&url).send().await.unwrap().status(), 502);
    assert_eq!(requests.load(Ordering::SeqCst), 0);

    let guarded = client_builder(loopback()).build().unwrap();
    assert_eq!(guarded.get(&url).send().await.unwrap().status(), 204);
    assert_eq!(requests.load(Ordering::SeqCst), 1);

    // The pinned client of an approved local endpoint goes direct as well.
    let endpoints = approved(port);
    let (pinned, metrics) = endpoints
        .metrics_endpoint(&format!("http://upstream.invalid:{port}/metrics"))
        .unwrap();
    assert_eq!(pinned.get(metrics).send().await.unwrap().status(), 204);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}

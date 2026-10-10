use super::*;
use futures_util::StreamExt;

async fn spawn(config: Config) -> (String, Arc<Mock>) {
    let mock = Mock::new(config).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let served = mock.clone();
    tokio::spawn(async move {
        serve(listener, served, std::future::pending())
            .await
            .unwrap();
    });
    (base, mock)
}

fn fast() -> Config {
    Config {
        latency_ms: 1,
        ttft_ms: 1,
        inter_token_ms: 0,
        completion_tokens: 3,
        prompt_tokens: 7,
        ..Config::default()
    }
}

fn body(nonce: &str, stream: bool, max: u64) -> Value {
    json!({"model":"mock-chat","stream":stream,"max_completion_tokens":max,
           "messages":[{"role":"system","content":"nonce:ignored-earlier"},{"role":"user","content":format!("hello nonce:{nonce} bye")}]})
}

#[test]
fn nonces_are_bounded_and_last_one_wins() {
    let messages = json!([
        {"role":"user","content":"nonce:first"},
        {"role":"user","content":[{"type":"text","text":"x nonce:second-2_b!"}]},
        {"role":"user","content":"nonce: nothing"}
    ]);
    assert_eq!(find_nonce(&messages).as_deref(), Some("second-2_b"));
    let long = json!([{"role":"user","content":format!("nonce:{}", "a".repeat(200))}]);
    assert_eq!(find_nonce(&long).unwrap().len(), MAX_NONCE_CHARS);
    assert_eq!(find_nonce(&json!([{"role":"user","content":"none"}])), None);
}

#[test]
fn error_decisions_are_deterministic_and_near_the_rate() {
    let mock = Mock::new(Config {
        error_rate: 0.1,
        ..Config::default()
    })
    .unwrap();
    let failures = (0..10_000)
        .filter(|i| mock.fails(Some(&format!("n{i}")), 0))
        .count();
    assert!((800..1200).contains(&failures), "{failures}");
    let again = (0..10_000)
        .filter(|i| mock.fails(Some(&format!("n{i}")), 0))
        .count();
    assert_eq!(failures, again);
    assert!(!Mock::new(Config::default()).unwrap().fails(Some("x"), 0));
    assert!(
        Mock::new(Config {
            error_rate: 1.5,
            ..Config::default()
        })
        .is_err()
    );
}

#[tokio::test]
async fn complete_and_streamed_responses_have_deterministic_usage_and_are_recorded() {
    let (base, mock) = spawn(fast()).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/v1/chat/completions");
    let complete: Value = client
        .post(&url)
        .json(&body("a1", false, 2))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(complete["object"], "chat.completion");
    assert_eq!(complete["model"], "mock-chat");
    assert_eq!(complete["usage"]["prompt_tokens"], 7);
    // Capped by max_completion_tokens.
    assert_eq!(complete["usage"]["completion_tokens"], 2);
    assert_eq!(complete["choices"][0]["message"]["content"], "tok tok ");

    let response = client
        .post(&url)
        .json(&body("b2", true, 64))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let text = response.text().await.unwrap();
    let frames: Vec<&str> = text
        .split("\n\n")
        .filter(|f| !f.is_empty())
        .map(|f| f.strip_prefix("data: ").unwrap())
        .collect();
    assert_eq!(frames.len(), 3 + 2 + 1, "{text}");
    assert_eq!(*frames.last().unwrap(), "[DONE]");
    let deltas: Vec<Value> = frames[..frames.len() - 1]
        .iter()
        .map(|f| serde_json::from_str(f).unwrap())
        .collect();
    assert_eq!(deltas[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(deltas[3]["choices"][0]["finish_reason"], "stop");
    assert_eq!(deltas[4]["usage"]["completion_tokens"], 3);
    assert_eq!(deltas[4]["usage"]["total_tokens"], 10);

    let calls = mock.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].nonce.as_deref(), Some("a1"));
    assert_eq!(calls[0].completion_tokens, 2);
    assert!(calls.iter().all(|c| c.completed && c.status == 200));
    assert!(calls[1].stream);
    let stats: Value = client
        .get(format!("{base}/__mock/stats"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stats["calls"], 2);
    assert_eq!(stats["config"]["prompt_tokens"], 7);
    client
        .post(format!("{base}/__mock/reset"))
        .send()
        .await
        .unwrap();
    assert!(mock.calls().is_empty());
}

#[tokio::test]
async fn injected_errors_and_bad_requests_are_explicit() {
    let (base, mock) = spawn(Config {
        error_rate: 1.0,
        error_status: 503,
        ..fast()
    })
    .await;
    let client = reqwest::Client::new();
    let url = format!("{base}/v1/chat/completions");
    let response = client
        .post(&url)
        .json(&body("e1", true, 8))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "mock_injected_error");
    let bad = client.post(&url).body("{").send().await.unwrap();
    assert_eq!(bad.status(), 400);
    let calls = mock.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].status, 503);
    assert_eq!(mock.stats().errors, 1);
}

#[tokio::test]
async fn a_dropped_stream_is_recorded_as_incomplete() {
    let (base, mock) = spawn(Config {
        ttft_ms: 1,
        inter_token_ms: 200,
        completion_tokens: 50,
        ..fast()
    })
    .await;
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body("d1", true, 64))
        .send()
        .await
        .unwrap();
    let mut chunks = response.bytes_stream();
    assert!(chunks.next().await.unwrap().is_ok());
    drop(chunks);
    for _ in 0..100 {
        if !mock.calls().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let calls = mock.calls();
    assert_eq!(calls.len(), 1);
    assert!(!calls[0].completed);
    assert_eq!(mock.stats().in_flight, 0);
}

#[tokio::test]
async fn serves_many_concurrent_calls() {
    let (base, mock) = spawn(Config {
        latency_ms: 50,
        ..fast()
    })
    .await;
    let client = reqwest::Client::new();
    let started = Instant::now();
    let requests = (0..500).map(|i| {
        let client = client.clone();
        let url = format!("{base}/v1/chat/completions");
        async move {
            client
                .post(url)
                .json(&body(&format!("c{i}"), i % 2 == 0, 8))
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
        }
    });
    futures_util::future::join_all(requests).await;
    // 500 concurrent 50 ms calls finish in far less than 500 x 50 ms.
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(mock.calls().len(), 500);
}

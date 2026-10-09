//! Realtime contract over a real listener, PostgreSQL and a mock upstream
//! WebSocket (no paid calls): settlement per response, unknown usage,
//! disconnect cancellation, budget exhaustion, unsupported events, auth,
//! concurrency and privacy.
use std::{sync::Arc, sync::atomic::Ordering, time::Duration};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{self, Message, client::IntoClientRequest},
};
use uuid::Uuid;

use crate::{
    bootstrap::{self, DevelopmentKeys},
    config::Environment,
    http,
    inference::{Engine, EngineLimits, realtime::RealtimeLimits},
    providers::{
        ProviderRegistry,
        openai::realtime::{
            mock::{Init, Mock, Reply, SERVER_SECRET, UPSTREAM_MODEL, usage},
            test_adapter,
        },
        secrets::{Secret, SecretResolver},
    },
    store::Store,
};

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;
const AUDIO: &str = "U0VDUkVUQVVESU9QQVlMT0FE";
const INSTRUCTIONS: &str = "never store this private instruction";
/// One window: input ceiling 1000, output 100 →
/// 1000×($4 + $0.40 + $32 + $0.40)/M + 100×($16 + $64)/M = 44,800 µUSD.
const WINDOW: i64 = 44_800;
/// usage(10 text incl. 4 cached, 20 audio incl. 5 cached, 10 text out, 40 audio out):
/// 6×4 + ceil(4×0.4) + 10×16 + 15×32 + ceil(5×0.4) + 40×64 = 3,228 µUSD.
const RESPONSE: i64 = 3_228;
/// Upper bounds that only turn a hang into a failure (never a timing
/// assumption): generous so a loaded machine cannot trip them.
const EVENT_BOUND: Duration = Duration::from_secs(30);
const SETTLE_BOUND: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(20);

struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<Secret, crate::inference::error::InferenceError> {
        Secret::new(SERVER_SECRET.into())
    }
}

struct Gateway {
    addr: std::net::SocketAddr,
    keys: DevelopmentKeys,
    mock: Mock,
    pool: PgPool,
}
fn line(meter: &str, amount: &str) -> Value {
    json!({"meter":meter,"microusd_per_batch":amount,"batch":1000000,"unit_label":"/M tokens","sku_label":meter})
}
async fn gateway(pool: PgPool, init: Init, replies: Vec<Reply>, limits: RealtimeLimits) -> Gateway {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE provider_connections SET enabled=true")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled=true,upstream_model=$1")
        .bind(UPSTREAM_MODEL)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['realtime']")
        .execute(&pool)
        .await
        .unwrap();
    let na = |m: &str| json!({"meter":m,"not_applicable":true});
    let lines = json!([
        line("input_tokens", "4000000"),
        line("cache_read_tokens", "400000"),
        line("output_tokens", "16000000"),
        line("input_audio_tokens", "32000000"),
        line("cache_read_audio_tokens", "400000"),
        line("output_audio_tokens", "64000000"),
        na("cache_write_tokens"), na("cache_write_5m_tokens"), na("cache_write_1h_tokens"),
        na("output_images"), na("input_characters"), na("input_audio_seconds_ms"),
        na("output_audio_seconds_ms"), na("search_units"),
        {"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"}
    ]);
    let deployment: Uuid = sqlx::query_scalar("SELECT id FROM deployments")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,1000,100,3,$3,'{}')")
        .bind(Uuid::new_v4()).bind(deployment).bind(lines).execute(&pool).await.unwrap();
    let mock = Mock::start(init, replies).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(test_adapter(
            Arc::new(Resolver),
            mock.base.clone(),
        )))
        .unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default())
        .unwrap()
        .with_realtime_limits(limits)
        .unwrap();
    let router = http::router_with_engine(store, None, engine);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Gateway {
        addr,
        keys,
        mock,
        pool,
    }
}
impl Gateway {
    fn request(&self, model: &str) -> tungstenite::handshake::client::Request {
        format!(
            "ws://{}/v1/realtime?model={}",
            self.addr,
            model.replace('/', "%2F")
        )
        .into_client_request()
        .unwrap()
    }
    fn key(&self) -> &str {
        &self.keys.team_key.token
    }
    async fn connect(&self) -> Client {
        let mut request = self.request("company/smart");
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", self.key()).parse().unwrap(),
        );
        let (client, _) = connect_async(request).await.unwrap();
        client
    }
    async fn refused(&self, request: tungstenite::handshake::client::Request) -> u16 {
        match connect_async(request).await {
            Err(tungstenite::Error::Http(response)) => response.status().as_u16(),
            other => panic!("expected an HTTP refusal, got {:?}", other.map(|_| ())),
        }
    }
    /// The latest realtime session once every admitted realtime attempt is
    /// terminal. Waiting for all of them matters: finalization is detached
    /// from the client's close, so an earlier finished session must never
    /// stand in for a later one that is still settling.
    async fn finished(&self) -> Value {
        let deadline = tokio::time::Instant::now() + SETTLE_BOUND;
        loop {
            let row: Option<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',e.id,'state',e.state,'error_code',e.error_code,'finish_reason',e.finish_reason,'input_tokens',e.input_tokens,'output_tokens',e.output_tokens,'meter_usage',e.meter_usage,'streamed',e.streamed,'reservation',r.state,'actual',r.actual_microusd,'held',r.held_microusd,'unbounded',r.unbounded_cost,'components',r.cost_components,'reserved_tokens',r.reserved_tokens) FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.workload_kind='realtime' AND NOT EXISTS(SELECT 1 FROM inference_executions o WHERE o.workload_kind='realtime' AND o.state='started') ORDER BY e.started_at DESC LIMIT 1")
                .fetch_optional(&self.pool).await.unwrap();
            if let Some(row) = row {
                return row;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "session never finished"
            );
            tokio::time::sleep(POLL).await;
        }
    }
    async fn responses(&self, id: &str) -> Vec<Value> {
        sqlx::query_scalar("SELECT jsonb_build_object('sequence',sequence,'state',state,'status',status,'window',window_hold_microusd,'actual',actual_microusd,'input_audio_tokens',input_audio_tokens) FROM realtime_responses WHERE execution_id=$1::uuid ORDER BY sequence")
            .bind(id).fetch_all(&self.pool).await.unwrap()
    }
    async fn ledger(&self, id: &str) -> Vec<(String, Option<i64>)> {
        sqlx::query_as("SELECT kind,amount_microusd FROM monetary_ledger WHERE execution_id=$1::uuid ORDER BY created_at")
            .bind(id).fetch_all(&self.pool).await.unwrap()
    }
}
async fn next(client: &mut Client) -> Value {
    loop {
        match tokio::time::timeout(EVENT_BOUND, client.next())
            .await
            .unwrap()
        {
            Some(Ok(Message::Text(t))) => return serde_json::from_str(t.as_str()).unwrap(),
            Some(Ok(Message::Close(frame))) => {
                return json!({"type":"close","code":frame.as_ref().map(|f| u16::from(f.code)),"reason":frame.map(|f| f.reason.to_string())});
            }
            Some(Ok(_)) => continue,
            other => {
                return json!({"type":"gone","detail":format!("{:?}", other.map(|r| r.map(|_| ())))});
            }
        }
    }
}
async fn until(client: &mut Client, kind: &str) -> Value {
    for _ in 0..20 {
        let event = next(client).await;
        if event["type"] == kind {
            return event;
        }
        assert!(
            event["type"] != "close" && event["type"] != "gone",
            "{event}"
        );
    }
    panic!("no {kind}");
}
async fn send(client: &mut Client, event: Value) {
    client
        .send(Message::Text(event.to_string().into()))
        .await
        .unwrap();
}
async fn opened(client: &mut Client) {
    let created = until(client, "session.created").await;
    assert_eq!(
        created["session"]["model"], "company/smart",
        "public alias, never the upstream id"
    );
    let updated = until(client, "session.updated").await;
    assert_eq!(
        updated["session"]["audio"]["input"]["turn_detection"]["create_response"],
        false
    );
}
async fn respond(client: &mut Client) -> Value {
    send(
        client,
        json!({"type":"response.create","response":{"instructions":INSTRUCTIONS}}),
    )
    .await;
    until(client, "response.created").await;
    until(client, "response.done").await
}
fn limits() -> RealtimeLimits {
    RealtimeLimits::default()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn responses_settle_individually_and_the_session_sums_them(pool: PgPool) {
    let u = usage(10, 4, 20, 5, 10, 40);
    let g = gateway(
        pool,
        Init::Safe,
        vec![Reply::Done(u.clone()), Reply::Done(u)],
        limits(),
    )
    .await;
    let mut client = g.connect().await;
    opened(&mut client).await;
    send(
        &mut client,
        json!({"type":"input_audio_buffer.append","audio":AUDIO}),
    )
    .await;
    send(&mut client, json!({"type":"input_audio_buffer.commit"})).await;
    let done = respond(&mut client).await;
    assert_eq!(done["response"]["usage"]["output_tokens"], 50);
    respond(&mut client).await;
    client.close(None).await.unwrap();
    let row = g.finished().await;
    assert_eq!(row["state"], "succeeded");
    assert_eq!(row["finish_reason"], "stop");
    assert_eq!(row["streamed"], true);
    assert_eq!(
        (row["input_tokens"].as_i64(), row["output_tokens"].as_i64()),
        (Some(60), Some(100))
    );
    assert_eq!(row["meter_usage"]["requests"], "2");
    assert_eq!(row["reservation"], "settled");
    assert_eq!(row["actual"], 2 * RESPONSE);
    assert_eq!(row["components"].as_object().unwrap().len(), 15);
    assert_eq!(row["components"]["output_audio_tokens_microusd"], "5120");
    // The second response extended the session by one window.
    assert_eq!(row["reserved_tokens"], 2 * 1100);
    let id = row["id"].as_str().unwrap();
    let responses = g.responses(id).await;
    assert_eq!(responses.len(), 2);
    for r in &responses {
        assert_eq!(
            (r["state"].as_str(), r["status"].as_str()),
            (Some("settled"), Some("completed"))
        );
        assert_eq!(
            (r["window"].as_i64(), r["actual"].as_i64()),
            (Some(WINDOW), Some(RESPONSE))
        );
        assert_eq!(r["input_audio_tokens"], 20);
    }
    assert_eq!(
        g.ledger(id).await,
        [
            ("hold".into(), Some(WINDOW)),
            ("settlement".into(), Some(2 * RESPONSE))
        ]
    );
    // The upstream got explicit per-response ceilings and never a client key.
    let creates = g.mock.shared.received_of("response.create");
    assert_eq!(creates.len(), 2);
    assert!(
        creates
            .iter()
            .all(|c| c["response"]["max_output_tokens"] == 100)
    );
    let handshakes = g.mock.shared.handshakes.lock().unwrap().clone();
    assert_eq!(handshakes.len(), 1);
    assert!(
        handshakes[0]
            .0
            .ends_with(&format!("model={UPSTREAM_MODEL}"))
    );
    assert!(
        handshakes[0]
            .1
            .iter()
            .any(|(k, v)| k == "authorization" && v == &format!("Bearer {SERVER_SECRET}"))
    );
    let seen = format!("{handshakes:?}{:?}", g.mock.shared.received.lock().unwrap());
    assert!(!seen.contains(g.key()) && !seen.contains("omg_"));
    // Privacy: nothing of the conversation is stored.
    let stored: String = sqlx::query_scalar("SELECT concat((SELECT string_agg(row_to_json(e)::text,'') FROM inference_executions e),(SELECT string_agg(row_to_json(r)::text,'') FROM governance_reservations r),(SELECT string_agg(row_to_json(l)::text,'') FROM monetary_ledger l),(SELECT string_agg(row_to_json(x)::text,'') FROM realtime_responses x),(SELECT string_agg(row_to_json(a)::text,'') FROM audit_events a))")
        .fetch_one(&g.pool).await.unwrap();
    for secret in [AUDIO, INSTRUCTIONS, "resp_1", "sess_private", SERVER_SECRET] {
        assert!(!stored.contains(secret), "{secret} stored");
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn unknown_usage_retains_the_window_hold(pool: PgPool) {
    let g = gateway(pool, Init::Safe, vec![Reply::NoUsage], limits()).await;
    let mut client = g.connect().await;
    opened(&mut client).await;
    respond(&mut client).await;
    client.close(None).await.unwrap();
    let row = g.finished().await;
    assert_eq!(row["state"], "succeeded");
    assert_eq!(row["reservation"], "unknown");
    assert_eq!(row["actual"], Value::Null);
    assert_eq!(row["held"], WINDOW);
    assert_eq!(
        row["input_tokens"],
        Value::Null,
        "unknown usage is never zero"
    );
    let id = row["id"].as_str().unwrap();
    assert_eq!(g.responses(id).await[0]["state"], "unknown");
    assert_eq!(g.ledger(id).await[1], ("unknown".into(), Some(0)));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn client_disconnect_closes_upstream_and_keeps_the_hold(pool: PgPool) {
    let g = gateway(pool, Init::Safe, vec![Reply::Hang], limits()).await;
    let mut client = g.connect().await;
    opened(&mut client).await;
    send(&mut client, json!({"type":"response.create"})).await;
    until(&mut client, "response.created").await;
    drop(client);
    assert!(
        g.mock.shared.wait_closed().await,
        "upstream closed on disconnect"
    );
    let row = g.finished().await;
    assert_eq!(row["state"], "cancelled");
    assert_eq!(row["finish_reason"], "cancelled");
    assert_eq!(row["reservation"], "unknown");
    assert_eq!(row["held"], WINDOW);
    let responses = g.responses(row["id"].as_str().unwrap()).await;
    assert_eq!(
        (responses.len(), responses[0]["state"].as_str()),
        (1, Some("unknown"))
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn budget_exhaustion_sends_an_error_event_and_closes(pool: PgPool) {
    let u = usage(10, 4, 20, 5, 10, 40);
    let g = gateway(
        pool,
        Init::Safe,
        vec![Reply::Done(u.clone()), Reply::Done(u)],
        limits(),
    )
    .await;
    // Admits one window; after one settled response a second window no longer fits.
    crate::governance::set_test_budget(
        &g.pool,
        "local",
        None,
        Some(g.keys.team_workspace_id),
        None,
        "month",
        Some(WINDOW + 1000),
    )
    .await;
    let mut client = g.connect().await;
    opened(&mut client).await;
    respond(&mut client).await;
    send(
        &mut client,
        json!({"type":"response.create","event_id":"second"}),
    )
    .await;
    let error = until(&mut client, "error").await;
    assert_eq!(error["error"]["code"], "budget_exceeded");
    assert_eq!(error["error"]["type"], "insufficient_quota");
    let close = next(&mut client).await;
    assert_eq!(
        (close["type"].as_str(), close["code"].as_u64()),
        (Some("close"), Some(1008))
    );
    assert_eq!(
        g.mock.shared.received_of("response.create").len(),
        1,
        "denied request never forwarded"
    );
    let row = g.finished().await;
    assert_eq!(
        (row["state"].as_str(), row["error_code"].as_str()),
        (Some("failed"), Some("budget_exceeded"))
    );
    assert_eq!(row["reservation"], "settled");
    assert_eq!(row["actual"], RESPONSE);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn unsupported_events_are_refused_with_an_error_and_never_forwarded(pool: PgPool) {
    let g = gateway(pool, Init::Safe, vec![], limits()).await;
    for event in [
        json!({"type":"transcription_session.update","session":{}}),
        json!({"type":"session.update","session":{"type":"realtime","audio":{"input":{"turn_detection":{"type":"server_vad","create_response":true}}}}}),
    ] {
        let mut client = g.connect().await;
        opened(&mut client).await;
        send(&mut client, event.clone()).await;
        let error = until(&mut client, "error").await;
        assert!(
            error["error"]["code"] == "unsupported_event"
                || error["error"]["code"] == "unsupported_capability",
            "{error}"
        );
        let close = next(&mut client).await;
        assert_eq!(close["code"], 1008);
    }
    let types = g.mock.shared.received_types();
    assert!(types.iter().all(|t| t == "session.update"), "{types:?}");
    assert_eq!(
        g.mock.shared.received_of("session.update").len(),
        2,
        "only the gateway's own updates"
    );
    let row = g.finished().await;
    assert_eq!(
        (row["state"].as_str(), row["error_code"].as_str()),
        (Some("failed"), Some("unsupported_capability"))
    );
    // No response ran: a known zero, not a fabricated success with charges.
    assert_eq!(
        (row["reservation"].as_str(), row["actual"].as_i64()),
        (Some("settled"), Some(0))
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn handshake_auth_and_capability_failures_are_http_errors(pool: PgPool) {
    let g = gateway(pool, Init::Safe, vec![], limits()).await;
    // No key, both forms, an invalid key, a header key with x-api-key.
    assert_eq!(g.refused(g.request("company/smart")).await, 401);
    let mut both = g.request("company/smart");
    both.headers_mut().insert(
        "authorization",
        format!("Bearer {}", g.key()).parse().unwrap(),
    );
    both.headers_mut().insert(
        "sec-websocket-protocol",
        format!("realtime, openai-insecure-api-key.{}", g.key())
            .parse()
            .unwrap(),
    );
    assert_eq!(g.refused(both).await, 401);
    let mut bad = g.request("company/smart");
    bad.headers_mut()
        .insert("authorization", "Bearer omg_invalid".parse().unwrap());
    assert_eq!(g.refused(bad).await, 401);
    // The browser subprotocol form works and only `realtime` is echoed.
    let mut browser = g.request("company/smart");
    browser.headers_mut().insert(
        "sec-websocket-protocol",
        format!("realtime, openai-insecure-api-key.{}", g.key())
            .parse()
            .unwrap(),
    );
    let (mut client, response) = connect_async(browser).await.unwrap();
    assert_eq!(response.headers()["sec-websocket-protocol"], "realtime");
    opened(&mut client).await;
    client.close(None).await.unwrap();
    // Beta interface, unknown subprotocols and query parameters are explicit 400s.
    let with_key = |mut r: tungstenite::handshake::client::Request, protocols: &str| {
        r.headers_mut().insert(
            "sec-websocket-protocol",
            format!("{protocols}, openai-insecure-api-key.{}", g.key())
                .parse()
                .unwrap(),
        );
        r
    };
    assert_eq!(
        g.refused(with_key(
            g.request("company/smart"),
            "realtime, openai-beta.realtime-v1"
        ))
        .await,
        400
    );
    let mut beta = with_key(g.request("company/smart"), "realtime");
    beta.headers_mut()
        .insert("openai-beta", "realtime=v1".parse().unwrap());
    assert_eq!(g.refused(beta).await, 400);
    let call = format!(
        "ws://{}/v1/realtime?model=company%2Fsmart&call_id=x",
        g.addr
    )
    .into_client_request()
    .unwrap();
    assert_eq!(g.refused(with_key(call, "realtime")).await, 400);
    // Unknown model 404; a model without the realtime protocol is unsupported.
    assert_eq!(
        g.refused(with_key(g.request("company/missing"), "realtime"))
            .await,
        404
    );
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions']")
        .execute(&g.pool)
        .await
        .unwrap();
    assert_eq!(
        g.refused(with_key(g.request("company/smart"), "realtime"))
            .await,
        400
    );
    assert_eq!(
        g.mock.shared.connections.load(Ordering::SeqCst),
        1,
        "refusals never reach upstream"
    );
    // Client secrets / WebRTC are explicit and authenticated.
    let client = reqwest::Client::new();
    let url = format!("http://{}/v1/realtime/client_secrets", g.addr);
    assert_eq!(client.post(&url).send().await.unwrap().status(), 401);
    let response = client.post(&url).bearer_auth(g.key()).send().await.unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "unsupported_capability"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_session_is_one_in_flight_request_and_upstream_failures_are_not_clean(pool: PgPool) {
    let g = gateway(pool, Init::Safe, vec![], limits()).await;
    sqlx::query(
        "INSERT INTO workspace_local_policies(workspace_id,concurrent_requests) VALUES($1,1)",
    )
    .bind(g.keys.team_workspace_id)
    .execute(&g.pool)
    .await
    .unwrap();
    let mut first = g.connect().await;
    opened(&mut first).await;
    let mut second = g.request("company/smart");
    second.headers_mut().insert(
        "authorization",
        format!("Bearer {}", g.key()).parse().unwrap(),
    );
    assert_eq!(g.refused(second).await, 429);
    first.close(None).await.unwrap();
    g.finished().await;
    // The slot is free again once the session ended.
    let mut again = g.connect().await;
    opened(&mut again).await;
    drop(again);
    // An upstream configuration rejection fails before the upgrade.
    sqlx::query("DELETE FROM workspace_local_policies")
        .execute(&g.pool)
        .await
        .unwrap();
    *g.mock.shared.init.lock().unwrap() = Some(Init::Reject);
    let mut rejected = g.request("company/smart");
    rejected.headers_mut().insert(
        "authorization",
        format!("Bearer {}", g.key()).parse().unwrap(),
    );
    assert_eq!(g.refused(rejected).await, 400);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_rejected_request_keeps_its_window_for_the_next_one(pool: PgPool) {
    let g = gateway(
        pool,
        Init::Safe,
        vec![Reply::Reject, Reply::Done(usage(10, 4, 20, 5, 10, 40))],
        limits(),
    )
    .await;
    let mut client = g.connect().await;
    opened(&mut client).await;
    send(
        &mut client,
        json!({"type":"response.create","event_id":"first"}),
    )
    .await;
    let error = until(&mut client, "error").await;
    assert_eq!(error["error"]["event_id"], "first");
    assert_eq!(
        error["error"]["code"],
        "conversation_already_has_active_response"
    );
    assert_eq!(
        error["error"]["message"], "The provider reported an error for this session",
        "upstream text is never forwarded"
    );
    respond(&mut client).await;
    client.close(None).await.unwrap();
    let row = g.finished().await;
    assert_eq!(row["reservation"], "settled");
    assert_eq!(row["actual"], RESPONSE);
    assert_eq!(
        row["reserved_tokens"], 1100,
        "the rejected request's window was reused, not extended"
    );
    assert_eq!(g.responses(row["id"].as_str().unwrap()).await.len(), 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn event_rate_and_message_size_caps_close_with_an_error(pool: PgPool) {
    let tight = RealtimeLimits {
        max_events_per_second: 1,
        max_message_bytes: 1024,
        ..RealtimeLimits::default()
    };
    let g = gateway(pool, Init::Safe, vec![], tight).await;
    let mut client = g.connect().await;
    opened(&mut client).await;
    for _ in 0..4 {
        send(&mut client, json!({"type":"input_audio_buffer.clear"})).await;
    }
    let error = until(&mut client, "error").await;
    assert_eq!(error["error"]["code"], "rate_limit_error");
    let close = next(&mut client).await;
    assert_eq!(close["code"], 1008, "{close}");
    let row = g.finished().await;
    assert_eq!(
        (row["state"].as_str(), row["error_code"].as_str()),
        (Some("failed"), Some("rate_limit_error"))
    );
    let mut client = g.connect().await;
    opened(&mut client).await;
    let _ = client
        .send(Message::Text(
            json!({"type":"input_audio_buffer.append","audio":"A".repeat(4096)})
                .to_string()
                .into(),
        ))
        .await;
    let error = until(&mut client, "error").await;
    assert_eq!(error["error"]["code"], "message_too_big");
    // Wait for the oversized session to settle before checking upstream.
    assert_eq!(g.finished().await["error_code"], "invalid_request_error");
    assert!(
        g.mock
            .shared
            .received_of("input_audio_buffer.append")
            .is_empty(),
        "oversized events never reach upstream"
    );
}

//! #486 end to end: the real Bedrock adapter against a mock Bedrock endpoint
//! that answers 200 and then sends an exception frame, behind the real engine
//! and HTTP routers on PostgreSQL. The response stays a 200 that ends in an
//! error (the gateway does not wait for content and does not fail over after a
//! stream is returned); what the exception type decides is the class: the
//! terminal error each client protocol sees, the recorded execution, the
//! reservation, and the routing health observation.
use std::sync::{Arc, Mutex};

use axum::{Router, body::Body, http::Response, http::Uri};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::{events, frame, wire};
use crate::{
    billing::v3::Meter,
    governance::tests::db::{Fixture, fixture},
    inference::admission_validation_tests::{
        add_deployment, add_unpriced_deployment, issue_key, serve, start_gateway,
    },
    providers::{ProviderRegistry, bedrock::BedrockAdapter},
};

fn eventstream(frames: Vec<u8>) -> Response<Body> {
    Response::builder()
        .header("content-type", "application/vnd.amazon.eventstream")
        .body(Body::from(frames))
        .unwrap()
}
/// One mock endpoint whose every ConverseStream answers 200 with `frames`.
fn mock(frames: Vec<u8>) -> Router {
    Router::new().fallback(move || {
        let frames = frames.clone();
        async move { eventstream(frames) }
    })
}

/// A streamed request; returns the status, the request id and the whole
/// response text.
async fn stream(base: &str, token: &str, messages: bool) -> (u16, Uuid, String) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let request = if messages {
        client.post(format!("{base}/v1/messages")).json(&json!({
            "model":"company/smart","max_tokens":16,"stream":true,
            "messages":[{"role":"user","content":"hi"}]
        }))
    } else {
        client
            .post(format!("{base}/v1/chat/completions"))
            .json(&json!({
                "model":"company/smart","stream":true,
                "messages":[{"role":"user","content":"hi"}]
            }))
    };
    let response = request
        .bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    (status, id, response.text().await.unwrap())
}

/// The last `data:` payload of an SSE body that is JSON.
fn last_event(body: &str) -> Value {
    body.lines()
        .rev()
        .find_map(|line| line.strip_prefix("data:"))
        .and_then(|data| serde_json::from_str(data.trim()).ok())
        .unwrap_or_else(|| panic!("no JSON event in {body}"))
}

/// A pricing v3 version that is explicitly free: zero rates on the token and
/// request meters and every other meter not applicable, the shape a free
/// pre-processing rejection settles against (`governance::free_rejection`).
async fn free_price(pool: &PgPool, deployment: Uuid) {
    let lines: Vec<Value> = Meter::ALL
        .into_iter()
        .map(|m| match m {
            Meter::InputTokens | Meter::OutputTokens | Meter::Requests | Meter::SearchUnits => {
                let batch = m.batches()[0];
                json!({"meter":m.as_str(),"microusd_per_batch":"0","batch":batch,"unit_label":m.unit_label(batch).unwrap(),"sku_label":"Free"})
            }
            m => json!({"meter":m.as_str(),"not_applicable":true}),
        })
        .collect();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,100,50,3,$3,'{}')")
        .bind(Uuid::new_v4()).bind(deployment).bind(json!(lines)).execute(pool).await.unwrap();
}

/// A gateway over one Bedrock deployment (v1-priced, or explicitly free v3)
/// whose mock answers every stream with `frames`.
struct Gateway {
    f: Fixture,
    deployment: Uuid,
    token: String,
    base: String,
    _servers: Vec<crate::inference::admission_validation_tests::Server>,
}
async fn gateway(pool: PgPool, free: bool, frames: Vec<u8>) -> Gateway {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions','messages']")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let deployment = if free {
        let d = add_unpriced_deployment(
            &f,
            "bedrock",
            "aws:default",
            Some("us-east-1"),
            "anthropic.claude-test",
            0,
        )
        .await;
        free_price(&f.store.pool, d).await;
        d
    } else {
        add_deployment(
            &f,
            "bedrock",
            "aws:default",
            Some("us-east-1"),
            "anthropic.claude-test",
            0,
        )
        .await
    };
    let (upstream, upstream_server) = serve(mock(frames)).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(BedrockAdapter::for_test(&upstream)))
        .unwrap();
    let (base, server) = start_gateway(&f, registry).await;
    Gateway {
        f,
        deployment,
        token,
        base,
        _servers: vec![upstream_server, server],
    }
}

/// What one request left behind.
#[derive(Debug, PartialEq)]
struct Recorded {
    state: String,
    error_code: Option<String>,
    input: Option<i64>,
    output: Option<i64>,
    reservation: String,
    actual: Option<i64>,
    ledger: Vec<String>,
    health_failures: i64,
}
/// state, error code, input and output tokens, reservation state, actual cost, id.
type Row = (
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    String,
    Option<i64>,
    Uuid,
);
async fn recorded(pool: &PgPool, deployment: Uuid, request: Uuid) -> Recorded {
    let (state, error_code, input, output, reservation, actual, id): Row = sqlx::query_as(
        "SELECT e.state,e.error_code,e.input_tokens,e.output_tokens,r.state,r.actual_microusd,e.id FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.root_request_id=$1",
    )
    .bind(request)
    .fetch_one(pool)
    .await
    .unwrap();
    let ledger = sqlx::query_scalar::<_, String>(
        "SELECT kind FROM monetary_ledger WHERE execution_id=$1 ORDER BY kind",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap();
    let health_failures: i64 = sqlx::query_scalar(
        "SELECT COALESCE((SELECT consecutive_failures FROM deployment_health WHERE deployment_id=$1),0)::bigint",
    )
    .bind(deployment)
    .fetch_one(pool)
    .await
    .unwrap();
    Recorded {
        state,
        error_code,
        input,
        output,
        reservation,
        actual,
        ledger,
        health_failures,
    }
}

#[derive(Clone, Copy, Debug)]
enum Shape {
    BeforeContent,
    AfterContent,
    AfterUsage,
}
fn frames_of(kind: &str, shape: Shape) -> Vec<u8> {
    let mut frames = match shape {
        Shape::BeforeContent => Vec::new(),
        Shape::AfterContent => wire(events(false)[..2].to_vec()),
        Shape::AfterUsage => wire(events(false)),
    };
    frames.extend(frame(
        kind,
        "exception",
        json!({"message":"upstream secret prompt"}),
    ));
    frames
}

/// Every exception type, each protocol, each place in the stream: the 200
/// response ends in the type's own error, the execution failed, the
/// reservation is `unknown` with nothing settled, usage the stream did not
/// report stays NULL (never 0), and the routing health follows the class.
/// On a free price too: a rejection inside a returned stream is not a
/// rejection before processing, so it never settles as a known zero.
async fn matrix(pool: PgPool, free: bool) {
    // (exception type, chat `error.code`, Messages `error.type`, counts
    // against routing health)
    let cases = [
        (
            "throttlingException",
            "rate_limit_error",
            "rate_limit_error",
            true,
        ),
        (
            "validationException",
            "upstream_rejected",
            "invalid_request_error",
            false,
        ),
        (
            "serviceUnavailableException",
            "upstream_unavailable",
            "overloaded_error",
            true,
        ),
        (
            "internalServerException",
            "upstream_unavailable",
            "overloaded_error",
            true,
        ),
        (
            "modelStreamErrorException",
            "upstream_unavailable",
            "overloaded_error",
            true,
        ),
        ("modelTimeoutException", "timeout_error", "api_error", true),
        (
            "someFutureException",
            "upstream_unavailable",
            "overloaded_error",
            true,
        ),
    ];
    let mut g = gateway(pool, free, Vec::new()).await;
    let mut problems = Vec::new();
    for (kind, chat_code, messages_type, health) in cases {
        for shape in [Shape::BeforeContent, Shape::AfterContent, Shape::AfterUsage] {
            // A new mock per case, behind the one gateway.
            let (upstream, _upstream) = serve(mock(frames_of(kind, shape))).await;
            let mut registry = ProviderRegistry::default();
            registry
                .register(Arc::new(BedrockAdapter::for_test(&upstream)))
                .unwrap();
            let (base, _server) = start_gateway(&g.f, registry).await;
            g.base = base;
            for messages in [false, true] {
                sqlx::query("DELETE FROM deployment_health")
                    .execute(&g.f.store.pool)
                    .await
                    .unwrap();
                let (status, request, body) = stream(&g.base, &g.token, messages).await;
                let error = last_event(&body);
                let class = if messages {
                    error["error"]["type"].as_str().unwrap().to_owned()
                } else {
                    error["error"]["code"].as_str().unwrap().to_owned()
                };
                let got = recorded(&g.f.store.pool, g.deployment, request).await;
                let execution_code = match (kind, messages) {
                    ("throttlingException", _) => "rate_limit_error",
                    ("validationException", _) => "upstream_rejected",
                    ("serviceUnavailableException", true) => "overloaded_error",
                    ("modelTimeoutException", _) => "timeout_error",
                    _ => "upstream_unavailable",
                };
                // Tokens the stream reported stay recorded; nothing else is 0.
                let tokens = match shape {
                    Shape::AfterUsage => (Some(3), Some(5)),
                    _ => (None, None),
                };
                let want = Recorded {
                    state: "failed".into(),
                    error_code: Some(execution_code.into()),
                    input: tokens.0,
                    output: tokens.1,
                    reservation: "unknown".into(),
                    actual: None,
                    // The hold, then the marker that its cost is unknown: no
                    // settlement and no reconciliation.
                    ledger: vec!["hold".into(), "unknown".into()],
                    health_failures: i64::from(health),
                };
                let at = format!("{kind} {shape:?} messages={messages} free={free}");
                let expected_class = if messages { messages_type } else { chat_code };
                if status != 200 {
                    problems.push(format!("{at}: status {status}"));
                }
                if class != expected_class {
                    problems.push(format!("{at}: client {class}, want {expected_class}"));
                }
                if body.contains("[DONE]") || body.contains("message_stop") {
                    problems.push(format!("{at}: a finish was sent"));
                }
                if body.contains("secret") {
                    problems.push(format!("{at}: upstream text reached the client"));
                }
                if got != want {
                    problems.push(format!("{at}: recorded {got:?}, want {want:?}"));
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[dbtest::test]
async fn exception_frames_end_the_stream_with_their_own_class(pool: PgPool) {
    matrix(pool, false).await;
}

#[dbtest::test]
async fn exception_frames_on_a_free_price_never_settle_as_a_known_zero(pool: PgPool) {
    matrix(pool, true).await;
}

/// The decision's main promise: a stream that fails, even before any
/// content, is never retried or failed over, however permissive the route's
/// policy. Control: the same throttle as an HTTP error before any stream does
/// fail over under that policy, so the policy is live.
#[dbtest::test]
async fn a_failed_stream_is_never_failed_over(pool: PgPool) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions','messages']")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let mut deployments = Vec::new();
    for (order, model) in [(0, "anthropic.first"), (1, "anthropic.second")] {
        deployments.push(
            add_deployment(
                &f,
                "bedrock",
                "aws:default",
                Some("us-east-1"),
                model,
                order,
            )
            .await,
        );
    }
    sqlx::query("INSERT INTO routing_policies(model_id,max_attempts,allow_ambiguous_failover) VALUES($1,2,true)")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    for (d, priority) in deployments.iter().zip([0, 1]) {
        // Failover candidates must share an explicit residency with the first.
        sqlx::query(
            "INSERT INTO deployment_routing(deployment_id,priority,residency) VALUES($1,$2,'us')",
        )
        .bind(d)
        .bind(priority)
        .execute(&f.store.pool)
        .await
        .unwrap();
    }
    for as_stream in [true, false] {
        for messages in [false, true] {
            sqlx::query("DELETE FROM deployment_health")
                .execute(&f.store.pool)
                .await
                .unwrap();
            let paths = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = paths.clone();
            let app = Router::new().fallback(move |uri: Uri| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push(uri.path().to_owned());
                    if !uri.path().contains("anthropic.first") {
                        return eventstream(wire(events(false)));
                    }
                    if as_stream {
                        eventstream(frame(
                            "throttlingException",
                            "exception",
                            json!({"message":"slow down"}),
                        ))
                    } else {
                        Response::builder()
                            .status(429)
                            .header("content-type", "application/json")
                            .header("x-amzn-errortype", "ThrottlingException")
                            .body(Body::from(r#"{"message":"slow down"}"#))
                            .unwrap()
                    }
                }
            });
            let (upstream, _upstream) = serve(app).await;
            let mut registry = ProviderRegistry::default();
            registry
                .register(Arc::new(BedrockAdapter::for_test(&upstream)))
                .unwrap();
            let (base, _gateway) = start_gateway(&f, registry).await;
            let before: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
            let (status, _, body) = stream(&base, &token, messages).await;
            let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
            let paths = paths.lock().unwrap().clone();
            let at = format!("as_stream={as_stream} messages={messages}");
            assert_eq!(status, 200, "{at}: {body} {paths:?}");
            if as_stream {
                // One upstream request, to the first deployment only.
                assert_eq!(paths.len(), 1, "{at}: {paths:?}");
                assert!(paths[0].contains("anthropic.first"), "{at}: {paths:?}");
                assert_eq!(attempts - before, 1, "{at}");
                assert!(body.contains("rate_limit_error"), "{at}: {body}");
                assert!(!body.contains("message_stop") && !body.contains("[DONE]"));
            } else {
                // Before any stream the same throttle fails over.
                assert_eq!(paths.len(), 2, "{at}: {paths:?}");
                assert!(paths[1].contains("anthropic.second"), "{at}: {paths:?}");
                assert_eq!(attempts - before, 2, "{at}");
                assert!(
                    body.contains("[DONE]") || body.contains("message_stop"),
                    "{at}: {body}"
                );
            }
        }
    }
}

/// The rule is the engine's, not Bedrock's: any adapter whose returned stream
/// ends in a rejection (OpenRouter maps an in-stream numeric 4xx code to
/// `UpstreamRejected`; a Messages stream can end in `ContextLength`) leaves
/// usage unknown and the hold in place on a free price, before or after
/// content. A rejection before any stream still settles as a known zero.
struct EndsInRejection {
    error: crate::inference::error::InferenceError,
    content: bool,
    as_stream: bool,
}
#[async_trait::async_trait]
impl crate::providers::ProviderAdapter for EndsInRejection {
    fn id(&self) -> &'static str {
        "test_vendor"
    }
    fn capabilities(&self) -> crate::inference::types::Capabilities {
        crate::inference::types::Capabilities {
            text_chat: true,
            streaming: true,
            tools: true,
        }
    }
    fn supports_protocol(&self, protocol: crate::inference::types::ApiProtocol) -> bool {
        protocol == crate::inference::types::ApiProtocol::ChatCompletions
    }
    async fn execute(
        &self,
        _: &crate::inference::types::Deployment,
        _: crate::inference::types::ChatRequest,
    ) -> Result<crate::inference::types::ProviderOutput, crate::inference::error::InferenceError>
    {
        use crate::inference::types::{ChatEvent, ProviderOutput};
        if !self.as_stream {
            return Err(self.error);
        }
        let mut items = Vec::new();
        if self.content {
            items.push(Ok(ChatEvent::Delta {
                text: Some("Hello".into()),
                tool_calls: Vec::new(),
            }));
        }
        items.push(Err(self.error));
        Ok(ProviderOutput::Stream(Box::pin(
            futures_util::stream::iter(items),
        )))
    }
}

#[dbtest::test]
async fn any_adapters_in_stream_rejection_on_a_free_price_stays_unknown(pool: PgPool) {
    use crate::inference::error::InferenceError as E;
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    let deployment =
        add_unpriced_deployment(&f, "test_vendor", "env:NOT_RESOLVED", None, "model", 0).await;
    free_price(&f.store.pool, deployment).await;
    let mut problems = Vec::new();
    for error in [E::UpstreamRejected, E::ContextLength] {
        for (as_stream, content) in [(false, false), (true, false), (true, true)] {
            let mut registry = ProviderRegistry::default();
            registry
                .register(Arc::new(EndsInRejection {
                    error,
                    content,
                    as_stream,
                }))
                .unwrap();
            let (base, _gateway) = start_gateway(&f, registry).await;
            let (_, request, _) = stream(&base, &token, false).await;
            let got = recorded(&f.store.pool, deployment, request).await;
            // Known zero only for a rejection before any stream.
            let (reservation, tokens) = if as_stream {
                ("unknown", None)
            } else {
                ("settled", Some(0))
            };
            if (got.reservation.as_str(), got.input, got.output) != (reservation, tokens, tokens) {
                problems.push(format!(
                    "{error:?} stream={as_stream} content={content}: {got:?}"
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

// ---- #622: failures of the transport inside a returned stream -----------------

use crate::providers::stream_class_tests::{
    Server as MockServer, Step, response, serve as serve_mock,
};

/// A gateway over one Bedrock deployment whose mock plays the current script.
struct Scripted {
    f: Fixture,
    deployment: Uuid,
    token: String,
    base: String,
    script: Arc<Mutex<Vec<Step>>>,
    _servers: Vec<crate::inference::admission_validation_tests::Server>,
    _mock: MockServer,
}
async fn scripted(pool: PgPool) -> Scripted {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions','messages']")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let deployment = add_deployment(
        &f,
        "bedrock",
        "aws:default",
        Some("us-east-1"),
        "anthropic.claude-test",
        0,
    )
    .await;
    let script = Arc::new(Mutex::new(Vec::<Step>::new()));
    let shared = script.clone();
    let app = Router::new().fallback(move || {
        let steps = shared.lock().unwrap().clone();
        async move { response(200, "application/vnd.amazon.eventstream", steps) }
    });
    let (upstream, mock) = serve_mock(app).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(BedrockAdapter::for_test(&upstream)))
        .unwrap();
    let (base, server) = start_gateway(&f, registry).await;
    Scripted {
        f,
        deployment,
        token,
        base,
        script,
        _servers: vec![server],
        _mock: mock,
    }
}
fn bedrock_frames() -> Vec<Vec<u8>> {
    events(false)
        .into_iter()
        .map(|(kind, value)| frame(kind, "event", value))
        .collect()
}

/// A reset, a body that ends inside a frame or before the terminal sequence,
/// and a frame the event-stream decoder cannot decode end the 200 as
/// `upstream_unavailable` (Messages: `overloaded_error`) and count against
/// routing health; a frame that arrived whole with wrong content ends it as
/// `invalid_upstream_response` (Messages: `api_error`) and does not. Content
/// already delivered stays delivered, usage the stream did not report stays
/// unknown, and the reservation stays `unknown` with its hold.
#[dbtest::test]
async fn transport_failures_end_the_stream_as_unavailable_and_count(pool: PgPool) {
    let g = scripted(pool).await;
    let f = bedrock_frames();
    let mut corrupt = f.clone();
    let last = corrupt[2].len() - 1;
    corrupt[2][last] ^= 1;
    let mut bad_usage = f.clone();
    bad_usage[4] = frame(
        "metadata",
        "event",
        json!({"usage":{"inputTokens":3,"outputTokens":5}}),
    );
    let half = f[2][..f[2].len() / 2].to_vec();
    // (place, steps, chat code, Messages type, counts against health)
    let cases: Vec<(&str, Vec<Step>, &str, &str, bool)> = vec![
        (
            "reset after content",
            vec![Step::Bytes(f[..2].concat()), Step::Reset],
            "upstream_unavailable",
            "overloaded_error",
            true,
        ),
        (
            "reset inside a frame",
            vec![
                Step::Bytes([f[..2].concat(), half.clone()].concat()),
                Step::Reset,
            ],
            "upstream_unavailable",
            "overloaded_error",
            true,
        ),
        (
            "end inside a frame",
            vec![Step::Bytes([f[..2].concat(), half].concat())],
            "upstream_unavailable",
            "overloaded_error",
            true,
        ),
        (
            "end before messageStop",
            vec![Step::Bytes(f[..3].concat())],
            "upstream_unavailable",
            "overloaded_error",
            true,
        ),
        (
            "end before metadata",
            vec![Step::Bytes(f[..4].concat())],
            "upstream_unavailable",
            "overloaded_error",
            true,
        ),
        (
            "a failed CRC",
            vec![Step::Bytes(corrupt.concat())],
            "upstream_unavailable",
            "overloaded_error",
            true,
        ),
        (
            "a usage envelope with a missing count",
            vec![Step::Bytes(bad_usage.concat())],
            "invalid_upstream_response",
            "api_error",
            false,
        ),
    ];
    let mut problems = Vec::new();
    for (place, steps, chat_code, messages_type, health) in cases {
        for messages in [false, true] {
            sqlx::query("DELETE FROM deployment_health")
                .execute(&g.f.store.pool)
                .await
                .unwrap();
            *g.script.lock().unwrap() = steps.clone();
            let (status, request, body) = stream(&g.base, &g.token, messages).await;
            let error = last_event(&body);
            let (told, want_told) = if messages {
                (
                    error["error"]["type"].as_str().unwrap().to_owned(),
                    messages_type,
                )
            } else {
                (
                    error["error"]["code"].as_str().unwrap().to_owned(),
                    chat_code,
                )
            };
            let got = recorded(&g.f.store.pool, g.deployment, request).await;
            let at = format!("{place} messages={messages}");
            let want = Recorded {
                state: "failed".into(),
                error_code: Some(chat_code.into()),
                input: None,
                output: None,
                reservation: "unknown".into(),
                actual: None,
                ledger: vec!["hold".into(), "unknown".into()],
                health_failures: i64::from(health),
            };
            if status != 200 {
                problems.push(format!("{at}: status {status}"));
            }
            if told != want_told {
                problems.push(format!("{at}: client {told}, want {want_told}"));
            }
            if body.contains("[DONE]") || body.contains("message_stop") {
                problems.push(format!("{at}: a finish was sent"));
            }
            if got != want {
                problems.push(format!("{at}: recorded {got:?}, want {want:?}"));
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Three resets open the breaker (the next request is refused as cooling
/// down); three whole frames with wrong content do not.
#[dbtest::test]
async fn the_breaker_opens_on_resets_and_not_on_invalid_frames(pool: PgPool) {
    let g = scripted(pool).await;
    let f = bedrock_frames();
    let mut bad_usage = f.clone();
    bad_usage[4] = frame(
        "metadata",
        "event",
        json!({"usage":{"inputTokens":3,"outputTokens":5}}),
    );
    for (label, steps, opens) in [
        (
            "a reset",
            vec![Step::Bytes(f[..2].concat()), Step::Reset],
            true,
        ),
        (
            "a truncated stream",
            vec![Step::Bytes(f[..3].concat())],
            true,
        ),
        (
            "an invalid frame",
            vec![Step::Bytes(bad_usage.concat())],
            false,
        ),
    ] {
        sqlx::query("DELETE FROM deployment_health")
            .execute(&g.f.store.pool)
            .await
            .unwrap();
        *g.script.lock().unwrap() = steps;
        for _ in 0..3 {
            let (status, _, body) = stream(&g.base, &g.token, false).await;
            assert_eq!(status, 200, "{label}: {body}");
        }
        let (failures, open): (i32, bool) = sqlx::query_as(
            "SELECT COALESCE(consecutive_failures,0), COALESCE(open_until>clock_timestamp(),false) FROM (SELECT 1) one LEFT JOIN deployment_health ON deployment_id=$1",
        )
        .bind(g.deployment)
        .fetch_one(&g.f.store.pool)
        .await
        .unwrap();
        assert_eq!(
            (failures, open),
            (if opens { 3 } else { 0 }, opens),
            "{label}"
        );
        if opens {
            let (status, _, body) = stream(&g.base, &g.token, false).await;
            assert_ne!(status, 200, "{label}: {body}");
            assert!(
                body.contains("model_temporarily_unavailable"),
                "{label}: {body}"
            );
        }
    }
}

/// A client that goes away mid-stream cancels the attempt: no failure and no
/// success is recorded against the route, whatever the provider was doing.
#[dbtest::test]
async fn a_client_that_goes_away_mid_stream_is_not_a_provider_failure(pool: PgPool) {
    let g = scripted(pool).await;
    let f = bedrock_frames();
    *g.script.lock().unwrap() = vec![Step::Bytes(f[..2].concat()), Step::Hang];
    for messages in [false, true] {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let request = if messages {
            client.post(format!("{}/v1/messages", g.base)).json(&json!({
                "model":"company/smart","max_tokens":16,"stream":true,
                "messages":[{"role":"user","content":"hi"}]
            }))
        } else {
            client.post(format!("{}/v1/chat/completions", g.base)).json(&json!({
                "model":"company/smart","stream":true,"messages":[{"role":"user","content":"hi"}]
            }))
        };
        let mut response = request
            .bearer_auth(&g.token)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .unwrap();
        let id: Uuid = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), response.chunk()).await;
        drop(response);
        let mut state = String::new();
        for _ in 0..100 {
            state = sqlx::query_scalar(
                "SELECT state FROM inference_executions WHERE root_request_id=$1",
            )
            .bind(id)
            .fetch_one(&g.f.store.pool)
            .await
            .unwrap();
            if state != "started" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(state, "cancelled", "messages={messages}");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM deployment_health WHERE deployment_id=$1")
                .bind(g.deployment)
                .fetch_one(&g.f.store.pool)
                .await
                .unwrap();
        assert_eq!(
            rows, 0,
            "messages={messages}: a cancelled attempt left a health observation"
        );
    }
}

/// A reset or truncation is never failed over either, however permissive the
/// route's policy.
#[dbtest::test]
async fn a_stream_that_breaks_in_transport_is_never_failed_over(pool: PgPool) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions','messages']")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let mut deployments = Vec::new();
    for (order, model) in [(0, "anthropic.first"), (1, "anthropic.second")] {
        deployments.push(
            add_deployment(
                &f,
                "bedrock",
                "aws:default",
                Some("us-east-1"),
                model,
                order,
            )
            .await,
        );
    }
    sqlx::query("INSERT INTO routing_policies(model_id,max_attempts,allow_ambiguous_failover) VALUES($1,2,true)")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    for (d, priority) in deployments.iter().zip([0, 1]) {
        sqlx::query(
            "INSERT INTO deployment_routing(deployment_id,priority,residency) VALUES($1,$2,'us')",
        )
        .bind(d)
        .bind(priority)
        .execute(&f.store.pool)
        .await
        .unwrap();
    }
    let frames = bedrock_frames();
    for (label, broken) in [
        (
            "a reset",
            vec![Step::Bytes(frames[..2].concat()), Step::Reset],
        ),
        (
            "a truncated stream",
            vec![Step::Bytes(frames[..3].concat())],
        ),
    ] {
        for messages in [false, true] {
            sqlx::query("DELETE FROM deployment_health")
                .execute(&f.store.pool)
                .await
                .unwrap();
            let paths = Arc::new(Mutex::new(Vec::<String>::new()));
            let (seen, broken, good) = (paths.clone(), broken.clone(), frames.concat());
            let app = Router::new().fallback(move |uri: Uri| {
                let (seen, broken, good) = (seen.clone(), broken.clone(), good.clone());
                async move {
                    seen.lock().unwrap().push(uri.path().to_owned());
                    if uri.path().contains("anthropic.first") {
                        response(200, "application/vnd.amazon.eventstream", broken)
                    } else {
                        response(
                            200,
                            "application/vnd.amazon.eventstream",
                            vec![Step::Bytes(good)],
                        )
                    }
                }
            });
            let (upstream, _upstream) = serve(app).await;
            let mut registry = ProviderRegistry::default();
            registry
                .register(Arc::new(BedrockAdapter::for_test(&upstream)))
                .unwrap();
            let (base, _gateway) = start_gateway(&f, registry).await;
            let before: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
            let (status, _, body) = stream(&base, &token, messages).await;
            let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
            let paths = paths.lock().unwrap().clone();
            let at = format!("{label} messages={messages}");
            assert_eq!(status, 200, "{at}: {body}");
            assert_eq!(paths.len(), 1, "{at}: {paths:?}");
            assert!(paths[0].contains("anthropic.first"), "{at}: {paths:?}");
            assert_eq!(attempts - before, 1, "{at}");
            assert!(
                !body.contains("[DONE]") && !body.contains("message_stop"),
                "{at}: {body}"
            );
        }
    }
}

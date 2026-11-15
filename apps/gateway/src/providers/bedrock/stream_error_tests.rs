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

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn exception_frames_end_the_stream_with_their_own_class(pool: PgPool) {
    matrix(pool, false).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn exception_frames_on_a_free_price_never_settle_as_a_known_zero(pool: PgPool) {
    matrix(pool, true).await;
}

/// The decision's main promise: a stream that fails, even before any
/// content, is never retried or failed over, however permissive the route's
/// policy. Control: the same throttle as an HTTP error before any stream does
/// fail over under that policy, so the policy is live.
#[sqlx::test(migrations = "./enterprise_migrations")]
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

#[sqlx::test(migrations = "./enterprise_migrations")]
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

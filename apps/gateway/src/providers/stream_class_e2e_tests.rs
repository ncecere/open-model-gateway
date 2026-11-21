//! #614 and #622 end to end: the real adapters against a mock upstream that
//! answers 200 and then fails inside the stream, behind the real engine and
//! HTTP routers on PostgreSQL. The response stays a 200 that ends in an error
//! (the gateway does not wait for content and never fails over after a stream
//! is returned); what the failure decides is its class: the terminal error
//! each client protocol sees, the recorded execution, the reservation and the
//! routing-health observation. The adapters' own parsing is
//! `stream_class_tests`; Bedrock's is `bedrock::stream_error_tests`.
use std::sync::{Arc, Mutex};

use axum::{Router, body::Bytes};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::stream_class_tests::{Kind, NAMES, Step, http_class, response, serve, sse};
use crate::{
    billing::v3::Meter,
    governance::tests::db::{Fixture, fixture},
    inference::{
        admission_validation_tests::{
            add_deployment, add_unpriced_deployment, issue_key, start_gateway,
        },
        error::InferenceError,
    },
    providers::ProviderRegistry,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Client {
    Chat,
    Messages,
    Responses,
}
/// The client protocols that reach `kind`: Responses reaches the native
/// Responses transport only; local profiles use pinned approved endpoints.
fn clients(kind: Kind) -> Vec<Client> {
    match kind {
        Kind::Responses => vec![Client::Responses],
        _ => vec![Client::Chat, Client::Messages],
    }
}

/// A gateway over one deployment of `kind` whose mock answers with `script`.
struct World {
    f: Fixture,
    token: String,
    deployment: Uuid,
    script: Arc<Mutex<Vec<Step>>>,
    base: String,
    _servers: Vec<crate::inference::admission_validation_tests::Server>,
    _mock: super::stream_class_tests::Server,
}
async fn register(
    f: &Fixture,
    kind: Kind,
    mock_base: &str,
) -> (String, crate::inference::admission_validation_tests::Server) {
    if kind == Kind::Local {
        sqlx::query("UPDATE provider_connections SET endpoint=$1 WHERE provider='vllm'")
            .bind(kind.target(mock_base).endpoint)
            .execute(&f.store.pool)
            .await
            .unwrap();
    }
    let mut registry = ProviderRegistry::default();
    registry.register(kind.adapter(mock_base)).unwrap();
    start_gateway(f, registry).await
}
async fn world(pool: PgPool, kind: Kind, free: bool) -> World {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['chat_completions','messages','responses']",
    )
    .execute(&f.store.pool)
    .await
    .unwrap();
    let deployment = if free {
        let d = add_unpriced_deployment(
            &f,
            kind.provider(),
            kind.credential(),
            None,
            "private-model",
            0,
        )
        .await;
        free_price(&f.store.pool, d).await;
        d
    } else {
        add_deployment(
            &f,
            kind.provider(),
            kind.credential(),
            None,
            "private-model",
            0,
        )
        .await
    };
    only_deployments(&f, &[deployment]).await;
    let script = Arc::new(Mutex::new(Vec::<Step>::new()));
    let shared = script.clone();
    let app = Router::new().fallback(move || {
        let steps = shared.lock().unwrap().clone();
        async move { response(200, "text/event-stream", steps) }
    });
    let (mock_base, mock) = serve(app).await;
    let (base, server) = register(&f, kind, &mock_base).await;
    World {
        f,
        token,
        deployment,
        script,
        base,
        _servers: vec![server],
        _mock: mock,
    }
}

/// The fixture's model has a deployment of its own (an OpenAI one, which
/// would be picked first for `openai`): leave only `keep` enabled.
async fn only_deployments(f: &Fixture, keep: &[Uuid]) {
    sqlx::query("UPDATE deployments SET enabled=false WHERE model_id=$1 AND id<>ALL($2)")
        .bind(f.model)
        .bind(keep)
        .execute(&f.store.pool)
        .await
        .unwrap();
}

/// An explicitly free pricing v3 version: the shape a free pre-processing
/// rejection settles against (`governance::free_rejection`).
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

struct Reply {
    status: u16,
    id: Uuid,
    body: String,
}
fn client_request(base: &str, client: Client) -> reqwest::RequestBuilder {
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    match client {
        Client::Chat => http
            .post(format!("{base}/v1/chat/completions"))
            .json(&json!({
                "model":"company/smart","stream":true,"messages":[{"role":"user","content":"hi"}]
            })),
        Client::Messages => http
            .post(format!("{base}/v1/messages"))
            .header("anthropic-version", "2023-06-01")
            .json(&json!({
                "model":"company/smart","max_tokens":16,"stream":true,
                "messages":[{"role":"user","content":"hi"}]
            })),
        Client::Responses => http.post(format!("{base}/v1/responses")).json(&json!({
            "model":"company/smart","input":"hi","stream":true
        })),
    }
}
async fn call(base: &str, token: &str, client: Client) -> Reply {
    let response = client_request(base, client)
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    Reply {
        status,
        id,
        body: response.text().await.unwrap(),
    }
}
impl World {
    async fn ask(&self, client: Client, steps: Vec<Step>) -> Reply {
        *self.script.lock().unwrap() = steps;
        sqlx::query("DELETE FROM deployment_health")
            .execute(&self.f.store.pool)
            .await
            .unwrap();
        call(&self.base, &self.token, client).await
    }
}

/// The last `data:` payload of an SSE body that is JSON.
fn last_event(body: &str) -> Value {
    body.lines()
        .rev()
        .find_map(|line| line.strip_prefix("data:"))
        .and_then(|data| serde_json::from_str(data.trim()).ok())
        .unwrap_or_else(|| panic!("no JSON event in {body}"))
}
/// The error the client was told, in its protocol's own field.
fn told(client: Client, body: &str) -> String {
    let event = last_event(body);
    match client {
        Client::Chat => event["error"]["code"].as_str(),
        Client::Messages => event["error"]["type"].as_str(),
        Client::Responses => event["code"].as_str(),
    }
    .unwrap_or_else(|| panic!("no error in {body}"))
    .to_owned()
}
/// What each client protocol is told for each class (`protocols::*` error
/// bodies), restated as an oracle.
fn shown(client: Client, class: InferenceError) -> &'static str {
    match (client, class) {
        (Client::Messages, InferenceError::Busy) => "rate_limit_error",
        (Client::Messages, InferenceError::UpstreamUnavailable | InferenceError::Overloaded) => {
            "overloaded_error"
        }
        (Client::Messages, InferenceError::UpstreamRejected | InferenceError::ContextLength) => {
            "invalid_request_error"
        }
        (Client::Messages, _) => "api_error",
        (_, class) => class.code(),
    }
}
/// The classes that count against routing health.
fn counts(class: InferenceError) -> bool {
    matches!(
        class,
        InferenceError::Busy
            | InferenceError::Overloaded
            | InferenceError::UpstreamUnavailable
            | InferenceError::Timeout
    )
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
/// A returned stream that failed: a failed execution, the hold retained
/// (`unknown`, nothing settled), no usage invented, and the health observation
/// of its class. `tokens` are the counts the stream reported before it failed.
fn failed_with(class: InferenceError, tokens: Option<(i64, i64)>) -> Recorded {
    Recorded {
        state: "failed".into(),
        error_code: Some(class.code().into()),
        input: tokens.map(|t| t.0),
        output: tokens.map(|t| t.1),
        reservation: "unknown".into(),
        actual: None,
        ledger: vec!["hold".into(), "unknown".into()],
        health_failures: i64::from(counts(class)),
    }
}
fn expected_class(kind: Kind, client: Client, name: &str) -> InferenceError {
    let status = NAMES.iter().find(|(n, _)| *n == name).map(|(_, s)| *s);
    match status {
        Some(status) => http_class(kind, client == Client::Messages, name, status),
        None => InferenceError::UpstreamUnavailable,
    }
}

/// Checks one failed stream against the class it must have ended in.
async fn check(
    w: &World,
    client: Client,
    reply: &Reply,
    class: InferenceError,
    tokens: Option<(i64, i64)>,
    at: &str,
    problems: &mut Vec<String>,
) {
    if reply.status != 200 {
        problems.push(format!("{at}: status {}", reply.status));
    }
    let shown = shown(client, class);
    if told(client, &reply.body) != shown {
        problems.push(format!(
            "{at}: client {}, want {shown}",
            told(client, &reply.body)
        ));
    }
    for finished in ["[DONE]", "message_stop", "response.completed"] {
        if reply.body.contains(finished) {
            problems.push(format!("{at}: a finish was sent"));
        }
    }
    if reply.body.contains("secret") {
        problems.push(format!("{at}: upstream text reached the client"));
    }
    let got = recorded(&w.f.store.pool, w.deployment, reply.id).await;
    let want = failed_with(class, tokens);
    if got != want {
        problems.push(format!("{at}: recorded {got:?}, want {want:?}"));
    }
}

/// Every in-stream error name, each client protocol, before and after
/// content: the 200 ends in the class the same error has as an HTTP response,
/// and nothing about settlement changes.
async fn in_stream_errors(pool: PgPool, kind: Kind, free: bool, names: &[&str]) {
    let w = world(pool, kind, free).await;
    let mut problems = Vec::new();
    let (content, _, _) = kind.valid();
    for client in clients(kind) {
        for name in names {
            let class = expected_class(kind, client, name);
            for frame in kind.errors(name) {
                for (place, prefix) in [
                    ("before content", Vec::new()),
                    ("after content", content.clone()),
                ] {
                    let steps = vec![Step::Bytes(
                        [prefix, sse(std::slice::from_ref(&frame))].concat(),
                    )];
                    let reply = w.ask(client, steps).await;
                    let at = format!("{kind:?} {client:?} {name} {place} free={free}");
                    check(&w, client, &reply, class, None, &at, &mut problems).await;
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}
const NAMES_UNDER_TEST: &[&str] = &[
    "rate_limit_error",
    "rate_limit_exceeded",
    "api_error",
    "server_error",
    "overloaded_error",
    "invalid_request_error",
    "context_length_exceeded",
    "authentication_error",
    "brand_new_error_name",
];
/// The client-fault names: on a free price too, a rejection inside a returned
/// stream is not a rejection before processing, so it never settles as a
/// known zero.
const REJECTIONS: &[&str] = &["invalid_request_error", "context_length_exceeded"];

macro_rules! per_kind {
    ($($name:ident: $kind:expr,)*) => {$(
        mod $name {
            use super::*;
            #[dbtest::test]
            async fn in_stream_errors_end_with_the_class_of_the_same_http_error(pool: PgPool) {
                in_stream_errors(pool, $kind, false, NAMES_UNDER_TEST).await;
            }
            #[dbtest::test]
            async fn in_stream_rejections_on_a_free_price_never_settle_as_a_known_zero(pool: PgPool) {
                in_stream_errors(pool, $kind, true, REJECTIONS).await;
            }
            #[dbtest::test]
            async fn transport_failures_inside_a_stream_are_unavailable_and_count(pool: PgPool) {
                transport_failures(pool, $kind).await;
            }
            #[dbtest::test]
            async fn a_failed_stream_is_never_failed_over(pool: PgPool) {
                never_failed_over(pool, $kind).await;
            }
            #[dbtest::test]
            async fn the_breaker_follows_the_class_of_in_stream_failures(pool: PgPool) {
                breaker(pool, $kind).await;
            }
            #[dbtest::test]
            async fn a_client_that_goes_away_mid_stream_is_not_a_provider_failure(pool: PgPool) {
                client_drop(pool, $kind).await;
            }
        }
    )*};
}
per_kind! {
    anthropic: Kind::Anthropic,
    openai_chat: Kind::OpenAi,
    openai_responses: Kind::Responses,
    openrouter: Kind::OpenRouter,
    local: Kind::Local,
}

/// #622: a reset or a body that ends before the terminal frame (including
/// inside a frame) ends the 200 as `upstream_unavailable` and count against
/// health; a whole frame with wrong content ends it as
/// `invalid_upstream_response` and does not.
async fn transport_failures(pool: PgPool, kind: Kind) {
    use InferenceError as E;
    let w = world(pool, kind, false).await;
    let (content, rest, _) = kind.valid();
    let cut = rest[..rest.len() / 2].to_vec();
    let cases: Vec<(&str, Vec<Step>, E)> = vec![
        (
            "reset after content",
            vec![Step::Bytes(content.clone()), Step::Reset],
            E::UpstreamUnavailable,
        ),
        (
            "reset inside a frame",
            vec![
                Step::Bytes([content.clone(), cut.clone()].concat()),
                Step::Reset,
            ],
            E::UpstreamUnavailable,
        ),
        (
            "end after content",
            vec![Step::Bytes(content.clone())],
            E::UpstreamUnavailable,
        ),
        (
            "end inside a frame",
            vec![Step::Bytes([content.clone(), cut].concat())],
            E::UpstreamUnavailable,
        ),
        (
            "end before the terminal frame",
            vec![Step::Bytes([content.clone(), rest].concat())],
            E::UpstreamUnavailable,
        ),
        (
            "bytes that are not UTF-8",
            vec![Step::Bytes(
                [content.clone(), b"data: \xff\xfe\n\n".to_vec()].concat(),
            )],
            E::InvalidUpstream,
        ),
        (
            "a frame that is not JSON",
            vec![Step::Bytes(
                [content.clone(), b"data: {not json\n\n".to_vec()].concat(),
            )],
            E::InvalidUpstream,
        ),
        (
            "a frame of the wrong shape",
            vec![Step::Bytes([content, kind.wrong_shape()].concat())],
            E::InvalidUpstream,
        ),
    ];
    let mut problems = Vec::new();
    for client in clients(kind) {
        for (place, steps, class) in &cases {
            let reply = w.ask(client, steps.clone()).await;
            let at = format!("{kind:?} {client:?} {place}");
            // The chat-shaped providers report usage before their terminal
            // frame: counts the stream did report stay recorded.
            let tokens = (*place == "end before the terminal frame"
                && matches!(kind, Kind::OpenAi | Kind::OpenRouter | Kind::Local))
            .then_some((3, 2));
            check(&w, client, &reply, *class, tokens, &at, &mut problems).await;
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The decision's main promise: a stream that fails, even before any content,
/// is never retried or failed over, however permissive the route's policy.
/// Control: the same overload as an HTTP error before any stream does fail
/// over under that policy, so the policy is live.
async fn never_failed_over(pool: PgPool, kind: Kind) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['chat_completions','messages','responses']",
    )
    .execute(&f.store.pool)
    .await
    .unwrap();
    let mut deployments = Vec::new();
    for (order, model) in [(0, "model-first"), (1, "model-second")] {
        deployments
            .push(add_deployment(&f, kind.provider(), kind.credential(), None, model, order).await);
    }
    only_deployments(&f, &deployments).await;
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
    let (content, rest, terminal) = kind.valid();
    let good = [content.clone(), rest, terminal].concat();
    // (label, the first deployment's answer: an HTTP status or a stream)
    let first_answers: Vec<(&str, Option<u16>, Vec<Step>)> = vec![
        (
            "in-stream rate limit before content",
            None,
            vec![Step::Bytes(sse(&kind.errors("rate_limit_error")[..1]))],
        ),
        (
            "in-stream overload after content",
            None,
            vec![Step::Bytes(
                [content.clone(), sse(&kind.errors("overloaded_error")[..1])].concat(),
            )],
        ),
        (
            "in-stream server error after content",
            None,
            vec![Step::Bytes(
                [content.clone(), sse(&kind.errors("server_error")[..1])].concat(),
            )],
        ),
        (
            "reset after content",
            None,
            vec![Step::Bytes(content), Step::Reset],
        ),
        (
            "an HTTP 429 (control)",
            Some(429),
            vec![Step::Bytes(b"{}".to_vec())],
        ),
    ];
    for client in clients(kind) {
        for (label, status, first) in &first_answers {
            sqlx::query("DELETE FROM deployment_health")
                .execute(&f.store.pool)
                .await
                .unwrap();
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let (log, first, good, status) = (seen.clone(), first.clone(), good.clone(), *status);
            let app = Router::new().fallback(move |body: Bytes| {
                let (log, first, good) = (log.clone(), first.clone(), good.clone());
                async move {
                    let model = serde_json::from_slice::<Value>(&body)
                        .ok()
                        .and_then(|v| v["model"].as_str().map(str::to_owned))
                        .unwrap_or_default();
                    log.lock().unwrap().push(model.clone());
                    if model != "model-first" {
                        return response(200, "text/event-stream", vec![Step::Bytes(good)]);
                    }
                    match status {
                        Some(code) => response(code, "application/json", first),
                        None => response(200, "text/event-stream", first),
                    }
                }
            });
            let (mock_base, _mock) = serve(app).await;
            let (base, _gateway) = register(&f, kind, &mock_base).await;
            let before: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
            let reply = call(&base, &token, client).await;
            let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
            let models = seen.lock().unwrap().clone();
            let at = format!("{kind:?} {client:?} {label}");
            assert_eq!(reply.status, 200, "{at}: {}", reply.body);
            if status.is_some() {
                // Before any stream the same overload fails over.
                assert_eq!(models, ["model-first", "model-second"], "{at}");
                assert_eq!(attempts - before, 2, "{at}");
                assert!(
                    reply.body.contains("[DONE]")
                        || reply.body.contains("message_stop")
                        || reply.body.contains("response.completed"),
                    "{at}: {}",
                    reply.body
                );
            } else {
                assert_eq!(
                    models,
                    ["model-first"],
                    "{at}: only the first deployment is called"
                );
                assert_eq!(attempts - before, 1, "{at}");
                assert!(
                    !reply.body.contains("[DONE]") && !reply.body.contains("message_stop"),
                    "{at}"
                );
            }
        }
    }
}

/// Routing health follows the class: three in-stream overloads, server errors
/// or resets open the breaker (and the next request is refused as cooling
/// down); three client-fault errors or three invalid responses do not.
async fn breaker(pool: PgPool, kind: Kind) {
    let w = world(pool, kind, false).await;
    let (content, _, _) = kind.valid();
    let client = clients(kind)[0];
    let frame = |name: &str| sse(&kind.errors(name)[..1]);
    let cases: Vec<(&str, Vec<Step>, bool)> = vec![
        (
            "an in-stream overload",
            vec![Step::Bytes(
                [content.clone(), frame("server_error")].concat(),
            )],
            true,
        ),
        (
            "an in-stream rate limit",
            vec![Step::Bytes(
                [content.clone(), frame("rate_limit_error")].concat(),
            )],
            true,
        ),
        (
            "a reset",
            vec![Step::Bytes(content.clone()), Step::Reset],
            true,
        ),
        (
            "a stream cut before its terminal frame",
            vec![Step::Bytes(content.clone())],
            true,
        ),
        (
            "an in-stream client-fault error",
            vec![Step::Bytes(
                [content.clone(), frame("invalid_request_error")].concat(),
            )],
            false,
        ),
        (
            "an in-stream configuration error",
            vec![Step::Bytes(
                [content.clone(), frame("authentication_error")].concat(),
            )],
            false,
        ),
        (
            "an invalid response",
            vec![Step::Bytes([content, kind.wrong_shape()].concat())],
            false,
        ),
    ];
    for (label, steps, opens) in cases {
        sqlx::query("DELETE FROM deployment_health")
            .execute(&w.f.store.pool)
            .await
            .unwrap();
        *w.script.lock().unwrap() = steps;
        for n in 1..=3 {
            let reply = call(&w.base, &w.token, client).await;
            assert_eq!(reply.status, 200, "{kind:?} {label} #{n}: {}", reply.body);
        }
        let (failures, open): (i32, bool) = sqlx::query_as(
            "SELECT COALESCE(consecutive_failures,0), COALESCE(open_until>clock_timestamp(),false) FROM (SELECT 1) one LEFT JOIN deployment_health ON deployment_id=$1",
        )
        .bind(w.deployment)
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
        assert_eq!(
            (failures, open),
            (if opens { 3 } else { 0 }, opens),
            "{kind:?} {label}"
        );
        if opens {
            // The route is cooling down: refused before dispatch.
            let reply = call(&w.base, &w.token, client).await;
            assert_ne!(reply.status, 200, "{kind:?} {label}: {}", reply.body);
            assert!(
                reply.body.contains("model_temporarily_unavailable"),
                "{kind:?} {label}: {}",
                reply.body
            );
        }
    }
}

/// A client that goes away mid-stream cancels the attempt; that is the
/// gateway's side, not the provider's: no failure and no success is recorded
/// against the route's health, whatever the provider was doing.
async fn client_drop(pool: PgPool, kind: Kind) {
    let w = world(pool, kind, false).await;
    let (content, _, _) = kind.valid();
    for client in clients(kind) {
        sqlx::query("DELETE FROM deployment_health")
            .execute(&w.f.store.pool)
            .await
            .unwrap();
        *w.script.lock().unwrap() = vec![Step::Bytes(content.clone()), Step::Hang];
        let mut response = client_request(&w.base, client)
            .bearer_auth(&w.token)
            .send()
            .await
            .unwrap();
        let id: Uuid = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        // The client has the start of the stream, then goes away.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), response.chunk()).await;
        drop(response);
        let mut state = String::new();
        for _ in 0..100 {
            state = sqlx::query_scalar(
                "SELECT state FROM inference_executions WHERE root_request_id=$1",
            )
            .bind(id)
            .fetch_one(&w.f.store.pool)
            .await
            .unwrap();
            if state != "started" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let at = format!("{kind:?} {client:?}");
        assert_eq!(state, "cancelled", "{at}");
        // Give a wrongly recorded health observation time to land.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM deployment_health WHERE deployment_id=$1")
                .bind(w.deployment)
                .fetch_one(&w.f.store.pool)
                .await
                .unwrap();
        assert_eq!(
            rows, 0,
            "{at}: a cancelled attempt left a health observation"
        );
    }
}

/// Named errors in non-stream 200 bodies retain the prior class and therefore
/// do not gain Busy's unconditional failover. An ambiguous OpenRouter error
/// may still fail over under its existing opt-in policy.
#[dbtest::test]
async fn non_stream_200_errors_do_not_gain_failover(pool: PgPool) {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    for kind in [Kind::OpenAi, Kind::OpenRouter] {
        let first = add_deployment(&f, kind.provider(), kind.credential(), None, "first", 0).await;
        let second =
            add_deployment(&f, kind.provider(), kind.credential(), None, "second", 1).await;
        only_deployments(&f, &[first, second]).await;
        sqlx::query("INSERT INTO routing_policies(model_id,max_attempts) VALUES($1,2) ON CONFLICT(model_id) DO UPDATE SET max_attempts=2,allow_ambiguous_failover=false")
            .bind(f.model).execute(&f.store.pool).await.unwrap();
        for (d, priority) in [(first, 0), (second, 1)] {
            sqlx::query("INSERT INTO deployment_routing(deployment_id,priority,residency) VALUES($1,$2,'us')")
                .bind(d).bind(priority).execute(&f.store.pool).await.unwrap();
        }
        for name in ["rate_limit_exceeded", "insufficient_quota"] {
            for content in [false, true] {
                sqlx::query("DELETE FROM deployment_health")
                    .execute(&f.store.pool)
                    .await
                    .unwrap();
                let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let log = seen.clone();
                let body = json!({"error":{"code":name},"choices":if content {json!([{"index":0,"message":{"role":"assistant","content":"partial"},"finish_reason":"stop"}])} else {json!([])}}).to_string().into_bytes();
                let app = Router::new().fallback(move || {
                    log.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let body = body.clone();
                    async move { response(200, "application/json", vec![Step::Bytes(body)]) }
                });
                let (mock, _mock) = serve(app).await;
                let (base, _gateway) = register(&f, kind, &mock).await;
                let reply = reqwest::Client::builder().no_proxy().build().unwrap()
                    .post(format!("{base}/v1/chat/completions")).bearer_auth(&token)
                    .json(&json!({"model":"company/smart","stream":false,"messages":[{"role":"user","content":"hi"}]}))
                    .send().await.unwrap();
                let id: Uuid = reply.headers()["x-request-id"]
                    .to_str()
                    .unwrap()
                    .parse()
                    .unwrap();
                assert_eq!(reply.status().as_u16(), 502);
                let body = reply.text().await.unwrap();
                let expected = if kind == Kind::OpenAi {
                    InferenceError::InvalidUpstream
                } else {
                    InferenceError::UpstreamUnavailable
                };
                assert!(body.contains(expected.code()), "{body}");
                assert_eq!(
                    seen.load(std::sync::atomic::Ordering::SeqCst),
                    1,
                    "{kind:?} {name} content={content}"
                );
                let attempts: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM inference_executions WHERE root_request_id=$1",
                )
                .bind(id)
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
                assert_eq!(attempts, 1);
            }
        }
    }
}

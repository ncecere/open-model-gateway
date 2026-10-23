//! Engine-level tracing tests (#98 PR 2) against the mock upstream: a
//! captured [`telemetry::Recorder`] (its test-only channel, see
//! `telemetry::tests`) lets these tests see exactly what a real request
//! would export, or fail to export, without a collector.
//!
//! [`TelemetryRepository`] simulates governance's eligibility decision
//! directly (`telemetry::mark_phase`/`telemetry::mark_shared`, both
//! `pub(crate)`) instead of running real SQL: the real decision itself
//! (personal vs. team/project, live-revalidated) is proven by the
//! real-PostgreSQL integration tests in `governance::telemetry_tests`.
//! These tests prove the engine's own mechanics: the span set and
//! parentage it builds, first-content timing, missing-usage attributes,
//! timeout/cancellation finalization, failover attempt counts, settlement
//! failure, and that export never happens without eligibility or sampling.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::{
    inference::{repository::GenerationAdmission, types::*},
    providers::ProviderAdapter,
    telemetry,
};

#[derive(Default)]
struct TelemetryRepository {
    deployments: Mutex<Vec<Deployment>>,
    plan: Mutex<Option<crate::routing::RoutePlan>>,
    /// Whether `admit` simulates a committed shared-workspace admission.
    shared: AtomicBool,
    fail_finish: AtomicBool,
    /// Settlement sleeps briefly before returning (#98 PR 2 review item 1):
    /// proves `upstream.attempt`'s own duration never grows by settlement's.
    slow_finish: AtomicBool,
    finishes: Mutex<Vec<ExecutionFinish>>,
}
#[async_trait]
impl InferenceRepository for TelemetryRepository {
    async fn deployments(&self, _: &Principal, _: &str) -> Result<Vec<Deployment>, InferenceError> {
        Ok(self.deployments.lock().unwrap().clone())
    }
    async fn start(&self, _record: &ExecutionStart) -> Result<(), InferenceError> {
        Ok(())
    }
    async fn finish(&self, _record: &ExecutionFinish) -> Result<(), InferenceError> {
        Ok(())
    }
    async fn route_plan(
        &self,
        _: &Principal,
        _: &str,
        candidates: &[Deployment],
        _: Uuid,
    ) -> Result<crate::routing::RoutePlan, InferenceError> {
        Ok(self.plan.lock().unwrap().clone().unwrap_or_else(|| {
            crate::routing::RoutePlan::single(candidates.iter().map(|d| d.id).collect())
        }))
    }
    async fn route_result(&self, _: Uuid, _: Option<InferenceError>) -> Result<(), InferenceError> {
        Ok(())
    }
    async fn admit(
        &self,
        record: &ExecutionStart,
        request: &ChatRequest,
        _lease_seconds: i64,
        _deployment: &Deployment,
        _output_limit: u32,
    ) -> Result<GenerationAdmission, InferenceError> {
        use telemetry::SpanName::*;
        for phase in [
            AdmissionQueue,
            AdmissionConnect,
            AdmissionAuthorityLocks,
            AdmissionRead,
            AdmissionTotalsLocks,
            AdmissionLimits,
            AdmissionWrite,
            AdmissionCommit,
        ] {
            telemetry::mark_phase(phase);
        }
        // The only two call sites of this crate's own `mark_shared`, in a
        // test file (excluded from
        // `telemetry::tests::mark_shared_has_one_production_call_site` by
        // its `_tests.rs` name): simulating what a real committed
        // shared-workspace admission does, so these engine tests can
        // observe exported spans without a real database.
        if self.shared.load(Ordering::SeqCst) {
            telemetry::mark_shared(record.root_request_id, &request.model);
        }
        Ok(GenerationAdmission {
            max_output_tokens: None,
        })
    }
    async fn finish_attempt(
        &self,
        record: &ExecutionFinish,
        _telemetry: &AttemptTelemetry,
    ) -> Result<(), InferenceError> {
        if self.slow_finish.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        use telemetry::SpanName::*;
        for phase in [
            SettlementQueue,
            SettlementConnect,
            SettlementLocks,
            SettlementRead,
            SettlementTotalsLocks,
            SettlementWrite,
            SettlementCommit,
        ] {
            telemetry::mark_phase(phase);
        }
        if self.fail_finish.load(Ordering::SeqCst) {
            return Err(InferenceError::Storage);
        }
        self.finishes.lock().unwrap().push(ExecutionFinish {
            id: record.id,
            outcome: record.outcome,
            error: record.error,
            usage: record.usage,
            elapsed_ms: record.elapsed_ms,
        });
        Ok(())
    }
}

#[allow(clippy::large_enum_variant)]
enum Mode {
    Complete(Usage),
    Stream(Vec<ChatEvent>),
    Hang,
    Error(InferenceError),
}
struct Adapter {
    mode: Mutex<Vec<Mode>>,
}
#[async_trait]
impl ProviderAdapter for Adapter {
    fn id(&self) -> &'static str {
        "mock"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: true,
            tools: false,
        }
    }
    async fn execute(
        &self,
        _: &Deployment,
        _: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        let mode = self.mode.lock().unwrap().pop();
        match mode.unwrap_or(Mode::Error(InferenceError::Storage)) {
            Mode::Complete(usage) => Ok(ProviderOutput::Complete(ChatResponse {
                content: Some("hello".into()),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage,
            })),
            Mode::Error(error) => Err(error),
            Mode::Hang => std::future::pending().await,
            Mode::Stream(events) => Ok(ProviderOutput::Stream(Box::pin(stream::iter(
                events.into_iter().map(Ok),
            )))),
        }
    }
}

fn deployment() -> Deployment {
    Deployment {
        id: Uuid::new_v4(),
        provider: "mock".into(),
        upstream_model: "private-model".into(),
        credential_ref: "ref".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec!["chat_completions".into()],
    }
}
fn principal() -> Principal {
    Principal {
        key_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
        user_id: Some(Uuid::new_v4()),
    }
}
fn request(stream: bool) -> ChatRequest {
    ChatRequest {
        model: "public/alias".into(),
        messages: vec![Message {
            role: Role::User,
            content: Some("hi".into()),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        tools: vec![],
        tool_choice: None,
        temperature: None,
        max_output_tokens: None,
        stream,
    }
}
/// `Err` variant of `execute`'s result, without requiring `ProviderOutput`
/// (its `Ok` variant) to implement `Debug`.
fn expect_err(result: Result<ProviderOutput, InferenceError>) -> InferenceError {
    match result {
        Err(error) => error,
        Ok(_) => panic!("expected an error"),
    }
}
fn expect_stream(result: Result<ProviderOutput, InferenceError>) -> EventStream {
    match result.unwrap() {
        ProviderOutput::Stream(s) => s,
        ProviderOutput::Complete(_) => panic!("expected a stream"),
    }
}

/// `(engine, repository, receiver)` wired to one mock deployment/adapter,
/// telemetry enabled at `ratio`. `shared` controls whether admission
/// simulates committing for a shared workspace.
fn harness(
    modes: Vec<Mode>,
    shared: bool,
    ratio: f64,
) -> (
    Engine,
    Arc<TelemetryRepository>,
    tokio::sync::mpsc::Receiver<Box<serde_json::value::RawValue>>,
) {
    let repository = Arc::new(TelemetryRepository::default());
    repository.deployments.lock().unwrap().push(deployment());
    repository.shared.store(shared, Ordering::SeqCst);
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(Adapter {
            mode: Mutex::new(modes),
        }))
        .unwrap();
    let (recorder, receiver) = telemetry::Recorder::channel(ratio);
    let engine = Engine::new(repository.clone(), registry, EngineLimits::default())
        .unwrap()
        .with_telemetry(Some(recorder));
    (engine, repository, receiver)
}

/// Drains every span currently queued. `export` is a synchronous
/// `try_send`, so this needs no real wait on the happy path; the short,
/// generous sleep only covers `ExecutionGuard`'s async drop path.
async fn drain(
    receiver: &mut tokio::sync::mpsc::Receiver<Box<serde_json::value::RawValue>>,
) -> Vec<Value> {
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut spans = Vec::new();
    while let Ok(raw) = receiver.try_recv() {
        spans.push(serde_json::from_str(raw.get()).unwrap());
    }
    spans
}
fn name(span: &Value) -> &str {
    span["name"].as_str().unwrap()
}
fn id(span: &Value) -> String {
    span["spanId"].as_str().unwrap().to_owned()
}
fn parent(span: &Value) -> Option<String> {
    span["parentSpanId"].as_str().map(str::to_owned)
}
fn trace(span: &Value) -> String {
    span["traceId"].as_str().unwrap().to_owned()
}
/// The raw `{"<kind>Value": ...}` object of attribute `key`, if present.
fn attr(span: &Value, key: &str) -> Option<Value> {
    span["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == key)
        .map(|a| a["value"].clone())
}

const ADMISSION_SUBSPANS: [&str; 8] = [
    "admission.queue",
    "admission.connect",
    "admission.authority_locks",
    "admission.read",
    "admission.totals_locks",
    "admission.limits",
    "admission.write",
    "admission.commit",
];
const SETTLEMENT_SUBSPANS: [&str; 7] = [
    "settlement.queue",
    "settlement.connect",
    "settlement.locks",
    "settlement.read",
    "settlement.totals_locks",
    "settlement.write",
    "settlement.commit",
];

#[tokio::test]
async fn shared_workspace_request_exports_the_exact_span_set_and_parentage() {
    let (engine, _repo, mut rx) = harness(
        vec![Mode::Complete(Usage {
            input_tokens: Some(3),
            output_tokens: Some(1),
            ..Default::default()
        })],
        true,
        1.0,
    );
    engine
        .execute(principal(), request(false), Uuid::new_v4())
        .await
        .unwrap();
    let spans = drain(&mut rx).await;
    let names: std::collections::BTreeSet<&str> = spans.iter().map(name).collect();
    let mut expected: Vec<&str> = vec![
        "inference.request",
        "routing",
        "admission",
        "upstream.attempt",
        "settlement",
    ];
    expected.extend(ADMISSION_SUBSPANS);
    expected.extend(SETTLEMENT_SUBSPANS);
    let expected: std::collections::BTreeSet<&str> = expected.into_iter().collect();
    assert_eq!(names, expected, "{spans:#?}");

    // Every span shares one trace id.
    let traces: std::collections::BTreeSet<String> = spans.iter().map(trace).collect();
    assert_eq!(traces.len(), 1);

    let find = |n: &str| spans.iter().find(|s| name(s) == n).unwrap();
    let root = find("inference.request");
    assert_eq!(parent(root), None);
    assert_eq!(
        attr(root, "gateway.outcome"),
        Some(json!({"stringValue": "completed"}))
    );
    for child in ["routing", "admission", "upstream.attempt", "settlement"] {
        assert_eq!(parent(find(child)), Some(id(root)), "{child}");
    }
    let admission = find("admission");
    for sub in ADMISSION_SUBSPANS {
        assert_eq!(parent(find(sub)), Some(id(admission)), "{sub}");
    }
    let settlement = find("settlement");
    for sub in SETTLEMENT_SUBSPANS {
        assert_eq!(parent(find(sub)), Some(id(settlement)), "{sub}");
    }
    let attempt = find("upstream.attempt");
    assert_eq!(
        attr(attempt, "gen_ai.request.model"),
        Some(json!({"stringValue": "public/alias"}))
    );
    assert_eq!(
        attr(attempt, "gen_ai.usage.input_tokens"),
        Some(json!({"intValue": "3"}))
    );
    assert_eq!(
        attr(attempt, "gen_ai.usage.output_tokens"),
        Some(json!({"intValue": "1"}))
    );
    assert_eq!(
        attr(attempt, "gateway.first_content_ns"),
        None,
        "non-streaming TTFT stays absent"
    );
}

#[tokio::test]
async fn personal_workspace_equivalent_exports_nothing() {
    let (engine, _repo, mut rx) = harness(
        vec![Mode::Complete(Usage {
            input_tokens: Some(3),
            output_tokens: Some(1),
            ..Default::default()
        })],
        false, // never commits a shared-workspace admission
        1.0,
    );
    engine
        .execute(principal(), request(false), Uuid::new_v4())
        .await
        .unwrap();
    assert!(drain(&mut rx).await.is_empty());
}

#[tokio::test]
async fn an_unsampled_request_records_nothing() {
    let (engine, _repo, mut rx) = harness(
        vec![Mode::Complete(Usage::default())],
        true,
        0.0, // never sampled, regardless of eligibility
    );
    engine
        .execute(principal(), request(false), Uuid::new_v4())
        .await
        .unwrap();
    assert!(drain(&mut rx).await.is_empty());
}

#[tokio::test]
async fn missing_usage_never_becomes_a_zero_token_attribute() {
    let (engine, _repo, mut rx) = harness(vec![Mode::Complete(Usage::default())], true, 1.0);
    engine
        .execute(principal(), request(false), Uuid::new_v4())
        .await
        .unwrap();
    let spans = drain(&mut rx).await;
    let attempt = spans
        .iter()
        .find(|s| name(s) == "upstream.attempt")
        .unwrap();
    assert_eq!(attr(attempt, "gen_ai.usage.input_tokens"), None);
    assert_eq!(attr(attempt, "gen_ai.usage.output_tokens"), None);
}

#[tokio::test]
async fn empty_and_tool_call_deltas_set_first_content_correctly() {
    let events = vec![
        ChatEvent::Delta {
            text: Some(String::new()),
            tool_calls: vec![],
        },
        ChatEvent::Delta {
            text: None,
            tool_calls: vec![ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("f".into()),
                arguments: Some("{}".into()),
            }],
        },
        ChatEvent::Finish(FinishReason::ToolCalls),
        ChatEvent::Usage(Usage {
            input_tokens: Some(2),
            output_tokens: Some(1),
            ..Default::default()
        }),
        ChatEvent::Done,
    ];
    let (engine, _repo, mut rx) = harness(vec![Mode::Stream(events)], true, 1.0);
    let mut stream = expect_stream(
        engine
            .execute(principal(), request(true), Uuid::new_v4())
            .await,
    );
    // Drive the stream to Done: settlement, and the upstream.attempt span,
    // only happen once the stream's consumer actually reaches it.
    while stream.next().await.transpose().unwrap().is_some() {}
    let spans = drain(&mut rx).await;
    let attempt = spans
        .iter()
        .find(|s| name(s) == "upstream.attempt")
        .unwrap();
    assert!(
        attr(attempt, "gateway.first_content_ns").is_some(),
        "a tool-call delta sets first content, even after an earlier empty text delta: {attempt:#?}"
    );
}

#[tokio::test]
async fn an_attempt_that_times_out_is_recorded_as_a_timeout_error() {
    let repository = Arc::new(TelemetryRepository::default());
    repository.deployments.lock().unwrap().push(deployment());
    repository.shared.store(true, Ordering::SeqCst);
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(Adapter {
            mode: Mutex::new(vec![Mode::Hang]),
        }))
        .unwrap();
    let (recorder, mut rx) = telemetry::Recorder::channel(1.0);
    let limits = EngineLimits {
        request_timeout: std::time::Duration::from_millis(50),
        ..EngineLimits::default()
    };
    let engine = Engine::new(repository, registry, limits)
        .unwrap()
        .with_telemetry(Some(recorder));
    let error = expect_err(
        engine
            .execute(principal(), request(false), Uuid::new_v4())
            .await,
    );
    assert_eq!(error, InferenceError::Timeout);
    let spans = drain(&mut rx).await;
    let attempt = spans
        .iter()
        .find(|s| name(s) == "upstream.attempt")
        .unwrap();
    assert_eq!(
        attr(attempt, "error.type"),
        Some(json!({"stringValue": "timeout"}))
    );
    let root = spans
        .iter()
        .find(|s| name(s) == "inference.request")
        .unwrap();
    assert_eq!(
        attr(root, "gateway.outcome"),
        Some(json!({"stringValue": "error"}))
    );
    assert_eq!(
        attr(root, "error.type"),
        Some(json!({"stringValue": "timeout"}))
    );
}

#[tokio::test]
async fn cancellation_before_the_first_poll_still_exports_a_cancelled_trace() {
    let events = vec![ChatEvent::Delta {
        text: Some("hello".into()),
        tool_calls: vec![],
    }];
    let (engine, _repo, mut rx) = harness(vec![Mode::Stream(events)], true, 1.0);
    let stream = expect_stream(
        engine
            .execute(principal(), request(true), Uuid::new_v4())
            .await,
    );
    drop(stream); // never polled
    let spans = drain(&mut rx).await;
    let root = spans
        .iter()
        .find(|s| name(s) == "inference.request")
        .expect("a cancelled trace is still exported for a shared-workspace admission");
    assert_eq!(
        attr(root, "gateway.outcome"),
        Some(json!({"stringValue": "cancelled"}))
    );
    // Settlement never ran (the stream was never polled): no settlement span.
    assert!(spans.iter().all(|s| name(s) != "settlement"));
    assert!(spans.iter().any(|s| name(s) == "admission"));
}

#[tokio::test]
async fn explicit_failover_produces_two_upstream_attempt_spans() {
    let repository = Arc::new(TelemetryRepository::default());
    let first = deployment();
    let second = deployment();
    repository
        .deployments
        .lock()
        .unwrap()
        .extend([first.clone(), second.clone()]);
    repository.shared.store(true, Ordering::SeqCst);
    *repository.plan.lock().unwrap() = Some(crate::routing::RoutePlan {
        deployment_ids: vec![first.id, second.id],
        max_attempts: 2,
        allow_ambiguous_failover: false,
    });
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(Adapter {
            // Popped in reverse: first call returns Busy, second succeeds.
            mode: Mutex::new(vec![
                Mode::Complete(Usage {
                    input_tokens: Some(1),
                    output_tokens: Some(1),
                    ..Default::default()
                }),
                Mode::Error(InferenceError::Busy),
            ]),
        }))
        .unwrap();
    let (recorder, mut rx) = telemetry::Recorder::channel(1.0);
    let engine = Engine::new(repository, registry, EngineLimits::default())
        .unwrap()
        .with_telemetry(Some(recorder));
    engine
        .execute(principal(), request(false), Uuid::new_v4())
        .await
        .unwrap();
    let spans = drain(&mut rx).await;
    let attempts: Vec<_> = spans
        .iter()
        .filter(|s| name(s) == "upstream.attempt")
        .collect();
    assert_eq!(attempts.len(), 2, "{spans:#?}");
    let mut numbers: Vec<String> = attempts
        .iter()
        .map(|s| {
            attr(s, "gateway.attempt").unwrap()["intValue"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    numbers.sort_unstable();
    assert_eq!(numbers, ["1", "2"]);
    assert_eq!(
        spans.iter().filter(|s| name(s) == "admission").count(),
        2,
        "one admission per attempt"
    );
    assert_eq!(
        spans.iter().filter(|s| name(s) == "settlement").count(),
        2,
        "one settlement per attempt"
    );
    let root = spans
        .iter()
        .find(|s| name(s) == "inference.request")
        .unwrap();
    assert_eq!(
        attr(root, "gateway.outcome"),
        Some(json!({"stringValue": "completed"}))
    );
}

#[tokio::test]
async fn a_settlement_write_failure_is_recorded_and_returned() {
    let repository = Arc::new(TelemetryRepository::default());
    repository.deployments.lock().unwrap().push(deployment());
    repository.shared.store(true, Ordering::SeqCst);
    repository.fail_finish.store(true, Ordering::SeqCst);
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(Adapter {
            mode: Mutex::new(vec![Mode::Complete(Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                ..Default::default()
            })]),
        }))
        .unwrap();
    let (recorder, mut rx) = telemetry::Recorder::channel(1.0);
    let engine = Engine::new(repository, registry, EngineLimits::default())
        .unwrap()
        .with_telemetry(Some(recorder));
    let error = expect_err(
        engine
            .execute(principal(), request(false), Uuid::new_v4())
            .await,
    );
    assert_eq!(error, InferenceError::Storage);
    let spans = drain(&mut rx).await;
    let root = spans
        .iter()
        .find(|s| name(s) == "inference.request")
        .unwrap();
    assert_eq!(
        attr(root, "gateway.outcome"),
        Some(json!({"stringValue": "error"}))
    );
    assert_eq!(
        attr(root, "error.type"),
        Some(json!({"stringValue": "storage"}))
    );
    assert!(
        spans
            .iter()
            .any(|s| name(s) == "settlement" || name(s) == "settlement.queue"),
        "settlement's real phase boundaries are still recorded on a write failure: {spans:#?}"
    );
}

/// #98 PR 2 review item 1: `upstream.attempt`'s own duration must end at the
/// terminal upstream outcome, never grow by a slow settlement write's own
/// time (settlement has its own, separate duration). A slow
/// `finish_attempt` (150ms) makes the pre-fix bug - the attempt span's end
/// timestamp taken only after settlement has already finished - fail this
/// by a wide, unmistakable margin.
#[tokio::test]
async fn the_attempt_span_ends_before_settlement_even_when_settlement_is_slow() {
    let repository = Arc::new(TelemetryRepository::default());
    repository.deployments.lock().unwrap().push(deployment());
    repository.shared.store(true, Ordering::SeqCst);
    repository.slow_finish.store(true, Ordering::SeqCst);
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(Adapter {
            mode: Mutex::new(vec![Mode::Complete(Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                ..Default::default()
            })]),
        }))
        .unwrap();
    let (recorder, mut rx) = telemetry::Recorder::channel(1.0);
    let engine = Engine::new(repository, registry, EngineLimits::default())
        .unwrap()
        .with_telemetry(Some(recorder));
    engine
        .execute(principal(), request(false), Uuid::new_v4())
        .await
        .unwrap();
    let spans = drain(&mut rx).await;
    let attempt = spans
        .iter()
        .find(|s| name(s) == "upstream.attempt")
        .unwrap();
    let settlement = spans.iter().find(|s| name(s) == "settlement").unwrap();
    let attempt_end: u64 = attempt["endTimeUnixNano"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let settlement_start: u64 = settlement["startTimeUnixNano"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        attempt_end <= settlement_start,
        "the upstream.attempt span must end at the terminal upstream outcome, \
         before settlement's own (possibly slow) duration begins: \
         attempt_end={attempt_end} settlement_start={settlement_start}"
    );
}

/// Captures formatted JSON log lines for one test (same shape as
/// `management::snapshot_tests::Captured`, duplicated locally: that one is
/// private to its own module).
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// #98 PR 2 review item 2: `ExecutionGuard`'s reconciliation log lines pass
/// `self.log_trace_id.as_deref()` (an `Option<&str>`) as the `trace_id`
/// field, not `?self.log_trace_id` (`Debug`-formatted `Option<String>`,
/// which would log `Some("…")` - never matching a Tempo/Loki search by the
/// bare hex id - and the literal string `"None"` for an untraced request).
/// Proven directly against `tracing`'s own `Value` impl for `Option<&str>`,
/// through a dedicated local call site: the shared production call sites
/// (`inference finalization failed…`) are also hit, unguarded, by several
/// other tests and by production code, so racing tracing's process-wide
/// per-callsite interest cache under parallel `cargo test` made capturing
/// *that* exact call site flaky; this call site exists only here.
#[test]
fn trace_id_field_is_a_bare_string_when_present_and_absent_when_none() {
    fn emit(trace_id: Option<&str>) {
        tracing::error!(
            trace_id = trace_id,
            "#98 pr2 review item 2: trace_id field probe"
        );
    }
    let out = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(out.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();
    emit(Some("992a49ad267e06d4659fc313c1b9c6ba"));
    emit(None);
    drop(guard);
    let logged = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<Value> = logged
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{logged}");
    assert_eq!(
        lines[0]["fields"]["trace_id"], "992a49ad267e06d4659fc313c1b9c6ba",
        "a known trace id must log as the bare hex string, not Debug-formatted: {logged}"
    );
    assert!(
        lines[1]["fields"].get("trace_id").is_none(),
        "an absent trace id must omit the field entirely, never log the literal string \"None\": {logged}"
    );
}

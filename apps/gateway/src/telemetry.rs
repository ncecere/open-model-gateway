//! Bounded, typed OTLP/HTTP JSON traces. No automatic tracing-event export.
//!
//! Wire mapping (opentelemetry-proto v1.9.0):
//! - collector/trace/v1/trace_service.proto: ExportTraceServiceRequest.resource_spans
//!   and ExportTraceServiceResponse.partial_success / ExportTracePartialSuccess.rejected_spans.
//! - trace/v1/trace.proto: ResourceSpans.resource / scope_spans, ScopeSpans.scope / spans,
//!   Span.trace_id / span_id / parent_span_id / name / kind / flags /
//!   start_time_unix_nano / end_time_unix_nano / attributes.
//! - resource/v1/resource.proto and common/v1/common.proto: Resource.attributes,
//!   InstrumentationScope.name / version, KeyValue.key / value and
//!   AnyValue.string_value / int_value / bool_value.
//!
//! JSON uses lowerCamelCase, integer enums, hexadecimal ID bytes (the OTLP
//! exception to protobuf base64), decimal-string 64-bit integers/timestamps.
//! Sources: <https://github.com/open-telemetry/opentelemetry-proto/tree/v1.9.0/opentelemetry/proto>
//! and <https://opentelemetry.io/docs/specs/otlp/#json-protobuf-encoding>.

use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use serde::{Serialize, Serializer};
use serde_json::{Value, json, value::RawValue};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use uuid::Uuid;

/// Current wall-clock time as OTLP's Unix nanoseconds. Not monotonic: two
/// calls may regress across a system clock adjustment, which [`Span::new`]
/// and [`Span::reserved`] already refuse (`end < start`) rather than panic.
pub fn now_unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

/// Reserves a span id usable as `parent` before the reserving span itself is
/// known to have completed: an "admission"/"settlement"/"upstream.attempt"/
/// "inference.request" wrapper whose own [`Span::reserved`] is only built
/// once its real end boundary is known. `None` only on entropy exhaustion,
/// matching [`Span::new`]'s own failure mode (dropped, never a panic).
pub fn reserve_span_id() -> Option<[u8; 8]> {
    let mut id = crate::entropy::bytes::<8>().ok()?;
    if id == [0; 8] {
        id = crate::entropy::bytes::<8>().ok()?;
    }
    (id != [0; 8]).then_some(id)
}

const MAX_SPAN: usize = 1024;
const MAX_ATTRIBUTES: usize = 24;
const MAX_REQUEST: usize = 64;
const QUEUE: usize = 2048;
const ACTIVE: usize = 256;
const MAX_BATCH: usize = 64;
const MAX_BODY: usize = 64 * 1024;
const EXCHANGE: Duration = Duration::from_secs(2);
const DRAIN: Duration = Duration::from_secs(3);

/// Correlation only, not proof of sampling or delivery. No client trace context
/// participates. The versioned domain is part of this stable mapping.
pub fn trace_id(request: Uuid) -> [u8; 16] {
    let hash = Sha256::new()
        .chain_update(b"omg.inference.trace.v1\0")
        .chain_update(request.as_bytes())
        .finalize();
    let mut id = [0; 16];
    id.copy_from_slice(&hash[..16]);
    if id == [0; 16] {
        id[15] = 1;
    }
    id
}

/// Deterministic root decision. Children inherit their gateway context's
/// decision; inbound traceparent/tracestate/baggage are never read here.
pub fn root_sampled(request: Uuid, ratio: f64) -> bool {
    if !ratio.is_finite() || ratio <= 0.0 {
        return false;
    }
    if ratio >= 1.0 {
        return true;
    }
    let hash = Sha256::new()
        .chain_update(b"omg.inference.sample.v1\0")
        .chain_update(request.as_bytes())
        .finalize();
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&hash[..8]);
    // 53 bits are exactly representable, avoiding rounding the maximum to 1.
    let fraction = (u64::from_be_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64;
    fraction < ratio
}

macro_rules! fixed_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub const COUNT: usize = Self::ALL.len();
            pub fn as_str(self) -> &'static str { match self { $(Self::$variant => $text),+ } }
        }
    };
}
fixed_enum!(SpanName {
    InferenceRequest => "inference.request", Routing => "routing", Admission => "admission",
    UpstreamAttempt => "upstream.attempt", Settlement => "settlement",
    RatePrune => "rate_prune", LockWaitPrune => "lock_wait_prune",
    AdmissionQueue => "admission.queue", AdmissionConnect => "admission.connect",
    AdmissionCatalogLocks => "admission.catalog_locks", AdmissionAuthorityLocks => "admission.authority_locks",
    AdmissionRead => "admission.read", AdmissionPrice => "admission.price", AdmissionTotalsLocks => "admission.totals_locks",
    AdmissionLimits => "admission.limits", AdmissionWrite => "admission.write", AdmissionCommit => "admission.commit",
    SettlementQueue => "settlement.queue", SettlementConnect => "settlement.connect", SettlementLocks => "settlement.locks",
    SettlementRead => "settlement.read", SettlementTotalsLocks => "settlement.totals_locks",
    SettlementWrite => "settlement.write", SettlementCommit => "settlement.commit"
});
fixed_enum!(Operation { Chat => "chat", Responses => "responses", Messages => "messages" });
fixed_enum!(Provider { OpenAi => "openai", Anthropic => "anthropic", Bedrock => "bedrock", OpenRouter => "openrouter", Compatible => "openai_compatible", Ollama => "ollama", Vllm => "vllm", Sglang => "sglang" });
fixed_enum!(Route { Chat => "/v1/chat/completions", Responses => "/v1/responses", Messages => "/v1/messages" });
fixed_enum!(Outcome { Completed => "completed", Error => "error", Cancelled => "cancelled", Unknown => "unknown" });
fixed_enum!(ErrorKind { InvalidRequest => "invalid_request", Unauthorized => "unauthorized", Forbidden => "forbidden", NotFound => "not_found", Unsupported => "unsupported", Busy => "busy", Timeout => "timeout", Upstream => "upstream", Storage => "storage", Configuration => "configuration", Cancelled => "cancelled", Unknown => "unknown" });
fixed_enum!(DropReason { Contexts => "contexts", RequestLimit => "request_limit", SpanLimit => "span_limit", QueueFull => "queue_full", Shutdown => "shutdown", Ineligible => "ineligible", Delivery => "delivery" });
fixed_enum!(Delivery { Accepted => "accepted", HttpStatus => "http_status", Transport => "transport", Timeout => "timeout", Acknowledgement => "acknowledgement" });

/// Constructed only after primary admission has proved shared-workspace
/// eligibility. Intentionally has no production constructor in PR 1: PR 2
/// connects this capability to live admission. Dropping an unclassified
/// context exports nothing, including pre-admission observations.
///
/// It also carries the canonical public model alias that admission resolved,
/// so the alias attribute's provenance is admission itself: no caller can
/// pass arbitrary text (a prompt, a key, a raw client model string) as a
/// "validated" alias, or enrol strings into the process-wide alias registry.
pub struct SharedAdmission {
    request: Uuid,
    alias: String,
}
impl SharedAdmission {
    /// The only production constructor, called from exactly one place: this
    /// module's own [`mark_shared`], itself called only from the commit
    /// branch of `governance::admit_unobserved`, after `tx.commit()` has
    /// succeeded for a `team`/`project` workspace: never personal, never
    /// from client input, never before commit.
    /// `telemetry::tests::shared_admission_has_one_production_call_site` and
    /// `telemetry::tests::mark_shared_has_one_production_call_site` enforce
    /// both halves of that chain.
    pub(crate) fn new(request: Uuid, alias: String) -> Self {
        Self { request, alias }
    }
}

#[derive(Clone)]
pub struct PublicAlias(String);
impl PublicAlias {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Default)]
struct AliasRegistry {
    values: Mutex<HashSet<String>>,
}
impl AliasRegistry {
    /// Only reachable through [`Context::admitted_alias`], with the alias a
    /// [`SharedAdmission`] carries. Contention folds to other rather than waiting.
    fn admission_validated(&self, alias: &str) -> PublicAlias {
        let other = || PublicAlias("other".into());
        if alias.is_empty() || alias.chars().count() > 96 || alias.chars().any(char::is_control) {
            return other();
        }
        let Ok(mut values) = self.values.try_lock() else {
            return other();
        };
        if !values.contains(alias) && values.len() >= 200 {
            return other();
        }
        values.insert(alias.to_owned());
        PublicAlias(alias.to_owned())
    }
}

/// No arbitrary keys, strings, Debug values, URLs or exception messages.
/// UUID variants are server-generated request/attempt IDs, never identity IDs.
///
/// ```compile_fail
/// use open_model_gateway::telemetry::Attribute;
/// let forbidden = Attribute::Prompt("private prompt".to_owned());
/// ```
/// ```compile_fail
/// use open_model_gateway::telemetry::PublicAlias;
/// let unvalidated = PublicAlias("raw client model".to_owned());
/// ```
pub enum Attribute {
    Operation(Operation),
    Model(PublicAlias),
    Provider(Provider),
    InputTokens(u64),
    OutputTokens(u64),
    Streamed(bool),
    Attempt(u8),
    Route(Route),
    Outcome(Outcome),
    Error(ErrorKind),
    FirstContent(Duration),
    Generation(Duration),
    RequestId(Uuid),
    AttemptId(Uuid),
}
impl Attribute {
    fn value(self) -> Option<Value> {
        let string = |key, value: &str| json!({"key":key,"value":{"stringValue":value}});
        let integer = |key, n: u64| {
            (n <= i64::MAX as u64).then(|| json!({"key":key,"value":{"intValue":n.to_string()}}))
        };
        Some(match self {
            Self::Operation(v) => string("gen_ai.operation.name", v.as_str()),
            Self::Model(v) => string("gen_ai.request.model", v.as_str()),
            Self::Provider(v) => string("gen_ai.provider.name", v.as_str()),
            Self::InputTokens(v) => return integer("gen_ai.usage.input_tokens", v),
            Self::OutputTokens(v) => return integer("gen_ai.usage.output_tokens", v),
            Self::Streamed(v) => json!({"key":"gateway.streamed","value":{"boolValue":v}}),
            Self::Attempt(v) if (1..=3).contains(&v) => {
                return integer("gateway.attempt", u64::from(v));
            }
            Self::Attempt(_) => return None,
            Self::Route(v) => string("http.route", v.as_str()),
            Self::Outcome(v) => string("gateway.outcome", v.as_str()),
            Self::Error(v) => string("error.type", v.as_str()),
            Self::FirstContent(v) => {
                return integer("gateway.first_content_ns", v.as_nanos().try_into().ok()?);
            }
            Self::Generation(v) => {
                return integer("gateway.generation_ns", v.as_nanos().try_into().ok()?);
            }
            Self::RequestId(v) => string("gateway.request_id", &v.to_string()),
            Self::AttemptId(v) => string("gateway.attempt_id", &v.to_string()),
        })
    }
}

/// A sanitized completed span. Construction and mutation enforce the wire
/// budget; Serialize only (no untrusted deserialization escape hatch).
pub struct Span {
    name: SpanName,
    trace: [u8; 16],
    id: [u8; 8],
    parent: Option<[u8; 8]>,
    start: u64,
    end: u64,
    attributes: Vec<Value>,
}
impl Span {
    pub fn new(
        name: SpanName,
        trace: [u8; 16],
        parent: Option<[u8; 8]>,
        start: u64,
        end: u64,
    ) -> Option<Self> {
        if trace == [0; 16] || parent == Some([0; 8]) || end < start {
            return None;
        }
        let mut id = crate::entropy::bytes::<8>().ok()?;
        if id == [0; 8] {
            id = crate::entropy::bytes::<8>().ok()?;
        }
        if id == [0; 8] {
            return None;
        }
        Some(Self {
            name,
            trace,
            id,
            parent,
            start,
            end,
            attributes: Vec::new(),
        })
    }
    /// [`Span::new`] with a previously [`reserve_span_id`]d, non-zero id
    /// (rather than a freshly generated one), so a span reserved before it
    /// completed keeps the exact id its children already carry as `parent`.
    pub fn reserved(
        name: SpanName,
        trace: [u8; 16],
        id: [u8; 8],
        parent: Option<[u8; 8]>,
        start: u64,
        end: u64,
    ) -> Option<Self> {
        if trace == [0; 16] || id == [0; 8] || parent == Some([0; 8]) || end < start {
            return None;
        }
        Some(Self {
            name,
            trace,
            id,
            parent,
            start,
            end,
            attributes: Vec::new(),
        })
    }
    pub fn id(&self) -> [u8; 8] {
        self.id
    }
    pub fn attribute(&mut self, attribute: Attribute) -> bool {
        let Some(value) = attribute.value() else {
            return false;
        };
        // Duplicate keys are not legal OTLP attributes. Keep the first.
        if self.attributes.len() >= MAX_ATTRIBUTES
            || self.attributes.iter().any(|v| v["key"] == value["key"])
        {
            return false;
        }
        self.attributes.push(value);
        if serde_json::to_vec(self).map_or(true, |bytes| bytes.len() > MAX_SPAN) {
            self.attributes.pop();
            // This span is retained. Attribute refusal is not a dropped span.
            return false;
        }
        true
    }
}
impl Serialize for Span {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut value = json!({"traceId":hex::encode(self.trace), "spanId":hex::encode(self.id),
            "name":self.name.as_str(), "kind":1, "flags":1,
            "startTimeUnixNano":self.start.to_string(), "endTimeUnixNano":self.end.to_string(), "attributes":self.attributes});
        if let Some(parent) = self.parent {
            value["parentSpanId"] = json!(hex::encode(parent));
        }
        value.serialize(serializer)
    }
}

#[derive(Default)]
struct Counts {
    drops: [AtomicU64; DropReason::COUNT],
    deliveries: [AtomicU64; Delivery::COUNT],
    rejected: AtomicU64,
}
impl Counts {
    fn drop_spans(&self, reason: DropReason, n: u64) {
        self.drops[reason as usize].fetch_add(n, Ordering::Relaxed);
        crate::metrics::METRICS.trace_drop(reason, n);
    }
    fn delivery(&self, outcome: Delivery) {
        self.deliveries[outcome as usize].fetch_add(1, Ordering::Relaxed);
        crate::metrics::METRICS.trace_delivery(outcome);
    }
    fn reject(&self, n: u64) {
        self.rejected.fetch_add(n, Ordering::Relaxed);
        crate::metrics::METRICS.trace_rejected(n);
    }
    #[cfg(test)]
    fn dropped(&self, reason: DropReason) -> u64 {
        self.drops[reason as usize].load(Ordering::Relaxed)
    }
    #[cfg(test)]
    fn deliveries(&self, outcome: Delivery) -> u64 {
        self.deliveries[outcome as usize].load(Ordering::Relaxed)
    }
    #[cfg(test)]
    fn rejected(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }
}

static ALIASES: std::sync::LazyLock<Arc<AliasRegistry>> = std::sync::LazyLock::new(Arc::default);

#[derive(Clone)]
pub struct Recorder {
    sender: mpsc::Sender<Box<RawValue>>,
    active: Arc<Semaphore>,
    counts: Arc<Counts>,
    stopping: Arc<AtomicBool>,
    ratio: f64,
    aliases: Arc<AliasRegistry>,
}
impl Recorder {
    fn channel(ratio: f64) -> (Self, mpsc::Receiver<Box<RawValue>>) {
        std::sync::LazyLock::force(&crate::metrics::METRICS);
        let (sender, receiver) = mpsc::channel(QUEUE);
        (
            Self {
                sender,
                active: Arc::new(Semaphore::new(ACTIVE)),
                counts: Arc::default(),
                stopping: Arc::default(),
                ratio,
                aliases: std::sync::LazyLock::force(&ALIASES).clone(),
            },
            receiver,
        )
    }
    pub fn begin(&self, request: Uuid) -> Option<Context> {
        if self.stopping.load(Ordering::Relaxed) || !root_sampled(request, self.ratio) {
            return None;
        }
        let Ok(permit) = self.active.clone().try_acquire_owned() else {
            self.counts.drop_spans(DropReason::Contexts, 1);
            return None;
        };
        Some(Context {
            recorder: self.clone(),
            _permit: permit,
            trace: trace_id(request),
            request,
            spans: Vec::new(),
            observed: 0,
        })
    }
}

pub struct Context {
    recorder: Recorder,
    _permit: OwnedSemaphorePermit,
    trace: [u8; 16],
    request: Uuid,
    spans: Vec<Span>,
    observed: usize,
}
impl Context {
    /// Children share this trace and root sampling decision, without reacquiring
    /// an active context. Timing is supplied at actual phase boundaries in PR 2.
    pub fn trace_id(&self) -> [u8; 16] {
        self.trace
    }
    pub fn record(&mut self, mut span: Span) {
        if self.observed >= MAX_REQUEST {
            self.recorder.counts.drop_spans(DropReason::RequestLimit, 1);
            return;
        }
        self.observed += 1;
        span.trace = self.trace;
        self.spans.push(span);
    }
    /// The model alias admission resolved for this request, folded to
    /// `other` past the registry bound. `None` for another request's admission.
    pub fn admitted_alias(&self, admission: &SharedAdmission) -> Option<PublicAlias> {
        (admission.request == self.request)
            .then(|| self.recorder.aliases.admission_validated(&admission.alias))
    }

    pub fn export(mut self, eligibility: SharedAdmission) {
        if eligibility.request != self.request {
            return;
        }
        for span in self.spans.drain(..) {
            // Practically unreachable (Span serializes a plain JSON tree), but
            // every span is either delivered or counted: never a silent skip.
            let Ok(text) = serde_json::to_string(&span) else {
                self.recorder.counts.drop_spans(DropReason::SpanLimit, 1);
                continue;
            };
            if text.len() > MAX_SPAN {
                self.recorder.counts.drop_spans(DropReason::SpanLimit, 1);
                continue;
            }
            let Ok(raw) = RawValue::from_string(text) else {
                self.recorder.counts.drop_spans(DropReason::SpanLimit, 1);
                continue;
            };
            if let Err(error) = self.recorder.sender.try_send(raw) {
                let reason = match error {
                    mpsc::error::TrySendError::Full(_) => DropReason::QueueFull,
                    mpsc::error::TrySendError::Closed(_) => DropReason::Shutdown,
                };
                self.recorder.counts.drop_spans(reason, 1);
            }
        }
    }
}
impl Drop for Context {
    fn drop(&mut self) {
        self.recorder
            .counts
            .drop_spans(DropReason::Ineligible, self.spans.len() as u64);
    }
}

tokio::task_local! {
    /// Ambient per the one wrapped await, never a request-wide thread-local:
    /// bridges a reserved "admission"/"settlement" wrapper span to the real
    /// phase boundaries reached inside `governance.rs`'s admission/
    /// settlement transactions, across the `InferenceRepository`
    /// trait-object boundary (PR 2 does not change that trait). Absent for
    /// every call that is not wrapped in [`with_phases`] (disabled
    /// telemetry, or any admission/settlement path outside interactive
    /// generation), so [`mark_phase`]/[`mark_shared`] are then a single
    /// `try_with` failure: no allocation, no hashing.
    static PHASES: Arc<Mutex<PhaseBuffer>>;
}

/// Phase boundaries of one ambient admission or settlement call, collected
/// as real [`Span`]s with [`mark_phase`], plus the [`SharedAdmission`]
/// capability an admission commit resolved, if any ([`mark_shared`]).
pub struct PhaseBuffer {
    trace: [u8; 16],
    parent: [u8; 8],
    mark: u64,
    spans: Vec<Span>,
    eligibility: Option<SharedAdmission>,
}
impl PhaseBuffer {
    /// `parent` is the reserved id of the "admission"/"settlement" wrapper
    /// span the caller will build once this ambient call returns.
    pub fn new(trace: [u8; 16], parent: [u8; 8], start: u64) -> Self {
        Self {
            trace,
            parent,
            mark: start,
            spans: Vec::new(),
            eligibility: None,
        }
    }
    /// Spans recorded since construction, and the eligibility capability a
    /// commit resolved, if any. Takes `self` so a caller merges exactly once.
    pub fn take(self) -> (Vec<Span>, Option<SharedAdmission>) {
        (self.spans, self.eligibility)
    }
}

/// Runs `fut` with `buffer` as the ambient phase recorder nested
/// admission/settlement calls record real boundaries into via [`mark_phase`].
/// The caller still owns `buffer` afterward (an `Arc` clone) to merge its
/// spans into the request's real [`Context`].
pub async fn with_phases<F: std::future::Future>(
    buffer: Arc<Mutex<PhaseBuffer>>,
    fut: F,
) -> F::Output {
    PHASES.scope(buffer, fut).await
}

/// Records one real phase boundary of the ambient admission/settlement call
/// if telemetry is active for this request; otherwise a no-op. `try_lock`
/// only: a contended buffer (never expected, since one request's admission
/// and settlement never run concurrently with themselves) just drops this
/// one boundary rather than waiting.
pub(crate) fn mark_phase(name: SpanName) {
    let _ = PHASES.try_with(|buffer| {
        let Ok(mut buffer) = buffer.try_lock() else {
            return;
        };
        let now = now_unix_nanos();
        if let Some(span) = Span::new(name, buffer.trace, Some(buffer.parent), buffer.mark, now) {
            buffer.spans.push(span);
        }
        buffer.mark = now;
    });
}

/// Records the [`SharedAdmission`] an admission commit resolved for the
/// ambient request, if telemetry is active. The only call site is the
/// commit branch of `governance::admit_unobserved`, itself gated on the
/// workspace being `team`/`project`: never personal, never from client text.
pub(crate) fn mark_shared(request: Uuid, alias: &str) {
    let _ = PHASES.try_with(|buffer| {
        if let Ok(mut buffer) = buffer.try_lock() {
            buffer.eligibility = Some(SharedAdmission::new(request, alias.to_owned()));
        }
    });
}

struct Exporter {
    url: reqwest::Url,
    client: reqwest::Client,
    headers: reqwest::header::HeaderMap,
    environment: crate::config::Environment,
}
impl Exporter {
    /// ExportTraceServiceRequest.resource_spans -> ResourceSpans.scope_spans ->
    /// ScopeSpans.spans. OTLP JSON uses lowerCamelCase, hex ID bytes and decimal
    /// strings for fixed64 timestamps and AnyValue.int_value. No discovery.
    fn body(&self, spans: &[Box<RawValue>]) -> Vec<u8> {
        serde_json::to_vec(&json!({"resourceSpans":[{
            "resource":{"attributes":[
                {"key":"service.name","value":{"stringValue":"open-model-gateway"}},
                {"key":"service.version","value":{"stringValue":env!("CARGO_PKG_VERSION")}},
                {"key":"deployment.environment.name","value":{"stringValue":self.environment.as_str()}}
            ]},
            "scopeSpans":[{"scope":{"name":"omg.inference","version":env!("CARGO_PKG_VERSION")},"spans":spans}]
        }]})).expect("bounded JSON values serialize")
    }
    async fn send(&self, body: Vec<u8>, count: usize) -> Result<u64, Delivery> {
        // Explicit outer deadline covers connect, headers AND bounded ack reads.
        tokio::time::timeout(EXCHANGE, async {
            let mut response = self
                .client
                .post(self.url.clone())
                .headers(self.headers.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| {
                    if e.is_timeout() {
                        Delivery::Timeout
                    } else {
                        Delivery::Transport
                    }
                })?;
            if !response.status().is_success() {
                return Err(Delivery::HttpStatus);
            }
            let mut ack = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|error| {
                if error.is_timeout() {
                    Delivery::Timeout
                } else {
                    Delivery::Acknowledgement
                }
            })? {
                if ack.len() + chunk.len() > MAX_BODY {
                    return Err(Delivery::Acknowledgement);
                }
                ack.extend_from_slice(&chunk);
            }
            if ack.is_empty() {
                return Ok(0);
            }
            let value: Value =
                serde_json::from_slice(&ack).map_err(|_| Delivery::Acknowledgement)?;
            if !value.is_object()
                || (!value["partialSuccess"].is_null() && !value["partialSuccess"].is_object())
            {
                return Err(Delivery::Acknowledgement);
            }
            let rejected = &value["partialSuccess"]["rejectedSpans"];
            if rejected.is_null() {
                return Ok(0);
            }
            let n = rejected
                .as_u64()
                .or_else(|| rejected.as_str()?.parse().ok())
                .ok_or(Delivery::Acknowledgement)?;
            Ok(n.min(count as u64))
        })
        .await
        .unwrap_or(Err(Delivery::Timeout))
    }
}

/// Owns exactly one worker. No inference request waits for it, retries, disk
/// spool, readiness dependency, or per-span tasks.
pub struct Runtime {
    recorder: Recorder,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}
impl Runtime {
    pub fn start(config: crate::config::TraceConfig) -> Option<Self> {
        let (url, client) = config.endpoint?;
        let exporter = Exporter {
            url,
            client,
            headers: config.headers,
            environment: config.environment,
        };
        let (recorder, receiver) = Recorder::channel(config.sample_ratio);
        let (stop, stopping) = oneshot::channel();
        let counts = recorder.counts.clone();
        let task = tokio::spawn(worker(
            exporter,
            Pending {
                receiver,
                counts,
                in_flight: 0,
            },
            stopping,
        ));
        Some(Self {
            recorder,
            stop: Some(stop),
            task,
        })
    }
    pub fn recorder(&self) -> Recorder {
        self.recorder.clone()
    }
    pub async fn shutdown(mut self) {
        self.recorder.stopping.store(true, Ordering::Relaxed);
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if tokio::time::timeout(DRAIN, &mut self.task).await.is_err() {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.recorder.stopping.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

struct Pending {
    receiver: mpsc::Receiver<Box<RawValue>>,
    counts: Arc<Counts>,
    in_flight: usize,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.receiver.close();
        let mut n = self.in_flight as u64;
        while self.receiver.try_recv().is_ok() {
            n += 1;
        }
        self.counts.drop_spans(DropReason::Shutdown, n);
    }
}
async fn worker(exporter: Exporter, mut pending: Pending, mut stop: oneshot::Receiver<()>) {
    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(1),
        Duration::from_secs(1),
    );
    // A slow exchange must not align the next two batches to adjacent old
    // tick boundaries and create a catch-up burst (also true while draining).
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut draining = false;
    let overhead = exporter.body(&[]).len();
    loop {
        if draining {
            if pending.receiver.is_empty() {
                break;
            }
            interval.tick().await;
        } else {
            tokio::select! {
                _ = &mut stop => {
                    draining = true;
                    pending.receiver.close();
                    if pending.receiver.is_empty() { break; }
                    // Drain obeys the same cadence as ordinary delivery.
                    interval.tick().await;
                }
                _ = interval.tick() => {}
            }
        }
        let mut batch = Vec::with_capacity(MAX_BATCH);
        let mut size = overhead;
        while batch.len() < MAX_BATCH {
            // Reserve room for the largest possible next span before dequeue,
            // so no extra off-queue carry slot can escape shutdown accounting.
            if size + MAX_SPAN + usize::from(!batch.is_empty()) > MAX_BODY {
                break;
            }
            let Ok(span) = pending.receiver.try_recv() else {
                break;
            };
            size += span.get().len() + usize::from(!batch.is_empty());
            batch.push(span);
        }
        if batch.is_empty() {
            if draining {
                break;
            }
            continue;
        }
        let body = exporter.body(&batch);
        debug_assert!(body.len() <= MAX_BODY);
        pending.in_flight = batch.len();
        match exporter.send(body, batch.len()).await {
            Ok(rejected) => {
                pending.counts.delivery(Delivery::Accepted);
                pending.counts.reject(rejected);
            }
            Err(outcome) => {
                pending.counts.delivery(outcome);
                pending
                    .counts
                    .drop_spans(DropReason::Delivery, batch.len() as u64);
            }
        }
        pending.in_flight = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Environment, TraceConfig};
    use serde_json::{Value, json};
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    use uuid::Uuid;

    fn config(values: &[(&str, &str)]) -> anyhow::Result<TraceConfig> {
        TraceConfig::from_lookup(
            |key| {
                values
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| (*v).into())
            },
            Environment::Development,
        )
    }

    #[test]
    fn configuration_refuses_invalid_values_without_echoing_them() {
        assert!(config(&[]).unwrap().endpoint.is_none());
        assert_eq!(config(&[]).unwrap().sample_ratio, 0.01);
        assert!(
            config(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "bad")])
                .unwrap()
                .endpoint
                .is_none()
        );
        for ratio in ["NaN", "inf", "-0.1", "1.1", "secret-ratio"] {
            let error = config(&[("GATEWAY_TRACES_SAMPLE_RATIO", ratio)])
                .err()
                .unwrap()
                .to_string();
            assert!(!error.contains(ratio));
        }
        for url in [
            "https://user:secret@example.com/v1/traces",
            "https://example.com/v1/traces?secret",
            "https://example.com/v1/traces#secret",
            "http://example.com/v1/traces",
            "https://169.254.169.254/v1/traces",
            "https://[fd00:ec2::254]/v1/traces",
            "https://10.1.1.1/v1/traces",
            "https://example.com/v1/logs",
            "not-a-url",
        ] {
            let error = config(&[("GATEWAY_TRACES_OTLP_URL", url)])
                .err()
                .unwrap()
                .to_string();
            assert!(!error.contains(url));
        }
        for header in [
            "Content-Type=secret",
            "Host=secret",
            "X=secret\r\nX:y",
            "bad-secret",
            "X=",
        ] {
            let error = config(&[
                ("GATEWAY_TRACES_OTLP_URL", "https://example.com/v1/traces"),
                ("GATEWAY_TRACES_OTLP_HEADERS_ENV", "TRACE_SECRET"),
                ("TRACE_SECRET", header),
            ])
            .err()
            .unwrap()
            .to_string();
            assert!(!error.contains("secret"));
        }
        assert!(config(&[("GATEWAY_TRACES_OTLP_HEADERS_ENV", "TRACE_SECRET")]).is_err());
        for reference in [
            "GATEWAY_SECRET_ENV_ALLOWLIST",
            "GATEWAY_SMTP_SECRET_ENV_ALLOWLIST",
            "GATEWAY_AUDIT_OTLP_HEADERS_ENV",
            "GATEWAY_AUDIT_WEBHOOK_SECRET_ENV",
        ] {
            assert!(
                config(&[
                    ("GATEWAY_TRACES_OTLP_URL", "https://example.com/v1/traces"),
                    ("GATEWAY_TRACES_OTLP_HEADERS_ENV", "TRACE_SECRET"),
                    ("TRACE_SECRET", "Authorization=Bearer secret"),
                    (reference, "TRACE_SECRET")
                ])
                .is_err()
            );
        }
        let large = format!("X={}", "x".repeat(4096));
        assert!(
            config(&[
                ("GATEWAY_TRACES_OTLP_URL", "https://example.com/v1/traces"),
                ("GATEWAY_TRACES_OTLP_HEADERS_ENV", "TRACE_SECRET"),
                ("TRACE_SECRET", &large)
            ])
            .is_err()
        );
        let many = (0..17)
            .map(|i| format!("X-{i}=x"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            config(&[
                ("GATEWAY_TRACES_OTLP_URL", "https://example.com/v1/traces"),
                ("GATEWAY_TRACES_OTLP_HEADERS_ENV", "TRACE_SECRET"),
                ("TRACE_SECRET", &many)
            ])
            .is_err()
        );
    }

    #[test]
    fn alias_budget_is_process_wide() {
        let (a, _) = Recorder::channel(1.0);
        let (b, _) = Recorder::channel(1.0);
        assert!(Arc::ptr_eq(&a.aliases, &b.aliases));
    }

    #[test]
    fn sampling_and_trace_ids_are_deterministic() {
        let id = Uuid::from_u128(42);
        assert_eq!(trace_id(id), trace_id(id));
        assert_ne!(trace_id(id), trace_id(Uuid::from_u128(43)));
        assert_ne!(trace_id(id), [0; 16]);
        assert_ne!(trace_id(id), *id.as_bytes());
        assert_eq!(
            hex::encode(trace_id(id)),
            "6c77e4c0e472d7dff3c61cccbe5e19b6"
        );
        let sampled = (1..=10000)
            .filter(|n| root_sampled(Uuid::from_u128(*n), 0.01))
            .count();
        assert!((60..=140).contains(&sampled), "{sampled}");
        for n in 1..100 {
            let id = Uuid::from_u128(n);
            assert!(!root_sampled(id, 0.0));
            assert!(root_sampled(id, 1.0));
            assert_eq!(root_sampled(id, 0.3), root_sampled(id, 0.3));
        }
    }

    fn span() -> Span {
        Span::new(
            SpanName::InferenceRequest,
            trace_id(Uuid::from_u128(42)),
            None,
            1_700_000_000_000_000_001,
            1_700_000_000_000_000_002,
        )
        .unwrap()
    }

    #[test]
    fn typed_attributes_and_limits_never_export_arbitrary_fields() {
        let mut span = span();
        span.attribute(Attribute::Operation(Operation::Chat));
        span.attribute(Attribute::InputTokens(42));
        span.attribute(Attribute::Streamed(true));
        let body = serde_json::to_value(&span).unwrap();
        assert_eq!(body["name"], "inference.request");
        assert_eq!(
            body["attributes"][1],
            json!({"key":"gen_ai.usage.input_tokens","value":{"intValue":"42"}})
        );
        let aliases = AliasRegistry::default();
        assert_eq!(aliases.admission_validated("a").as_str(), "a");
        assert_eq!(
            aliases.admission_validated(&"x".repeat(97)).as_str(),
            "other"
        );
        for n in 0..200 {
            aliases.admission_validated(&format!("model-{n}"));
        }
        assert_eq!(aliases.admission_validated("new-value").as_str(), "other");
        for _ in 0..25 {
            span.attribute(Attribute::InputTokens(i64::MAX as u64));
        }
        assert!(span.attributes.len() <= 24);
        assert!(serde_json::to_vec(&span).unwrap().len() <= 1024);
        let text = serde_json::to_string(&span).unwrap();
        for forbidden in [
            "prompt",
            "response",
            "workspace",
            "user",
            "secret",
            "hostname",
            "exception",
        ] {
            assert!(!text.contains(forbidden));
        }
    }

    #[tokio::test]
    async fn contexts_queue_and_span_budgets_are_nonblocking_and_count_drops() {
        let (recorder, mut rx) = Recorder::channel(1.0);
        let contexts = (0..256)
            .map(|_| recorder.begin(Uuid::new_v4()).unwrap())
            .collect::<Vec<_>>();
        assert!(recorder.begin(Uuid::new_v4()).is_none());
        assert_eq!(recorder.counts.dropped(DropReason::Contexts), 1);
        drop(contexts);
        let mut context = recorder.begin(Uuid::new_v4()).unwrap();
        for _ in 0..65 {
            context.record(span());
        }
        assert_eq!(recorder.counts.dropped(DropReason::RequestLimit), 1);
        // Unknown or failed eligibility never exports buffered observations.
        drop(context);
        assert!(rx.try_recv().is_err());
        for _ in 0..32 {
            let mut context = recorder.begin(Uuid::new_v4()).unwrap();
            for _ in 0..64 {
                context.record(span());
            }
            export_shared(context);
        }
        assert_eq!(rx.len(), 2048);
        let started = Instant::now();
        let mut context = recorder.begin(Uuid::new_v4()).unwrap();
        context.record(span());
        export_shared(context);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(recorder.counts.dropped(DropReason::QueueFull), 1);
        rx.close();
    }

    async fn collector(
        reply: &'static str,
        delay: Duration,
    ) -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
        use axum::{Json, Router, extract::State, routing::post};
        type Seen = Arc<Mutex<Vec<Value>>>;
        let seen: Seen = Arc::default();
        let app = Router::new()
            .route(
                "/v1/traces",
                post(
                    move |State(seen): State<Seen>, Json(body): Json<Value>| async move {
                        seen.lock().unwrap().push(body);
                        tokio::time::sleep(delay).await;
                        ([("content-type", "application/json")], reply)
                    },
                ),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, seen, task)
    }

    fn runtime(url: &str) -> Runtime {
        Runtime::start(
            config(&[
                ("GATEWAY_TRACES_OTLP_URL", url),
                ("GATEWAY_TRACES_SAMPLE_RATIO", "1"),
            ])
            .unwrap(),
        )
        .unwrap()
    }

    fn export_shared(context: Context) {
        let admission = SharedAdmission {
            request: context.request,
            alias: "public-model".to_owned(),
        };
        context.export(admission);
    }

    fn emit(recorder: &Recorder, n: usize) {
        let mut context = recorder.begin(Uuid::new_v4()).unwrap();
        for _ in 0..n {
            context.record(span());
        }
        export_shared(context);
    }

    #[tokio::test]
    async fn mock_collector_receives_exact_otlp_shape_and_partial_rejections() {
        let (url, seen, task) = collector(r#"{"partialSuccess":{"rejectedSpans":"1","errorMessage":"SECRET must not be logged"}}"#, Duration::ZERO).await;
        let runtime = runtime(&url);
        let recorder = runtime.recorder();
        emit(&recorder, 2);
        runtime.shutdown().await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let body = &seen[0];
        assert!(serde_json::to_vec(body).unwrap().len() <= 64 * 1024);
        assert_eq!(
            body["resourceSpans"][0]["resource"]["attributes"],
            json!([
                {"key":"service.name","value":{"stringValue":"open-model-gateway"}},
                {"key":"service.version","value":{"stringValue":env!("CARGO_PKG_VERSION")}},
                {"key":"deployment.environment.name","value":{"stringValue":"development"}}
            ])
        );
        let spans = body["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0]["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(spans[0]["spanId"].as_str().unwrap().len(), 16);
        assert!(hex::decode(spans[0]["traceId"].as_str().unwrap()).is_ok());
        assert_eq!(spans[0]["startTimeUnixNano"], "1700000000000000001");
        assert_eq!(spans[0]["endTimeUnixNano"], "1700000000000000002");
        assert_eq!(recorder.counts.rejected(), 1);
        assert_eq!(recorder.counts.deliveries(Delivery::Accepted), 1);
        task.abort();
    }

    #[test]
    fn full_allowlist_obeys_wire_budget_and_invalid_values_are_dropped() {
        let aliases = AliasRegistry::default();
        let mut span = span();
        // Escaping expands even an admission-validated 96-character alias.
        let attributes = [
            Attribute::Model(aliases.admission_validated(&"\\\"".repeat(96))),
            Attribute::Operation(Operation::Messages),
            Attribute::Provider(Provider::Anthropic),
            Attribute::InputTokens(i64::MAX as u64),
            Attribute::OutputTokens(i64::MAX as u64),
            Attribute::Streamed(true),
            Attribute::Attempt(3),
            Attribute::Route(Route::Messages),
            Attribute::Outcome(Outcome::Cancelled),
            Attribute::Error(ErrorKind::Timeout),
            Attribute::FirstContent(Duration::from_nanos(i64::MAX as u64)),
            Attribute::Generation(Duration::from_nanos(i64::MAX as u64)),
            Attribute::RequestId(Uuid::from_u128(42)),
            Attribute::AttemptId(Uuid::from_u128(43)),
        ];
        let mut refused = 0;
        for attribute in attributes {
            if !span.attribute(attribute) {
                refused += 1;
            }
        }
        assert!(refused > 0, "exercise the actual serialized size limit");
        assert!(serde_json::to_vec(&span).unwrap().len() <= MAX_SPAN);
        assert!(!span.attribute(Attribute::InputTokens(u64::MAX)));
        assert!(!span.attribute(Attribute::Attempt(4)));
        assert!(!span.attribute(Attribute::Generation(Duration::MAX)));
        assert!(Span::new(SpanName::Routing, [0; 16], None, 1, 2).is_none());
        assert!(Span::new(SpanName::Routing, [1; 16], Some([0; 8]), 1, 2).is_none());
        assert!(Span::new(SpanName::Routing, [1; 16], None, 2, 1).is_none());
    }

    async fn exchange(url: &str) -> Result<u64, Delivery> {
        let config = config(&[("GATEWAY_TRACES_OTLP_URL", url)]).unwrap();
        let (url, client) = config.endpoint.unwrap();
        let exporter = Exporter {
            url,
            client,
            headers: config.headers,
            environment: config.environment,
        };
        let raw = RawValue::from_string(serde_json::to_string(&span()).unwrap()).unwrap();
        let body = exporter.body(&[raw]);
        exporter.send(body, 1).await
    }

    #[tokio::test]
    async fn acknowledgement_is_bounded_and_rejections_are_clamped_without_retry() {
        for (reply, expected) in [
            ("", Ok(0)),
            ("{}", Ok(0)),
            (r#"{"partialSuccess":{"rejectedSpans":1}}"#, Ok(1)),
            (
                r#"{"partialSuccess":{"rejectedSpans":"999","errorMessage":"SECRET"}}"#,
                Ok(1),
            ),
            (
                r#"{"partialSuccess":{"rejectedSpans":-1}}"#,
                Err(Delivery::Acknowledgement),
            ),
            ("not-json", Err(Delivery::Acknowledgement)),
            ("[]", Err(Delivery::Acknowledgement)),
            (r#"{"partialSuccess":"x"}"#, Err(Delivery::Acknowledgement)),
            (
                r#"{"partialSuccess":{"rejectedSpans":1.5}}"#,
                Err(Delivery::Acknowledgement),
            ),
            (
                r#"{"partialSuccess":{"rejectedSpans":"18446744073709551616"}}"#,
                Err(Delivery::Acknowledgement),
            ),
        ] {
            let (url, seen, task) = collector(reply, Duration::ZERO).await;
            assert_eq!(exchange(&url).await, expected);
            assert_eq!(seen.lock().unwrap().len(), 1);
            task.abort();
        }
        let oversized = Box::leak(
            format!(
                "{{\"partialSuccess\":{{\"rejectedSpans\":\"1\"}},\"padding\":\"{}\"}}",
                "x".repeat(MAX_BODY)
            )
            .into_boxed_str(),
        );
        let (url, seen, task) = collector(oversized, Duration::ZERO).await;
        assert_eq!(exchange(&url).await, Err(Delivery::Acknowledgement));
        assert_eq!(seen.lock().unwrap().len(), 1);
        task.abort();
    }

    #[tokio::test]
    async fn exporter_refuses_redirects_and_rejects_untrusted_tls() {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
        let (target, seen, target_task) = collector("{}", Duration::ZERO).await;
        let redirect = axum::Router::new().route(
            "/v1/traces",
            axum::routing::post(move || async move {
                (
                    axum::http::StatusCode::TEMPORARY_REDIRECT,
                    [("location", target)],
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, redirect).await.unwrap();
        });
        assert_eq!(exchange(&url).await, Err(Delivery::HttpStatus));
        assert!(seen.lock().unwrap().is_empty());
        task.abort();
        target_task.abort();

        let cert = CertificateDer::from_pem_slice(crate::audit_export::pki::SERVER_CERT.as_bytes())
            .unwrap();
        let key =
            PrivateKeyDer::from_pem_slice(crate::audit_export::pki::SERVER_KEY.as_bytes()).unwrap();
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}/v1/traces", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            assert!(
                acceptor.accept(socket).await.is_err(),
                "public roots must reject the test CA"
            );
        });
        assert_eq!(exchange(&url).await, Err(Delivery::Transport));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn batch_size_and_periodic_flush_preserve_wire_bounds() {
        let (url, seen, task) = collector("{}", Duration::ZERO).await;
        let runtime = runtime(&url);
        let recorder = runtime.recorder();
        emit(&recorder, 64);
        emit(&recorder, 64);
        emit(&recorder, 2);
        // The normal path flushes after one second, even while senders live.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(seen.lock().unwrap().len(), 1);
        runtime.shutdown().await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        let mut total = 0;
        for batch in seen.iter() {
            let spans = batch["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .unwrap();
            assert!(spans.len() <= 64);
            assert!(serde_json::to_vec(batch).unwrap().len() <= MAX_BODY);
            total += spans.len();
        }
        assert_eq!(total, 130);
        assert!(recorder.begin(Uuid::new_v4()).is_none());
        task.abort();
    }

    #[tokio::test]
    async fn the_alias_attribute_comes_only_from_the_admission() {
        let (recorder, _receiver) = Recorder::channel(1.0);
        let request = Uuid::from_u128(43);
        let context = recorder.begin(request).unwrap();
        let admission = SharedAdmission {
            request,
            alias: "admitted-model".to_owned(),
        };
        // The only way to obtain a PublicAlias for a request: no string argument.
        let alias = context.admitted_alias(&admission).unwrap();
        assert!(matches!(alias.as_str(), "admitted-model" | "other"));
        drop(context);
    }

    #[tokio::test]
    async fn eligibility_is_bound_to_the_same_server_request() {
        let (recorder, mut receiver) = Recorder::channel(1.0);
        let request = Uuid::from_u128(41);
        let admission = SharedAdmission {
            request: Uuid::from_u128(42),
            alias: "public-model".to_owned(),
        };
        let mut context = recorder.begin(request).unwrap();
        context.record(span());
        assert!(context.admitted_alias(&admission).is_none());
        context.export(admission);
        assert!(receiver.try_recv().is_err());
        assert_eq!(recorder.counts.dropped(DropReason::Ineligible), 1);
    }

    #[tokio::test]
    async fn slow_acknowledgement_and_production_dns_refusal_use_real_transport() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 64\r\n\r\n{").await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let started = Instant::now();
        assert_eq!(
            exchange(&format!("http://{address}/v1/traces")).await,
            Err(Delivery::Timeout)
        );
        assert!(started.elapsed() < Duration::from_millis(2700));
        task.abort();
        // A live target distinguishes policy refusal from connection-refused.
        // A resolver bypass would queue a TCP connection on this listener.
        let dns_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dns_port = dns_listener.local_addr().unwrap().port();
        let config = TraceConfig::from_lookup(
            |key| {
                (key == TraceConfig::URL).then(|| format!("https://localhost:{dns_port}/v1/traces"))
            },
            Environment::Production,
        )
        .unwrap();
        let (url, client) = config.endpoint.unwrap();
        // DNS is intentionally deferred at startup; resolution must refuse all
        // localhost answers in production, before reaching even a TLS server.
        let exporter = Exporter {
            url,
            client,
            headers: config.headers,
            environment: config.environment,
        };
        assert_eq!(
            exporter.send(exporter.body(&[]), 0).await,
            Err(Delivery::Transport)
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), dns_listener.accept())
                .await
                .is_err(),
            "production telemetry connected to a refused loopback DNS answer"
        );
    }

    #[tokio::test]
    async fn trace_metrics_expose_only_fixed_labels() {
        let text = crate::metrics::METRICS.render(None).await;
        for reason in DropReason::ALL {
            assert!(text.contains(&format!(
                "gateway_trace_dropped_spans_total{{reason=\"{}\"}}",
                reason.as_str()
            )));
        }
        for outcome in Delivery::ALL {
            assert!(text.contains(&format!(
                "gateway_trace_deliveries_total{{outcome=\"{}\"}}",
                outcome.as_str()
            )));
        }
        assert!(text.contains("gateway_trace_rejected_spans_total"));
    }

    #[tokio::test]
    async fn slow_then_fast_collector_cannot_catch_up_with_burst_batches() {
        use axum::{Json, Router, extract::State, routing::post};
        type Starts = Arc<(Mutex<Vec<Instant>>, tokio::sync::Notify)>;
        async fn wait_for(starts: &Starts, count: usize) {
            tokio::time::timeout(Duration::from_secs(6), async {
                loop {
                    let notified = starts.1.notified();
                    if starts.0.lock().unwrap().len() >= count {
                        break;
                    }
                    notified.await;
                }
            })
            .await
            .unwrap();
        }
        for drain in [false, true] {
            let starts: Starts = Arc::default();
            let app = Router::new()
                .route(
                    "/v1/traces",
                    post(
                        |State(starts): State<Starts>, Json(_): Json<Value>| async move {
                            let first = {
                                let mut times = starts.0.lock().unwrap();
                                times.push(Instant::now());
                                times.len() == 1
                            };
                            starts.1.notify_one();
                            if first {
                                tokio::time::sleep(Duration::from_millis(1500)).await;
                            }
                            Json(json!({}))
                        },
                    ),
                )
                .with_state(starts.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let runtime = runtime(&url);
            let recorder = runtime.recorder();
            for _ in 0..3 {
                emit(&recorder, 64);
            }
            wait_for(&starts, if drain { 1 } else { 3 }).await;
            runtime.shutdown().await;
            let times = starts.0.lock().unwrap();
            assert_eq!(times.len(), 3);
            for pair in times.windows(2) {
                assert!(
                    pair[1].duration_since(pair[0]) >= Duration::from_millis(900),
                    "catch-up burst (drain={drain}): {:?}",
                    pair[1].duration_since(pair[0])
                );
            }
            task.abort();
        }
    }

    #[tokio::test]
    async fn shutdown_preserves_batch_cadence_and_counts_unflushed_spans() {
        let (url, seen, task) = collector("{}", Duration::ZERO).await;
        let runtime = runtime(&url);
        let recorder = runtime.recorder();
        for _ in 0..32 {
            emit(&recorder, 64);
        }
        runtime.shutdown().await;
        let requests = seen.lock().unwrap().len();
        assert!(
            (1..=3).contains(&requests),
            "shutdown sent {requests} batches in three seconds"
        );
        assert!(recorder.counts.dropped(DropReason::Shutdown) >= 2048 - 3 * 64);
        task.abort();
    }

    #[tokio::test]
    async fn slow_collector_and_shutdown_are_bounded_and_never_block_recording() {
        let (url, _, task) = collector("{}", Duration::from_secs(30)).await;
        let runtime = runtime(&url);
        let recorder = runtime.recorder();
        let started = Instant::now();
        for _ in 0..100 {
            emit(&recorder, 64);
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        // Let the first scheduled exchange start before testing drain timeout.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let started = Instant::now();
        runtime.shutdown().await;
        assert!(started.elapsed() < Duration::from_millis(3700));
        assert!(recorder.counts.deliveries(Delivery::Timeout) > 0);
        assert!(recorder.counts.dropped(DropReason::Shutdown) > 0);
        task.abort();
    }

    #[tokio::test]
    async fn down_collector_is_not_retried_and_https_never_falls_back_to_plaintext() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        drop(listener);
        let runtime = self::runtime(&url);
        let recorder = runtime.recorder();
        emit(&recorder, 1);
        runtime.shutdown().await;
        assert_eq!(recorder.counts.deliveries(Delivery::Transport), 1);
        let (url, seen, task) = collector("{}", Duration::ZERO).await;
        let runtime = self::runtime(&url.replacen("http:", "https:", 1));
        let recorder = runtime.recorder();
        emit(&recorder, 1);
        runtime.shutdown().await;
        assert_eq!(recorder.counts.deliveries(Delivery::Transport), 1);
        assert!(seen.lock().unwrap().is_empty());
        task.abort();
    }

    #[test]
    fn shared_admission_has_one_production_call_site() {
        // SharedAdmission::new is pub(crate): it cannot be constructed from
        // outside this crate at all. Within the crate, grep enforces the
        // narrower, intended constraint: exactly one call site, in the
        // admission commit path (`governance.rs`), never anywhere else.
        // Built from parts so this assertion's own source line is never a
        // match for its own needle.
        let needle = format!("{}{}", "SharedAdmission::", "new(");
        let sites = grep_sources(&needle);
        assert_eq!(sites.len(), 1, "SharedAdmission::new call sites: {sites:?}");
        assert!(sites[0].0.ends_with("telemetry.rs"), "{sites:?}");
        assert!(sites[0].1.contains("fn mark_shared"), "{sites:?}");
    }

    #[test]
    fn mark_shared_has_one_production_call_site() {
        // The bridge back into governance's admission commit path: the only
        // place allowed to tell telemetry a shared-workspace admission
        // committed.
        let needle = format!("{}{}", "telemetry::mark_", "shared(");
        let sites = grep_sources(&needle);
        assert_eq!(sites.len(), 1, "mark_shared call sites: {sites:?}");
        assert!(sites[0].0.ends_with("governance.rs"), "{sites:?}");
    }

    /// `(file path, enclosing function name)` of every source line containing
    /// `needle`, restricted to the function whose body the line falls in
    /// (a coarse `fn ` scan, good enough to name the enclosing production
    /// function for this file's own architecture tests).
    fn grep_sources(needle: &str) -> Vec<(String, String)> {
        let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
        let mut found = Vec::new();
        visit(root, needle, &mut found);
        return found;

        fn visit(dir: &std::path::Path, needle: &str, found: &mut Vec<(String, String)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, needle, found);
                    continue;
                }
                if path.extension().is_some_and(|e| e == "rs")
                    && let Ok(text) = std::fs::read_to_string(&path)
                {
                    let mut enclosing = String::new();
                    for line in text.lines() {
                        if let Some(rest) = line.trim_start().strip_prefix("fn ").or_else(|| {
                            line.trim_start()
                                .strip_prefix("pub fn ")
                                .or_else(|| line.trim_start().strip_prefix("pub(crate) fn "))
                        }) {
                            enclosing = format!("fn {}", rest.split(['(', '<']).next().unwrap_or(rest));
                        }
                        if line.contains(needle) {
                            found.push((path.to_string_lossy().into_owned(), enclosing.clone()));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn reserved_spans_require_a_nonzero_id_and_reject_the_same_invalid_shapes_as_new() {
        let trace = trace_id(Uuid::from_u128(42));
        let id = reserve_span_id().unwrap();
        assert_ne!(id, [0; 8]);
        assert!(Span::reserved(SpanName::Admission, trace, id, None, 1, 2).is_some());
        assert!(Span::reserved(SpanName::Admission, [0; 16], id, None, 1, 2).is_none());
        assert!(Span::reserved(SpanName::Admission, trace, [0; 8], None, 1, 2).is_none());
        assert!(Span::reserved(SpanName::Admission, trace, id, Some([0; 8]), 1, 2).is_none());
        assert!(Span::reserved(SpanName::Admission, trace, id, None, 2, 1).is_none());
    }

    #[tokio::test]
    async fn phase_buffer_collects_real_boundaries_and_eligibility_only_inside_with_phases() {
        let trace = trace_id(Uuid::from_u128(7));
        let parent = reserve_span_id().unwrap();
        let start = now_unix_nanos();
        let buffer = Arc::new(Mutex::new(PhaseBuffer::new(trace, parent, start)));
        // Outside with_phases, marks are a no-op: no ambient buffer.
        mark_phase(SpanName::AdmissionQueue);
        mark_shared(Uuid::from_u128(7), "m");
        with_phases(buffer.clone(), async {
            mark_phase(SpanName::AdmissionQueue);
            mark_phase(SpanName::AdmissionConnect);
            mark_shared(Uuid::from_u128(7), "public-model");
        })
        .await;
        let Ok(mutex) = Arc::try_unwrap(buffer) else {
            panic!("buffer still shared")
        };
        let (spans, eligibility) = mutex.into_inner().expect("buffer not poisoned").take();
        assert_eq!(spans.len(), 2);
        for span in &spans {
            let body = serde_json::to_value(span).unwrap();
            assert_eq!(body["parentSpanId"], hex::encode(parent));
        }
        let eligibility = eligibility.unwrap();
        assert_eq!(eligibility.request, Uuid::from_u128(7));
        assert_eq!(eligibility.alias, "public-model");
    }
}

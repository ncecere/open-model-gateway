//! Non-generation workload path shared by embeddings, rerank, System One,
//! images and audio (speech streams its body: see `Workload::STREAMED`).
//! Each workload is a typed request implementing
//! [`Workload`]; [`Engine::execute_workload`] owns everything else: pure
//! validation before storage/network, live catalog/key filtering, protocol and
//! adapter capability gating, the route plan and explicit failover policy,
//! per-attempt durable admission (v3 meter bounds), deadline/cancellation, and
//! preservation of usage evidence from invalid upstream bodies.
//!
//! # Adding a workload
//!
//! 1. Add canonical request/response types to `workload_types.rs` (no `Debug`
//!    on content, bounded fields, `validate()` returning `InvalidRequest`, or
//!    `Unsupported` for representable-but-unsupported features).
//! 2. Add a capability method to `ProviderAdapter` with a default
//!    `Err(InferenceError::Unsupported)` and implement it per provider.
//!    Adapters report every meter the workload can produce (semantic zeros for
//!    meters it cannot) plus `requests: Some(1)` per upstream request.
//! 3. `impl Workload` for the request: `PROTOCOL`, `admission()` (output-token
//!    reservation and request-derived unit ceilings that `valid_response`
//!    enforces), `dispatch`, `usage`, `valid_response`.
//! 4. Add a body cap to [`WorkloadLimits`], a protocol frontend under
//!    `protocols/`, and route it in `http.rs` (inference-key auth applies).
//! 5. Tests: frontend validation, engine admission/failover/cancellation with a
//!    fake adapter, provider mock contracts, database admission/settlement.
use async_trait::async_trait;

use super::*;
use crate::{
    billing::MeterUsage,
    inference::types::{
        Deployment, EmbeddingRequest, EmbeddingResponse, RerankRequest, RerankResponse,
        SystemoneRequest, SystemoneResponse, WorkloadKind,
    },
    providers::ProviderAdapter,
};

/// How admission reserves output tokens for a workload attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputReservation {
    /// Input-only workload: output is semantically zero (embeddings, rerank).
    None,
    /// Client-requested maximum; a priced admission requires it.
    Requested(Option<u32>),
    /// The pinned price's trusted `output_token_limit` (e.g. System One,
    /// whose providers may report output tokens the request cannot bound).
    PriceCeiling,
    /// One async batch attempt covering `requests` requests: the input
    /// ceiling is `requests × input_token_limit`, the output ceiling the sum
    /// of the per-line maxima, and every line maximum must be within the
    /// price's `output_token_limit` (see `jobs::batch`).
    Batch {
        requests: u32,
        output_tokens: u64,
        max_line_output: u32,
    },
}
/// Per-attempt admission inputs. `unit_ceilings` are request-derived hard
/// upper bounds per non-token meter (e.g. `requests: Some(1)`). They tighten,
/// never loosen, the price's `max_units`, so the workload's `valid_response`
/// must reject any observation above them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkloadAdmission {
    pub kind: WorkloadKind,
    pub output: OutputReservation,
    pub unit_ceilings: MeterUsage,
}

/// Inbound HTTP body caps per workload route (bytes). Generation/embeddings
/// keep the shared 2 MiB cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkloadLimits {
    pub images_body_bytes: usize,
    pub audio_transcriptions_body_bytes: usize,
    pub audio_speech_body_bytes: usize,
    pub rerank_body_bytes: usize,
    pub systemone_body_bytes: usize,
    /// Upstream image response cap (base64 JSON can be several MiB);
    /// independent of every other workload's provider body cap.
    pub images_response_bytes: usize,
}
const MIB: usize = 1024 * 1024;
impl Default for WorkloadLimits {
    fn default() -> Self {
        Self {
            images_body_bytes: 2 * MIB,
            // 25 MiB upload plus multipart framing.
            audio_transcriptions_body_bytes: 26 * MIB,
            audio_speech_body_bytes: 2 * MIB,
            rerank_body_bytes: 2 * MIB,
            systemone_body_bytes: 2 * MIB,
            images_response_bytes: 20 * MIB,
        }
    }
}
impl WorkloadLimits {
    pub const MIN_BODY_BYTES: usize = 1024;
    pub const MAX_BODY_BYTES: usize = 64 * MIB;
    pub fn body_bytes(self, kind: WorkloadKind) -> Option<usize> {
        match kind {
            // Realtime is a WebSocket upgrade without a request body.
            WorkloadKind::Generation | WorkloadKind::Embeddings | WorkloadKind::Realtime => None,
            // Async jobs carry their own caps (`jobs::JobLimits`).
            WorkloadKind::Videos | WorkloadKind::Batches => None,
            WorkloadKind::Images => Some(self.images_body_bytes),
            WorkloadKind::AudioTranscriptions => Some(self.audio_transcriptions_body_bytes),
            WorkloadKind::AudioSpeech => Some(self.audio_speech_body_bytes),
            WorkloadKind::Rerank => Some(self.rerank_body_bytes),
            WorkloadKind::Systemone => Some(self.systemone_body_bytes),
        }
    }
    pub fn validate(self) -> bool {
        [
            self.images_body_bytes,
            self.audio_transcriptions_body_bytes,
            self.audio_speech_body_bytes,
            self.rerank_body_bytes,
            self.systemone_body_bytes,
        ]
        .iter()
        .all(|n| (Self::MIN_BODY_BYTES..=Self::MAX_BODY_BYTES).contains(n))
            && (MIB..=Self::MAX_BODY_BYTES).contains(&self.images_response_bytes)
    }
    /// `GATEWAY_MAX_BODY_BYTES_{IMAGES,AUDIO_TRANSCRIPTIONS,AUDIO_SPEECH,RERANK,SYSTEMONE}`.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let mut limits = Self::default();
        for (name, slot) in [
            ("IMAGES", &mut limits.images_body_bytes),
            (
                "AUDIO_TRANSCRIPTIONS",
                &mut limits.audio_transcriptions_body_bytes,
            ),
            ("AUDIO_SPEECH", &mut limits.audio_speech_body_bytes),
            ("RERANK", &mut limits.rerank_body_bytes),
            ("SYSTEMONE", &mut limits.systemone_body_bytes),
        ] {
            if let Some(value) = get(&format!("GATEWAY_MAX_BODY_BYTES_{name}")) {
                *slot = value.trim().parse().map_err(|_| {
                    anyhow::anyhow!("GATEWAY_MAX_BODY_BYTES_{name} must be an integer")
                })?;
            }
        }
        // `GATEWAY_MAX_RESPONSE_BYTES_IMAGES` (1–64 MiB, default 20 MiB).
        if let Some(value) = get("GATEWAY_MAX_RESPONSE_BYTES_IMAGES") {
            limits.images_response_bytes = value.trim().parse().map_err(|_| {
                anyhow::anyhow!("GATEWAY_MAX_RESPONSE_BYTES_IMAGES must be an integer")
            })?;
        }
        anyhow::ensure!(limits.validate(), "workload body limits out of range");
        Ok(limits)
    }
}

/// A typed non-generation workload request. See the module docs.
#[async_trait]
pub trait Workload: Clone + Send + Sync + 'static {
    type Response: Send + 'static;
    const PROTOCOL: ApiProtocol;
    fn model(&self) -> &str;
    /// Pure bounds, evaluated before any storage or network work.
    fn validate(&self) -> Result<(), InferenceError>;
    fn admission(&self) -> WorkloadAdmission;
    /// Request-specific adapter/deployment support (beyond the protocol).
    fn supported_by(&self, _adapter: &dyn ProviderAdapter, _target: &Deployment) -> bool {
        true
    }
    /// Dropping the returned future MUST cancel upstream work.
    async fn dispatch(
        self,
        adapter: &dyn ProviderAdapter,
        target: &Deployment,
    ) -> Result<Self::Response, InferenceError>;
    fn usage(response: &Self::Response) -> Usage;
    fn valid_response(&self, response: &Self::Response) -> bool;
    /// The response body is still streaming from upstream when returned
    /// (speech). The attempt is then recorded as streamed and settled by
    /// [`Workload::attach`] instead of before return.
    const STREAMED: bool = false;
    /// Called only when `STREAMED`: wrap the body so the attempt settles when
    /// it ends (success), fails, or is dropped (cancelled).
    fn attach(response: Self::Response, settlement: StreamSettlement) -> Self::Response {
        drop(settlement);
        response
    }
}

/// Ownership of a validated, admitted attempt whose body is still streaming.
/// Dropping it without `succeed`/`fail` records a cancelled attempt (hold kept).
pub struct StreamSettlement {
    guard: ExecutionGuard,
    deadline: Instant,
    permit: SharedPermit,
    limits: EngineLimits,
}
impl StreamSettlement {
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn limits(&self) -> EngineLimits {
        self.limits
    }
    pub(super) fn permit(&self) -> SharedPermit {
        self.permit.clone()
    }
    /// Durably settle success; only then may the body terminate cleanly.
    pub async fn succeed(&mut self) -> Result<(), InferenceError> {
        self.guard.finish(Outcome::Succeeded, None).await
    }
    /// Record failure and return the error the client should see.
    pub async fn fail(&mut self, error: InferenceError) -> InferenceError {
        if error == InferenceError::Timeout && Instant::now() >= self.deadline {
            self.guard.health_observation = false;
        }
        self.guard
            .finish(Outcome::Failed, Some(error))
            .await
            .err()
            .unwrap_or(error)
    }
}

fn requests_once() -> MeterUsage {
    MeterUsage {
        requests: Some(1),
        ..MeterUsage::default()
    }
}
pub(super) fn within_ceilings(usage: Usage, ceilings: MeterUsage) -> bool {
    usage.meters.is_none_or(|m| {
        m.counts()
            .into_iter()
            .zip(ceilings.counts())
            .all(|(n, c)| c.is_none_or(|c| n.is_none_or(|n| n <= c)))
    })
}

#[async_trait]
impl Workload for EmbeddingRequest {
    type Response = EmbeddingResponse;
    const PROTOCOL: ApiProtocol = ApiProtocol::Embeddings;
    fn model(&self) -> &str {
        &self.model
    }
    fn validate(&self) -> Result<(), InferenceError> {
        super::embeddings::valid_request(self)
            .then_some(())
            .ok_or(InferenceError::Unsupported)
    }
    fn admission(&self) -> WorkloadAdmission {
        WorkloadAdmission {
            kind: WorkloadKind::Embeddings,
            output: OutputReservation::None,
            unit_ceilings: MeterUsage::default(),
        }
    }
    fn supported_by(&self, adapter: &dyn ProviderAdapter, target: &Deployment) -> bool {
        adapter.supports_embedding_target(target, self)
    }
    async fn dispatch(
        self,
        adapter: &dyn ProviderAdapter,
        target: &Deployment,
    ) -> Result<EmbeddingResponse, InferenceError> {
        adapter.execute_embeddings(target, self).await
    }
    fn usage(response: &EmbeddingResponse) -> Usage {
        response.usage
    }
    fn valid_response(&self, response: &EmbeddingResponse) -> bool {
        super::embeddings::valid_response(self, response)
    }
}

#[async_trait]
impl Workload for RerankRequest {
    type Response = RerankResponse;
    const PROTOCOL: ApiProtocol = ApiProtocol::Rerank;
    fn model(&self) -> &str {
        &self.model
    }
    fn validate(&self) -> Result<(), InferenceError> {
        RerankRequest::validate(self)
    }
    fn admission(&self) -> WorkloadAdmission {
        WorkloadAdmission {
            kind: WorkloadKind::Rerank,
            output: OutputReservation::None,
            unit_ceilings: requests_once(),
        }
    }
    async fn dispatch(
        self,
        adapter: &dyn ProviderAdapter,
        target: &Deployment,
    ) -> Result<RerankResponse, InferenceError> {
        adapter.execute_rerank(target, self).await
    }
    fn usage(response: &RerankResponse) -> Usage {
        response.usage
    }
    fn valid_response(&self, response: &RerankResponse) -> bool {
        response.valid_for(self)
            && response.usage.output_tokens.is_none_or(|n| n == 0)
            && within_ceilings(response.usage, self.admission().unit_ceilings)
    }
}

#[async_trait]
impl Workload for SystemoneRequest {
    type Response = SystemoneResponse;
    const PROTOCOL: ApiProtocol = ApiProtocol::Systemone;
    fn model(&self) -> &str {
        &self.model
    }
    fn validate(&self) -> Result<(), InferenceError> {
        SystemoneRequest::validate(self)
    }
    fn admission(&self) -> WorkloadAdmission {
        WorkloadAdmission {
            kind: WorkloadKind::Systemone,
            output: OutputReservation::PriceCeiling,
            unit_ceilings: requests_once(),
        }
    }
    async fn dispatch(
        self,
        adapter: &dyn ProviderAdapter,
        target: &Deployment,
    ) -> Result<SystemoneResponse, InferenceError> {
        adapter.execute_systemone(target, self).await
    }
    fn usage(response: &SystemoneResponse) -> Usage {
        response.usage
    }
    fn valid_response(&self, response: &SystemoneResponse) -> bool {
        response.valid_for(self) && within_ceilings(response.usage, self.admission().unit_ceilings)
    }
}

impl Engine {
    pub fn limits(&self) -> EngineLimits {
        self.limits
    }
    /// Registered adapters (async jobs dispatch outside the request engine).
    pub fn registry(&self) -> &ProviderRegistry {
        &self.registry
    }

    /// Execute one non-generation workload with per-attempt admission and
    /// explicit failover. Unsupported model/protocol pairs fail before any
    /// admission or dispatch.
    pub async fn execute_workload<W: Workload>(
        &self,
        principal: Principal,
        request: W,
        request_id: Uuid,
    ) -> Result<W::Response, InferenceError> {
        request.validate()?;
        let admission = request.admission();
        if admission.kind != W::PROTOCOL.workload() || admission.kind == WorkloadKind::Generation {
            return Err(InferenceError::Configuration);
        }
        let input_only = admission.output == OutputReservation::None;
        let permit = Arc::new(Mutex::new(Some(
            self.capacity
                .clone()
                .try_acquire_owned()
                .map_err(|_| crate::metrics::capacity_denied())?,
        )));
        let deadline = Instant::now() + self.limits.request_timeout;
        let labels = super::client::current();
        let deployments = timeout_at(
            deadline,
            self.repository.deployments(&principal, request.model()),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        if deployments.is_empty() {
            return Err(InferenceError::ModelUnavailable);
        }
        let candidates: Vec<_> = deployments
            .into_iter()
            .filter(|d| {
                d.supported_protocols
                    .iter()
                    .any(|p| p == W::PROTOCOL.as_str())
                    && self.registry.get(&d.provider).is_some_and(|a| {
                        a.supports_protocol(W::PROTOCOL) && request.supported_by(a.as_ref(), d)
                    })
            })
            .collect();
        if candidates.is_empty() {
            return Err(InferenceError::Unsupported);
        }
        let plan = timeout_at(
            deadline,
            self.repository
                .route_plan(&principal, request.model(), &candidates, request_id),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        if !(1..=3).contains(&plan.max_attempts) {
            return Err(InferenceError::Configuration);
        }
        let lease_seconds = self
            .limits
            .request_timeout
            .as_secs()
            .min(i64::MAX as u64 - 5) as i64
            + 5;
        for (attempt, id) in plan
            .deployment_ids
            .iter()
            .take(plan.max_attempts)
            .enumerate()
        {
            let target = candidates
                .iter()
                .find(|d| d.id == *id)
                .ok_or(InferenceError::Configuration)?;
            let adapter = self
                .registry
                .get(&target.provider)
                .ok_or(InferenceError::Configuration)?;
            let execution_id = if attempt == 0 {
                request_id
            } else {
                Uuid::new_v4()
            };
            let started = Instant::now();
            timeout_at(
                deadline,
                self.repository.admit_workload(
                    &ExecutionStart {
                        id: execution_id,
                        root_request_id: request_id,
                        attempt_number: (attempt + 1) as i32,
                        principal,
                        deployment_id: target.id,
                        provider: target.provider.clone(),
                        model: request.model().to_owned(),
                        streamed: W::STREAMED,
                        upstream_model: super::upstream_snapshot(&target.upstream_model),
                        client: labels.clone(),
                    },
                    &admission,
                    lease_seconds,
                    target,
                ),
            )
            .await
            .map_err(|_| InferenceError::Timeout)??;
            let mut guard = ExecutionGuard::new(
                self.repository.clone(),
                execution_id,
                target.id,
                started,
                permit.clone(),
            );
            let evidence_ok =
                |u: Usage| valid_usage(u) && (!input_only || u.output_tokens == Some(0));
            let (result, invalid_body_usage) = evidence::capture(timeout_at(
                deadline,
                request.clone().dispatch(adapter.as_ref(), target),
            ))
            .await;
            if let Some(observed) = invalid_body_usage.filter(|u| evidence_ok(*u)) {
                guard.usage = observed;
            }
            let result = match result {
                Ok(result) => result,
                Err(_) => {
                    guard.health_observation = false;
                    Err(InferenceError::Timeout)
                }
            };
            match result {
                Ok(response) => {
                    // Shape failure does not erase independently validated
                    // metering evidence; failure still retains the hold.
                    let usage = W::usage(&response);
                    if evidence_ok(usage) {
                        guard.usage = usage;
                    }
                    if !evidence_ok(usage) || !request.valid_response(&response) {
                        guard
                            .finish(Outcome::Failed, Some(InferenceError::InvalidUpstream))
                            .await?;
                        return Err(InferenceError::InvalidUpstream);
                    }
                    if W::STREAMED {
                        // No failover after the body is returned.
                        return Ok(W::attach(
                            response,
                            StreamSettlement {
                                guard,
                                deadline,
                                permit: permit.clone(),
                                limits: self.limits,
                            },
                        ));
                    }
                    guard.finish(Outcome::Succeeded, None).await?;
                    return Ok(response);
                }
                Err(error) => {
                    guard.finish(Outcome::Failed, Some(error)).await?;
                    if attempt + 1 < plan.max_attempts
                        && attempt + 1 < plan.deployment_ids.len()
                        && crate::routing::may_failover(&plan, error)
                    {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        Err(InferenceError::Busy)
    }
}

#[cfg(test)]
mod tests;

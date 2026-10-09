use async_trait::async_trait;
use uuid::Uuid;

use super::{
    error::InferenceError,
    types::{Deployment, Usage},
};
use crate::{auth::Principal, store::Store};

pub struct ExecutionStart {
    pub id: Uuid,
    pub root_request_id: Uuid,
    pub attempt_number: i32,
    pub principal: Principal,
    pub deployment_id: Uuid,
    pub provider: String,
    pub model: String,
    pub streamed: bool,
    /// Configured upstream model id this attempt is sent to (snapshot).
    pub upstream_model: Option<String>,
    /// Optional client labels of the root request (see `inference::client`).
    pub client: super::client::ClientMetadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
}
impl Outcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Normalized finish reason of an attempt (stored in `finish_reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishLabel {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Error,
    Cancelled,
    Unknown,
}
impl FinishLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::ContentFilter => "content_filter",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
    pub const ALL: [&'static str; 7] = [
        "stop",
        "length",
        "tool_calls",
        "content_filter",
        "error",
        "cancelled",
        "unknown",
    ];
}
impl From<super::types::FinishReason> for FinishLabel {
    fn from(reason: super::types::FinishReason) -> Self {
        use super::types::FinishReason as F;
        match reason {
            F::Stop => Self::Stop,
            F::Length => Self::Length,
            F::ToolCalls => Self::ToolCalls,
            F::ContentFilter => Self::ContentFilter,
        }
    }
}

/// Per-attempt timing and outcome telemetry. Metadata only; `None` = not
/// observed (never a fabricated zero).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttemptTelemetry {
    pub finish_reason: Option<FinishLabel>,
    /// Streams: upstream dispatch to the first delta.
    pub time_to_first_token_ms: Option<u64>,
    /// Upstream dispatch to the end of the upstream response.
    pub generation_ms: Option<u64>,
}
impl AttemptTelemetry {
    /// The stored finish reason for an outcome: failures are `error`,
    /// cancellations `cancelled`; a success keeps the provider's reason.
    pub fn for_outcome(mut self, outcome: Outcome) -> Self {
        self.finish_reason = match outcome {
            Outcome::Failed => Some(FinishLabel::Error),
            Outcome::Cancelled => Some(FinishLabel::Cancelled),
            Outcome::Succeeded => self.finish_reason,
        };
        self
    }
}

pub struct ExecutionFinish {
    pub id: Uuid,
    pub outcome: Outcome,
    pub error: Option<InferenceError>,
    pub usage: Usage,
    pub elapsed_ms: u64,
}

#[async_trait]
pub trait InferenceRepository: Send + Sync {
    async fn deployments(
        &self,
        principal: &Principal,
        model: &str,
    ) -> Result<Vec<Deployment>, InferenceError>;
    async fn start(&self, record: &ExecutionStart) -> Result<(), InferenceError>;
    async fn finish(&self, record: &ExecutionFinish) -> Result<(), InferenceError>;
    /// Finish with attempt telemetry (finish reason, timings). Telemetry is
    /// metadata only; repositories without telemetry storage just finish.
    async fn finish_attempt(
        &self,
        record: &ExecutionFinish,
        _telemetry: &AttemptTelemetry,
    ) -> Result<(), InferenceError> {
        self.finish(record).await
    }
    async fn admit(
        &self,
        record: &ExecutionStart,
        _request: &super::types::ChatRequest,
        _lease_seconds: i64,
        _deployment: &Deployment,
    ) -> Result<(), InferenceError> {
        self.start(record).await
    }
    async fn admit_embeddings(
        &self,
        record: &ExecutionStart,
        _request: &super::types::EmbeddingRequest,
        _lease_seconds: i64,
        _deployment: &Deployment,
    ) -> Result<(), InferenceError> {
        self.start(record).await
    }
    /// Generic non-generation workload admission (see `inference::workload`).
    async fn admit_workload(
        &self,
        record: &ExecutionStart,
        _admission: &super::workload::WorkloadAdmission,
        _lease_seconds: i64,
        _deployment: &Deployment,
    ) -> Result<(), InferenceError> {
        self.start(record).await
    }
    async fn route_plan(
        &self,
        _principal: &Principal,
        _model: &str,
        candidates: &[Deployment],
        _request_id: Uuid,
    ) -> Result<crate::routing::RoutePlan, InferenceError> {
        Ok(crate::routing::RoutePlan {
            deployment_ids: candidates.iter().map(|d| d.id).collect(),
            max_attempts: 1,
            allow_ambiguous_failover: false,
        })
    }
    async fn route_result(
        &self,
        _deployment: Uuid,
        _error: Option<InferenceError>,
    ) -> Result<(), InferenceError> {
        Ok(())
    }
    // Realtime sessions (`inference::realtime`, `governance::realtime`).
    /// Per-response output window: `cap`, lowered to the latest price's
    /// output ceiling.
    async fn realtime_window_output(
        &self,
        _deployment: Uuid,
        cap: u32,
    ) -> Result<u32, InferenceError> {
        Ok(cap)
    }
    /// Extend the session hold by one response window (budget-checked).
    async fn realtime_reserve_window(
        &self,
        _principal: &Principal,
        _session: Uuid,
        _model: &str,
        _window_output_tokens: u32,
    ) -> Result<(), InferenceError> {
        Ok(())
    }
    /// Record that a reserved window now belongs to response `sequence`.
    async fn realtime_open_response(
        &self,
        _session: Uuid,
        _sequence: i32,
        _window_output_tokens: u32,
    ) -> Result<(), InferenceError> {
        Ok(())
    }
    /// Settle one response (`None` usage keeps its window as unknown).
    async fn realtime_settle_response(
        &self,
        _session: Uuid,
        _sequence: i32,
        _status: Option<super::realtime::ResponseStatus>,
        _usage: Option<super::realtime::RealtimeUsage>,
        _window_output_tokens: u32,
    ) -> Result<(), InferenceError> {
        Ok(())
    }
    /// Finish the session's single attempt from its response rows.
    async fn realtime_finish(
        &self,
        record: &super::realtime::RealtimeFinish,
    ) -> Result<(), InferenceError> {
        self.finish_attempt(
            &ExecutionFinish {
                id: record.id,
                outcome: record.outcome,
                error: record.error,
                usage: Usage::default(),
                elapsed_ms: record.elapsed_ms,
            },
            &record.telemetry,
        )
        .await
    }
}

#[async_trait]
impl InferenceRepository for Store {
    async fn deployments(
        &self,
        principal: &Principal,
        model: &str,
    ) -> Result<Vec<Deployment>, InferenceError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| InferenceError::Storage)?;
        let lineage = crate::auth::revalidate(&mut tx, principal)
            .await
            .map_err(|_| InferenceError::Storage)?;
        let Some(lineage) = lineage else {
            return Ok(Vec::new());
        };
        let deployments = sqlx::query_as::<_, Deployment>(r#"
            SELECT d.id,p.provider,d.upstream_model,p.credential_ref,p.endpoint,p.region,m.supported_protocols
            FROM models m JOIN deployments d ON d.model_id=m.id
            JOIN provider_connections p ON p.id=d.provider_connection_id
            WHERE m.public_name=$2 AND workspace_model_allowed($1,m.id) AND d.enabled AND p.enabled
              AND (NOT EXISTS(SELECT 1 FROM key_model_restrictions WHERE workspace_id=$1 AND governance_key_id=$3)
                OR EXISTS(SELECT 1 FROM key_model_selections WHERE workspace_id=$1 AND governance_key_id=$3 AND model_id=m.id))
            ORDER BY d.created_at,d.id LIMIT 257
        "#).bind(principal.workspace_id).bind(model).bind(lineage)
            .fetch_all(&mut *tx).await.map_err(|_| InferenceError::Storage)?;
        tx.commit().await.map_err(|_| InferenceError::Storage)?;
        if deployments.len() > crate::routing::MAX_CANDIDATES {
            return Err(InferenceError::Configuration);
        }
        Ok(deployments)
    }

    async fn start(&self, record: &ExecutionStart) -> Result<(), InferenceError> {
        sqlx::query(r#"INSERT INTO inference_executions
            (id, workspace_id, api_key_id, deployment_id, public_model, provider, streamed, state, root_request_id, attempt_number, upstream_model, client_session_id, client_app)
            VALUES ($1,$2,$3,$4,$5,$6,$7,'started',$8,$9,$10,$11,$12)"#)
            .bind(record.id).bind(record.principal.workspace_id)
            .bind(record.principal.key_id).bind(record.deployment_id).bind(&record.model)
            .bind(&record.provider).bind(record.streamed).bind(record.root_request_id).bind(record.attempt_number)
            .bind(&record.upstream_model).bind(&record.client.session_id).bind(&record.client.app).execute(&self.pool).await
            .map_err(|_| InferenceError::Storage)?;
        Ok(())
    }

    async fn finish(&self, record: &ExecutionFinish) -> Result<(), InferenceError> {
        crate::governance::finish(self, record).await
    }
    async fn finish_attempt(
        &self,
        record: &ExecutionFinish,
        telemetry: &AttemptTelemetry,
    ) -> Result<(), InferenceError> {
        crate::governance::finish_with_telemetry(self, record, telemetry).await
    }
    async fn admit(
        &self,
        record: &ExecutionStart,
        request: &super::types::ChatRequest,
        lease_seconds: i64,
        deployment: &Deployment,
    ) -> Result<(), InferenceError> {
        crate::governance::admit_for_deployment(self, record, request, lease_seconds, deployment)
            .await
    }
    async fn admit_embeddings(
        &self,
        record: &ExecutionStart,
        request: &super::types::EmbeddingRequest,
        lease_seconds: i64,
        deployment: &Deployment,
    ) -> Result<(), InferenceError> {
        crate::governance::admit_embeddings_for_deployment(
            self,
            record,
            request,
            lease_seconds,
            deployment,
        )
        .await
    }
    async fn admit_workload(
        &self,
        record: &ExecutionStart,
        admission: &super::workload::WorkloadAdmission,
        lease_seconds: i64,
        deployment: &Deployment,
    ) -> Result<(), InferenceError> {
        crate::governance::admit_workload_for_deployment(
            self,
            record,
            admission,
            lease_seconds,
            deployment,
        )
        .await
    }
    async fn route_plan(
        &self,
        principal: &Principal,
        model: &str,
        candidates: &[Deployment],
        request_id: Uuid,
    ) -> Result<crate::routing::RoutePlan, InferenceError> {
        crate::routing::plan(self, principal, model, candidates, request_id).await
    }
    async fn route_result(
        &self,
        deployment: Uuid,
        error: Option<InferenceError>,
    ) -> Result<(), InferenceError> {
        crate::routing::record_result(self, deployment, error).await
    }
    async fn realtime_window_output(
        &self,
        deployment: Uuid,
        cap: u32,
    ) -> Result<u32, InferenceError> {
        crate::governance::realtime::window_output(self, deployment, cap).await
    }
    async fn realtime_reserve_window(
        &self,
        principal: &Principal,
        session: Uuid,
        model: &str,
        window_output_tokens: u32,
    ) -> Result<(), InferenceError> {
        crate::governance::realtime::reserve_window(
            self,
            principal,
            session,
            model,
            window_output_tokens,
        )
        .await
    }
    async fn realtime_open_response(
        &self,
        session: Uuid,
        sequence: i32,
        window_output_tokens: u32,
    ) -> Result<(), InferenceError> {
        crate::governance::realtime::open_response(self, session, sequence, window_output_tokens)
            .await
    }
    async fn realtime_settle_response(
        &self,
        session: Uuid,
        sequence: i32,
        status: Option<super::realtime::ResponseStatus>,
        usage: Option<super::realtime::RealtimeUsage>,
        window_output_tokens: u32,
    ) -> Result<(), InferenceError> {
        crate::governance::realtime::settle_response(
            self,
            session,
            sequence,
            status,
            usage,
            window_output_tokens,
        )
        .await
    }
    async fn realtime_finish(
        &self,
        record: &super::realtime::RealtimeFinish,
    ) -> Result<(), InferenceError> {
        crate::governance::realtime::finish(self, record).await
    }
}

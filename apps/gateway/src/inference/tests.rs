use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use super::*;
use crate::{inference::types::*, providers::ProviderAdapter};
use async_trait::async_trait;
use futures_util::stream;

#[derive(Default)]
struct MemoryRepository {
    deployments: Mutex<Vec<Deployment>>,
    starts: AtomicUsize,
    finishes: Mutex<Vec<ExecutionFinish>>,
    finished: tokio::sync::Notify,
    fail_start: AtomicBool,
    fail_finish: AtomicBool,
    plan: Mutex<Option<crate::routing::RoutePlan>>,
    attempts: Mutex<Vec<(Uuid, i32)>>,
    health: Mutex<Vec<Option<InferenceError>>>,
}
#[async_trait]
impl InferenceRepository for MemoryRepository {
    async fn deployments(&self, _: &Principal, _: &str) -> Result<Vec<Deployment>, InferenceError> {
        Ok(self.deployments.lock().unwrap().clone())
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
    async fn route_result(
        &self,
        _: Uuid,
        error: Option<InferenceError>,
    ) -> Result<(), InferenceError> {
        self.health.lock().unwrap().push(error);
        Ok(())
    }
    async fn start(&self, record: &ExecutionStart) -> Result<(), InferenceError> {
        self.attempts
            .lock()
            .unwrap()
            .push((record.root_request_id, record.attempt_number));
        self.starts.fetch_add(1, Ordering::SeqCst);
        if self.fail_start.load(Ordering::SeqCst) {
            return Err(InferenceError::Storage);
        }
        Ok(())
    }
    async fn finish(&self, record: &ExecutionFinish) -> Result<(), InferenceError> {
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
        self.finished.notify_one();
        Ok(())
    }
}
#[derive(Clone, Copy)]
enum Mode {
    Complete,
    Stream,
    Truncated,
    Pending,
    Connecting,
    BadOrder,
    Error,
    Throttled,
    InvalidBody,
    InvalidStream,
}
const OBSERVED: Usage = Usage {
    input_tokens: Some(9),
    output_tokens: Some(16),
    billing: None,
    meters: None,
    output_image_variant: None,
    provider_cost_microusd: None,
    reasoning_tokens: None,
};
struct Adapter {
    mode: Mode,
    calls: AtomicUsize,
    id: &'static str,
}
#[async_trait]
impl ProviderAdapter for Adapter {
    fn id(&self) -> &'static str {
        self.id
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events = match self.mode {
            Mode::Complete => {
                return Ok(ProviderOutput::Complete(ChatResponse {
                    content: Some("hello".into()),
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                    usage: Usage {
                        input_tokens: Some(3),
                        output_tokens: Some(1),
                        billing: None,
                        ..Default::default()
                    },
                }));
            }
            Mode::Error => return Err(InferenceError::UpstreamUnavailable),
            Mode::Throttled => return Err(InferenceError::Busy),
            Mode::InvalidBody => {
                return evidence::preserve(Err(InferenceError::InvalidUpstream), || {
                    Some(Ok(OBSERVED))
                });
            }
            Mode::InvalidStream => {
                return Ok(ProviderOutput::Stream(Box::pin(stream::once(async {
                    evidence::preserve(Err(InferenceError::InvalidUpstream), || Some(Ok(OBSERVED)))
                }))));
            }
            Mode::Connecting => return std::future::pending().await,
            Mode::Pending => return Ok(ProviderOutput::Stream(Box::pin(stream::pending()))),
            Mode::Stream => vec![
                Ok(ChatEvent::Delta {
                    text: Some("hello".into()),
                    tool_calls: vec![],
                }),
                Ok(ChatEvent::Finish(FinishReason::Stop)),
                Ok(ChatEvent::Usage(Usage {
                    input_tokens: Some(3),
                    output_tokens: Some(1),
                    billing: None,
                    ..Default::default()
                })),
                Ok(ChatEvent::Done),
            ],
            Mode::Truncated => vec![Ok(ChatEvent::Finish(FinishReason::Stop))],
            Mode::BadOrder => vec![Ok(ChatEvent::Done)],
        };
        Ok(ProviderOutput::Stream(Box::pin(stream::iter(events))))
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
fn principal() -> Principal {
    Principal {
        key_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
        user_id: Some(Uuid::new_v4()),
    }
}
fn fixture(mode: Mode, limits: EngineLimits) -> (Engine, Arc<MemoryRepository>, Arc<Adapter>) {
    let repository = Arc::new(MemoryRepository::default());
    repository.deployments.lock().unwrap().push(Deployment {
        id: Uuid::new_v4(),
        provider: "future_vendor".into(),
        upstream_model: "private-model".into(),
        credential_ref: "unused".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec!["chat_completions".into()],
    });
    let adapter = Arc::new(Adapter {
        mode,
        calls: AtomicUsize::new(0),
        id: "future_vendor",
    });
    let mut registry = ProviderRegistry::default();
    registry.register(adapter.clone()).unwrap();
    (
        Engine::new(repository.clone(), registry, limits).unwrap(),
        repository,
        adapter,
    )
}

#[test]
fn billing_partitions_and_raw_inclusive_bounds_fail_closed() {
    let billing = crate::billing::BillingUsage {
        total_input_tokens: Some(3),
        uncached_input_tokens: Some(1),
        cache_read_input_tokens: Some(2),
        cache_write_input_tokens: Some(0),
        cache_write_default_input_tokens: Some(0),
        cache_write_5m_input_tokens: Some(0),
        cache_write_1h_input_tokens: Some(0),
    };
    let usage = Usage {
        input_tokens: Some(3),
        output_tokens: Some(1),
        billing: Some(billing),
        ..Default::default()
    };
    assert!(valid_usage(usage));
    assert!(!valid_usage(Usage {
        input_tokens: Some(4),
        ..usage
    }));
    assert!(!valid_usage(Usage {
        billing: Some(crate::billing::BillingUsage {
            cache_read_input_tokens: Some(4),
            ..billing
        }),
        ..usage
    }));
    assert!(valid_usage(Usage {
        billing: Some(crate::billing::BillingUsage::default()),
        ..usage
    }));
    assert!(!valid_usage(Usage {
        input_tokens: Some(i64::MAX as u64),
        output_tokens: Some(1),
        billing: None,
        ..Default::default()
    }));
    assert!(!valid_usage(Usage {
        input_tokens: Some(0),
        output_tokens: Some(1),
        billing: Some(crate::billing::BillingUsage {
            total_input_tokens: Some(i64::MAX as u64),
            ..Default::default()
        }),
        ..Default::default()
    }));
}
struct EmbeddingAdapter {
    malformed: bool,
}
#[async_trait]
impl ProviderAdapter for EmbeddingAdapter {
    fn id(&self) -> &'static str {
        "embedding_mock"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: false,
            streaming: false,
            tools: false,
        }
    }
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        protocol == ApiProtocol::Embeddings
    }
    async fn execute(
        &self,
        _: &Deployment,
        _: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    async fn execute_embeddings(
        &self,
        _: &Deployment,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, InferenceError> {
        Ok(EmbeddingResponse {
            embeddings: request
                .input
                .iter()
                .map(|_| vec![if self.malformed { f32::NAN } else { 0.1 }, 0.2])
                .collect(),
            usage: Usage {
                input_tokens: Some(3),
                output_tokens: Some(0),
                billing: None,
                ..Default::default()
            },
        })
    }
}
#[tokio::test]
async fn embeddings_require_declared_model_support_and_account_their_real_workload() {
    let (mut engine, repo, _) = fixture(Mode::Complete, EngineLimits::default());
    engine
        .registry
        .register(Arc::new(EmbeddingAdapter { malformed: false }))
        .unwrap();
    repo.deployments.lock().unwrap()[0].provider = "embedding_mock".into();
    let request = EmbeddingRequest {
        model: "public/alias".into(),
        input: vec!["text".into()],
        dimensions: Some(2),
    };
    assert!(matches!(
        engine
            .execute_embeddings(principal(), request.clone(), Uuid::new_v4())
            .await,
        Err(InferenceError::Unsupported)
    ));
    assert_eq!(repo.starts.load(Ordering::SeqCst), 0);
    repo.deployments.lock().unwrap()[0].supported_protocols = vec!["embeddings".into()];
    let response = engine
        .execute_embeddings(principal(), request, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(response.embeddings, vec![vec![0.1, 0.2]]);
    let rows = repo.finishes.lock().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, Outcome::Succeeded);
    assert_eq!(rows[0].usage.output_tokens, Some(0));
    assert_eq!(rows[0].usage.input_tokens, Some(3));
}
#[tokio::test]
async fn planned_workload_models_never_serve_chat_or_embeddings() {
    for kind in [
        "images",
        "audio_transcriptions",
        "audio_speech",
        "rerank",
        "systemone",
    ] {
        let (engine, repo, _) = fixture(Mode::Complete, EngineLimits::default());
        repo.deployments.lock().unwrap()[0].supported_protocols = vec![kind.into()];
        assert!(matches!(
            engine
                .execute(principal(), request(false), Uuid::new_v4())
                .await,
            Err(InferenceError::Unsupported)
        ));
        let embedding = EmbeddingRequest {
            model: "public/alias".into(),
            input: vec!["text".into()],
            dimensions: None,
        };
        assert!(matches!(
            engine
                .execute_embeddings(principal(), embedding, Uuid::new_v4())
                .await,
            Err(InferenceError::Unsupported)
        ));
        assert_eq!(repo.starts.load(Ordering::SeqCst), 0, "{kind}");
    }
    assert!(ApiProtocol::valid_set(&[
        "chat_completions",
        "responses",
        "messages"
    ]));
    assert!(!ApiProtocol::valid_set(&["chat_completions", "embeddings"]));
    assert!(!ApiProtocol::valid_set(&["images", "rerank"]));
    assert!(!ApiProtocol::valid_set::<&str>(&[]));
    assert!(ApiProtocol::valid_set(&["systemone"]));
}
#[tokio::test]
async fn malformed_embedding_vectors_fail_but_preserve_valid_metering_evidence() {
    let (mut engine, repo, _) = fixture(Mode::Complete, EngineLimits::default());
    engine
        .registry
        .register(Arc::new(EmbeddingAdapter { malformed: true }))
        .unwrap();
    {
        let mut targets = repo.deployments.lock().unwrap();
        targets[0].provider = "embedding_mock".into();
        targets[0].supported_protocols = vec!["embeddings".into()];
    }
    let result = engine
        .execute_embeddings(
            principal(),
            EmbeddingRequest {
                model: "public/alias".into(),
                input: vec!["text".into()],
                dimensions: None,
            },
            Uuid::new_v4(),
        )
        .await;
    assert!(matches!(result, Err(InferenceError::InvalidUpstream)));
    let rows = repo.finishes.lock().unwrap();
    assert_eq!(rows[0].outcome, Outcome::Failed);
    assert_eq!(rows[0].usage.input_tokens, Some(3));
    assert_eq!(rows[0].usage.output_tokens, Some(0));
}

#[tokio::test]
async fn expired_unpolled_stream_releases_capacity_without_poisoning_health() {
    let (engine, repo, _) = fixture(
        Mode::Stream,
        EngineLimits {
            max_concurrent: 1,
            request_timeout: Duration::from_millis(20),
            ..EngineLimits::default()
        },
    );
    let Ok(ProviderOutput::Stream(mut old)) = engine
        .execute(principal(), request(true), Uuid::new_v4())
        .await
    else {
        panic!("expected stream")
    };
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        engine
            .execute(principal(), request(true), Uuid::new_v4())
            .await
            .is_ok()
    );
    assert!(matches!(
        old.next().await,
        Some(Err(InferenceError::Timeout))
    ));
    assert!(repo.health.lock().unwrap().is_empty());
}
fn failover_fixture(
    mode: Mode,
    stream: bool,
    ambiguous: bool,
) -> (Engine, Arc<MemoryRepository>, Arc<Adapter>) {
    let (mut engine, repo, _) = fixture(mode, EngineLimits::default());
    let backup = Arc::new(Adapter {
        mode: if stream { Mode::Stream } else { Mode::Complete },
        calls: AtomicUsize::new(0),
        id: "backup_vendor",
    });
    engine.registry.register(backup.clone()).unwrap();
    let mut deployments = repo.deployments.lock().unwrap();
    let mut second = deployments[0].clone();
    second.id = Uuid::new_v4();
    second.provider = "backup_vendor".into();
    deployments.push(second);
    *repo.plan.lock().unwrap() = Some(crate::routing::RoutePlan {
        deployment_ids: deployments.iter().map(|d| d.id).collect(),
        max_attempts: 2,
        allow_ambiguous_failover: ambiguous,
    });
    drop(deployments);
    (engine, repo, backup)
}
#[tokio::test]
async fn opt_in_failover_accounts_each_attempt_and_preserves_root_request() {
    let (engine, repo, backup) = failover_fixture(Mode::Throttled, false, false);
    let root = Uuid::new_v4();
    assert!(matches!(
        engine.execute(principal(), request(false), root).await,
        Ok(ProviderOutput::Complete(_))
    ));
    assert_eq!(backup.calls.load(Ordering::SeqCst), 1);
    assert_eq!(*repo.attempts.lock().unwrap(), vec![(root, 1), (root, 2)]);
    let finishes = repo.finishes.lock().unwrap();
    assert_eq!(finishes.len(), 2);
    assert_eq!(finishes[0].outcome, Outcome::Failed);
    assert_eq!(finishes[1].outcome, Outcome::Succeeded);
    assert_ne!(finishes[0].id, finishes[1].id);
}
#[tokio::test]
async fn ambiguous_transport_failover_requires_separate_opt_in() {
    for allow in [false, true] {
        let (engine, _, backup) = failover_fixture(Mode::Error, false, allow);
        let result = engine
            .execute(principal(), request(false), Uuid::new_v4())
            .await;
        assert_eq!(result.is_ok(), allow);
        assert_eq!(backup.calls.load(Ordering::SeqCst), usize::from(allow));
    }
}
#[tokio::test]
async fn no_failover_after_stream_boundary_even_before_any_content() {
    let (engine, repo, backup) = failover_fixture(Mode::BadOrder, true, true);
    let ProviderOutput::Stream(mut output) = engine
        .execute(principal(), request(true), Uuid::new_v4())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(output.next().await.unwrap().is_err());
    assert_eq!(backup.calls.load(Ordering::SeqCst), 0);
    assert_eq!(repo.starts.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn accounting_failure_stops_opt_in_failover() {
    let (engine, repo, backup) = failover_fixture(Mode::Throttled, false, true);
    repo.fail_finish.store(true, Ordering::SeqCst);
    assert!(matches!(
        engine
            .execute(principal(), request(false), Uuid::new_v4())
            .await,
        Err(InferenceError::Storage)
    ));
    assert_eq!(backup.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn new_provider_runs_without_engine_or_database_enum_changes() {
    let (engine, repository, adapter) = fixture(Mode::Complete, EngineLimits::default());
    let result = engine
        .execute(principal(), request(false), Uuid::new_v4())
        .await
        .unwrap();
    assert!(matches!(result, ProviderOutput::Complete(_)));
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    let finishes = repository.finishes.lock().unwrap();
    assert_eq!(finishes[0].outcome, Outcome::Succeeded);
    assert_eq!(finishes[0].usage.input_tokens, Some(3));
}
#[tokio::test]
async fn inaccessible_models_and_unsupported_capabilities_never_call_adapter() {
    let (engine, repository, adapter) = fixture(Mode::Complete, EngineLimits::default());
    let mut req = request(false);
    req.tools.push(FunctionTool {
        name: "f".into(),
        description: None,
        parameters: serde_json::json!({}),
        strict: None,
    });
    assert!(matches!(
        engine.execute(principal(), req, Uuid::new_v4()).await,
        Err(InferenceError::Unsupported)
    ));
    repository.deployments.lock().unwrap().clear();
    assert!(matches!(
        engine
            .execute(principal(), request(false), Uuid::new_v4())
            .await,
        Err(InferenceError::ModelUnavailable)
    ));
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(repository.starts.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn streaming_is_accounted_only_after_explicit_terminal_event() {
    let (engine, repository, _) = fixture(Mode::Stream, EngineLimits::default());
    let ProviderOutput::Stream(mut output) = engine
        .execute(principal(), request(true), Uuid::new_v4())
        .await
        .unwrap()
    else {
        panic!("stream expected")
    };
    assert!(repository.finishes.lock().unwrap().is_empty());
    let mut done = 0;
    while let Some(event) = output.next().await {
        if matches!(event.unwrap(), ChatEvent::Done) {
            done += 1;
        }
    }
    assert_eq!(done, 1);
    assert_eq!(
        repository.finishes.lock().unwrap()[0].outcome,
        Outcome::Succeeded
    );
}
#[tokio::test]
async fn stream_eof_bad_order_and_wrong_output_mode_fail() {
    for mode in [Mode::Truncated, Mode::BadOrder, Mode::Complete] {
        let (engine, repository, _) = fixture(mode, EngineLimits::default());
        match engine
            .execute(principal(), request(true), Uuid::new_v4())
            .await
        {
            Err(error) => assert_eq!(error, InferenceError::InvalidUpstream),
            Ok(ProviderOutput::Stream(output)) => {
                let events: Vec<_> = output.collect().await;
                assert!(
                    events
                        .iter()
                        .any(|event| matches!(event, Err(InferenceError::InvalidUpstream)))
                );
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event, Ok(ChatEvent::Done)))
                );
            }
            _ => panic!("invalid mode accepted"),
        }
        assert_eq!(
            repository.finishes.lock().unwrap()[0].outcome,
            Outcome::Failed
        );
    }
}
#[tokio::test]
async fn total_deadline_bounds_stalled_streams() {
    let (engine, repository, _) = fixture(
        Mode::Pending,
        EngineLimits {
            max_concurrent: 1,
            request_timeout: Duration::from_millis(20),
            ..EngineLimits::default()
        },
    );
    let ProviderOutput::Stream(mut output) = engine
        .execute(principal(), request(true), Uuid::new_v4())
        .await
        .unwrap()
    else {
        panic!("stream expected")
    };
    assert!(matches!(
        output.next().await,
        Some(Err(InferenceError::Timeout))
    ));
    assert_eq!(
        repository.finishes.lock().unwrap()[0].outcome,
        Outcome::Failed
    );
}
#[tokio::test]
async fn dropping_unpolled_stream_releases_capacity_and_records_cancellation() {
    let (engine, repository, _) = fixture(
        Mode::Pending,
        EngineLimits {
            max_concurrent: 1,
            ..EngineLimits::default()
        },
    );
    let output = engine
        .execute(principal(), request(true), Uuid::new_v4())
        .await
        .unwrap();
    assert!(matches!(
        engine
            .execute(principal(), request(true), Uuid::new_v4())
            .await,
        Err(InferenceError::Busy)
    ));
    drop(output);
    timeout(Duration::from_secs(1), repository.finished.notified())
        .await
        .unwrap();
    assert_eq!(
        repository.finishes.lock().unwrap()[0].outcome,
        Outcome::Cancelled
    );
    assert!(
        engine
            .execute(principal(), request(true), Uuid::new_v4())
            .await
            .is_ok()
    );
}
/// D1b: a body that fails validation still fails, but its valid usage object
/// is recorded on the failed attempt instead of being lost.
#[tokio::test]
async fn invalid_upstream_body_fails_but_preserves_observed_usage() {
    let (engine, repository, _) = fixture(Mode::InvalidBody, EngineLimits::default());
    assert!(matches!(
        engine
            .execute(principal(), request(false), Uuid::new_v4())
            .await,
        Err(InferenceError::InvalidUpstream)
    ));
    let (engine, stream_repository, _) = fixture(Mode::InvalidStream, EngineLimits::default());
    let ProviderOutput::Stream(output) = engine
        .execute(principal(), request(true), Uuid::new_v4())
        .await
        .unwrap()
    else {
        panic!("stream expected")
    };
    let events: Vec<_> = output.collect().await;
    assert!(matches!(
        events.last(),
        Some(Err(InferenceError::InvalidUpstream))
    ));
    for repository in [repository, stream_repository] {
        let finishes = repository.finishes.lock().unwrap();
        assert_eq!(finishes.len(), 1);
        assert_eq!(finishes[0].outcome, Outcome::Failed);
        assert_eq!(finishes[0].error, Some(InferenceError::InvalidUpstream));
        assert_eq!(finishes[0].usage, OBSERVED);
    }
}
#[tokio::test]
async fn provider_errors_are_recorded_without_retry() {
    let (engine, repository, adapter) = fixture(Mode::Error, EngineLimits::default());
    assert!(matches!(
        engine
            .execute(principal(), request(false), Uuid::new_v4())
            .await,
        Err(InferenceError::UpstreamUnavailable)
    ));
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        repository.finishes.lock().unwrap()[0].outcome,
        Outcome::Failed
    );
}
#[tokio::test]
async fn deadline_and_client_cancellation_cover_provider_setup() {
    let (engine, repository, _) = fixture(
        Mode::Connecting,
        EngineLimits {
            max_concurrent: 1,
            request_timeout: Duration::from_millis(20),
            ..EngineLimits::default()
        },
    );
    assert!(matches!(
        engine
            .execute(principal(), request(false), Uuid::new_v4())
            .await,
        Err(InferenceError::Timeout)
    ));
    assert_eq!(
        repository.finishes.lock().unwrap()[0].outcome,
        Outcome::Failed
    );
    let (engine, repository, _) = fixture(Mode::Connecting, EngineLimits::default());
    assert!(
        timeout(
            Duration::from_millis(20),
            engine.execute(principal(), request(false), Uuid::new_v4())
        )
        .await
        .is_err()
    );
    timeout(Duration::from_secs(1), repository.finished.notified())
        .await
        .unwrap();
    assert_eq!(
        repository.finishes.lock().unwrap()[0].outcome,
        Outcome::Cancelled
    );
}

#[tokio::test]
async fn accounting_failure_before_dispatch_prevents_billable_work() {
    let (engine, repository, adapter) = fixture(Mode::Complete, EngineLimits::default());
    repository.fail_start.store(true, Ordering::SeqCst);
    assert!(matches!(
        engine
            .execute(principal(), request(false), Uuid::new_v4())
            .await,
        Err(InferenceError::Storage)
    ));
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_finalization_does_not_report_success_or_retry_inference() {
    let (engine, repository, adapter) = fixture(Mode::Stream, EngineLimits::default());
    repository.fail_finish.store(true, Ordering::SeqCst);
    let ProviderOutput::Stream(output) = engine
        .execute(principal(), request(true), Uuid::new_v4())
        .await
        .unwrap()
    else {
        panic!("expected stream")
    };
    let events: Vec<_> = output.collect().await;
    assert!(matches!(events.last(), Some(Err(InferenceError::Storage))));
    assert!(!events.iter().any(|e| matches!(e, Ok(ChatEvent::Done))));
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn registry_rejects_duplicate_and_invalid_ids() {
    let mut registry = ProviderRegistry::default();
    assert!(
        registry
            .register(Arc::new(Adapter {
                mode: Mode::Complete,
                calls: AtomicUsize::new(0),
                id: "future_vendor"
            }))
            .is_ok()
    );
    assert!(
        registry
            .register(Arc::new(Adapter {
                mode: Mode::Complete,
                calls: AtomicUsize::new(0),
                id: "future_vendor"
            }))
            .is_err()
    );
    assert!(
        registry
            .register(Arc::new(Adapter {
                mode: Mode::Complete,
                calls: AtomicUsize::new(0),
                id: "Bad ID"
            }))
            .is_err()
    );
}

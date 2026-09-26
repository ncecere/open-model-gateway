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
}
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
                    },
                }));
            }
            Mode::Error => return Err(InferenceError::UpstreamUnavailable),
            Mode::Throttled => return Err(InferenceError::Busy),
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
        organization_id: Uuid::new_v4(),
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

#[tokio::test]
async fn expired_unpolled_stream_releases_capacity_without_poisoning_health() {
    let (engine, repo, _) = fixture(
        Mode::Stream,
        EngineLimits {
            max_concurrent: 1,
            request_timeout: Duration::from_millis(20),
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

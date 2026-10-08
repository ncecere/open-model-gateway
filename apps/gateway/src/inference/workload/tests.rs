use super::*;
use crate::inference::types::*;
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct Repo {
    deployments: Mutex<Vec<Deployment>>,
    plan: Mutex<Option<crate::routing::RoutePlan>>,
    admissions: Mutex<Vec<(i32, WorkloadAdmission)>>,
    finishes: Mutex<Vec<ExecutionFinish>>,
    finished: tokio::sync::Notify,
}
#[async_trait]
impl InferenceRepository for Repo {
    async fn deployments(&self, _: &Principal, _: &str) -> Result<Vec<Deployment>, InferenceError> {
        Ok(self.deployments.lock().unwrap().clone())
    }
    async fn start(&self, _: &ExecutionStart) -> Result<(), InferenceError> {
        panic!("workloads must use admit_workload")
    }
    async fn admit_workload(
        &self,
        record: &ExecutionStart,
        admission: &WorkloadAdmission,
        _: i64,
        _: &Deployment,
    ) -> Result<(), InferenceError> {
        assert!(!record.streamed);
        self.admissions
            .lock()
            .unwrap()
            .push((record.attempt_number, *admission));
        Ok(())
    }
    async fn finish(&self, record: &ExecutionFinish) -> Result<(), InferenceError> {
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
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ok,
    Busy,
    DuplicateIndex,
    OutputTokens,
    ExtraRequests,
    Pending,
}
struct Fake {
    id: &'static str,
    mode: Mode,
    calls: AtomicUsize,
    protocols: Vec<ApiProtocol>,
}
fn usage() -> Usage {
    let mut u = crate::providers::metering::input_only(Some(25));
    u.meters = Some(crate::providers::metering::text_workload_meters(None));
    u
}
#[async_trait]
impl ProviderAdapter for Fake {
    fn id(&self) -> &'static str {
        self.id
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: false,
            streaming: false,
            tools: false,
        }
    }
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        self.protocols.contains(&protocol)
    }
    async fn execute(
        &self,
        _: &Deployment,
        _: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    async fn execute_rerank(
        &self,
        target: &Deployment,
        request: RerankRequest,
    ) -> Result<RerankResponse, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mode = if target.upstream_model == "busy" {
            Mode::Busy
        } else {
            self.mode
        };
        let mut usage = usage();
        let results = match mode {
            Mode::Busy => return Err(InferenceError::Busy),
            Mode::Pending => std::future::pending().await,
            Mode::DuplicateIndex => vec![(0, 0.5), (0, 0.4)],
            Mode::OutputTokens => {
                usage.output_tokens = Some(3);
                vec![(0, 0.5)]
            }
            Mode::ExtraRequests => {
                usage.meters.as_mut().unwrap().requests = Some(2);
                vec![(0, 0.5)]
            }
            Mode::Ok => vec![(1, 0.9), (0, 0.1)],
        };
        assert!(request.documents.len() >= 2);
        Ok(RerankResponse {
            results: results
                .into_iter()
                .map(|(index, relevance_score)| RerankResult {
                    index,
                    relevance_score,
                })
                .collect(),
            usage,
        })
    }
    async fn execute_systemone(
        &self,
        _: &Deployment,
        request: SystemoneRequest,
    ) -> Result<SystemoneResponse, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut usage = crate::providers::metering::input_only(Some(274));
        usage.output_tokens = Some(21);
        usage.meters = Some(crate::providers::metering::text_workload_meters(Some(0)));
        Ok(SystemoneResponse {
            answers: request
                .questions
                .keys()
                .map(|k| (k.clone(), Answer::Noul { noul: 0.99 }))
                .collect(),
            usage,
        })
    }
}

fn principal() -> Principal {
    Principal {
        workspace_id: Uuid::new_v4(),
        key_id: Uuid::new_v4(),
        user_id: None,
    }
}
fn deployment(provider: &str, model: &str, protocol: &str) -> Deployment {
    Deployment {
        id: Uuid::new_v4(),
        provider: provider.into(),
        upstream_model: model.into(),
        credential_ref: "env:KEY".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec![protocol.into()],
    }
}
fn rerank() -> RerankRequest {
    RerankRequest {
        model: "company/rerank".into(),
        query: "cat".into(),
        documents: vec!["kitten".into(), "dog".into()],
        top_n: None,
    }
}
fn systemone() -> SystemoneRequest {
    SystemoneRequest {
        model: "company/jev".into(),
        state: serde_json::json!("hello"),
        questions: BTreeMap::from([(
            "q".to_owned(),
            Question {
                kind: QuestionKind::Noul,
                instructions: serde_json::json!("Greeting?"),
                criteria: None,
            },
        )]),
    }
}
fn setup(
    mode: Mode,
    deployments: Vec<Deployment>,
    timeout: Duration,
) -> (Engine, Arc<Repo>, Arc<Fake>) {
    let repo = Arc::new(Repo::default());
    *repo.deployments.lock().unwrap() = deployments;
    let fake = Arc::new(Fake {
        id: "fake",
        mode,
        calls: AtomicUsize::new(0),
        protocols: vec![ApiProtocol::Rerank, ApiProtocol::Systemone],
    });
    let mut registry = ProviderRegistry::default();
    registry.register(fake.clone()).unwrap();
    let engine = Engine::new(
        repo.clone(),
        registry,
        EngineLimits {
            request_timeout: timeout,
            ..EngineLimits::default()
        },
    )
    .unwrap();
    (engine, repo, fake)
}
const LONG: Duration = Duration::from_secs(30);

#[tokio::test]
async fn rerank_admits_with_meter_ceiling_and_settles_usage() {
    let (engine, repo, fake) = setup(Mode::Ok, vec![deployment("fake", "m", "rerank")], LONG);
    let id = Uuid::new_v4();
    let response = engine
        .execute_workload(principal(), rerank(), id)
        .await
        .unwrap();
    assert_eq!(response.results.len(), 2);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    let admissions = repo.admissions.lock().unwrap().clone();
    assert_eq!(admissions.len(), 1);
    assert_eq!(
        admissions[0].1,
        WorkloadAdmission {
            kind: WorkloadKind::Rerank,
            output: OutputReservation::None,
            unit_ceilings: MeterUsage {
                requests: Some(1),
                ..Default::default()
            },
        }
    );
    let finishes = repo.finishes.lock().unwrap();
    assert_eq!(finishes[0].id, id);
    assert_eq!(finishes[0].outcome, Outcome::Succeeded);
    assert_eq!(finishes[0].usage, usage());
}

#[tokio::test]
async fn systemone_reserves_the_price_output_ceiling() {
    let (engine, repo, _) = setup(Mode::Ok, vec![deployment("fake", "m", "systemone")], LONG);
    let response = engine
        .execute_workload(principal(), systemone(), Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(response.usage.output_tokens, Some(21));
    let (attempt, admission) = repo.admissions.lock().unwrap()[0];
    assert_eq!(attempt, 1);
    assert_eq!(admission.kind, WorkloadKind::Systemone);
    assert_eq!(admission.output, OutputReservation::PriceCeiling);
    assert_eq!(
        repo.finishes.lock().unwrap()[0].usage.output_tokens,
        Some(21)
    );
}

#[tokio::test]
async fn unsupported_model_protocol_or_adapter_never_admits() {
    for (deployments, expected) in [
        (vec![], InferenceError::ModelUnavailable),
        (
            vec![deployment("fake", "m", "chat_completions")],
            InferenceError::Unsupported,
        ),
        (
            vec![deployment("fake", "m", "embeddings")],
            InferenceError::Unsupported,
        ),
        (
            vec![deployment("unregistered", "m", "rerank")],
            InferenceError::Unsupported,
        ),
    ] {
        let (engine, repo, fake) = setup(Mode::Ok, deployments, LONG);
        assert_eq!(
            engine
                .execute_workload(principal(), rerank(), Uuid::new_v4())
                .await
                .err(),
            Some(expected)
        );
        assert!(repo.admissions.lock().unwrap().is_empty());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }
    // An adapter that does not declare the protocol is filtered out too.
    let (engine, repo, _) = setup(Mode::Ok, vec![deployment("fake", "m", "rerank")], LONG);
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(Fake {
            id: "fake",
            mode: Mode::Ok,
            calls: AtomicUsize::new(0),
            protocols: vec![ApiProtocol::Systemone],
        }))
        .unwrap();
    let engine = Engine::new(repo.clone(), registry, engine.limits()).unwrap();
    assert_eq!(
        engine
            .execute_workload(principal(), rerank(), Uuid::new_v4())
            .await
            .err(),
        Some(InferenceError::Unsupported)
    );
}

#[tokio::test]
async fn invalid_requests_fail_before_storage() {
    let (engine, repo, fake) = setup(Mode::Ok, vec![deployment("fake", "m", "rerank")], LONG);
    let mut bad = rerank();
    bad.documents.clear();
    assert_eq!(
        engine
            .execute_workload(principal(), bad, Uuid::new_v4())
            .await
            .err(),
        Some(InferenceError::InvalidRequest)
    );
    assert!(repo.admissions.lock().unwrap().is_empty());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn invalid_responses_fail_and_keep_only_valid_evidence() {
    for (mode, evidence) in [
        (Mode::DuplicateIndex, Some(usage())),
        (Mode::ExtraRequests, None),
        (Mode::OutputTokens, None),
    ] {
        let (engine, repo, _) = setup(mode, vec![deployment("fake", "m", "rerank")], LONG);
        assert_eq!(
            engine
                .execute_workload(principal(), rerank(), Uuid::new_v4())
                .await
                .err(),
            Some(InferenceError::InvalidUpstream)
        );
        let finishes = repo.finishes.lock().unwrap();
        assert_eq!(finishes[0].outcome, Outcome::Failed);
        assert_eq!(finishes[0].error, Some(InferenceError::InvalidUpstream));
        match evidence {
            Some(u) => assert_eq!(finishes[0].usage, u),
            // Over-ceiling meters are still recorded (they are observed), but
            // input-only workloads never record nonzero output tokens.
            None if mode == Mode::OutputTokens => {
                assert_eq!(finishes[0].usage, Usage::default())
            }
            None => assert_eq!(finishes[0].usage.meters.unwrap().requests, Some(2)),
        }
    }
}

#[tokio::test]
async fn explicit_failover_creates_distinct_admitted_attempts() {
    let first = deployment("fake", "busy", "rerank");
    let second = deployment("fake", "m", "rerank");
    let (engine, repo, fake) = setup(Mode::Ok, vec![first.clone(), second.clone()], LONG);
    *repo.plan.lock().unwrap() = Some(crate::routing::RoutePlan {
        deployment_ids: vec![first.id, second.id],
        max_attempts: 2,
        allow_ambiguous_failover: false,
    });
    let root = Uuid::new_v4();
    engine
        .execute_workload(principal(), rerank(), root)
        .await
        .unwrap();
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    let attempts: Vec<i32> = repo
        .admissions
        .lock()
        .unwrap()
        .iter()
        .map(|a| a.0)
        .collect();
    assert_eq!(attempts, vec![1, 2]);
    {
        let finishes = repo.finishes.lock().unwrap();
        assert_eq!(finishes[0].id, root);
        assert_eq!(finishes[0].error, Some(InferenceError::Busy));
        assert_ne!(finishes[1].id, root);
        assert_eq!(finishes[1].outcome, Outcome::Succeeded);
    }
    // Without an explicit multi-attempt policy there is no failover.
    let (engine, repo, fake) = setup(Mode::Ok, vec![first, second], LONG);
    assert_eq!(
        engine
            .execute_workload(principal(), rerank(), Uuid::new_v4())
            .await
            .err(),
        Some(InferenceError::Busy)
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert_eq!(repo.admissions.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn deadline_and_cancellation_are_accounted() {
    let (engine, repo, _) = setup(
        Mode::Pending,
        vec![deployment("fake", "m", "rerank")],
        Duration::from_millis(20),
    );
    assert_eq!(
        engine
            .execute_workload(principal(), rerank(), Uuid::new_v4())
            .await
            .err(),
        Some(InferenceError::Timeout)
    );
    assert_eq!(
        repo.finishes.lock().unwrap()[0].error,
        Some(InferenceError::Timeout)
    );

    let (engine, repo, fake) = setup(Mode::Pending, vec![deployment("fake", "m", "rerank")], LONG);
    let mut future = Box::pin(engine.execute_workload(principal(), rerank(), Uuid::new_v4()));
    tokio::select! {
        _ = &mut future => panic!("pending adapter completed"),
        _ = async { while fake.calls.load(Ordering::SeqCst) == 0 { tokio::task::yield_now().await } } => {}
    }
    drop(future);
    tokio::time::timeout(Duration::from_secs(3), repo.finished.notified())
        .await
        .unwrap();
    assert_eq!(repo.finishes.lock().unwrap()[0].outcome, Outcome::Cancelled);
}

#[test]
fn workload_body_limits_are_configurable_and_bounded() {
    let limits = WorkloadLimits::from_lookup(|name| match name {
        "GATEWAY_MAX_BODY_BYTES_RERANK" => Some("4194304".into()),
        _ => None,
    })
    .unwrap();
    assert_eq!(
        limits.body_bytes(WorkloadKind::Rerank),
        Some(4 * 1024 * 1024)
    );
    assert_eq!(
        limits.body_bytes(WorkloadKind::Systemone),
        Some(2 * 1024 * 1024)
    );
    assert_eq!(limits.body_bytes(WorkloadKind::Generation), None);
    for bad in ["12", "x", "999999999999"] {
        assert!(
            WorkloadLimits::from_lookup(
                |name| (name == "GATEWAY_MAX_BODY_BYTES_SYSTEMONE").then(|| bad.to_owned())
            )
            .is_err()
        );
    }
}

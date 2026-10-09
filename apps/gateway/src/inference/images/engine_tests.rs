//! Engine path for images: pre-admission gating, `n`-derived ceilings,
//! response validation with evidence, deadline and cancellation.
use super::{fixtures::*, *};
use crate::{
    auth::Principal,
    inference::{
        Engine, EngineLimits,
        repository::{ExecutionFinish, ExecutionStart, InferenceRepository, Outcome},
        types::{Capabilities, ChatRequest, ProviderOutput},
    },
    providers::{ProviderRegistry, images::meters},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;

#[derive(Default)]
struct Repo {
    deployments: Mutex<Vec<Deployment>>,
    admissions: Mutex<Vec<WorkloadAdmission>>,
    finishes: Mutex<Vec<(Outcome, Option<InferenceError>, Usage)>>,
    finished: tokio::sync::Notify,
}
#[async_trait]
impl InferenceRepository for Repo {
    async fn deployments(&self, _: &Principal, _: &str) -> Result<Vec<Deployment>> {
        Ok(self.deployments.lock().unwrap().clone())
    }
    async fn start(&self, _: &ExecutionStart) -> Result<()> {
        panic!("images must use admit_workload")
    }
    async fn admit_workload(
        &self,
        _: &ExecutionStart,
        admission: &WorkloadAdmission,
        _: i64,
        _: &Deployment,
    ) -> Result<()> {
        self.admissions.lock().unwrap().push(*admission);
        Ok(())
    }
    async fn finish(&self, r: &ExecutionFinish) -> Result<()> {
        self.finishes
            .lock()
            .unwrap()
            .push((r.outcome, r.error, r.usage));
        self.finished.notify_one();
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ok,
    WrongCount,
    Pending,
}
struct Fake {
    mode: Mode,
    calls: AtomicUsize,
}
fn usage(images: u64) -> Usage {
    Usage {
        input_tokens: Some(9),
        output_tokens: Some(272),
        meters: Some(meters(Some(images))),
        ..Default::default()
    }
}
#[async_trait]
impl ProviderAdapter for Fake {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: false,
            streaming: false,
            tools: false,
        }
    }
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        protocol == ApiProtocol::Images
    }
    fn supports_image_request(&self, _: &Deployment, request: &ImageRequest) -> bool {
        request.seed.is_none()
    }
    async fn execute(&self, _: &Deployment, _: ChatRequest) -> Result<ProviderOutput> {
        Err(InferenceError::Unsupported)
    }
    async fn execute_images(&self, _: &Deployment, request: ImageRequest) -> Result<ImageResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let n = match self.mode {
            Mode::Pending => std::future::pending().await,
            Mode::WrongCount => request.n + 1,
            Mode::Ok => request.n,
        };
        Ok(ImageResponse {
            created: 1,
            images: (0..n)
                .map(|_| GeneratedImage {
                    b64_json: PNG_1X1.into(),
                    media_type: ImageMediaType::Png,
                    revised_prompt: None,
                })
                .collect(),
            usage: usage(u64::from(n)),
        })
    }
}
fn deployment(protocol: &str) -> Deployment {
    Deployment {
        id: Uuid::new_v4(),
        provider: "fake".into(),
        upstream_model: "m".into(),
        credential_ref: "env:KEY".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec![protocol.into()],
    }
}
fn setup(mode: Mode, protocol: &str, timeout: Duration) -> (Engine, Arc<Repo>, Arc<Fake>) {
    let repo = Arc::new(Repo::default());
    *repo.deployments.lock().unwrap() = vec![deployment(protocol)];
    let fake = Arc::new(Fake {
        mode,
        calls: AtomicUsize::new(0),
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
fn principal() -> Principal {
    Principal {
        workspace_id: Uuid::new_v4(),
        key_id: Uuid::new_v4(),
        user_id: None,
    }
}
fn request(n: u32) -> ImageRequest {
    ImageRequest {
        model: "company/image".into(),
        prompt: "a red dot".into(),
        n,
        size: None,
        quality: None,
        seed: None,
        max_response_bytes: 1 << 20,
    }
}
const LONG: Duration = Duration::from_secs(30);

#[tokio::test]
async fn admits_n_ceiling_with_price_output_ceiling_and_settles_meters() {
    let (engine, repo, fake) = setup(Mode::Ok, "images", LONG);
    let response = engine
        .execute_workload(principal(), request(3), Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(response.images.len(), 3);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    let admission = repo.admissions.lock().unwrap()[0];
    assert_eq!(admission.kind, WorkloadKind::Images);
    assert_eq!(admission.output, OutputReservation::PriceCeiling);
    assert_eq!(
        admission.unit_ceilings,
        MeterUsage {
            output_images: Some(3),
            input_characters: Some(0),
            input_audio_seconds_ms: Some(0),
            output_audio_seconds_ms: Some(0),
            search_units: Some(0),
            requests: Some(1),
            output_video_seconds_ms: None,
        }
    );
    let finishes = repo.finishes.lock().unwrap();
    assert_eq!(finishes[0].0, Outcome::Succeeded);
    assert_eq!(finishes[0].2, usage(3));
}

#[tokio::test]
async fn unsupported_or_invalid_requests_never_admit() {
    for (protocol, request, expected) in [
        ("rerank", request(1), InferenceError::Unsupported),
        (
            "images",
            ImageRequest {
                seed: Some(1),
                ..request(1)
            },
            InferenceError::Unsupported,
        ),
        ("images", request(5), InferenceError::InvalidRequest),
    ] {
        let (engine, repo, fake) = setup(Mode::Ok, protocol, LONG);
        assert_eq!(
            engine
                .execute_workload(principal(), request, Uuid::new_v4())
                .await
                .err(),
            Some(expected)
        );
        assert!(repo.admissions.lock().unwrap().is_empty());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn wrong_count_fails_and_keeps_observed_evidence() {
    let (engine, repo, _) = setup(Mode::WrongCount, "images", LONG);
    assert_eq!(
        engine
            .execute_workload(principal(), request(2), Uuid::new_v4())
            .await
            .err(),
        Some(InferenceError::InvalidUpstream)
    );
    let finishes = repo.finishes.lock().unwrap();
    assert_eq!(finishes[0].0, Outcome::Failed);
    assert_eq!(finishes[0].1, Some(InferenceError::InvalidUpstream));
    // Over-ceiling image counts are observations, recorded for settlement.
    assert_eq!(finishes[0].2.meters.unwrap().output_images, Some(3));
}

#[tokio::test]
async fn deadline_and_cancellation_are_accounted() {
    let (engine, repo, _) = setup(Mode::Pending, "images", Duration::from_millis(20));
    assert_eq!(
        engine
            .execute_workload(principal(), request(1), Uuid::new_v4())
            .await
            .err(),
        Some(InferenceError::Timeout)
    );
    assert_eq!(
        repo.finishes.lock().unwrap()[0].1,
        Some(InferenceError::Timeout)
    );
    let (engine, repo, fake) = setup(Mode::Pending, "images", LONG);
    let mut future = Box::pin(engine.execute_workload(principal(), request(1), Uuid::new_v4()));
    tokio::select! {
        _ = &mut future => panic!("pending adapter completed"),
        _ = async { while fake.calls.load(Ordering::SeqCst) == 0 { tokio::task::yield_now().await } } => {}
    }
    drop(future);
    tokio::time::timeout(Duration::from_secs(3), repo.finished.notified())
        .await
        .unwrap();
    assert_eq!(repo.finishes.lock().unwrap()[0].0, Outcome::Cancelled);
}

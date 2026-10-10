//! Async jobs on a disposable database with a fake provider: the state
//! machine, reservation/settlement exactness, ownership/privacy, cancel,
//! expiry, unknown usage, the streamed upload contract and idempotent polling.
use super::*;
use crate::{
    billing::{MeterVariant, v3::Meter},
    governance::tests::db::{Fixture, amounts, fixture},
};
use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Mutex;

#[derive(Default)]
struct Script {
    videos: std::collections::VecDeque<UpstreamVideo>,
    batches: std::collections::VecDeque<UpstreamBatch>,
    calls: Vec<&'static str>,
}
#[derive(Clone, Default)]
struct Fake(Arc<Mutex<Script>>);
impl Fake {
    fn push_video(&self, v: UpstreamVideo) {
        self.0.lock().unwrap().videos.push_back(v);
    }
    fn calls(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().calls.clone()
    }
    fn call(&self, name: &'static str) {
        self.0.lock().unwrap().calls.push(name);
    }
    fn next_video(&self) -> Result<UpstreamVideo, InferenceError> {
        self.0
            .lock()
            .unwrap()
            .videos
            .pop_front()
            .ok_or(InferenceError::UpstreamUnavailable)
    }
    fn next_batch(&self) -> Result<UpstreamBatch, InferenceError> {
        self.0
            .lock()
            .unwrap()
            .batches
            .pop_front()
            .ok_or(InferenceError::UpstreamUnavailable)
    }
}
#[async_trait]
impl ProviderAdapter for Fake {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn capabilities(&self) -> crate::inference::types::Capabilities {
        crate::inference::types::Capabilities {
            text_chat: true,
            streaming: false,
            tools: false,
        }
    }
    fn supports_protocol(&self, p: ApiProtocol) -> bool {
        matches!(p, ApiProtocol::Videos | ApiProtocol::Batches)
    }
    fn supports_video_request(&self, _: &Deployment, _: &VideoRequest) -> bool {
        true
    }
    async fn execute(
        &self,
        _: &Deployment,
        _: crate::inference::types::ChatRequest,
    ) -> Result<crate::inference::types::ProviderOutput, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    async fn create_video(
        &self,
        target: &Deployment,
        request: VideoRequest,
    ) -> Result<UpstreamVideo, InferenceError> {
        assert_eq!(target.upstream_model, "mock-model");
        assert_eq!(request.prompt, "a calm lake");
        self.call("create_video");
        // Providers mint a unique id per job.
        self.next_video().map(|mut v| {
            v.id = UpstreamId::parse(&format!("video_{}", Uuid::new_v4().simple())).unwrap();
            v
        })
    }
    async fn retrieve_video(
        &self,
        _: &Deployment,
        _: &UpstreamId,
    ) -> Result<UpstreamVideo, InferenceError> {
        self.call("retrieve_video");
        self.next_video()
    }
    async fn delete_video(&self, _: &Deployment, _: &UpstreamId) -> Result<(), InferenceError> {
        self.call("delete_video");
        Ok(())
    }
    async fn retrieve_batch(
        &self,
        _: &Deployment,
        _: &UpstreamId,
    ) -> Result<UpstreamBatch, InferenceError> {
        self.call("retrieve_batch");
        self.next_batch()
    }
    async fn cancel_batch(
        &self,
        _: &Deployment,
        _: &UpstreamId,
    ) -> Result<UpstreamBatch, InferenceError> {
        self.call("cancel_batch");
        self.next_batch()
    }
}

fn jobs(f: &Fixture, fake: &Fake, poll: bool) -> Jobs {
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(fake.clone())).unwrap();
    Jobs::with(
        f.store.clone(),
        registry,
        JobLimits {
            poll_interval: poll.then(|| Duration::from_secs(1)),
            ..JobLimits::default()
        },
    )
}
fn line(meter: Meter, amount: &str, batch: u64) -> Value {
    json!({"meter":meter.as_str(),"microusd_per_batch":amount,"batch":batch,"unit_label":meter.unit_label(batch).unwrap(),"sku_label":"Line"})
}
fn na(meter: Meter) -> Value {
    json!({"meter":meter.as_str(),"not_applicable":true})
}
/// Per-second video price: 720p $0.10/s, 1792x1024 $0.50/s; tokens NA.
async fn video_setup(f: &Fixture) {
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['videos'] WHERE id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let mut lines: Vec<Value> = Meter::ALL
        .into_iter()
        .filter(|m| *m != Meter::Requests)
        .map(na)
        .collect();
    lines.push(line(Meter::Requests, "0", 1));
    let mut v = line(Meter::OutputVideoSecondsMs, "100000", 1000);
    v["variant"] = json!("720x1280");
    lines.push(v);
    let mut v = line(Meter::OutputVideoSecondsMs, "500000", 1000);
    v["variant"] = json!("1792x1024");
    lines.push(v);
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,0,1,3,$3,'{}')").bind(Uuid::new_v4()).bind(f.deployment).bind(json!(lines)).execute(&f.store.pool).await.unwrap();
}
fn video_request() -> VideoRequest {
    VideoRequest {
        model: "company/smart".into(),
        prompt: "a calm lake".into(),
        seconds: 8,
        size: VideoSize::P720x1280,
    }
}
fn upstream_video(state: JobState, seconds: Option<u32>) -> UpstreamVideo {
    UpstreamVideo {
        id: UpstreamId::parse("video_up1").unwrap(),
        state,
        progress: None,
        seconds,
        size: MeterVariant::new("720x1280"),
        completed_at: None,
        expires_at: None,
        error: None,
    }
}
async fn lease(f: &Fixture, id: Uuid) -> chrono::DateTime<Utc> {
    sqlx::query_scalar("SELECT lease_expires_at FROM governance_reservations WHERE execution_id=$1")
        .bind(id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn video_reserves_highest_tier_and_settles_actual_seconds_once(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let request_id = Uuid::new_v4();
    let job = j
        .create_video(f.principal, video_request(), request_id)
        .await
        .unwrap();
    assert_eq!(
        (job.state.as_str(), job.execution_id),
        ("queued", request_id)
    );
    // 8 s × the highest variant rate ($0.50/s), never the requested tier only.
    assert_eq!(
        amounts(&f, request_id).await,
        ("pending".into(), Some(4_000_000), None, false)
    );
    assert!(lease(&f, request_id).await > Utc::now() + TimeDelta::hours(5));
    // Poll: in progress, then completed with the actual duration and tier.
    fake.push_video(upstream_video(JobState::InProgress, Some(8)));
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 1);
    let mut done = upstream_video(JobState::Completed, Some(8));
    done.progress = Some(100);
    fake.push_video(done);
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 1);
    assert_eq!(
        amounts(&f, request_id).await,
        ("settled".into(), Some(4_000_000), Some(800_000), false)
    );
    let (state, meters, variant): (String, Value, String) = sqlx::query_as(
        "SELECT state,meter_usage,output_image_variant FROM inference_executions WHERE id=$1",
    )
    .bind(request_id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(state, "succeeded");
    assert_eq!(meters["output_video_seconds_ms"], "8000");
    assert_eq!(variant, "720x1280");
    let components: Value = sqlx::query_scalar(
        "SELECT cost_components FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(request_id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(components["output_video_microusd"], "800000");
    // Settled jobs are never claimed again; a repeated settle is a no-op.
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 0);
    let row = j
        .get_video(&f.principal, &client_id("video_", job.id))
        .await
        .unwrap();
    assert_eq!(row.state, "completed");
    assert!(row.settled_at.is_some());
    let ledger: i64 =
        sqlx::query_scalar("SELECT count(*) FROM monetary_ledger WHERE execution_id=$1")
            .bind(request_id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(ledger, 2, "one hold and one settlement");
    assert_eq!(
        fake.calls(),
        ["create_video", "retrieve_video", "retrieve_video"]
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn unknown_or_failed_video_keeps_the_hold(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, false);
    // Completed without a reported duration: unknown, hold retained.
    fake.push_video(upstream_video(JobState::InProgress, None));
    let a = Uuid::new_v4();
    let job = j
        .create_video(f.principal, video_request(), a)
        .await
        .unwrap();
    fake.push_video(upstream_video(JobState::Completed, None));
    let job = j
        .get_video(&f.principal, &client_id("video_", job.id))
        .await
        .unwrap();
    assert_eq!(job.state, "completed");
    assert_eq!(
        amounts(&f, a).await,
        ("unknown".into(), Some(4_000_000), None, false)
    );
    // Failed by the provider: unknown (the price is not entirely free).
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let b = Uuid::new_v4();
    let job = j
        .create_video(f.principal, video_request(), b)
        .await
        .unwrap();
    let mut failed = upstream_video(JobState::Failed, None);
    failed.error = Some(ErrorCode::parse("moderation_blocked"));
    fake.push_video(failed);
    let job = j
        .get_video(&f.principal, &client_id("video_", job.id))
        .await
        .unwrap();
    assert_eq!(
        (job.state.as_str(), job.error_code.as_deref()),
        ("failed", Some("moderation_blocked"))
    );
    assert_eq!(amounts(&f, b).await.0, "unknown");
    assert_eq!(amounts(&f, b).await.1, Some(4_000_000));
    // The upstream create failing records a failed attempt (hold kept).
    let c = Uuid::new_v4();
    assert_eq!(
        j.create_video(f.principal, video_request(), c).await.err(),
        Some(JobError::Inference(InferenceError::UpstreamUnavailable))
    );
    assert_eq!(amounts(&f, c).await.0, "unknown");
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM async_jobs")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(jobs, 2);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn jobs_are_private_to_their_workspace(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let job = j
        .create_video(f.principal, video_request(), Uuid::new_v4())
        .await
        .unwrap();
    let id = client_id("video_", job.id);
    // Another workspace's key: not found (not forbidden), no upstream call.
    assert_eq!(
        j.get_video(&f.team, &id).await.err(),
        Some(JobError::NotFound)
    );
    assert_eq!(
        j.delete_video(&f.team, &id).await.err(),
        Some(JobError::NotFound)
    );
    assert_eq!(
        j.video_content(&f.team, &id, VideoAsset::Video).await.err(),
        Some(JobError::NotFound)
    );
    assert!(
        j.list_videos(&f.team, None, 10, false)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        j.list_videos(&f.principal, None, 10, false)
            .await
            .unwrap()
            .len(),
        1
    );
    // A batch id with a video's uuid, or garbage, is not found either.
    assert_eq!(
        j.get_batch(&f.principal, &client_id("batch_", job.id))
            .await
            .err(),
        Some(JobError::NotFound)
    );
    assert_eq!(
        j.get_video(&f.principal, "video_../x").await.err(),
        Some(JobError::NotFound)
    );
    // Content needs a completed job; deleting needs a finished one.
    assert!(matches!(
        j.video_content(&f.principal, &id, VideoAsset::Video).await,
        Err(JobError::Conflict("video_not_ready", _))
    ));
    assert!(matches!(
        j.delete_video(&f.principal, &id).await,
        Err(JobError::Conflict("video_in_progress", _))
    ));
    assert_eq!(fake.calls(), ["create_video"]);
    // Disabling the connection stops client-initiated calls and polling.
    sqlx::query("UPDATE provider_connections SET enabled=false")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        j.get_video(&f.principal, &id).await.err(),
        Some(JobError::Inference(InferenceError::ModelUnavailable))
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn the_state_machine_is_forward_only_and_identity_immutable(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    fake.push_video(upstream_video(JobState::InProgress, Some(8)));
    let job = j
        .create_video(f.principal, video_request(), Uuid::new_v4())
        .await
        .unwrap();
    // An observed regression is ignored.
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let row = j.refresh_video(job.clone()).await.unwrap();
    assert_eq!(row.state, "in_progress");
    // The database refuses invalid transitions and identity changes.
    for sql in [
        "UPDATE async_jobs SET state='queued'",
        "UPDATE async_jobs SET upstream_id='other'",
        "UPDATE async_jobs SET workspace_id=gen_random_uuid()",
        "DELETE FROM async_jobs",
    ] {
        assert!(
            sqlx::query(sql).execute(&f.store.pool).await.is_err(),
            "{sql}"
        );
    }
    sqlx::query("UPDATE async_jobs SET state='failed',completed_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE async_jobs SET state='completed'")
            .execute(&f.store.pool)
            .await
            .is_err()
    );
    sqlx::query("UPDATE async_jobs SET settled_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE async_jobs SET settled_at=now()+interval '1 second'")
            .execute(&f.store.pool)
            .await
            .is_err()
    );
}

async fn bearer(f: &Fixture, workspace: Uuid) -> String {
    let key = crate::auth::NewApiKey::generate();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'http',$4)").bind(key.id).bind(workspace).bind(f.owner).bind(key.digest.as_slice()).execute(&f.store.pool).await.unwrap();
    format!("Bearer {}", key.token)
}
async fn send(
    app: &axum::Router,
    method: &str,
    path: &str,
    auth: &str,
    body: Option<Value>,
) -> (axum::http::StatusCode, Value) {
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", auth);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(axum::body::Body::from(
                    body.map(|b| b.to_string()).unwrap_or_default(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn http_video_routes_render_gateway_ids_and_scope_by_key(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(fake.clone())).unwrap();
    let engine = Engine::new(
        Arc::new(f.store.clone()),
        registry,
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_engine(f.store.clone(), None, engine);
    let mine = bearer(&f, f.principal.workspace_id).await;
    let theirs = bearer(&f, f.team.workspace_id).await;
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    let (status, created) = send(
        &app,
        "POST",
        "/v1/videos",
        &mine,
        Some(json!({"model":"company/smart","prompt":"a calm lake","seconds":"8"})),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("video_") && !created.to_string().contains("video_up"));
    assert_eq!(
        (
            created["object"].as_str(),
            created["status"].as_str(),
            created["seconds"].as_str()
        ),
        (Some("video"), Some("queued"), Some("8"))
    );
    assert_eq!(created["model"], "company/smart");
    // Another workspace's key cannot see, list, download or delete it.
    for (method, path) in [
        ("GET", format!("/v1/videos/{id}")),
        ("GET", format!("/v1/videos/{id}/content")),
        ("DELETE", format!("/v1/videos/{id}")),
    ] {
        let (status, body) = send(&app, method, &path, &theirs, None).await;
        assert_eq!(status, 404, "{method} {path}: {body}");
        assert_eq!(body["error"]["code"], "not_found");
    }
    assert_eq!(
        send(&app, "GET", "/v1/videos", &theirs, None).await.1["data"],
        json!([])
    );
    let (_, list) = send(&app, "GET", "/v1/videos?limit=1", &mine, None).await;
    assert_eq!(
        (
            list["object"].as_str(),
            list["data"][0]["id"].as_str(),
            list["has_more"].as_bool()
        ),
        (Some("list"), Some(id.as_str()), Some(false))
    );
    // Not ready yet; bad queries are client errors; unsupported fields explicit.
    let (status, body) = send(
        &app,
        "GET",
        &format!("/v1/videos/{id}/content"),
        &mine,
        None,
    )
    .await;
    assert_eq!(
        (status.as_u16(), body["error"]["code"].as_str()),
        (400, Some("video_not_ready"))
    );
    assert_eq!(
        send(&app, "GET", "/v1/videos?limit=0", &mine, None).await.0,
        400
    );
    let (status, body) = send(&app, "POST", "/v1/videos", &mine, Some(json!({"model":"company/smart","prompt":"x","input_reference":{"image_url":"https://x"}}))).await;
    assert_eq!(
        (status.as_u16(), body["error"]["code"].as_str()),
        (400, Some("unsupported_capability"))
    );
    // Batch routes answer explicitly too.
    // Without the file store, batches are refused explicitly.
    let (status, body) = send(&app, "POST", "/v1/batches", &mine, Some(json!({"input_file_id":"file-00000000000000000000000000000000","endpoint":"/v1/chat/completions","completion_window":"24h"}))).await;
    assert_eq!(
        (status.as_u16(), body["error"]["code"].as_str()),
        (400, Some("batch_files_disabled"))
    );
    let (status, _) = send(
        &app,
        "POST",
        "/v1/batches",
        &mine,
        Some(
            json!({"input_file_id":"file-x","endpoint":"/v1/audio/speech","completion_window":"24h"}),
        ),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(fake.calls(), ["create_video"]);
}

/// Active job slots of a workspace as admission counts them (0018).
async fn job_count(f: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id LEFT JOIN async_jobs j ON j.execution_id=r.execution_id WHERE r.workspace_id=$1 AND e.workload_kind IN('videos','batches') AND r.state='pending' AND r.lease_expires_at>now() AND NOT coalesce(j.state IN('completed','failed','cancelled','expired') OR j.cancel_requested_at IS NOT NULL,false)")
        .bind(f.principal.workspace_id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn video_jobs_are_exempt_from_per_minute_limits(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, false);
    // One request per minute and at once: both videos still start (job slots).
    f.policy("workspace_local_policies", Some(1), Some(1), Some(1), None)
        .await;
    for _ in 0..2 {
        fake.push_video(upstream_video(JobState::Queued, Some(8)));
        j.create_video(f.principal, video_request(), Uuid::new_v4())
            .await
            .unwrap();
    }
    assert_eq!(
        j.create_video(f.principal, video_request(), Uuid::new_v4())
            .await
            .err(),
        Some(JobError::Inference(InferenceError::JobLimitExceeded(
            crate::inference::error::LimitScope::Workspace
        )))
    );
    // Raising the workspace's job limit (platform override) admits a third.
    sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id,concurrent_jobs) VALUES($1,3)")
        .bind(f.principal.workspace_id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    j.create_video(f.principal, video_request(), Uuid::new_v4())
        .await
        .unwrap();
    // A key lineage limit still applies on top (there is no installation-wide limit).
    sqlx::query("INSERT INTO key_policies(workspace_id,governance_key_id,concurrent_jobs) SELECT workspace_id,governance_key_id,3 FROM api_keys WHERE id=$1")
        .bind(f.principal.key_id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE workspace_platform_policy_overrides SET concurrent_jobs=10")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        j.create_video(f.principal, video_request(), Uuid::new_v4())
            .await
            .err(),
        Some(JobError::Inference(InferenceError::JobLimitExceeded(
            crate::inference::error::LimitScope::ApiKey
        )))
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_job_admissions_never_exceed_the_limit(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, false);
    for _ in 0..6 {
        fake.push_video(upstream_video(JobState::Queued, Some(8)));
    }
    // Admission checks and inserts under the installation lock, so parallel
    // submissions cannot both take the last slot.
    let results = futures_util::future::join_all(
        (0..6).map(|_| j.create_video(f.principal, video_request(), Uuid::new_v4())),
    )
    .await;
    let admitted = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(admitted, 2, "{results:?}");
    assert!(results.iter().filter_map(|r| r.as_ref().err()).all(|e| *e
        == JobError::Inference(InferenceError::JobLimitExceeded(
            crate::inference::error::LimitScope::Workspace
        ))));
    assert_eq!(job_count(&f).await, 2);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn video_without_a_supported_provider_is_refused_before_admission(pool: PgPool) {
    let f = fixture(pool).await;
    video_setup(&f).await;
    // The production OpenAI adapter: its Videos API was retired on 2026-09-24.
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(
            crate::providers::openai::OpenAiAdapter::new(Arc::new(
                crate::providers::secrets::EnvSecrets::new([]),
            ))
            .unwrap(),
        ))
        .unwrap();
    let j = Jobs::with(f.store.clone(), registry, JobLimits::default());
    let request_id = Uuid::new_v4();
    match j
        .create_video(f.principal, video_request(), request_id)
        .await
    {
        Err(JobError::Conflict(code, message)) => {
            assert_eq!(code, "unsupported_capability");
            assert!(message.contains("2026-09-24") && message.contains("OpenRouter"));
        }
        other => panic!("expected an explicit refusal, got {:?}", other.map(|_| ())),
    }
    // Nothing was admitted or fabricated.
    let rows: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM inference_executions),(SELECT count(*) FROM governance_reservations),(SELECT count(*) FROM async_jobs)")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(rows, (0, 0, 0));
}

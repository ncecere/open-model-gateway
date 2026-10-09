//! Async jobs on a disposable database with a fake provider: the state
//! machine, reservation/settlement exactness, ownership/privacy, cancel,
//! expiry, unknown usage, the streamed upload contract and idempotent polling.
use super::*;
use crate::{
    billing::{MeterVariant, v3::Meter},
    governance::tests::db::{Fixture, amounts, fixture},
};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Mutex;

#[derive(Default)]
struct Script {
    videos: std::collections::VecDeque<UpstreamVideo>,
    batches: std::collections::VecDeque<UpstreamBatch>,
    uploaded: Vec<u8>,
    upload_aborted: bool,
    output: Option<OutputUsage>,
    calls: Vec<&'static str>,
}
#[derive(Clone, Default)]
struct Fake(Arc<Mutex<Script>>);
impl Fake {
    fn push_video(&self, v: UpstreamVideo) {
        self.0.lock().unwrap().videos.push_back(v);
    }
    fn push_batch(&self, b: UpstreamBatch) {
        self.0.lock().unwrap().batches.push_back(b);
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
    async fn upload_batch_file(
        &self,
        _: &Deployment,
        mut content: ByteStream,
    ) -> Result<UpstreamFile, InferenceError> {
        self.call("upload");
        let mut all = Vec::new();
        while let Some(chunk) = content.next().await {
            match chunk {
                Ok(c) => all.extend_from_slice(&c),
                Err(_) => {
                    self.0.lock().unwrap().upload_aborted = true;
                    return Err(InferenceError::UpstreamUnavailable);
                }
            }
        }
        let n = all.len() as u64;
        self.0.lock().unwrap().uploaded = all;
        Ok(UpstreamFile {
            id: UpstreamId::parse(&format!("file-up{}", Uuid::new_v4().simple())).unwrap(),
            bytes: Some(n),
        })
    }
    async fn create_batch(
        &self,
        _: &Deployment,
        input: &UpstreamId,
        _: Option<serde_json::Map<String, Value>>,
    ) -> Result<UpstreamBatch, InferenceError> {
        assert!(input.as_str().starts_with("file-up"));
        self.call("create_batch");
        self.next_batch().map(|mut b| {
            b.id = UpstreamId::parse(&format!("batch_{}", Uuid::new_v4().simple())).unwrap();
            b
        })
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
    async fn batch_output_usage(
        &self,
        _: &Deployment,
        _: &UpstreamId,
        _: u64,
    ) -> Result<OutputUsage, InferenceError> {
        self.call("output_usage");
        self.0
            .lock()
            .unwrap()
            .output
            .ok_or(InferenceError::UpstreamUnavailable)
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

async fn batch_setup(f: &Fixture) {
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['batches'] WHERE id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    // v1: 1,000,000 µUSD per million tokens (1 µUSD per token), input ceiling
    // 100 tokens and output ceiling 50 tokens per request.
    f.price(1_000_000).await;
}
fn jsonl(lines: &[(u32, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, (max, model)) in lines.iter().enumerate() {
        out.extend_from_slice(json!({"custom_id":format!("r{i}"),"method":"POST","url":"/v1/chat/completions","body":{"model":model,"messages":[{"role":"user","content":"private prompt"}],"max_completion_tokens":max}}).to_string().as_bytes());
        out.push(b'\n');
    }
    out
}
fn form(file: &[u8]) -> Vec<u8> {
    let mut b = b"--B\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"in.jsonl\"\r\nContent-Type: application/jsonl\r\n\r\n".to_vec();
    b.extend_from_slice(file);
    b.extend_from_slice(b"\r\n--B--\r\n");
    b
}
fn body(
    bytes: Vec<u8>,
) -> impl futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>> + Unpin + Send {
    let chunks: Vec<_> = bytes
        .chunks(37)
        .map(|c| Ok(axum::body::Bytes::copy_from_slice(c)))
        .collect();
    futures_util::stream::iter(chunks)
}
fn upstream_batch(status: BatchStatus, usage: Option<Usage>) -> UpstreamBatch {
    UpstreamBatch {
        id: UpstreamId::parse("batch_up1").unwrap(),
        status,
        output_file: status
            .state()
            .eq(&JobState::Completed)
            .then(|| UpstreamId::parse("file-out1").unwrap()),
        error_file: None,
        counts: Some(RequestCounts {
            total: 3,
            completed: 3,
            failed: 0,
        }),
        usage,
        created_at: None,
        in_progress_at: None,
        finalizing_at: None,
        completed_at: None,
        failed_at: None,
        expired_at: None,
        cancelling_at: None,
        cancelled_at: None,
        expires_at: None,
        metadata: None,
    }
}
fn tokens(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        billing: Some(crate::billing::BillingUsage {
            total_input_tokens: Some(input),
            uncached_input_tokens: Some(input),
            cache_read_input_tokens: Some(0),
            cache_write_input_tokens: Some(0),
            cache_write_default_input_tokens: Some(0),
            cache_write_5m_input_tokens: Some(0),
            cache_write_1h_input_tokens: Some(0),
        }),
        ..Usage::default()
    }
}
async fn upload(j: &Jobs, f: &Fixture, lines: &[(u32, &str)]) -> JobResult<store::FileRow> {
    j.upload_batch_file(
        f.principal,
        Uuid::new_v4(),
        "multipart/form-data; boundary=B",
        body(form(&jsonl(lines))),
    )
    .await
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn batch_upload_streams_validated_lines_and_reserves_the_file_ceiling(pool: PgPool) {
    let f = fixture(pool).await;
    batch_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    let m = "company/smart";
    let file = upload(&j, &f, &[(10, m), (20, m), (30, m)]).await.unwrap();
    assert_eq!(
        (
            file.request_count,
            file.output_token_sum,
            file.max_line_output
        ),
        (Some(3), Some(60), Some(30))
    );
    // The provider received each line with the upstream model, nothing stored.
    let sent = String::from_utf8(fake.0.lock().unwrap().uploaded.clone()).unwrap();
    assert_eq!(sent.lines().count(), 3);
    assert!(
        sent.lines()
            .all(|l| serde_json::from_str::<Value>(l).unwrap()["body"]["model"] == "mock-model")
    );
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM async_job_files WHERE purpose='batch'")
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(stored, 1);
    // Create: 3 × 100 input + 60 output tokens at 1 µUSD/token.
    fake.push_batch(upstream_batch(BatchStatus::Validating, None));
    let request_id = Uuid::new_v4();
    let (job, _) = j
        .create_batch(
            f.principal,
            request_id,
            &client_id(FILE_PREFIX, file.id),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        amounts(&f, request_id).await,
        ("pending".into(), Some(360), None, false)
    );
    let (reserved, count): (i64, i32) = sqlx::query_as(
        "SELECT reserved_tokens,request_count FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(request_id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!((reserved, count), (360, 3));
    assert!(lease(&f, request_id).await > Utc::now() + TimeDelta::hours(25));
    // One batch per file.
    assert!(matches!(
        j.create_batch(
            f.principal,
            Uuid::new_v4(),
            &client_id(FILE_PREFIX, file.id),
            None
        )
        .await,
        Err(JobError::Conflict("file_already_used", _))
    ));
    // Completed with aggregate usage: exact settlement, not a violation even
    // though the aggregate exceeds one request's input ceiling.
    fake.push_batch(upstream_batch(
        BatchStatus::Completed,
        Some(tokens(250, 40)),
    ));
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 1);
    assert_eq!(
        amounts(&f, request_id).await,
        ("settled".into(), Some(360), Some(290), false)
    );
    let (row, _) = j
        .get_batch(&f.principal, &client_id("batch_", job.id))
        .await
        .unwrap();
    assert_eq!(
        (row.state.as_str(), row.request_completed),
        ("completed", Some(3))
    );
    let files = j.batch_files(&row).await.unwrap();
    assert!(files.0.is_some() && files.1.is_some() && files.2.is_none());
    // The output file is a gateway id owned by this workspace only.
    let out = client_id(FILE_PREFIX, files.1.unwrap());
    assert_eq!(
        j.get_file(&f.principal, &out).await.unwrap().purpose,
        "batch_output"
    );
    assert_eq!(
        j.get_file(&f.team, &out).await.err(),
        Some(JobError::NotFound)
    );
    assert_eq!(
        j.get_batch(&f.team, &client_id("batch_", job.id))
            .await
            .err(),
        Some(JobError::NotFound)
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn batch_usage_from_the_output_file_or_unknown(pool: PgPool) {
    let f = fixture(pool).await;
    batch_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    let m = "company/smart";
    let mut ids = Vec::new();
    for _ in 0..2 {
        let file = upload(&j, &f, &[(10, m), (20, m), (30, m)]).await.unwrap();
        fake.push_batch(upstream_batch(BatchStatus::InProgress, None));
        let id = Uuid::new_v4();
        j.create_batch(f.principal, id, &client_id(FILE_PREFIX, file.id), None)
            .await
            .unwrap();
        ids.push(id);
    }
    // No aggregate usage: a client read leaves the output scan to the poller.
    fake.push_batch(upstream_batch(BatchStatus::Completed, None));
    let job: Uuid = sqlx::query_scalar("SELECT id FROM async_jobs WHERE execution_id=$1")
        .bind(ids[0])
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    let (row, _) = j
        .get_batch(&f.principal, &client_id("batch_", job))
        .await
        .unwrap();
    assert!(row.settled_at.is_none());
    assert_eq!(amounts(&f, ids[0]).await.0, "pending");
    // The poller sums per-line usage (3 lines = completed count).
    fake.0.lock().unwrap().output = Some(OutputUsage {
        lines: 3,
        usage: Some(tokens(120, 30)),
    });
    fake.push_batch(upstream_batch(BatchStatus::Completed, None));
    sqlx::query("UPDATE async_jobs SET next_poll_at=now() WHERE execution_id=$1")
        .bind(ids[0])
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 1);
    assert_eq!(
        amounts(&f, ids[0]).await,
        ("settled".into(), Some(360), Some(150), false)
    );
    // A line count that does not match the completed count is unknown.
    fake.0.lock().unwrap().output = Some(OutputUsage {
        lines: 2,
        usage: Some(tokens(80, 20)),
    });
    fake.push_batch(upstream_batch(BatchStatus::Completed, None));
    sqlx::query("UPDATE async_jobs SET next_poll_at=now() WHERE execution_id=$1")
        .bind(ids[1])
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 1);
    assert_eq!(
        amounts(&f, ids[1]).await,
        ("unknown".into(), Some(360), None, false)
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn cancel_and_expiry_keep_holds_with_evidence(pool: PgPool) {
    let f = fixture(pool).await;
    batch_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    let m = "company/smart";
    let file = upload(&j, &f, &[(10, m), (20, m), (30, m)]).await.unwrap();
    fake.push_batch(upstream_batch(BatchStatus::InProgress, None));
    let a = Uuid::new_v4();
    let (job, _) = j
        .create_batch(f.principal, a, &client_id(FILE_PREFIX, file.id), None)
        .await
        .unwrap();
    let id = client_id("batch_", job.id);
    assert_eq!(
        j.cancel_batch(&f.team, &id).await.err(),
        Some(JobError::NotFound)
    );
    fake.push_batch(upstream_batch(BatchStatus::Cancelling, None));
    let (row, _) = j.cancel_batch(&f.principal, &id).await.unwrap();
    assert_eq!(
        (row.state.as_str(), row.upstream_status.as_deref()),
        ("in_progress", Some("cancelling"))
    );
    assert!(row.cancel_requested_at.is_some());
    // Cancelled with partial usage: evidence recorded, hold retained.
    fake.push_batch(upstream_batch(
        BatchStatus::Cancelled,
        Some(tokens(100, 10)),
    ));
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 1);
    assert_eq!(
        amounts(&f, a).await,
        ("unknown".into(), Some(360), None, false)
    );
    let (state, input): (String, Option<i64>) =
        sqlx::query_as("SELECT state,input_tokens FROM inference_executions WHERE id=$1")
            .bind(a)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!((state.as_str(), input), ("cancelled", Some(100)));
    assert!(matches!(
        j.cancel_batch(&f.principal, &id).await,
        Err(JobError::Conflict("batch_not_cancellable", _))
    ));
    // Expiry: the lease lapses before the provider finishes; reconciliation
    // marks the reservation unknown and a later completion leaves it alone.
    let file = upload(&j, &f, &[(10, m)]).await.unwrap();
    fake.push_batch(upstream_batch(BatchStatus::InProgress, None));
    let b = Uuid::new_v4();
    j.create_batch(f.principal, b, &client_id(FILE_PREFIX, file.id), None)
        .await
        .unwrap();
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=now()-interval '1 second' WHERE execution_id=$1").bind(b).execute(&f.store.pool).await.unwrap();
    assert_eq!(
        crate::governance::reconcile_expired(&f.store, 10)
            .await
            .unwrap(),
        1
    );
    assert_eq!(amounts(&f, b).await.0, "unknown");
    fake.push_batch(upstream_batch(BatchStatus::Completed, Some(tokens(50, 5))));
    sqlx::query("UPDATE async_jobs SET next_poll_at=now() WHERE execution_id=$1")
        .bind(b)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 1);
    assert_eq!(
        amounts(&f, b).await,
        ("unknown".into(), Some(110), None, false)
    );
    let settled: bool =
        sqlx::query_scalar("SELECT settled_at IS NOT NULL FROM async_jobs WHERE execution_id=$1")
            .bind(b)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert!(settled);
    // Provider-expired batches keep their hold too.
    let file = upload(&j, &f, &[(10, m)]).await.unwrap();
    fake.push_batch(upstream_batch(BatchStatus::Expired, Some(tokens(0, 0))));
    let c = Uuid::new_v4();
    let (row, _) = j
        .create_batch(f.principal, c, &client_id(FILE_PREFIX, file.id), None)
        .await
        .unwrap();
    assert_eq!(row.state, "expired");
    assert_eq!(amounts(&f, c).await.0, "unknown");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn uploads_reject_unpriced_unbounded_and_invalid_files(pool: PgPool) {
    let f = fixture(pool).await;
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['batches'] WHERE id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    let m = "company/smart";
    // Unpriced: refused before the provider receives a complete file.
    assert_eq!(
        upload(&j, &f, &[(10, m)]).await.err(),
        Some(JobError::Inference(InferenceError::Configuration))
    );
    assert!(fake.0.lock().unwrap().upload_aborted);
    f.price(1_000_000).await;
    // A line above the price's output ceiling (50) cannot be reserved.
    assert_eq!(
        upload(&j, &f, &[(10, m), (51, m)]).await.err(),
        Some(JobError::Inference(InferenceError::Configuration))
    );
    // Mixed models, unbounded lines and unknown models.
    assert_eq!(
        upload(&j, &f, &[(10, m), (10, "company/other")])
            .await
            .err(),
        Some(JobError::Inference(InferenceError::InvalidRequest))
    );
    assert_eq!(
        upload(&j, &f, &[(0, m)]).await.err(),
        Some(JobError::Inference(InferenceError::InvalidRequest))
    );
    assert_eq!(
        upload(&j, &f, &[(10, "company/missing")]).await.err(),
        Some(JobError::Inference(InferenceError::ModelUnavailable))
    );
    // Wrong purpose / order.
    let r = j
        .upload_batch_file(
            f.principal,
            Uuid::new_v4(),
            "multipart/form-data; boundary=B",
            body(form(&jsonl(&[(10, m)])).replace_purpose()),
        )
        .await;
    assert_eq!(
        r.err(),
        Some(JobError::Inference(InferenceError::Unsupported))
    );
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM async_job_files")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(stored, 0);
    // A model without the batches protocol is unsupported.
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions'] WHERE id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        upload(&j, &f, &[(10, m)]).await.err(),
        Some(JobError::Inference(InferenceError::Unsupported))
    );
}

trait ReplacePurpose {
    fn replace_purpose(self) -> Vec<u8>;
}
impl ReplacePurpose for Vec<u8> {
    fn replace_purpose(self) -> Vec<u8> {
        String::from_utf8(self)
            .unwrap()
            .replacen("\r\n\r\nbatch\r\n", "\r\n\r\nfine-tune\r\n", 1)
            .into_bytes()
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn batch_budget_denial_releases_the_file(pool: PgPool) {
    let f = fixture(pool).await;
    batch_setup(&f).await;
    let fake = Fake::default();
    let j = jobs(&f, &fake, true);
    let m = "company/smart";
    let file = upload(&j, &f, &[(10, m), (20, m), (30, m)]).await.unwrap();
    crate::governance::set_test_budget(
        &f.store.pool,
        "local",
        None,
        Some(f.principal.workspace_id),
        None,
        "month",
        Some(359),
    )
    .await;
    let id = client_id(FILE_PREFIX, file.id);
    assert!(matches!(
        j.create_batch(f.principal, Uuid::new_v4(), &id, None).await,
        Err(JobError::Inference(InferenceError::BudgetExceeded(_)))
    ));
    // Nothing was sent upstream; the file can be used once the budget allows.
    crate::governance::set_test_budget(
        &f.store.pool,
        "local",
        None,
        Some(f.principal.workspace_id),
        None,
        "month",
        Some(360),
    )
    .await;
    fake.push_batch(upstream_batch(BatchStatus::Validating, None));
    let request_id = Uuid::new_v4();
    j.create_batch(f.principal, request_id, &id, None)
        .await
        .unwrap();
    assert_eq!(amounts(&f, request_id).await.1, Some(360));
    assert_eq!(
        fake.calls()
            .iter()
            .filter(|c| **c == "create_batch")
            .count(),
        1
    );
    // Budget totals (0015) saw exactly the job's hold.
    let held: String = sqlx::query_scalar("SELECT held_microusd::text FROM budget_totals WHERE scope_kind='workspace' AND scope_id=$1 AND period='month'").bind(f.principal.workspace_id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(held, "360");
    // Lease extension, settlement and a cancelled attempt keep totals exact.
    fake.push_batch(upstream_batch(
        BatchStatus::Completed,
        Some(tokens(250, 40)),
    ));
    sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(j.poll_once().await.unwrap(), 1);
    let report = crate::governance::totals::verify(&f.store).await.unwrap();
    assert!(report.consistent(), "{report:?}");
    let settled: String = sqlx::query_scalar("SELECT (settled_microusd+held_microusd)::text FROM budget_totals WHERE scope_kind='workspace' AND scope_id=$1 AND period='month'").bind(f.principal.workspace_id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(settled, "290");
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
    let (status, body) = send(&app, "POST", "/v1/batches", &mine, Some(json!({"input_file_id":"file-00000000000000000000000000000000","endpoint":"/v1/chat/completions","completion_window":"24h"}))).await;
    assert_eq!(
        (status.as_u16(), body["error"]["code"].as_str()),
        (404, Some("not_found"))
    );
    let (status, _) = send(
        &app,
        "POST",
        "/v1/batches",
        &mine,
        Some(
            json!({"input_file_id":"file-x","endpoint":"/v1/embeddings","completion_window":"24h"}),
        ),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(fake.calls(), ["create_video"]);
}

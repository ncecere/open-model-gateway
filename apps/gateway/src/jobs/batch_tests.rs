//! The batch engine (0021) on a disposable database with fake providers (no
//! network, no paid calls): native OpenAI-style and Anthropic-style flows,
//! gateway-run batches across providers, validation reports, budget
//! exhaustion mid-batch, cancel, restart resume, exactly-once lines under
//! concurrency, batch vs standard vs missing batch prices, privacy and the
//! batch alert kinds.
use std::sync::Mutex;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::{batch::CreateBatch, batch::CreateError, runner::Runner, *};
use crate::{
    billing::BillingUsage,
    filestore::{FileStoreRuntime, NewFile, Purpose, QuotaMode, files::public_id},
    governance::tests::db::{Fixture, fixture},
    inference::{
        EngineLimits,
        types::{
            Capabilities, ChatRequest, ChatResponse, EmbeddingRequest, EmbeddingResponse,
            FinishReason, ProviderOutput,
        },
    },
};

#[derive(Default)]
struct Script {
    /// Chat executions (gateway-run lines), by first message text.
    executed: Vec<String>,
    /// Encoded native records of the last submission.
    submitted: Vec<Value>,
    batches: std::collections::VecDeque<UpstreamBatch>,
    /// Result lines a native batch returns (fake format).
    results: Vec<Value>,
    calls: Vec<&'static str>,
}
/// A fake provider. `native` enables its batch API. Prompts steer gateway
/// runs: `fail` → upstream unavailable, `busy` → 429, `huge` → usage far
/// above the line's bound.
#[derive(Clone)]
struct Fake {
    id: &'static str,
    native: bool,
    script: Arc<Mutex<Script>>,
}
impl Fake {
    fn new(id: &'static str, native: bool) -> Self {
        Self {
            id,
            native,
            script: Arc::default(),
        }
    }
    fn executed(&self) -> Vec<String> {
        self.script.lock().unwrap().executed.clone()
    }
    fn calls(&self) -> Vec<&'static str> {
        self.script.lock().unwrap().calls.clone()
    }
    fn push_batch(&self, status: BatchStatus) {
        let mut s = self.script.lock().unwrap();
        s.batches.push_back(upstream(status));
    }
}
fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        billing: Some(BillingUsage {
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
fn upstream(status: BatchStatus) -> UpstreamBatch {
    UpstreamBatch {
        id: UpstreamId::parse("batch_native1").unwrap(),
        status,
        input_file: UpstreamId::parse("file-native-in"),
        output_file: None,
        error_file: None,
        counts: None,
        usage: None,
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
#[async_trait]
impl ProviderAdapter for Fake {
    fn id(&self) -> &'static str {
        self.id
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: false,
            tools: false,
        }
    }
    fn supports_protocol(&self, p: ApiProtocol) -> bool {
        match self.id {
            "anthropic" => matches!(p, ApiProtocol::ChatCompletions | ApiProtocol::Messages),
            _ => matches!(
                p,
                ApiProtocol::ChatCompletions
                    | ApiProtocol::Responses
                    | ApiProtocol::Embeddings
                    | ApiProtocol::Batches
            ),
        }
    }
    async fn execute(
        &self,
        _: &Deployment,
        request: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        let text = request.messages[0].content.clone().unwrap_or_default();
        self.script.lock().unwrap().executed.push(text.clone());
        match text.as_str() {
            "fail" => Err(InferenceError::UpstreamUnavailable),
            "busy" => Err(InferenceError::Busy),
            "slow" => {
                tokio::time::sleep(Duration::from_millis(300)).await;
                Ok(ProviderOutput::Complete(ChatResponse {
                    content: Some("ok".into()),
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                    usage: usage(10, 5),
                }))
            }
            "huge" => Ok(ProviderOutput::Complete(ChatResponse {
                content: Some("ok".into()),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: usage(5000, 5),
            })),
            _ => Ok(ProviderOutput::Complete(ChatResponse {
                content: Some(format!("answer to {text}")),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: usage(10, 5),
            })),
        }
    }
    async fn execute_protocol(
        &self,
        target: &Deployment,
        request: ChatRequest,
        _: ApiProtocol,
    ) -> Result<ProviderOutput, InferenceError> {
        self.execute(target, request).await
    }
    async fn execute_embeddings(
        &self,
        _: &Deployment,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, InferenceError> {
        Ok(EmbeddingResponse {
            embeddings: request.input.iter().map(|_| vec![0.5, 0.25]).collect(),
            usage: Usage {
                input_tokens: Some(4),
                output_tokens: Some(0),
                ..Usage::default()
            },
        })
    }
    fn native_batch(&self, _: &Deployment, endpoint: BatchEndpoint) -> bool {
        self.native && (self.id != "anthropic" || endpoint.is_generation())
    }
    fn encode_native_line(
        &self,
        target: &Deployment,
        _: BatchEndpoint,
        custom_id: &str,
        request: &BatchRequest,
    ) -> Result<Vec<u8>, InferenceError> {
        let text = match request {
            BatchRequest::Chat(r) => r.messages[0].content.clone().unwrap_or_default(),
            BatchRequest::Embeddings(r) => r.input.join(","),
        };
        if text == "unencodable" {
            return Err(InferenceError::Unsupported);
        }
        Ok(serde_json::to_vec(
            &json!({"custom_id":custom_id,"model":target.upstream_model,"text":text}),
        )
        .unwrap())
    }
    async fn submit_native_batch(
        &self,
        _: &Deployment,
        _: BatchEndpoint,
        mut records: ByteStream,
    ) -> Result<UpstreamBatch, InferenceError> {
        let mut seen = Vec::new();
        while let Some(r) = records.next().await {
            seen.push(serde_json::from_slice::<Value>(&r?).unwrap());
        }
        let mut s = self.script.lock().unwrap();
        s.calls.push("submit");
        s.submitted = seen;
        Ok(upstream(BatchStatus::Validating))
    }
    async fn retrieve_batch(
        &self,
        _: &Deployment,
        _: &UpstreamId,
    ) -> Result<UpstreamBatch, InferenceError> {
        let mut s = self.script.lock().unwrap();
        s.calls.push("retrieve");
        s.batches
            .pop_front()
            .ok_or(InferenceError::UpstreamUnavailable)
    }
    async fn cancel_batch(
        &self,
        _: &Deployment,
        _: &UpstreamId,
    ) -> Result<UpstreamBatch, InferenceError> {
        self.script.lock().unwrap().calls.push("cancel");
        Ok(upstream(BatchStatus::Cancelling))
    }
    async fn native_batch_results(
        &self,
        _: &Deployment,
        _: &UpstreamBatch,
    ) -> Result<ByteStream, InferenceError> {
        let lines: Vec<Result<axum::body::Bytes, InferenceError>> = self
            .script
            .lock()
            .unwrap()
            .results
            .iter()
            .map(|v| {
                let mut b = serde_json::to_vec(v).unwrap();
                b.push(b'\n');
                Ok(axum::body::Bytes::from(b))
            })
            .collect();
        Ok(Box::pin(futures_util::stream::iter(lines)))
    }
    fn decode_native_result(
        &self,
        _: &Deployment,
        _: BatchEndpoint,
        line: &[u8],
    ) -> Result<NativeResult, InferenceError> {
        let v: Value = serde_json::from_slice(line).map_err(|_| InferenceError::InvalidUpstream)?;
        let custom_id = v["custom_id"].as_str().unwrap().to_owned();
        let outcome = if v["ok"] == json!(true) {
            NativeOutcome::Succeeded(Box::new(BatchResponse::Chat(ChatResponse {
                content: Some("native answer".into()),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: usage(v["input"].as_u64().unwrap(), v["output"].as_u64().unwrap()),
            })))
        } else {
            NativeOutcome::Failed {
                status: 400,
                code: ErrorCode::parse("invalid_request_error"),
            }
        };
        Ok(NativeResult { custom_id, outcome })
    }
    async fn delete_native_batch(
        &self,
        _: &Deployment,
        _: &UpstreamBatch,
    ) -> Result<(), InferenceError> {
        self.script.lock().unwrap().calls.push("delete");
        Ok(())
    }
}

/// Engine, file store and the fakes, wired like `serve`.
struct World {
    f: Fixture,
    jobs: Jobs,
    files: crate::filestore::FileStorage,
    openai: Fake,
    anthropic: Fake,
    /// A second model on the Anthropic connection.
    claude: Uuid,
}
async fn world(
    pool: PgPool,
    openai_native: bool,
    anthropic_native: bool,
    concurrency: usize,
) -> World {
    let f = fixture(pool).await;
    sqlx::query("UPDATE installation_settings SET file_batch_enabled=true WHERE singleton")
        .execute(&f.store.pool)
        .await
        .unwrap();
    // A Claude model on an Anthropic connection, granted to both workspaces.
    let (claude, connection, deployment) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,'company/claude',ARRAY['chat_completions','messages'])").bind(claude).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Claude','anthropic','env:NOT_RESOLVED',true)").bind(connection).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'claude-mock',true)").bind(deployment).bind(claude).bind(connection).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) SELECT id,$1,'direct' FROM workspaces").bind(claude).execute(&f.store.pool).await.unwrap();
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['chat_completions','responses'] WHERE id=$1",
    )
    .bind(f.model)
    .execute(&f.store.pool)
    .await
    .unwrap();
    // v1: 1 µUSD per token, ceilings 100 in / 50 out.
    f.price(1_000_000).await;
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,2000000,2000000,100,50,1)").bind(Uuid::new_v4()).bind(deployment).execute(&f.store.pool).await.unwrap();
    let openai = Fake::new("openai", openai_native);
    let anthropic = Fake::new("anthropic", anthropic_native);
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(openai.clone())).unwrap();
    registry.register(Arc::new(anthropic.clone())).unwrap();
    let engine = Engine::new(
        Arc::new(f.store.clone()),
        registry.clone(),
        EngineLimits::default(),
    )
    .unwrap();
    let runtime = FileStoreRuntime::memory();
    let jobs = Jobs::with(
        f.store.clone(),
        registry,
        JobLimits {
            batch_concurrency: concurrency,
            ..JobLimits::default()
        },
    )
    .with_files(Some(runtime.clone()))
    .with_engine(engine);
    let files = crate::filestore::FileStorage::new(f.store.clone(), runtime);
    World {
        f,
        jobs,
        files,
        openai,
        anthropic,
        claude,
    }
}
fn chat_line(id: &str, model: &str, text: &str) -> Value {
    json!({"custom_id":id,"method":"POST","url":"/v1/chat/completions","body":{"model":model,"messages":[{"role":"user","content":text}],"max_completion_tokens":10}})
}
impl World {
    async fn upload(&self, principal: &crate::auth::Principal, lines: &[Value]) -> String {
        let mut body = Vec::new();
        for l in lines {
            body.extend_from_slice(&serde_json::to_vec(l).unwrap());
            body.push(b'\n');
        }
        self.upload_raw(principal, body).await
    }
    async fn upload_raw(&self, principal: &crate::auth::Principal, body: Vec<u8>) -> String {
        let stream: crate::filestore::ByteStream =
            Box::pin(futures_util::stream::once(async move {
                Ok(axum::body::Bytes::from(body))
            }));
        let stored = self
            .files
            .create(
                NewFile {
                    created_by_api_key_id: Some(principal.key_id),
                    quota: QuotaMode::CountOnly,
                    ..NewFile::new(Purpose::BatchInput, Some(principal.workspace_id))
                },
                stream,
            )
            .await
            .unwrap();
        public_id(stored.id)
    }
    async fn create(
        &self,
        principal: crate::auth::Principal,
        file: &str,
        endpoint: BatchEndpoint,
        metadata: Option<Value>,
    ) -> Result<JobRow, CreateError> {
        self.jobs
            .create_batch(
                principal,
                Uuid::new_v4(),
                CreateBatch {
                    input_file_id: file.into(),
                    endpoint,
                    metadata: metadata.and_then(|m| m.as_object().cloned()),
                    completion_window_hours: None,
                },
            )
            .await
    }
    async fn read(&self, workspace: Uuid, file: Option<Uuid>) -> Vec<Value> {
        let Some(file) = file else {
            return vec![];
        };
        let (_, mut stream) = self.files.open(file, Some(workspace)).await.unwrap();
        let mut all = Vec::new();
        while let Some(c) = stream.next().await {
            all.extend_from_slice(&c.unwrap());
        }
        all.split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect()
    }
    async fn reload(&self, job: &JobRow) -> JobRow {
        store::reload(&self.f.store, job.id).await.unwrap()
    }
    async fn poll(&self) {
        sqlx::query("UPDATE async_jobs SET next_poll_at=now()")
            .execute(&self.f.store.pool)
            .await
            .unwrap();
        self.jobs.poll_once().await.unwrap();
    }
}
async fn reservation(f: &Fixture, execution: Uuid) -> (String, Option<i64>, Option<i64>, String) {
    sqlx::query_as("SELECT state,held_microusd,actual_microusd,price_tier FROM governance_reservations WHERE execution_id=$1")
        .bind(execution)
        .fetch_one(&f.store.pool)
        .await
        .unwrap()
}
async fn consistent(f: &Fixture) {
    let report = crate::governance::totals::verify(&f.store).await.unwrap();
    assert!(report.consistent(), "{report:?}");
}
async fn file_deleted(f: &Fixture, id: Option<Uuid>) -> bool {
    sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM stored_files WHERE id=$1")
        .bind(id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn gateway_run_mixed_providers_end_to_end(pool: PgPool) {
    let w = world(pool, true, false, 2).await;
    let f = &w.f;
    // Interactive limits the batch's lines must not consume.
    f.policy("workspace_local_policies", Some(1), Some(10), Some(1), None)
        .await;
    let file = w
        .upload(
            &f.principal,
            &[
                chat_line("a", "company/smart", "one"),
                chat_line("b", "company/claude", "two"),
                chat_line("c", "company/smart", "three"),
            ],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Gateway));
    assert_eq!(
        (job.public_model.as_str(), job.provider.as_str()),
        ("mixed", "mixed")
    );
    assert_eq!(job.request_total, Some(3));
    // The ceiling is the sum of the lines' own bounds: 2 × 110 + 1 × 220.
    assert_eq!(
        reservation(f, job.execution_id).await,
        ("pending".into(), Some(440), None, "standard".into())
    );
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert!(job.settled_at.is_some());
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(3), Some(0))
    );
    let out = w.read(f.principal.workspace_id, job.output_file_id).await;
    let mut ids: Vec<&str> = out
        .iter()
        .map(|l| l["custom_id"].as_str().unwrap())
        .collect();
    ids.sort();
    assert_eq!(ids, ["a", "b", "c"]);
    for line in &out {
        assert_eq!(line["response"]["status_code"], 200);
        let body = &line["response"]["body"];
        assert_eq!(body["object"], "chat.completion");
        // The public model name, never the upstream model.
        assert!(matches!(
            body["model"].as_str(),
            Some("company/smart" | "company/claude")
        ));
    }
    assert!(job.error_file_id.is_none());
    // Each line was its own attempt with its own settled reservation.
    let lines: Vec<(Uuid, String, Option<i64>)> = sqlx::query_as("SELECT e.id,r.state,r.actual_microusd FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.batch_job_id=$1 ORDER BY e.started_at")
        .bind(job.id)
        .fetch_all(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(lines.len(), 3);
    assert!(lines.iter().all(|l| l.1 == "settled"));
    let total: i64 = lines.iter().map(|l| l.2.unwrap()).sum();
    assert_eq!(total, 15 + 15 + 30, "standard prices (1 and 2 µUSD/token)");
    // The envelope released what the lines did not use.
    assert_eq!(
        reservation(f, job.execution_id).await,
        (
            "settled".into(),
            Some(440 - 110 - 110 - 220),
            Some(0),
            "standard".into()
        )
    );
    assert_eq!(w.openai.executed().len(), 2);
    assert_eq!(w.anthropic.executed(), ["two"]);
    assert!(file_deleted(f, job.work_file_id).await);
    let segments: i64 = sqlx::query_scalar("SELECT count(*) FROM batch_segments s JOIN stored_files f ON f.id=s.file_id WHERE s.job_id=$1 AND f.deleted_at IS NULL")
        .bind(job.id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(segments, 0, "segments are removed after the merge");
    // The API object.
    let v = w
        .jobs
        .get_batch(&f.principal, &client_id("batch_", job.id))
        .await
        .unwrap();
    assert_eq!(v["status"], "completed");
    assert_eq!(v["output_file_id"], public_id(job.output_file_id.unwrap()));
    assert_eq!(
        v["request_counts"],
        json!({"total":3,"completed":3,"failed":0})
    );
    // No prompt or answer text reached PostgreSQL.
    let leaked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM async_jobs j WHERE j::text LIKE '%answer to%' OR j::text LIKE '%three%') OR EXISTS(SELECT 1 FROM batch_lines l WHERE l::text LIKE '%three%')")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert!(!leaked);
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn validation_rejects_with_line_numbers_and_creates_nothing(pool: PgPool) {
    let w = world(pool, false, false, 2).await;
    let f = &w.f;
    let mut body = Vec::new();
    for raw in [
        serde_json::to_string(&chat_line("a", "company/smart", "ok")).unwrap(),
        "{not json".into(),
        serde_json::to_string(&chat_line("a", "company/smart", "dup")).unwrap(),
        "   ".into(),
        serde_json::to_string(&chat_line("c", "company/unknown", "x")).unwrap(),
        r#"{"custom_id":"d","method":"POST","url":"/v1/chat/completions","body":{"model":"company/smart","messages":[{"role":"user","content":"x"}]}}"#.into(),
        r#"{"custom_id":"e","method":"POST","url":"/v1/embeddings","body":{"model":"company/smart","input":"x"}}"#.into(),
        r#"{"custom_id":"f","method":"POST","url":"/v1/chat/completions","body":{"model":"company/smart","messages":[{"role":"user","content":"x"}],"max_completion_tokens":51}}"#.into(),
    ] {
        body.extend_from_slice(raw.as_bytes());
        body.push(b'\n');
    }
    let file = w.upload_raw(&f.principal, body).await;
    let Err(CreateError::Invalid(issues)) = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
    else {
        panic!("expected a validation report");
    };
    let got: Vec<(u64, &str)> = issues.iter().map(|i| (i.line, i.error.code())).collect();
    assert_eq!(
        got,
        [
            (2, "invalid_json"),
            (3, "duplicate_custom_id"),
            (5, "model_not_found"),
            (6, "missing_max_tokens"),
            (7, "invalid_url"),
            (8, "max_tokens_too_large"),
        ]
    );
    // Messages are fixed strings: no line content is echoed.
    assert!(issues.iter().all(|i| !i.error.message().contains("dup")));
    let rows: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM async_jobs),(SELECT count(*) FROM governance_reservations),(SELECT count(*) FROM stored_files WHERE purpose='batch_output' AND api_purpose IS NULL AND deleted_at IS NULL)")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(rows, (0, 0, 0));
    // An empty file and an unpriced model are refused too.
    let empty = w.upload_raw(&f.principal, b"\n\n".to_vec()).await;
    assert!(matches!(
        w.create(f.principal, &empty, BatchEndpoint::ChatCompletions, None).await,
        Err(CreateError::Invalid(i)) if i[0].error.code() == "empty_file"
    ));
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions','responses','embeddings'] WHERE id=$1").bind(f.model).execute(&f.store.pool).await.ok();
    // Another workspace's file is not found; input_file_id must be a batch file.
    let team_file = w
        .upload(&f.team, &[chat_line("a", "company/smart", "x")])
        .await;
    assert!(matches!(
        w.create(
            f.principal,
            &team_file,
            BatchEndpoint::ChatCompletions,
            None
        )
        .await,
        Err(CreateError::Job(JobError::NotFound))
    ));
    // Bad options are refused before reading the file.
    assert!(matches!(
        w.create(
            f.principal,
            &file,
            BatchEndpoint::ChatCompletions,
            Some(json!({"omg_mode":"turbo"}))
        )
        .await,
        Err(CreateError::Job(JobError::Invalid("invalid_metadata", _)))
    ));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn budget_exhaustion_mid_batch_stops_and_keeps_partial_results(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    // Room for the 3-line ceiling (330), not for a line that overspends.
    crate::governance::set_test_budget(
        &f.store.pool,
        "local",
        None,
        Some(f.principal.workspace_id),
        None,
        "month",
        Some(400),
    )
    .await;
    let file = w
        .upload(
            &f.principal,
            &[
                chat_line("a", "company/smart", "huge"),
                chat_line("b", "company/smart", "two"),
                chat_line("c", "company/smart", "three"),
            ],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    Runner::new(w.jobs.clone()).run_next().await.unwrap();
    let job = w.reload(&job).await;
    assert_eq!(job.state, "failed");
    assert_eq!(job.error_code.as_deref(), Some("budget_exceeded"));
    // Only the first line ran; its result is kept.
    assert_eq!(w.openai.executed(), ["huge"]);
    let out = w.read(f.principal.workspace_id, job.output_file_id).await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["custom_id"], "a");
    let errors = w.read(f.principal.workspace_id, job.error_file_id).await;
    let mut codes: Vec<(String, String)> = errors
        .iter()
        .map(|e| {
            (
                e["custom_id"].as_str().unwrap().to_owned(),
                e["error"]["code"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    codes.sort();
    assert_eq!(
        codes,
        [
            ("b".into(), "budget_exceeded".into()),
            ("c".into(), "budget_exceeded".into())
        ]
    );
    // The overspend settled exactly; the envelope's remainder was released.
    let (state, _, actual, _) = reservation(f, job.execution_id).await;
    assert_eq!((state.as_str(), actual), ("settled", Some(0)));
    let spent: i64 = sqlx::query_scalar("SELECT coalesce(sum(r.actual_microusd),0)::bigint FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE e.batch_job_id=$1")
        .bind(job.id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(spent, 5005);
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn cancel_failures_and_explicit_retries(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    // Cancel before the runner starts: nothing runs, everything is reported.
    let file = w
        .upload(
            &f.principal,
            &[
                chat_line("a", "company/smart", "x"),
                chat_line("b", "company/smart", "y"),
            ],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    let v = w
        .jobs
        .cancel_batch(&f.principal, &client_id("batch_", job.id))
        .await
        .unwrap();
    assert_eq!(v["status"], "cancelling");
    // The job slot is released as soon as cancel is requested.
    let slots: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations r JOIN async_jobs j ON j.execution_id=r.execution_id WHERE r.state='pending' AND j.cancel_requested_at IS NULL")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(slots, 0);
    Runner::new(w.jobs.clone()).run_next().await.unwrap();
    let job = w.reload(&job).await;
    assert_eq!(job.state, "cancelled");
    assert!(w.openai.executed().is_empty());
    let errors = w.read(f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errors.len(), 2);
    assert!(
        errors
            .iter()
            .all(|e| e["error"]["code"] == "batch_cancelled")
    );
    assert!(matches!(
        w.jobs
            .cancel_batch(&f.principal, &client_id("batch_", job.id))
            .await,
        Err(JobError::Conflict("batch_not_cancellable", _))
    ));
    // A failed line is recorded failed (no implicit retry)…
    let file = w
        .upload(&f.principal, &[chat_line("a", "company/smart", "fail")])
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    Runner::new(w.jobs.clone()).run_next().await.unwrap();
    let job = w.reload(&job).await;
    assert_eq!(
        (job.state.as_str(), job.request_failed),
        ("completed", Some(1))
    );
    let errors = w.read(f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errors[0]["response"]["status_code"], 502);
    assert_eq!(w.openai.executed(), ["fail"]);
    // …unless the batch opts into retries: each retry is its own attempt.
    let file = w
        .upload(&f.principal, &[chat_line("a", "company/smart", "fail")])
        .await;
    let job = w
        .create(
            f.principal,
            &file,
            BatchEndpoint::ChatCompletions,
            Some(json!({"omg_retries":"1"})),
        )
        .await
        .unwrap();
    Runner::new(w.jobs.clone()).run_next().await.unwrap();
    assert_eq!(w.openai.executed(), ["fail", "fail", "fail"]);
    let attempts: Vec<(i16, String)> =
        sqlx::query_as("SELECT attempts,state FROM batch_lines WHERE job_id=$1")
            .bind(job.id)
            .fetch_all(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(attempts, [(2, "failed".to_owned())]);
    let executions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM inference_executions WHERE batch_job_id=$1")
            .bind(job.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(executions, 2);
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn restart_resumes_without_re_executing_lines(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    let file = w
        .upload(
            &f.principal,
            &[
                chat_line("a", "company/smart", "zero"),
                chat_line("b", "company/smart", "one"),
                chat_line("c", "company/smart", "two"),
            ],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    // A crashed runner left line 0 running and line 1 finished without a
    // stored result, and its lease expired.
    sqlx::query("INSERT INTO batch_lines(job_id,workspace_id,line_no,state,execution_id) VALUES($1,$2,0,'running',$3)")
        .bind(job.id).bind(job.workspace_id).bind(Uuid::new_v4()).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO batch_lines(job_id,workspace_id,line_no,state,execution_id,status_code,finished_at) VALUES($1,$2,1,'succeeded',$3,200,now())")
        .bind(job.id).bind(job.workspace_id).bind(Uuid::new_v4()).execute(&f.store.pool).await.unwrap();
    sqlx::query("UPDATE async_jobs SET runner_id=$2,runner_lease_until=now()-interval '1 second',state='in_progress' WHERE id=$1")
        .bind(job.id).bind(Uuid::new_v4()).execute(&f.store.pool).await.unwrap();
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    // Only line 2 ran.
    assert_eq!(w.openai.executed(), ["two"]);
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(2), Some(1))
    );
    let out = w.read(f.principal.workspace_id, job.output_file_id).await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["custom_id"], "c");
    let errors = w.read(f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errors.len(), 2);
    assert!(
        errors
            .iter()
            .all(|e| e["error"]["code"] == "result_unavailable")
    );
    let interrupted: String =
        sqlx::query_scalar("SELECT state FROM batch_lines WHERE job_id=$1 AND line_no=0")
            .bind(job.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(interrupted, "interrupted");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn lines_never_run_twice_under_concurrency(pool: PgPool) {
    let w = world(pool, false, false, 2).await;
    let f = &w.f;
    let mut batches = Vec::new();
    for n in 0..3 {
        let lines: Vec<Value> = (0..4)
            .map(|i| chat_line(&format!("{n}-{i}"), "company/smart", "slow"))
            .collect();
        let file = w.upload(&f.principal, &lines).await;
        // Room for every batch at once.
        sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id,concurrent_jobs) VALUES($1,10) ON CONFLICT(workspace_id) DO UPDATE SET concurrent_jobs=10")
            .bind(f.principal.workspace_id).execute(&f.store.pool).await.unwrap();
        batches.push(
            w.create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
                .await
                .unwrap(),
        );
    }
    // Two runners (two processes) compete for the same batches.
    let (a, b) = (Runner::new(w.jobs.clone()), Runner::new(w.jobs.clone()));
    let work = |r: Runner| async move { while r.run_next().await.unwrap() {} };
    tokio::join!(work(a), work(b));
    assert_eq!(w.openai.executed().len(), 12, "every line exactly once");
    for job in &batches {
        let job = w.reload(job).await;
        assert_eq!(
            (job.state.as_str(), job.request_completed),
            ("completed", Some(4))
        );
    }
    // The claim itself is exclusive: one of many concurrent claims wins.
    let job = &batches[0];
    let claims = futures_util::future::join_all(
        (0..8).map(|_| store::claim_line(&f.store, job.id, job.workspace_id, 999, Uuid::new_v4())),
    )
    .await;
    assert_eq!(claims.iter().filter(|c| matches!(c, Ok(true))).count(), 1);
    consistent(f).await;
}

/// v3 prices: standard 1 µUSD/token in and out; batch list 0.4 µUSD in, 0.5
/// µUSD out (an explicit published list, never derived).
async fn v3_price(f: &Fixture, deployment: Uuid, batch: bool) {
    let line = |meter: &str, amount: &str, label: &str| json!({"meter":meter,"microusd_per_batch":amount,"batch":1000000,"unit_label":"/M tokens","sku_label":label});
    let na = |meter: &str| json!({"meter":meter,"not_applicable":true});
    let mut standard = vec![
        line("input_tokens", "1000000", "In"),
        line("output_tokens", "1000000", "Out"),
    ];
    let mut batched = vec![
        line("input_tokens", "400000", "In"),
        line("output_tokens", "500000", "Out"),
    ];
    for m in [
        "cache_read_tokens",
        "cache_write_tokens",
        "cache_write_5m_tokens",
        "cache_write_1h_tokens",
        "output_images",
        "input_characters",
        "input_audio_seconds_ms",
        "output_audio_seconds_ms",
        "search_units",
        "requests",
    ] {
        standard.push(na(m));
        batched.push(na(m));
    }
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,batch_price_lines,max_units) VALUES($1,$2,100,50,3,$3,$4,'{}')")
        .bind(Uuid::new_v4()).bind(deployment).bind(json!(standard)).bind(batch.then(|| json!(batched)))
        .execute(&f.store.pool).await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn native_openai_batch_charges_published_batch_prices(pool: PgPool) {
    let w = world(pool, true, false, 2).await;
    let f = &w.f;
    v3_price(f, f.deployment, true).await;
    let file = w
        .upload(
            &f.principal,
            &[
                chat_line("first", "company/smart", "one"),
                chat_line("second", "company/smart", "two"),
            ],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Native));
    assert_eq!(job.price_tier.as_deref(), Some("batch"));
    // Hold at the batch list: 2 × (100 × 0.4 + 10 × 0.5) = 90.
    assert_eq!(
        reservation(f, job.execution_id).await,
        ("pending".into(), Some(90), None, "batch".into())
    );
    // Poll 1: submit (gateway ids, upstream model; never client ids).
    w.poll().await;
    let submitted = w.openai.script.lock().unwrap().submitted.clone();
    assert_eq!(
        submitted,
        [
            json!({"custom_id":"l0","model":"mock-model","text":"one"}),
            json!({"custom_id":"l1","model":"mock-model","text":"two"})
        ]
    );
    let job = w.reload(&job).await;
    assert_eq!(job.upstream_id.as_deref(), Some("batch_native1"));
    // Poll 2: OpenAI reports `total: 0` while validating; the gateway's own
    // line count is kept (seen live: the total dropped from 2 to 0).
    let mut validating = upstream(BatchStatus::Validating);
    validating.counts = Some(RequestCounts {
        total: 0,
        completed: 0,
        failed: 0,
    });
    w.openai
        .script
        .lock()
        .unwrap()
        .batches
        .push_back(validating);
    w.poll().await;
    let validated = w.reload(&job).await;
    assert_eq!(
        (validated.request_total, validated.request_completed),
        (Some(2), Some(0))
    );
    // Poll 3: in progress; poll 4: completed → results stored and settled.
    let mut running = upstream(BatchStatus::InProgress);
    running.counts = Some(RequestCounts {
        total: 2,
        completed: 1,
        failed: 0,
    });
    w.openai.script.lock().unwrap().batches.push_back(running);
    w.poll().await;
    assert_eq!(w.reload(&job).await.request_completed, Some(1));
    w.openai.push_batch(BatchStatus::Completed);
    w.openai.script.lock().unwrap().results = vec![
        json!({"custom_id":"l1","ok":true,"input":20,"output":4}),
        json!({"custom_id":"l0","ok":true,"input":30,"output":6}),
    ];
    w.poll().await;
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert!(job.settled_at.is_some());
    let out = w.read(f.principal.workspace_id, job.output_file_id).await;
    let ids: Vec<&str> = out
        .iter()
        .map(|l| l["custom_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["second", "first"]);
    assert_eq!(out[0]["response"]["body"]["model"], "company/smart");
    // Settled exactly at the batch list: 50 × 0.4 + 10 × 0.5 = 25.
    assert_eq!(
        reservation(f, job.execution_id).await,
        ("settled".into(), Some(90), Some(25), "batch".into())
    );
    assert_eq!(
        w.openai.calls(),
        ["submit", "retrieve", "retrieve", "retrieve", "delete"]
    );
    assert!(file_deleted(f, job.work_file_id).await);
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn standard_prices_apply_without_a_batch_list_and_to_gateway_runs(pool: PgPool) {
    let w = world(pool, true, false, 2).await;
    let f = &w.f;
    // Native without a published batch list: standard prices, flagged.
    v3_price(f, f.deployment, false).await;
    let file = w
        .upload(&f.principal, &[chat_line("a", "company/smart", "x")])
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    assert_eq!(job.price_tier.as_deref(), Some("standard"));
    assert_eq!(batch::batch_priced(&job), Some(false));
    assert_eq!(reservation(f, job.execution_id).await.1, Some(110));
    // A batch list published later never reprices an admitted batch.
    v3_price(f, f.deployment, true).await;
    w.poll().await;
    w.openai.push_batch(BatchStatus::Completed);
    w.openai.script.lock().unwrap().results =
        vec![json!({"custom_id":"l0","ok":true,"input":10,"output":10})];
    w.poll().await;
    assert_eq!(
        reservation(f, job.execution_id).await,
        ("settled".into(), Some(110), Some(20), "standard".into())
    );
    // Gateway-run lines always use standard prices, even with a batch list.
    let file = w
        .upload(&f.principal, &[chat_line("a", "company/smart", "x")])
        .await;
    let job = w
        .create(
            f.principal,
            &file,
            BatchEndpoint::ChatCompletions,
            Some(json!({"omg_mode":"gateway"})),
        )
        .await
        .unwrap();
    assert_eq!(
        (job.mode(), job.price_tier.as_deref()),
        (Some(BatchMode::Gateway), None)
    );
    Runner::new(w.jobs.clone()).run_next().await.unwrap();
    let (state, actual, tier): (String, Option<i64>, String) = sqlx::query_as("SELECT r.state,r.actual_microusd,r.price_tier FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE e.batch_job_id=$1")
        .bind(job.id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        (state.as_str(), actual, tier.as_str()),
        ("settled", Some(15), "standard")
    );
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn native_anthropic_messages_batch_with_errored_lines(pool: PgPool) {
    let w = world(pool, false, true, 2).await;
    let f = &w.f;
    let line = |id: &str, text: &str| json!({"custom_id":id,"method":"POST","url":"/v1/messages","body":{"model":"company/claude","max_tokens":10,"messages":[{"role":"user","content":text}]}});
    let file = w
        .upload(&f.principal, &[line("ok", "hi"), line("bad", "x")])
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::Messages, None)
        .await
        .unwrap();
    assert_eq!(
        (job.mode(), job.provider.as_str()),
        (Some(BatchMode::Native), "anthropic")
    );
    w.poll().await;
    assert_eq!(w.anthropic.calls(), ["submit"]);
    let mut done = upstream(BatchStatus::Completed);
    done.counts = Some(RequestCounts {
        total: 2,
        completed: 1,
        failed: 1,
    });
    w.anthropic.script.lock().unwrap().batches.push_back(done);
    w.anthropic.script.lock().unwrap().results = vec![
        json!({"custom_id":"l0","ok":true,"input":10,"output":2}),
        json!({"custom_id":"l1","ok":false}),
    ];
    w.poll().await;
    let job = w.reload(&job).await;
    assert_eq!(
        (
            job.state.as_str(),
            job.request_completed,
            job.request_failed
        ),
        ("completed", Some(1), Some(1))
    );
    let out = w.read(f.principal.workspace_id, job.output_file_id).await;
    assert_eq!(out[0]["custom_id"], "ok");
    assert_eq!(out[0]["response"]["body"]["type"], "message");
    let errors = w.read(f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errors[0]["custom_id"], "bad");
    assert_eq!(errors[0]["response"]["status_code"], 400);
    assert_eq!(errors[0]["response"]["body"]["type"], "error");
    // Standard (no batch list) at 2 µUSD/token: (10 + 2) × 2.
    assert_eq!(reservation(f, job.execution_id).await.2, Some(24));
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn native_cancel_before_submission_releases_the_hold(pool: PgPool) {
    let w = world(pool, true, false, 2).await;
    let f = &w.f;
    let file = w
        .upload(&f.principal, &[chat_line("a", "company/smart", "x")])
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    let v = w
        .jobs
        .cancel_batch(&f.principal, &client_id("batch_", job.id))
        .await
        .unwrap();
    assert_eq!(v["status"], "cancelled");
    // Nothing reached the provider; the ceiling is released at zero.
    assert!(w.openai.calls().is_empty());
    let (state, _, actual, _) = reservation(f, job.execution_id).await;
    assert_eq!((state.as_str(), actual), ("settled", Some(0)));
    // The poller never submits a cancelled batch.
    w.poll().await;
    assert!(w.openai.calls().is_empty());
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn batches_are_private_to_their_workspace(pool: PgPool) {
    let w = world(pool, false, false, 2).await;
    let f = &w.f;
    let file = w
        .upload(&f.principal, &[chat_line("a", "company/smart", "x")])
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    let id = client_id("batch_", job.id);
    assert_eq!(
        w.jobs.get_batch(&f.team, &id).await.err(),
        Some(JobError::NotFound)
    );
    assert_eq!(
        w.jobs.cancel_batch(&f.team, &id).await.err(),
        Some(JobError::NotFound)
    );
    assert!(
        w.jobs
            .list_batches(&f.team, None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        w.jobs
            .list_batches(&f.principal, None, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    // The private copy is never a Files API file.
    let api: Option<String> =
        sqlx::query_scalar("SELECT api_purpose FROM stored_files WHERE id=$1")
            .bind(job.work_file_id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(api, None);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn batch_alerts_fire_for_failed_and_stalled_batches(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    for (kind, window) in [("batch_failed", None), ("batch_stalled", Some(30))] {
        sqlx::query("INSERT INTO alert_rules(id,scope,workspace_id,kind,name,window_minutes) VALUES($1,'workspace',$2,$3,$3,$4)")
            .bind(Uuid::new_v4()).bind(f.team.workspace_id).bind(kind).bind(window)
            .execute(&f.store.pool).await.unwrap();
    }
    let lines = [chat_line("a", "company/smart", "x")];
    let failed = w
        .create(
            f.team,
            &w.upload(&f.team, &lines).await,
            BatchEndpoint::ChatCompletions,
            None,
        )
        .await
        .unwrap();
    store::finish_batch(
        &f.store,
        failed.id,
        &store::Finished {
            state: JobState::Failed,
            error_code: Some("budget_exceeded"),
            output_file: None,
            error_file: None,
            counts: None,
        },
    )
    .await
    .unwrap();
    let stalled = w
        .create(
            f.team,
            &w.upload(&f.team, &lines).await,
            BatchEndpoint::ChatCompletions,
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE async_jobs SET state='in_progress',last_progress_at=now()-interval '2 hours' WHERE id=$1")
        .bind(stalled.id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    // A batch of another workspace never feeds this workspace's rules.
    let other = w
        .create(
            f.principal,
            &w.upload(&f.principal, &lines).await,
            BatchEndpoint::ChatCompletions,
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE async_jobs SET state='in_progress',last_progress_at=now()-interval '2 hours' WHERE id=$1")
        .bind(other.id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    crate::alerts::evaluate_once(&f.store).await.unwrap();
    let events: Vec<(String, String, Value)> =
        sqlx::query_as("SELECT kind,summary,details FROM alert_events ORDER BY kind")
            .fetch_all(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0].0, "batch_failed");
    assert_eq!(events[0].1, "Batch stopped: a budget was exhausted");
    assert_eq!(events[0].2["batch_id"], client_id("batch_", failed.id));
    assert_eq!(events[1].0, "batch_stalled");
    assert!(events[1].1.contains("30 min"));
    // Progress resolves the stall.
    sqlx::query("UPDATE async_jobs SET last_progress_at=now() WHERE id=$1")
        .bind(stalled.id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    crate::alerts::evaluate_once(&f.store).await.unwrap();
    let open: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM alert_events WHERE resolved_at IS NULL")
            .fetch_all(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(open, ["batch_failed"]);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn embeddings_and_responses_batches_run_through_the_engine(pool: PgPool) {
    let w = world(pool, false, false, 2).await;
    let f = &w.f;
    sqlx::query("INSERT INTO models(id,public_name,supported_protocols) SELECT gen_random_uuid(),'company/embed',ARRAY['embeddings']")
        .execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) SELECT gen_random_uuid(),m.id,d.provider_connection_id,'embed-mock',true FROM models m, deployments d WHERE m.public_name='company/embed' AND d.id=$1")
        .bind(f.deployment).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) SELECT $1,id,'direct' FROM models WHERE public_name='company/embed'")
        .bind(f.principal.workspace_id).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) SELECT gen_random_uuid(),d.id,1000000,0,100,0,1 FROM deployments d JOIN models m ON m.id=d.model_id WHERE m.public_name='company/embed'")
        .execute(&f.store.pool).await.unwrap();
    let file = w
        .upload(
            &f.principal,
            &[json!({"custom_id":"e1","method":"POST","url":"/v1/embeddings","body":{"model":"company/embed","input":["a","b"]}})],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::Embeddings, None)
        .await
        .unwrap();
    assert_eq!(
        reservation(f, job.execution_id).await.1,
        Some(100),
        "input-only ceiling"
    );
    Runner::new(w.jobs.clone()).run_next().await.unwrap();
    let job = w.reload(&job).await;
    let out = w.read(f.principal.workspace_id, job.output_file_id).await;
    assert_eq!(out[0]["response"]["body"]["object"], "list");
    assert_eq!(
        out[0]["response"]["body"]["data"].as_array().unwrap().len(),
        2
    );
    let file = w
        .upload(
            &f.principal,
            &[json!({"custom_id":"r1","method":"POST","url":"/v1/responses","body":{"model":"company/smart","input":"hello","max_output_tokens":10}})],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::Responses, None)
        .await
        .unwrap();
    Runner::new(w.jobs.clone()).run_next().await.unwrap();
    let job = w.reload(&job).await;
    let out = w.read(f.principal.workspace_id, job.output_file_id).await;
    assert_eq!(out[0]["response"]["body"]["object"], "response");
    let _ = w.claude;
    consistent(f).await;
}

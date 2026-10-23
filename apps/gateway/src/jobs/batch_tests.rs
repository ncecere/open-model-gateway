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
    maxima: Vec<Option<u32>>,
    accept_tools: bool,
    /// #238 multi-route tests: when true, `validate_chat_request` never
    /// refuses - this provider accepts a body another provider's `Fake`
    /// (left at the default, `false`) would refuse, so a model with routes
    /// on both can show the difference between judging a line against one
    /// deployment and judging it against every authorized candidate.
    permissive: bool,
    /// Encoded native records of the last submission.
    submitted: Vec<Value>,
    submissions: Vec<Vec<Value>>,
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
            tools: self.script.lock().unwrap().accept_tools,
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
    /// Simulates a real adapter's pure dry-run encoder (#228): the coarse
    /// `capabilities()`/`supports_chat_request` bool stays true (this is not
    /// a model-wide incompatibility), but this exact body is deterministically
    /// refused, exactly like Anthropic/Bedrock reject a strict function tool.
    fn validate_chat_request(&self, request: &ChatRequest) -> Result<(), InferenceError> {
        if self.script.lock().unwrap().permissive {
            return Ok(());
        }
        match request.messages[0].content.as_deref().unwrap_or_default() {
            "strict-tool" => Err(InferenceError::Unsupported),
            "bad-tool-args" => Err(InferenceError::InvalidRequest),
            // #238 multi-route tests: every provider refuses, but not with
            // the same kind (anthropic 501, openai 400) - so the upload's
            // reported kind must come from every candidate, not just the
            // one deployment planning happened to size the line against.
            "mixed-reject" => Err(if self.id == "openai" {
                InferenceError::InvalidRequest
            } else {
                InferenceError::Unsupported
            }),
            // #238 clamped-request test: only the line's own resolved output
            // bound (the fixture's 50-token ceiling) satisfies this adapter -
            // the raw client omission (`None`) must not.
            "ceiling-sensitive" => {
                if request.max_output_tokens == Some(50) {
                    Ok(())
                } else {
                    Err(InferenceError::Unsupported)
                }
            }
            _ => Ok(()),
        }
    }
    async fn execute(
        &self,
        _: &Deployment,
        request: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        let text = request.messages[0].content.clone().unwrap_or_default();
        {
            let mut script = self.script.lock().unwrap();
            script.executed.push(text.clone());
            script.maxima.push(request.max_output_tokens);
        }
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
            &json!({"custom_id":custom_id,"model":target.upstream_model,"text":text,"maximum":request.max_output()}),
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
        s.submissions.push(seen.clone());
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
    /// `claude`'s own (anthropic) deployment: the tie-break winner (created
    /// first, default priority) of `self.select`'s routing, so planning
    /// always sizes/pins a `company/claude` line against this one.
    claude_deployment: Uuid,
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
        claude_deployment: deployment,
    }
}
/// A second, later-created `company/claude` deployment on the OpenAI
/// connection (`w.openai`), its own standard price: the model now has two
/// authorized, protocol-capable routes on different providers (#238's
/// multi-route tests). Stable priority ties keep `w.claude_deployment`
/// (created first) the tie-break winner of `self.select`'s routing.
async fn second_claude_route(w: &World) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) SELECT $1,$2,provider_connection_id,'claude-mock-2',true FROM deployments WHERE id=$3")
        .bind(id)
        .bind(w.claude)
        .bind(w.f.deployment)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,2000000,2000000,100,50,1)")
        .bind(Uuid::new_v4())
        .bind(id)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    id
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
async fn omitted_batch_maxima_pin_parent_child_and_native_execution_without_rewriting_input(
    pool: PgPool,
) {
    let w = world(pool, true, false, 1).await;
    let f = &w.f;
    // Four queued jobs in this snapshot test, rather than the type default 2.
    sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id,concurrent_jobs) VALUES($1,4)").bind(f.principal.workspace_id).execute(&f.store.pool).await.unwrap();
    let mut line = chat_line("a", "company/smart", "default");
    line["body"]
        .as_object_mut()
        .unwrap()
        .remove("max_completion_tokens");
    let file = w.upload(&f.principal, &[line.clone()]).await;
    let native = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    let gateway = w
        .create(
            f.principal,
            &file,
            BatchEndpoint::ChatCompletions,
            Some(json!({"omg_mode":"gateway"})),
        )
        .await
        .unwrap();
    let response_line = json!({"custom_id":"r","method":"POST","url":"/v1/responses","body":{"model":"company/smart","input":"default"}});
    let response_file = w.upload(&f.principal, &[response_line]).await;
    let native_responses = w
        .create(f.principal, &response_file, BatchEndpoint::Responses, None)
        .await
        .unwrap();
    assert_eq!(native_responses.mode(), Some(BatchMode::Native));
    let responses = w
        .create(
            f.principal,
            &response_file,
            BatchEndpoint::Responses,
            Some(json!({"omg_mode":"gateway"})),
        )
        .await
        .unwrap();
    let old_price: Uuid =
        sqlx::query_scalar("SELECT price_id FROM governance_reservations WHERE execution_id=$1")
            .bind(native.execution_id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    for job in [&native, &native_responses, &gateway, &responses] {
        assert_eq!(reservation(f, job.execution_id).await.1, Some(150));
    }
    let original = w.read(f.principal.workspace_id, native.input_file_id).await;
    assert_eq!(original, [line]);
    let private = w.read(f.principal.workspace_id, native.work_file_id).await;
    assert!(private[0]["body"].get("max_completion_tokens").is_none());
    assert_eq!(private[0]["_omg_plan"]["price"], json!(old_price));
    // Publication while queued must change neither the parent's hold nor the
    // execution cap/price of a child or a native line.
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,9000000,9000000,200,9,1)").bind(Uuid::new_v4()).bind(f.deployment).execute(&f.store.pool).await.unwrap();
    w.poll().await;
    {
        let script = w.openai.script.lock().unwrap();
        assert_eq!(script.submissions.len(), 2);
        assert!(
            script
                .submissions
                .iter()
                .all(|s| s.len() == 1 && s[0]["maximum"] == 50)
        );
    }
    let runner = Runner::new(w.jobs.clone());
    assert!(runner.run_next().await.unwrap());
    assert!(runner.run_next().await.unwrap());
    assert_eq!(w.openai.script.lock().unwrap().maxima, [Some(50), Some(50)]);
    let children: Vec<(Uuid,i64,i64)> = sqlx::query_as("SELECT r.price_id,r.reserved_tokens,l.amount_microusd FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id JOIN monetary_ledger l ON l.execution_id=e.id WHERE e.batch_job_id IS NOT NULL AND l.kind='hold'").fetch_all(&f.store.pool).await.unwrap();
    assert_eq!(children, [(old_price, 150, 150), (old_price, 150, 150)]);
    assert_eq!(
        w.read(f.principal.workspace_id, native.input_file_id).await,
        original
    );
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn omitted_maximum_stale_planning_price_is_refused_before_parent_admission(pool: PgPool) {
    stale_plan_is_refused_then_replanned(pool, crate::governance::locks::AdmissionMode::Scoped)
        .await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn omitted_maximum_stale_planning_price_is_refused_in_global_admission(pool: PgPool) {
    stale_plan_is_refused_then_replanned(pool, crate::governance::locks::AdmissionMode::Global)
        .await;
}

async fn stale_plan_is_refused_then_replanned(
    pool: PgPool,
    mode: crate::governance::locks::AdmissionMode,
) {
    let mut w = world(pool, true, false, 1).await;
    w.f.store = w.f.store.clone().with_admission_mode(mode);
    w.jobs.store = w.f.store.clone();
    let f = &w.f;
    let mut line = chat_line("a", "company/smart", "default");
    line["body"]
        .as_object_mut()
        .unwrap()
        .remove("max_completion_tokens");
    let file = w.upload(&f.principal, &[line]).await;
    let input = w
        .files
        .get(
            crate::filestore::files::parse_public_id(&file).unwrap(),
            Some(f.principal.workspace_id),
        )
        .await
        .unwrap()
        .unwrap();
    let plan = w
        .jobs
        .plan(
            &f.principal,
            Uuid::new_v4(),
            &input,
            BatchEndpoint::ChatCompletions,
            false,
        )
        .await
        .ok()
        .expect("valid plan");
    let original = w.read(f.principal.workspace_id, Some(input.id)).await;
    assert!(original[0]["body"].get("max_completion_tokens").is_none());
    let old = plan.groups[0].price_id.unwrap();
    let new = Uuid::new_v4();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,9000000,9000000,200,9,1)").bind(new).bind(f.deployment).execute(&f.store.pool).await.unwrap();
    for native in [false, true] {
        let record = f.start();
        assert_eq!(
            crate::governance::batch::admit_batch(&f.store, &record, &plan.groups, native, 600)
                .await,
            Err(InferenceError::Configuration)
        );
    }
    let rows: (i64,i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM inference_executions),(SELECT count(*) FROM governance_reservations),(SELECT count(*) FROM monetary_ledger),(SELECT count(*) FROM async_jobs)").fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(rows, (0, 0, 0, 0));
    assert!(w.openai.calls().is_empty());
    assert!(w.openai.executed().is_empty());
    assert_eq!(plan.groups[0].price_id, Some(old));
    assert_eq!(
        plan.groups[0].outputs,
        std::collections::BTreeMap::from([(50, 1)])
    );
    // A separate public create replans the very same uploaded file. There is
    // no server retry or recapping of the rejected private plan.
    let native = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    let gateway = w
        .create(
            f.principal,
            &file,
            BatchEndpoint::ChatCompletions,
            Some(json!({"omg_mode":"gateway"})),
        )
        .await
        .unwrap();
    for job in [&native, &gateway] {
        assert_eq!(job.input_file_id, Some(input.id));
        assert_ne!(job.work_file_id, Some(plan.work_file));
        let row: (Option<Uuid>,i64,i64) = sqlx::query_as("SELECT price_id,reserved_tokens,held_microusd FROM governance_reservations WHERE execution_id=$1").bind(job.execution_id).fetch_one(&f.store.pool).await.unwrap();
        assert_eq!(
            row,
            (
                (job.mode() == Some(BatchMode::Native)).then_some(new),
                209,
                1881
            )
        );
        let private = w.read(f.principal.workspace_id, job.work_file_id).await;
        assert_eq!(private[0]["_omg_plan"]["price"], json!(new));
        assert!(private[0]["body"].get("max_completion_tokens").is_none());
    }
    w.poll().await;
    assert_eq!(w.openai.script.lock().unwrap().submitted[0]["maximum"], 9);
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    assert_eq!(w.openai.script.lock().unwrap().maxima, [Some(9)]);
    let child: (Uuid,i64,i64) = sqlx::query_as("SELECT r.price_id,r.reserved_tokens,l.amount_microusd FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id JOIN monetary_ledger l ON l.execution_id=e.id WHERE e.batch_job_id=$1 AND l.kind='hold'").bind(gateway.id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(child, (new, 209, 1881));
    assert_eq!(
        w.read(f.principal.workspace_id, Some(input.id)).await,
        original
    );
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn omitted_maximum_boundary_preserves_compact_numbers_in_both_batch_modes(pool: PgPool) {
    let w = world(pool, true, false, 1).await;
    let f = &w.f;
    w.openai.script.lock().unwrap().accept_tools = true;
    let numbers = std::iter::repeat_n("1e9", 200)
        .collect::<Vec<_>>()
        .join(",");
    let template = format!(
        r#"{{"custom_id":"boundary","method":"POST","url":"/v1/chat/completions","body":{{"model":"company/smart","messages":[{{"role":"user","content":"boundary"}}],"tools":[{{"type":"function","function":{{"name":"noop","parameters":{{"type":"object","default":[{numbers}],"description":"PAD"}}}}}}]}}}}"#
    );
    let raw = template.replace(
        "PAD",
        &"x".repeat(BATCH_MAX_LINE_BYTES - template.len() + 3),
    );
    assert_eq!(raw.len(), BATCH_MAX_LINE_BYTES);
    assert!(lines::parse_line(raw.as_bytes(), BatchEndpoint::ChatCompletions).is_ok());
    let file = w.upload_raw(&f.principal, raw.into_bytes()).await;
    let native = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    let gateway = w
        .create(
            f.principal,
            &file,
            BatchEndpoint::ChatCompletions,
            Some(json!({"omg_mode":"gateway"})),
        )
        .await
        .unwrap();
    w.poll().await;
    assert_eq!(w.openai.script.lock().unwrap().submitted.len(), 1);
    assert!(w.reload(&native).await.upstream_id.is_some());
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    assert_eq!(w.reload(&gateway).await.request_completed, Some(1));
    assert_eq!(w.openai.script.lock().unwrap().maxima, [Some(50)]);
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
        r#"{"custom_id":"d","method":"POST","url":"/v1/chat/completions","body":{"model":"company/smart","messages":[{"role":"user","content":"x"}],"max_completion_tokens":0}}"#.into(),
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
            (6, "invalid_body"),
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

/// Counts of rows a batch's admission could have created, workspace-wide:
/// jobs, reservations, inference executions and ledger entries.
async fn admission_rowcounts(f: &Fixture) -> (i64, i64, i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM async_jobs),(SELECT count(*) FROM governance_reservations),(SELECT count(*) FROM inference_executions),(SELECT count(*) FROM monetary_ledger)")
        .fetch_one(&f.store.pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn strict_tool_lines_against_an_anthropic_model_reserve_and_dispatch_nothing(pool: PgPool) {
    // #238: every line is a deterministic adapter rejection (coarse
    // `supports_chat_request` passes; `validate_chat_request`, #228's
    // pure dry-run encoder, refuses this exact body). Planning must refuse
    // the whole upload, per line, before the parent envelope is ever sized
    // or reserved - not admit an empty batch.
    let w = world(pool, false, false, 2).await;
    let f = &w.f;
    let before = admission_rowcounts(f).await;
    let file = w
        .upload(
            &f.principal,
            &[
                chat_line("a", "company/claude", "strict-tool"),
                chat_line("b", "company/claude", "strict-tool"),
            ],
        )
        .await;
    let Err(CreateError::Invalid(issues)) = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
    else {
        panic!("expected every line to be refused before admission");
    };
    let got: Vec<(u64, &str)> = issues.iter().map(|i| (i.line, i.error.code())).collect();
    assert_eq!(
        got,
        [(1, "unsupported_feature"), (2, "unsupported_feature")]
    );
    assert_eq!(before, admission_rowcounts(f).await);
    // Nothing exists to dispatch.
    assert!(!Runner::new(w.jobs.clone()).run_next().await.unwrap());
    assert_eq!(w.anthropic.executed(), Vec::<String>::new());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn mixed_batch_reserves_only_for_the_valid_lines(pool: PgPool) {
    // #238: one line of two is a deterministic adapter rejection. Planning
    // must reserve the parent envelope as if that line did not exist, and
    // the batch still runs: the valid line completes, the rejected line is
    // refused per line, at the same classification an interactive request
    // would get, with no execution or reservation of its own (#228).
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    let solo_file = w
        .upload(&f.principal, &[chat_line("x", "company/claude", "two")])
        .await;
    let solo = w
        .create(
            f.principal,
            &solo_file,
            BatchEndpoint::ChatCompletions,
            None,
        )
        .await
        .unwrap();
    let solo_held = reservation(f, solo.execution_id).await.1;
    let mixed_file = w
        .upload(
            &f.principal,
            &[
                chat_line("a", "company/claude", "two"),
                chat_line("b", "company/claude", "strict-tool"),
            ],
        )
        .await;
    let before = admission_rowcounts(f).await;
    let job = w
        .create(
            f.principal,
            &mixed_file,
            BatchEndpoint::ChatCompletions,
            None,
        )
        .await
        .unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Gateway));
    assert_eq!(job.request_total, Some(2));
    // Sized as if the rejected line were never in the file.
    assert_eq!(reservation(f, job.execution_id).await.1, solo_held);
    // One new job, one new envelope reservation; the rejected line reserved
    // nothing of its own yet (it has not run).
    let after = admission_rowcounts(f).await;
    assert_eq!((after.0, after.1), (before.0 + 1, before.1 + 1));
    // Two gateway batches are queued (`solo` and `job`): run both.
    let runner = Runner::new(w.jobs.clone());
    assert!(runner.run_next().await.unwrap());
    assert!(runner.run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(1), Some(1))
    );
    let out = w.read(f.principal.workspace_id, job.output_file_id).await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["custom_id"], "a");
    assert_eq!(out[0]["response"]["status_code"], 200);
    let errs = w.read(f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0]["custom_id"], "b");
    assert_eq!(errs[0]["response"]["status_code"], 501);
    assert_eq!(
        errs[0]["response"]["body"]["error"]["code"],
        "unsupported_capability"
    );
    // The rejected line never got its own execution or reservation: only the
    // valid line's attempt exists.
    let lines: i64 =
        sqlx::query_scalar("SELECT count(*) FROM inference_executions WHERE batch_job_id=$1")
            .bind(job.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(lines, 1);
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn multi_route_unpinned_line_is_sized_and_runs_on_the_accepting_route(pool: PgPool) {
    // #238 MULTI-ROUTE (required): planning must judge an unpinned
    // (explicit-maximum) line against every authorized, protocol-capable
    // candidate route, not only the one deployment it happened to size
    // against. `company/claude` now has two routes: anthropic (created
    // first, so the tie-break winner `self.select` sizes the line against)
    // refuses "strict-tool"; openai (made permissive here) accepts it. The
    // line must still be sized, and dispatch - which may land on either
    // route and is pinned there with no failover once claimed - must still
    // succeed when the route it reaches is the accepting one.
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    w.openai.script.lock().unwrap().permissive = true;
    second_claude_route(&w).await;
    let solo_file = w
        .upload(&f.principal, &[chat_line("x", "company/claude", "two")])
        .await;
    let solo = w
        .create(
            f.principal,
            &solo_file,
            BatchEndpoint::ChatCompletions,
            None,
        )
        .await
        .unwrap();
    let solo_held = reservation(f, solo.execution_id).await.1;
    let file = w
        .upload(
            &f.principal,
            &[chat_line("a", "company/claude", "strict-tool")],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Gateway));
    // Sized exactly like a batch of one ordinary valid line: not excluded,
    // because openai still accepts it.
    assert_eq!(reservation(f, job.execution_id).await.1, solo_held);
    // Between planning and dispatch the route planning sized against stops
    // serving the model (disabled - the same kind of drift the runner's own
    // periodic replan already anticipates): dispatch must still succeed,
    // through the other authorized candidate.
    sqlx::query("UPDATE deployments SET enabled=false WHERE id=$1")
        .bind(w.claude_deployment)
        .execute(&f.store.pool)
        .await
        .unwrap();
    // Two gateway batches are queued (`solo` and `job`): run both.
    let runner = Runner::new(w.jobs.clone());
    assert!(runner.run_next().await.unwrap());
    assert!(runner.run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(1), Some(0))
    );
    // `solo` also now only has openai to dispatch to (anthropic was
    // disabled for both), so both lines ran there; anthropic ran neither.
    assert!(w.openai.executed().contains(&"strict-tool".to_owned()));
    assert_eq!(w.anthropic.executed(), Vec::<String>::new());
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduler_skips_a_rejecting_route_and_claims_the_accepting_one_with_both_enabled(
    pool: PgPool,
) {
    // #243: the scheduler's own dispatch-time candidate filter
    // (`jobs::runner::Runner::plan_route_inner`) must apply the same
    // deterministic eligibility the engine runs before admission
    // (`ProviderAdapter::validate_chat_request`, #228), not only the coarse
    // capability check - otherwise it can claim and pin a route that
    // deterministically rejects an unpinned (explicit-maximum) line while
    // another authorized route would accept it, refusing a line an
    // interactive request would succeed on.
    //
    // Unlike `multi_route_unpinned_line_is_sized_and_runs_on_the_accepting_route`
    // (which disables the rejecting route before dispatch and so does not
    // cover this), BOTH of `company/claude`'s routes stay enabled through
    // dispatch here. Anthropic is given the higher (numerically lower)
    // routing priority and refuses "strict-tool"; openai is given the lower
    // priority but is made permissive, so it accepts the same body.
    // Priority alone would make the scheduler claim and pin anthropic.
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    let openai_route = second_claude_route(&w).await;
    w.openai.script.lock().unwrap().permissive = true;
    for (deployment, priority) in [(w.claude_deployment, 0), (openai_route, 10)] {
        sqlx::query("INSERT INTO deployment_routing(deployment_id,priority) VALUES($1,$2)")
            .bind(deployment)
            .bind(priority)
            .execute(&f.store.pool)
            .await
            .unwrap();
    }
    let file = w
        .upload(
            &f.principal,
            &[chat_line("a", "company/claude", "strict-tool")],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Gateway));
    // Both routes remain enabled all the way through dispatch.
    let runner = Runner::new(w.jobs.clone());
    assert!(runner.run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(1), Some(0))
    );
    let out = w.read(f.principal.workspace_id, job.output_file_id).await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["response"]["status_code"], 200);
    assert!(job.error_file_id.is_none());
    // The line ran on the accepting route (openai), never on the rejecting
    // one (anthropic), even though anthropic has the higher priority.
    assert!(w.openai.executed().contains(&"strict-tool".to_owned()));
    assert_eq!(w.anthropic.executed(), Vec::<String>::new());
    // Exactly one line execution, on the accepting route.
    let executions: Vec<Uuid> =
        sqlx::query_scalar("SELECT deployment_id FROM inference_executions WHERE batch_job_id=$1")
            .bind(job.id)
            .fetch_all(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(executions, [openai_route]);
    // The rejecting route recorded no execution and no reservation of its
    // own for this line (only the planning-time envelope reservation, keyed
    // by `job.execution_id`, ever touched it).
    let anthropic_line_reservations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE e.batch_job_id=$1 AND r.deployment_id=$2",
    )
    .bind(job.id)
    .bind(w.claude_deployment)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(anthropic_line_reservations, 0);
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn multi_route_all_candidates_reject_with_differing_kinds_is_invalid_request_not_unsupported(
    pool: PgPool,
) {
    // #238 MULTI-ROUTE classification: when every authorized,
    // protocol-capable candidate route refuses an unpinned line, the
    // reported kind must come from every one of them, not only the single
    // deployment planning happened to size against. Anthropic (the
    // tie-break winner) refuses `mixed-reject` as `unsupported_capability`;
    // openai also refuses it, but as `invalid_request_error` - the upload
    // must be refused with that kind, like `Engine::execute_protocol` would
    // for the same interactive request.
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    second_claude_route(&w).await;
    let file = w
        .upload(
            &f.principal,
            &[chat_line("a", "company/claude", "mixed-reject")],
        )
        .await;
    let Err(CreateError::Invalid(issues)) = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
    else {
        panic!("expected every candidate route's rejection to refuse the upload");
    };
    assert_eq!(
        issues
            .iter()
            .map(|i| (i.line, i.error.code()))
            .collect::<Vec<_>>(),
        [(1, "invalid_body")]
    );
    assert!(w.anthropic.executed().is_empty());
    assert!(w.openai.executed().is_empty());
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn batch_planning_memoizes_model_level_lookups_within_one_scan(pool: PgPool) {
    // #238 follow-up (adversarial re-review, medium): with configuration
    // caches bypassed (the test default - `ConfigVersions::new` starts
    // disabled until `enable()`/`start()`), `every_candidate_route_refuses`
    // must fetch `company/claude`'s candidate deployments and classify its
    // universal refusal at most once per scan, not once per
    // explicit-maximum line. Without memoization this upload of 50
    // identical lines would run 50 `Store::deployments` reads and 50
    // `Store::route_plan` selects before `admit_batch` ever checks a
    // budget or job limit; repeated uploads amplify that load.
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    second_claude_route(&w).await;
    const LINES: usize = 50;
    let lines: Vec<Value> = (0..LINES)
        .map(|i| chat_line(&format!("l{i}"), "company/claude", "strict-tool"))
        .collect();
    let file = w.upload(&f.principal, &lines).await;
    let input = w
        .files
        .get(
            crate::filestore::files::parse_public_id(&file).unwrap(),
            Some(f.principal.workspace_id),
        )
        .await
        .unwrap()
        .unwrap();
    let result = w
        .jobs
        .plan(
            &f.principal,
            Uuid::new_v4(),
            &input,
            BatchEndpoint::ChatCompletions,
            false,
        )
        .await;
    // Every candidate route refuses "strict-tool": the upload is invalid,
    // but planning must still have classified it, not skipped the check.
    assert!(
        result.is_err(),
        "every candidate route refusing the same body must still refuse the upload"
    );
    assert_eq!(
        w.jobs
            .plan_lookup_counts
            .deployments
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one candidate-deployment lookup for the whole scan, not one per line"
    );
    assert_eq!(
        w.jobs
            .plan_lookup_counts
            .route_plan
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "at most one route-plan classification for the whole scan, not one per line"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn pinned_line_refused_by_its_own_route_is_classified_like_interactive_not_model_unavailable(
    pool: PgPool,
) {
    // #238 PINNED-LINE CLASSIFICATION: an omitted-maximum line is pinned at
    // plan time to `resolved.deployment` (anthropic, the tie-break winner)
    // for price/cap. When that pinned route deterministically refuses it,
    // the line must fail with the same classification an interactive
    // request pinned to that one route would get (here 501
    // `unsupported_capability`), even though a *different*, unpinned route
    // (openai, made permissive here) for the same model would accept it -
    // never a bare `model_not_found`.
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    w.openai.script.lock().unwrap().permissive = true;
    second_claude_route(&w).await;
    let mut pinned = chat_line("a", "company/claude", "strict-tool");
    pinned["body"]
        .as_object_mut()
        .unwrap()
        .remove("max_completion_tokens");
    let file = w
        .upload(
            &f.principal,
            &[pinned, chat_line("b", "company/claude", "two")],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Gateway));
    assert_eq!(job.request_total, Some(2));
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(1), Some(1))
    );
    let out = w.read(f.principal.workspace_id, job.output_file_id).await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["custom_id"], "b");
    let errs = w.read(f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0]["custom_id"], "a");
    assert_eq!(errs[0]["response"]["status_code"], 501);
    assert_eq!(
        errs[0]["response"]["body"]["error"]["code"], "unsupported_capability",
        "a pinned line refused by its own route must get the interactive classification, not model_not_found"
    );
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn omitted_maximum_validation_uses_the_clamped_bound_not_the_raw_omission(pool: PgPool) {
    // #238 CLAMPED REQUEST: the deterministic pre-admission check must see
    // the same clamped body `encode_native_line` already does (the line's
    // own resolved output bound), not the raw client omission. This fake
    // adapter's pure encoder only accepts the fixture's 50-token ceiling;
    // judged against the raw omission (`None`) it would always refuse.
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    let mut line = chat_line("a", "company/claude", "ceiling-sensitive");
    line["body"]
        .as_object_mut()
        .unwrap()
        .remove("max_completion_tokens");
    let file = w.upload(&f.principal, &[line]).await;
    let input = w
        .files
        .get(
            crate::filestore::files::parse_public_id(&file).unwrap(),
            Some(f.principal.workspace_id),
        )
        .await
        .unwrap()
        .unwrap();
    let plan = w
        .jobs
        .plan(
            &f.principal,
            Uuid::new_v4(),
            &input,
            BatchEndpoint::ChatCompletions,
            false,
        )
        .await
        .ok()
        .expect("the clamped bound (50) must satisfy the adapter; the raw omission would not");
    assert_eq!(plan.groups.len(), 1);
    assert_eq!(
        plan.groups[0].outputs,
        std::collections::BTreeMap::from([(50, 1)])
    );
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_deterministic_exclusion_forces_gateway_mode_never_native(pool: PgPool) {
    // #238 NATIVE BATCHES: the native decision must not ignore deterministic
    // exclusions. This fake's `encode_native_line` is more permissive than
    // `validate_chat_request` (it does not reject "strict-tool"), so without
    // the fix the whole batch would wrongly decide native and submit every
    // line - including the excluded one - to the provider's batch API,
    // billing it natively with nothing reserved for it.
    let w = world(pool, false, true, 1).await;
    let f = &w.f;
    let file = w
        .upload(
            &f.principal,
            &[
                chat_line("a", "company/claude", "strict-tool"),
                chat_line("b", "company/claude", "two"),
            ],
        )
        .await;
    let solo_file = w
        .upload(&f.principal, &[chat_line("x", "company/claude", "two")])
        .await;
    let solo = w
        .create(
            f.principal,
            &solo_file,
            BatchEndpoint::ChatCompletions,
            None,
        )
        .await
        .unwrap();
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    assert_eq!(
        job.mode(),
        Some(BatchMode::Gateway),
        "an excluded line must force gateway mode, never native"
    );
    // Sized as if the excluded line were never in the file.
    assert_eq!(
        reservation(f, job.execution_id).await.1,
        reservation(f, solo.execution_id).await.1
    );
    // `solo` has no exclusion, so it is native (`anthropic_native` is
    // true): it is left pending here, polling is not this test's
    // concern. Only `job` - the one with the excluded line - is
    // gateway-mode and runnable by the gateway runner.
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    // Gateway mode never calls the native submit/retrieve/cancel/delete API.
    assert!(w.anthropic.calls().is_empty());
    assert_eq!(w.anthropic.executed(), ["two"]);
    let job = w.reload(&job).await;
    assert_eq!(job.state, "completed");
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(1), Some(1))
    );
    let errs = w.read(f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0]["custom_id"], "a");
    assert_eq!(errs[0]["response"]["status_code"], 501);
    consistent(f).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn mixed_upload_with_the_rejected_line_first_reports_invalid_request_in_order(pool: PgPool) {
    // #238 coverage: `validate_chat_request`'s other contractual outcome
    // (`InvalidRequest`, a `bad-tool-args` body) and a rejection that is not
    // the batch's first physical line were both untested. The rejected line
    // here is line 1, the valid line is line 2 - the opposite order from the
    // existing mixed-batch test.
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    let solo_file = w
        .upload(&f.principal, &[chat_line("x", "company/claude", "two")])
        .await;
    let solo = w
        .create(
            f.principal,
            &solo_file,
            BatchEndpoint::ChatCompletions,
            None,
        )
        .await
        .unwrap();
    let solo_held = reservation(f, solo.execution_id).await.1;
    let mixed_file = w
        .upload(
            &f.principal,
            &[
                chat_line("a", "company/claude", "bad-tool-args"),
                chat_line("b", "company/claude", "two"),
            ],
        )
        .await;
    let job = w
        .create(
            f.principal,
            &mixed_file,
            BatchEndpoint::ChatCompletions,
            None,
        )
        .await
        .unwrap();
    // Sized as if the rejected (first) line were never in the file.
    assert_eq!(reservation(f, job.execution_id).await.1, solo_held);
    let runner = Runner::new(w.jobs.clone());
    assert!(runner.run_next().await.unwrap());
    assert!(runner.run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(1), Some(1))
    );
    let out = w.read(f.principal.workspace_id, job.output_file_id).await;
    assert_eq!(out[0]["custom_id"], "b");
    let errs = w.read(f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errs[0]["custom_id"], "a");
    assert_eq!(errs[0]["response"]["status_code"], 400);
    assert_eq!(
        errs[0]["response"]["body"]["error"]["code"],
        "invalid_request_error"
    );
    consistent(f).await;
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

/// A key revoked after its gateway-run batch was created: every line that
/// runs afterwards fails exactly as the interactive endpoint refuses the key
/// (401 `authentication_error`, never `model_not_found`), and nothing reaches
/// a provider.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn lines_of_a_revoked_key_fail_with_the_authentication_error(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    let file = w
        .upload(
            &f.principal,
            &[
                chat_line("a", "company/smart", "one"),
                chat_line("b", "company/smart", "two"),
            ],
        )
        .await;
    let job = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(f.principal.key_id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let _ = Runner::new(w.jobs.clone()).run_next().await;
    let job = w.reload(&job).await;
    assert!(w.openai.executed().is_empty());
    let errors = w.read(f.principal.workspace_id, job.error_file_id).await;
    assert_eq!(errors.len(), 2, "{} {:?}", job.state, job.error_code);
    let refused = json!({"error":{"message":"Invalid or missing API key","type":"authentication_error","code":"authentication_error","param":null}});
    for e in &errors {
        assert_eq!(e["response"]["status_code"], 401, "{e}");
        assert_eq!(e["response"]["body"], refused, "{e}");
    }
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
            json!({"custom_id":"l0","model":"mock-model","text":"one","maximum":10}),
            json!({"custom_id":"l1","model":"mock-model","text":"two","maximum":10})
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

/// One HTTP call: status and raw body (JSON bodies or a multipart upload).
async fn http(
    app: &axum::Router,
    method: &str,
    path: &str,
    bearer: &str,
    body: Option<(&str, Vec<u8>)>,
) -> (axum::http::StatusCode, Vec<u8>) {
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", bearer);
    let body = match body {
        Some((content_type, bytes)) => {
            request = request.header("content-type", content_type);
            axum::body::Body::from(bytes)
        }
        None => axum::body::Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 16 << 20)
        .await
        .unwrap();
    (status, bytes.to_vec())
}
fn parsed(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}
/// `POST /v1/files` with a one-line batch input.
async fn upload_http(app: &axum::Router, bearer: &str) -> String {
    let mut form = b"--m\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n--m\r\nContent-Disposition: form-data; name=\"file\"; filename=\"in.jsonl\"\r\n\r\n".to_vec();
    form.extend_from_slice(&serde_json::to_vec(&chat_line("a", "company/smart", "x")).unwrap());
    form.extend_from_slice(b"\n\r\n--m--\r\n");
    let (status, body) = http(
        app,
        "POST",
        "/v1/files",
        bearer,
        Some(("multipart/form-data; boundary=m", form)),
    )
    .await;
    assert_eq!(status, 200, "{}", parsed(&body));
    parsed(&body)["id"].as_str().unwrap().to_owned()
}
fn create_body(file: &str, gateway: bool) -> Option<(&'static str, Vec<u8>)> {
    let mut v = json!({"input_file_id": file, "endpoint": "/v1/chat/completions", "completion_window": "24h"});
    if gateway {
        v["metadata"] = json!({"omg_mode": "gateway"});
    }
    Some(("application/json", serde_json::to_vec(&v).unwrap()))
}
fn listed(list: &[u8], id: &str) -> bool {
    parsed(list)["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|o| o["id"] == id)
}

/// Object visibility (#141) over HTTP: every caller type × both modes ×
/// files (list, retrieve, content, delete, use as a batch's input_file_id)
/// and batches (list, retrieve, cancel), objects of a member's key and of a
/// service-account key. Hidden objects answer exactly like missing ones.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn object_visibility_matrix_for_files_and_batches(pool: PgPool) {
    use crate::visibility::test_support::{CALLERS, Owner, expected, may_modify, team};
    let w = world(pool, true, false, 2).await;
    let f = &w.f;
    sqlx::query("UPDATE installation_settings SET file_user_files_enabled=true WHERE singleton")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let t = team(f).await;
    let app =
        crate::http::router_with_engine(f.store.clone(), None, w.jobs.engine.clone().unwrap())
            .layer(axum::Extension(w.files.runtime().clone()));
    let missing_file = "file-00000000000000000000000000000000";
    let missing_batch = "batch_00000000000000000000000000000000";
    let service = &t.get(crate::visibility::test_support::Who::Service).bearer;
    let (status, file_404) = http(
        &app,
        "GET",
        &format!("/v1/files/{missing_file}"),
        service,
        None,
    )
    .await;
    assert_eq!(status, 404);
    let (_, batch_404) = http(
        &app,
        "GET",
        &format!("/v1/batches/{missing_batch}"),
        service,
        None,
    )
    .await;
    assert_eq!(parsed(&batch_404)["error"]["code"], "not_found");
    let (status, input_404) = http(
        &app,
        "POST",
        "/v1/batches",
        service,
        create_body(missing_file, false),
    )
    .await;
    assert_eq!(status, 404);

    // Batch result files follow the batch's owner: a gateway-run batch of
    // the member's key, run to completion before the matrix (member mode).
    let member = t.owner(Owner::Member);
    let input = upload_http(&app, &member.bearer).await;
    let (status, created) = http(
        &app,
        "POST",
        "/v1/batches",
        &member.bearer,
        create_body(&input, true),
    )
    .await;
    assert_eq!(status, 200, "{}", parsed(&created));
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let (_, done) = http(
        &app,
        "GET",
        &format!("/v1/batches/{}", parsed(&created)["id"].as_str().unwrap()),
        &member.bearer,
        None,
    )
    .await;
    let output = parsed(&done)["output_file_id"].as_str().unwrap().to_owned();
    for who in CALLERS {
        let (status, _) = http(
            &app,
            "GET",
            &format!("/v1/files/{output}/content"),
            &t.get(who).bearer,
            None,
        )
        .await;
        assert_eq!(
            status == 200,
            expected("member", Owner::Member, who),
            "result file, {who:?}"
        );
    }

    for mode in ["member", "workspace"] {
        t.mode(f, mode).await;
        for owner in [Owner::Member, Owner::Service] {
            let creator = &t.owner(owner).bearer;
            let input = upload_http(&app, creator).await;
            let (status, created) = http(
                &app,
                "POST",
                "/v1/batches",
                creator,
                create_body(&input, false),
            )
            .await;
            assert_eq!(status, 200, "{}", parsed(&created));
            let batch = parsed(&created)["id"].as_str().unwrap().to_owned();
            for who in CALLERS {
                let see = expected(mode, owner, who);
                let case = format!("{mode}/{owner:?}/{who:?}");
                let bearer = &t.get(who).bearer;
                // Files: a fresh file per case (delete is destructive).
                let file = upload_http(&app, creator).await;
                let (_, list) = http(&app, "GET", "/v1/files?limit=10000", bearer, None).await;
                assert_eq!(listed(&list, &file), see, "list files {case}");
                for (method, path) in [
                    ("GET", format!("/v1/files/{file}")),
                    ("GET", format!("/v1/files/{file}/content")),
                ] {
                    let (status, body) = http(&app, method, &path, bearer, None).await;
                    if see {
                        assert_eq!(status, 200, "{method} {path} {case}");
                    } else {
                        assert_eq!(
                            (status.as_u16(), body.as_slice()),
                            (404, file_404.as_slice()),
                            "{method} {path} {case}"
                        );
                    }
                }
                // A hidden file id is the same 404 as a missing one as a cursor too.
                let (_, page) = http(
                    &app,
                    "GET",
                    &format!("/v1/files?after={file}"),
                    bearer,
                    None,
                )
                .await;
                let (_, nothing) = http(
                    &app,
                    "GET",
                    &format!("/v1/files?after={missing_file}"),
                    bearer,
                    None,
                )
                .await;
                if !see {
                    assert_eq!(page, nothing, "cursor {case}");
                }
                // The file as another batch's input_file_id.
                let (status, body) = http(
                    &app,
                    "POST",
                    "/v1/batches",
                    bearer,
                    create_body(&file, false),
                )
                .await;
                if see {
                    assert_eq!(status, 200, "input_file_id {case}: {}", parsed(&body));
                    let id = parsed(&body)["id"].as_str().unwrap().to_owned();
                    let (status, _) = http(
                        &app,
                        "POST",
                        &format!("/v1/batches/{id}/cancel"),
                        bearer,
                        None,
                    )
                    .await;
                    assert_eq!(status, 200, "cancel own new batch {case}");
                } else {
                    assert_eq!(
                        (status.as_u16(), body.as_slice()),
                        (404, input_404.as_slice()),
                        "input_file_id {case}"
                    );
                }
                let modify = see && may_modify(owner, who);
                let (status, body) =
                    http(&app, "DELETE", &format!("/v1/files/{file}"), bearer, None).await;
                if modify {
                    assert_eq!(status, 200, "delete {case}");
                } else if see {
                    assert_eq!(status, 403, "delete another member's file {case}");
                    let (status, _) =
                        http(&app, "GET", &format!("/v1/files/{file}"), creator, None).await;
                    assert_eq!(status, 200, "a refused delete keeps the file {case}");
                } else {
                    assert_eq!(
                        (status.as_u16(), body.as_slice()),
                        (404, file_404.as_slice()),
                        "delete {case}"
                    );
                    let (status, _) =
                        http(&app, "GET", &format!("/v1/files/{file}"), creator, None).await;
                    assert_eq!(status, 200, "a refused delete keeps the file {case}");
                }
                // Batches.
                let (_, list) = http(&app, "GET", "/v1/batches?limit=100", bearer, None).await;
                assert_eq!(listed(&list, &batch), see, "list batches {case}");
                let (status, body) =
                    http(&app, "GET", &format!("/v1/batches/{batch}"), bearer, None).await;
                if see {
                    assert_eq!(status, 200, "get batch {case}");
                } else {
                    assert_eq!(
                        (status.as_u16(), body.as_slice()),
                        (404, batch_404.as_slice()),
                        "get batch {case}"
                    );
                }
                // Visible: cancelled (or already finished, 400); hidden: 404.
                let (status, body) = http(
                    &app,
                    "POST",
                    &format!("/v1/batches/{batch}/cancel"),
                    bearer,
                    None,
                )
                .await;
                if modify {
                    assert!(
                        matches!(status.as_u16(), 200 | 400),
                        "cancel batch {case}: {status}"
                    );
                } else if see {
                    assert_eq!(status, 403, "cancel another member's batch {case}");
                } else {
                    assert_eq!(
                        (status.as_u16(), body.as_slice()),
                        (404, batch_404.as_slice()),
                        "cancel batch {case}"
                    );
                }
            }
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn memoized_refusal_classification_stays_per_line(pool: PgPool) {
    // Coordinator review of the #238 memoization: the cached lookups must not
    // fix one model's refusal *kind* for every later line. Two lines of the
    // same model, each refused by every capable route, but with different
    // kinds: the first is unsupported everywhere, the second is invalid
    // everywhere. Each line must be reported with its own kind.
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    second_claude_route(&w).await;
    let file = w
        .upload(
            &f.principal,
            &[
                chat_line("a", "company/claude", "strict-tool"),
                chat_line("b", "company/claude", "bad-tool-args"),
            ],
        )
        .await;
    let Err(CreateError::Invalid(issues)) = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
    else {
        panic!("expected both lines to be refused before admission");
    };
    assert_eq!(
        issues
            .iter()
            .map(|i| (i.line, i.error.code()))
            .collect::<Vec<_>>(),
        [(1, "unsupported_feature"), (2, "invalid_body")]
    );
    consistent(f).await;
}

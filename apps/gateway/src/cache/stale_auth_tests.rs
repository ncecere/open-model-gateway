//! A replica whose key cache has not yet learned that a key stopped being
//! valid (revoked, disabled, expired, membership or platform access removed)
//! refuses the key exactly as the uncached path does, on every key-
//! authenticated endpoint family: the live re-check (admission, candidate
//! listing, `/v1/models`) reports `authentication_error` (HTTP 401, same
//! body and `WWW-Authenticate`), never `model_not_found`, drops the
//! replica's cached entries of the key at once, and starts no upstream work.
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use super::invalidation_tests::{issue, replica, stale_replica};
use super::*;
use crate::{
    governance::tests::db::fixture,
    inference::{
        Engine, EngineLimits,
        error::InferenceError,
        repository::InferenceRepository,
        types::{
            ApiProtocol, Capabilities, ChatRequest, EmbeddingRequest, EmbeddingResponse,
            ImageRequest, ImageResponse, ProviderOutput, RerankRequest, RerankResponse,
            SpeechRequest, SpeechResponse, SystemoneRequest, SystemoneResponse,
        },
    },
    providers::{ProviderAdapter, ProviderRegistry},
    store::Store,
};

/// Accepts every request shape before admission and counts any upstream
/// call (there must be none).
#[derive(Clone, Default)]
struct Never(Arc<AtomicUsize>);
impl Never {
    fn called<T>(&self) -> Result<T, InferenceError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(InferenceError::UpstreamUnavailable)
    }
}
#[async_trait::async_trait]
impl ProviderAdapter for Never {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: true,
            tools: true,
        }
    }
    fn supports_protocol(&self, _: ApiProtocol) -> bool {
        true
    }
    fn supports_image_request(&self, _: &Deployment, _: &ImageRequest) -> bool {
        true
    }
    fn supports_speech_request(&self, _: &Deployment, _: &SpeechRequest) -> bool {
        true
    }
    async fn execute(
        &self,
        _: &Deployment,
        _: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        self.called()
    }
    async fn execute_embeddings(
        &self,
        _: &Deployment,
        _: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, InferenceError> {
        self.called()
    }
    async fn execute_images(
        &self,
        _: &Deployment,
        _: ImageRequest,
    ) -> Result<ImageResponse, InferenceError> {
        self.called()
    }
    async fn execute_audio_speech(
        &self,
        _: &Deployment,
        _: SpeechRequest,
    ) -> Result<SpeechResponse, InferenceError> {
        self.called()
    }
    async fn execute_rerank(
        &self,
        _: &Deployment,
        _: RerankRequest,
    ) -> Result<RerankResponse, InferenceError> {
        self.called()
    }
    async fn execute_systemone(
        &self,
        _: &Deployment,
        _: SystemoneRequest,
    ) -> Result<SystemoneResponse, InferenceError> {
        self.called()
    }
}

/// One model per engine workload (the fixture's model serves the three
/// generation protocols), each granted to both fixture workspaces.
const MODELS: [(&str, &str); 6] = [
    ("company/smart", ""),
    ("company/embed", "embeddings"),
    ("company/image", "images"),
    ("company/speech", "audio_speech"),
    ("company/rerank", "rerank"),
    ("company/systemone", "systemone"),
];

async fn models(pool: &PgPool, smart: Uuid, deployment: Uuid) {
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['chat_completions','responses','messages'] WHERE id=$1",
    )
    .bind(smart)
    .execute(pool)
    .await
    .unwrap();
    for (name, protocol) in MODELS.iter().skip(1) {
        let (model, id) = (Uuid::new_v4(), Uuid::new_v4());
        sqlx::query(
            "INSERT INTO models(id,public_name,supported_protocols) VALUES($1,$2,ARRAY[$3])",
        )
        .bind(model)
        .bind(name)
        .bind(protocol)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) SELECT $1,$2,provider_connection_id,'mock-model',true FROM deployments WHERE id=$3")
            .bind(id)
            .bind(model)
            .bind(deployment)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) SELECT id,$1,'direct' FROM workspaces")
            .bind(model)
            .execute(pool)
            .await
            .unwrap();
    }
}

fn router(store: &Store, adapter: &Never) -> Router {
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(adapter.clone())).unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    crate::http::router_with_engine(store.clone(), None, engine)
}

/// Every key-authenticated endpoint family: the engine routes (served from
/// the key cache) and the live-authenticated ones (files, batches, videos,
/// realtime).
fn requests() -> Vec<(&'static str, &'static str, Option<Value>)> {
    let chat = json!({"model":"company/smart","messages":[{"role":"user","content":"hi"}],"max_tokens":10});
    let mut stream = chat.clone();
    stream["stream"] = json!(true);
    vec![
        ("POST", "/v1/chat/completions", Some(chat)),
        ("POST", "/v1/chat/completions", Some(stream)),
        (
            "POST",
            "/v1/responses",
            Some(json!({"model":"company/smart","input":"hi","max_output_tokens":10})),
        ),
        (
            "POST",
            "/v1/messages",
            Some(
                json!({"model":"company/smart","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}),
            ),
        ),
        (
            "POST",
            "/v1/embeddings",
            Some(json!({"model":"company/embed","input":"hi"})),
        ),
        (
            "POST",
            "/v1/images/generations",
            Some(
                json!({"model":"company/image","prompt":"a lake","n":1,"size":"1024x1024","quality":"low","response_format":"b64_json"}),
            ),
        ),
        (
            "POST",
            "/v1/audio/speech",
            Some(json!({"model":"company/speech","input":"Hello world!","voice":"alloy"})),
        ),
        (
            "POST",
            "/v1/rerank",
            Some(json!({"model":"company/rerank","query":"cat","documents":["kitten","dog"]})),
        ),
        (
            "POST",
            "/v1/systemone",
            Some(
                json!({"model":"company/systemone","state":"hello there","questions":{"is_q":{"type":"noul","instructions":"Is the text a greeting?"}}}),
            ),
        ),
        ("GET", "/v1/models", None),
        ("GET", "/v1/files", None),
        (
            "POST",
            "/v1/batches",
            Some(
                json!({"input_file_id":"file-00000000000000000000000000000000","endpoint":"/v1/chat/completions","completion_window":"24h"}),
            ),
        ),
        ("GET", "/v1/batches", None),
        ("GET", "/v1/videos", None),
        ("GET", "/v1/realtime?model=company%2Fsmart", None),
    ]
}

/// Status, `WWW-Authenticate` and JSON body of `request` sent with `token`.
async fn send(
    app: &Router,
    token: &str,
    (method, path, body): &(&str, &str, Option<Value>),
) -> (StatusCode, Option<String>, Value) {
    let mut builder = Request::builder()
        .method(*method)
        .uri(*path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    if *path == "/v1/messages" {
        builder = builder.header("anthropic-version", "2023-06-01");
    }
    let request = match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => builder.body(Body::empty()),
    }
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let challenge = response
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (
        status,
        challenge,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A candidate-cache entry: (workspace, key, model) and its deployments.
type Candidates = ((Uuid, Uuid, String), Arc<Vec<Deployment>>);
/// What B cached for the key while it was valid.
struct Warm {
    hash: [u8; 32],
    entry: KeyEntry,
    candidates: Vec<Candidates>,
}
async fn warm(b: &Store, token: &str) -> Warm {
    let p = b.authenticate_inference(token).await.unwrap().unwrap();
    let hash = crate::auth::token_digest(token);
    let Lookup::Hit(entry) = b.caches.keys.get(&b.caches.versions, &hash) else {
        panic!("key cached")
    };
    let mut candidates = Vec::new();
    for (model, _) in MODELS {
        let list = b.deployments(&p, model).await.unwrap();
        assert_eq!(list.len(), 1, "{model}");
        let key = (p.workspace_id, p.key_id, model.to_owned());
        assert!(matches!(
            b.caches.candidates.get(&b.caches.versions, &key),
            Lookup::Hit(_)
        ));
        candidates.push((key, Arc::new(list)));
    }
    Warm {
        hash,
        entry,
        candidates,
    }
}
/// Put B's stale entries back (a refusal drops them), still under B's
/// unchanged versions: the next request authenticates from the cache.
fn restale(b: &Store, w: &Warm) {
    b.caches.versions.backdate_sync(std::time::Duration::ZERO);
    if let Lookup::Miss(stamp) = b.caches.keys.get(&b.caches.versions, &w.hash) {
        b.caches.keys.insert(w.hash, w.entry, stamp);
    }
    for (key, list) in &w.candidates {
        if let Lookup::Miss(stamp) = b.caches.candidates.get(&b.caches.versions, key) {
            b.caches.candidates.insert(key.clone(), list.clone(), stamp);
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_stale_replica_refuses_invalid_keys_exactly_like_the_uncached_path(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    models(&pool, f.model, f.deployment).await;
    let a = replica(&pool).await.pool.clone();
    let (team, personal) = (f.team.workspace_id, f.principal.workspace_id);
    let adapter = Never::default();
    let uncached = router(&replica(&pool).await, &adapter);
    let refused_openai = json!({"error":{"message":"Invalid or missing API key","type":"authentication_error","param":null,"code":"authentication_error"}});
    let refused_anthropic = json!({"type":"error","error":{"type":"authentication_error","message":"Invalid or missing API key"}});
    let cases: [(&str, Uuid, Uuid, String); 6] = [
        (
            "key revoked",
            team,
            f.owner,
            "UPDATE api_keys SET revoked_at=now() WHERE id=$1".into(),
        ),
        (
            "key disabled",
            team,
            f.owner,
            "UPDATE api_keys SET disabled_at=now() WHERE id=$1".into(),
        ),
        (
            "key expired",
            team,
            f.owner,
            "UPDATE api_keys SET expires_at=now()-interval '1 second' WHERE id=$1".into(),
        ),
        (
            "membership removed",
            team,
            f.other,
            format!(
                "UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id='{team}' AND user_id='{}' AND $1 IS NOT NULL",
                f.other
            ),
        ),
        (
            "owner disabled",
            personal,
            f.other,
            format!(
                "UPDATE users SET disabled_at=now() WHERE id='{}' AND $1 IS NOT NULL",
                f.other
            ),
        ),
        (
            "owner lost platform access",
            personal,
            f.owner,
            format!(
                "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id='{}' AND $1 IS NOT NULL",
                f.owner
            ),
        ),
    ];
    for (name, workspace, user, change) in cases {
        if workspace == personal && user == f.other {
            // The member's own personal workspace.
            sqlx::query("INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES(gen_random_uuid(),'Mine','personal',$1)")
                .bind(user)
                .execute(&pool)
                .await
                .unwrap();
        }
        let workspace = if workspace == personal && user == f.other {
            sqlx::query_scalar(
                "SELECT id FROM workspaces WHERE kind='personal' AND owner_user_id=$1",
            )
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap()
        } else {
            workspace
        };
        sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) SELECT $1,id,'direct' FROM models ON CONFLICT DO NOTHING")
            .bind(workspace)
            .execute(&pool)
            .await
            .unwrap();
        let (token, p) = issue(&pool, workspace, Some(user), None).await;
        let b = stale_replica(&pool).await;
        let stale = router(&b, &adapter);
        let w = warm(&b, &token).await;
        sqlx::query(&change)
            .bind(p.key_id)
            .execute(&a)
            .await
            .unwrap();
        for request in requests() {
            let path = request.1;
            restale(&b, &w);
            // B still authenticates the key from its cache...
            assert_eq!(
                b.authenticate_inference(&token)
                    .await
                    .unwrap()
                    .map(|x| x.key_id),
                Some(p.key_id),
                "{name} {path}: stale hit expected"
            );
            // ...yet answers exactly what the uncached path answers.
            let expected = send(&uncached, &token, &request).await;
            let body = if path == "/v1/messages" {
                &refused_anthropic
            } else {
                &refused_openai
            };
            assert_eq!(
                expected,
                (
                    StatusCode::UNAUTHORIZED,
                    Some("Bearer".into()),
                    body.clone()
                ),
                "{name} {path}: uncached"
            );
            assert_eq!(
                send(&stale, &token, &request).await,
                expected,
                "{name} {path}"
            );
            if !matches!(
                path,
                "/v1/files" | "/v1/batches" | "/v1/videos" | "/v1/realtime?model=company%2Fsmart"
            ) {
                // The live refusal dropped B's entries of the key at once.
                assert!(
                    matches!(
                        b.caches.keys.get(&b.caches.versions, &w.hash),
                        Lookup::Miss(_)
                    ),
                    "{name} {path}: key entry kept"
                );
                for (key, _) in &w.candidates {
                    assert!(
                        matches!(
                            b.caches.candidates.get(&b.caches.versions, key),
                            Lookup::Miss(_)
                        ),
                        "{name} {path}: candidates kept"
                    );
                }
            }
        }
        // After a refusal B's next authentication of the key is live.
        restale(&b, &w);
        let chat = requests().remove(0);
        assert_eq!(
            send(&stale, &token, &chat).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert!(b.authenticate_inference(&token).await.unwrap().is_none());
        let executions: i64 =
            sqlx::query_scalar("SELECT count(*) FROM inference_executions WHERE api_key_id=$1")
                .bind(p.key_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(executions, 0, "{name}: nothing admitted");
    }
    assert_eq!(adapter.0.load(Ordering::SeqCst), 0, "no upstream call");
}

/// The live candidate listing of a stale replica (key cached, candidates
/// not) refuses the key the same way and forgets it.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_live_candidate_listing_of_an_invalid_key_is_an_authentication_error(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let (token, p) = issue(&pool, f.team.workspace_id, Some(f.owner), None).await;
    let b = stale_replica(&pool).await;
    b.authenticate_inference(&token).await.unwrap().unwrap();
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(p.key_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        b.deployments(&p, "company/smart").await,
        Err(InferenceError::Unauthenticated)
    ));
    assert!(b.authenticate_inference(&token).await.unwrap().is_none());
    // `/v1/models` likewise.
    assert!(b.key_models(&p).await.unwrap().is_none());
}

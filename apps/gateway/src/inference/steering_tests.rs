//! No request input can choose an upstream destination or a credential (#196).
//!
//! LiteLLM CVE-2026-59823: a "client-side credentials" feature let a caller
//! supply `api_base` and keys in the request, which redirected the gateway's
//! outbound call, with the provider key, to the caller's server. Here the real
//! router, authentication, catalog, routing, admission and the real
//! OpenAI/Anthropic/OpenRouter/local adapters run against two mock servers:
//! the **approved** upstream the connections name, and an **attacker** that
//! every steering attempt points at. Every request either is rejected before
//! any connection, or leaves the gateway as the same request the clean control
//! produces (same method, path, Host and header set, server credential, never
//! the caller's key), and the attacker receives no connection at all.
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Instant,
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use crate::{
    bootstrap::{self, DevelopmentKeys},
    config::Environment,
    filestore::FileStoreRuntime,
    http,
    inference::{Engine, EngineLimits, error::InferenceError},
    providers::{
        ProviderRegistry,
        anthropic::AnthropicAdapter,
        local::{LocalAdapter, Profile, endpoints::ApprovedEndpoints},
        openai::OpenAiAdapter,
        openrouter::{OpenRouterAdapter, OpenRouterConfig},
        secrets::{Secret, SecretResolver},
    },
    store::Store,
    telemetry,
};

// --------------------------------------------------------------- mocks ----

#[derive(Clone, Debug)]
struct Hit {
    method: String,
    target: String,
    /// Lower-case names; values as received.
    headers: Vec<(String, String)>,
    body: String,
}
impl Hit {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
    fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.headers.iter().map(|(n, _)| n.as_str()).collect();
        names.sort_unstable();
        names
    }
    fn everything(&self) -> String {
        format!(
            "{} {} {:?} {}",
            self.method, self.target, self.headers, self.body
        )
    }
}

/// A mock upstream that records every request it receives.
struct Upstream {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<Hit>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Upstream {
    async fn start() -> Self {
        let hits: Arc<Mutex<Vec<Hit>>> = Arc::default();
        let app = Router::new().fallback(record).with_state(hits.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, hits, task }
    }
    fn authority(&self) -> String {
        format!("127.0.0.1:{}", self.addr.port())
    }
    fn base(&self) -> String {
        format!("http://{}/v1", self.authority())
    }
    fn take(&self) -> Vec<Hit> {
        std::mem::take(&mut *self.hits.lock().unwrap())
    }
}
impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn record(State(hits): State<Arc<Mutex<Vec<Hit>>>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, 8 << 20).await.unwrap_or_default();
    let target = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_owned(), |p| p.as_str().to_owned());
    let mut headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(n, v)| {
            (
                n.as_str().to_owned(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    headers.sort();
    let chat = target.ends_with("/chat/completions");
    hits.lock().unwrap().push(Hit {
        method: parts.method.to_string(),
        target,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    if chat {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json!({"id":"c1","object":"chat.completion","created":1,"model":"m",
                "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}})
            .to_string(),
        )
            .into_response();
    }
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json")],
        "{}",
    )
        .into_response()
}

/// Every connection reference resolves to a value that names it, so the test
/// can tell the server's credential from anything a caller sent.
struct Secrets;
impl SecretResolver for Secrets {
    fn resolve(&self, reference: &str) -> Result<Secret, InferenceError> {
        Secret::new(format!(
            "server-secret-{}",
            reference.trim_start_matches("env:")
        ))
    }
}

// ------------------------------------------------------------- fixture ----

struct World {
    app: Router,
    pool: PgPool,
    keys: DevelopmentKeys,
    approved: Upstream,
    attacker: Upstream,
    diagnostics: Arc<Mutex<Vec<FailureDiagnostic>>>,
}
impl World {
    async fn close(self) {
        drop(self.app);
        self.pool.close().await;
    }
}

const NA_METERS: [&str; 11] = [
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "cache_write_5m_tokens",
    "cache_write_1h_tokens",
    "output_images",
    "input_characters",
    "input_audio_seconds_ms",
    "output_audio_seconds_ms",
    "search_units",
];

fn tokens(meter: &str) -> Value {
    json!({"meter":meter,"microusd_per_batch":"1000000","batch":1000000,"unit_label":"/M tokens","sku_label":"Tokens"})
}
fn image(variant: &str) -> Value {
    json!({"meter":"output_images","microusd_per_batch":"5000","batch":1,"unit_label":"/image","sku_label":"Image","variant":variant})
}

/// What a route is priced by (v3 lines; every other meter is not applicable).
enum Price {
    /// Legacy token prices (chat, embeddings, System One).
    Tokens { output_limit: i32 },
    Lines {
        priced: Vec<Value>,
        max_units: &'static str,
        output_limit: i32,
    },
}

async fn add_route(
    pool: &PgPool,
    name: &str,
    protocol: &str,
    connection: Uuid,
    upstream: &str,
    price: Price,
) {
    let (model, deployment) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,$2,ARRAY[$3])")
        .bind(model)
        .bind(name)
        .bind(protocol)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,$4,true)")
        .bind(deployment).bind(model).bind(connection).bind(upstream).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) SELECT id,$1,'direct' FROM workspaces")
        .bind(model).execute(pool).await.unwrap();
    match price {
        Price::Tokens { output_limit } => {
            sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,2000000,1000,$3,1)")
                .bind(Uuid::new_v4()).bind(deployment).bind(output_limit).execute(pool).await.unwrap();
        }
        Price::Lines {
            priced,
            max_units,
            output_limit,
        } => {
            let mut lines: Vec<Value> = NA_METERS
                .iter()
                .filter(|m| !priced.iter().any(|p| p["meter"] == **m))
                .map(|m| json!({"meter":m,"not_applicable":true}))
                .collect();
            lines.extend(priced);
            lines.push(json!({"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"}));
            sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,1000,$3,3,$4,$5::jsonb)")
                .bind(Uuid::new_v4()).bind(deployment).bind(output_limit).bind(json!(lines)).bind(max_units).execute(pool).await.unwrap();
        }
    }
}

async fn connection(pool: &PgPool, provider: &str, endpoint: Option<&str>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,$2,$3,$4,$5,true)")
        .bind(id)
        .bind(format!("steering {provider}"))
        .bind(provider)
        .bind(format!("env:{}", provider.to_uppercase()))
        .bind(endpoint)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// The routes: one model per protocol, on the adapter that serves it.
/// (public name, client path, protocol, provider, upstream model, price)
async fn world(provided: PgPool) -> World {
    // These callers use fresh databases, without session SETs or pool callbacks.
    // Keep their connection options, but not SQLx's shared parent capacity or
    // short test-pool idle expiry. Open connections only as needed, up to five.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .min_connections(0)
        .connect_with((*provided.connect_options()).clone())
        .await
        .unwrap();
    let (approved, attacker) = (Upstream::start().await, Upstream::start().await);
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("INSERT INTO policy_budgets(layer,kind,period,amount_microusd) VALUES('type','personal','month',1000000000000)")
        .execute(&pool).await.unwrap();
    sqlx::query(
        "UPDATE installation_settings SET file_batch_enabled=true,file_user_files_enabled=true",
    )
    .execute(&pool)
    .await
    .unwrap();
    let base = approved.base();
    let local = connection(&pool, "openai_compatible", Some(&base)).await;
    let vllm = connection(&pool, "vllm", Some(&base)).await;
    let sglang = connection(&pool, "sglang", Some(&base)).await;
    let ollama = connection(&pool, "ollama", Some(&base)).await;
    let openai = connection(&pool, "openai", None).await;
    let anthropic = connection(&pool, "anthropic", None).await;
    let openrouter = connection(&pool, "openrouter", None).await;
    let tok = |output_limit| Price::Tokens { output_limit };
    let lines = |priced: Vec<Value>, max_units, output_limit| Price::Lines {
        priced,
        max_units,
        output_limit,
    };
    let search = || json!({"meter":"search_units","microusd_per_batch":"2000","batch":1,"unit_label":"/search","sku_label":"Search"});
    let rerank = || {
        lines(
            vec![tokens("input_tokens"), search()],
            "{\"search_units\":\"1\"}",
            0,
        )
    };
    let characters = || {
        lines(
            vec![
                json!({"meter":"input_characters","microusd_per_batch":"15000000","batch":1000000,"unit_label":"/M characters","sku_label":"Characters"}),
            ],
            "{}",
            0,
        )
    };
    let minutes = || {
        lines(
            vec![
                json!({"meter":"input_audio_seconds_ms","microusd_per_batch":"6000","batch":60000,"unit_label":"/minute","sku_label":"Audio"}),
            ],
            "{}",
            0,
        )
    };
    let images = |variants: &[&str]| {
        let mut priced = vec![tokens("input_tokens"), tokens("output_tokens")];
        priced.extend(variants.iter().map(|v| image(v)));
        lines(priced, "{}", 4000)
    };
    let routes: Vec<(&str, &str, Uuid, &str, Price)> = vec![
        (
            "t/local-chat",
            "chat_completions",
            local,
            "local-model",
            tok(1000),
        ),
        ("t/local-embed", "embeddings", local, "local-embed", tok(0)),
        ("t/local-rerank", "rerank", local, "local-rerank", rerank()),
        ("t/local-s1", "systemone", local, "local-s1", tok(0)),
        (
            "t/vllm-chat",
            "chat_completions",
            vllm,
            "vllm-model",
            tok(1000),
        ),
        ("t/vllm-embed", "embeddings", vllm, "vllm-embed", tok(0)),
        ("t/vllm-rerank", "rerank", vllm, "vllm-rerank", rerank()),
        (
            "t/sglang-chat",
            "chat_completions",
            sglang,
            "sglang-model",
            tok(1000),
        ),
        (
            "t/sglang-embed",
            "embeddings",
            sglang,
            "sglang-embed",
            tok(0),
        ),
        (
            "t/sglang-rerank",
            "rerank",
            sglang,
            "sglang-rerank",
            rerank(),
        ),
        (
            "t/ollama-chat",
            "chat_completions",
            ollama,
            "ollama-model",
            tok(1000),
        ),
        (
            "t/ollama-embed",
            "embeddings",
            ollama,
            "ollama-embed",
            tok(0),
        ),
        ("t/ollama-s1", "systemone", ollama, "ollama-s1", tok(0)),
        (
            "t/oa-chat",
            "chat_completions",
            openai,
            "gpt-upstream",
            tok(1000),
        ),
        (
            "t/oa-responses",
            "responses",
            openai,
            "gpt-upstream",
            tok(1000),
        ),
        ("t/oa-embed", "embeddings", openai, "embed-upstream", tok(0)),
        (
            "t/oa-images",
            "images",
            openai,
            "gpt-image-1",
            images(&["1024x1024", "1536x1024"]),
        ),
        (
            "t/oa-speech",
            "audio_speech",
            openai,
            "tts-upstream",
            characters(),
        ),
        (
            "t/oa-trans",
            "audio_transcriptions",
            openai,
            "whisper-upstream",
            minutes(),
        ),
        (
            "t/an-messages",
            "messages",
            anthropic,
            "claude-upstream",
            tok(1000),
        ),
        (
            "t/an-chat",
            "chat_completions",
            anthropic,
            "claude-upstream",
            tok(1000),
        ),
        (
            "t/or-chat",
            "chat_completions",
            openrouter,
            "vendor/model",
            tok(1000),
        ),
        (
            "t/or-embed",
            "embeddings",
            openrouter,
            "vendor/embed",
            tok(0),
        ),
        (
            "t/or-rerank",
            "rerank",
            openrouter,
            "vendor/rerank",
            rerank(),
        ),
        ("t/or-s1", "systemone", openrouter, "vendor/s1", tok(0)),
        (
            "t/or-images",
            "images",
            openrouter,
            "vendor/image",
            images(&["768", "1k", "1.5k", "2k", "4k"]),
        ),
        (
            "t/or-speech",
            "audio_speech",
            openrouter,
            "vendor/tts",
            characters(),
        ),
        (
            "t/or-trans",
            "audio_transcriptions",
            openrouter,
            "vendor/stt",
            minutes(),
        ),
    ];
    for (name, protocol, conn, upstream, price) in routes {
        add_route(&pool, name, protocol, conn, upstream, price).await;
    }
    let secrets: Arc<dyn SecretResolver> = Arc::new(Secrets);
    let mut registry = ProviderRegistry::default();
    let adapters: Vec<Arc<dyn crate::providers::ProviderAdapter>> = vec![
        Arc::new(OpenAiAdapter::for_test(secrets.clone(), base.clone())),
        Arc::new(AnthropicAdapter::for_test(secrets.clone(), base.clone())),
        Arc::new(OpenRouterAdapter::for_test(
            secrets.clone(),
            OpenRouterConfig::default(),
            base.clone(),
        )),
    ];
    for adapter in adapters {
        registry.register(adapter).unwrap();
    }
    for profile in [
        Profile::OpenAiCompatible,
        Profile::Vllm,
        Profile::Sglang,
        Profile::Ollama,
    ] {
        registry
            .register(Arc::new(LocalAdapter::new(
                profile,
                secrets.clone(),
                Arc::new(ApprovedEndpoints::for_test(&base)),
            )))
            .unwrap();
    }
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    let app = http::router_with_engine(store.clone(), None, engine)
        .layer(axum::Extension(FileStoreRuntime::memory()));
    World {
        app,
        pool,
        keys,
        approved,
        attacker,
        diagnostics: Arc::default(),
    }
}

// ------------------------------------------------------------ requests ----

fn json_request(key: &str, path: &str, headers: &[(String, String)], body: &Value) -> Request {
    let mut builder = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

#[derive(Serialize)]
struct PhaseTiming {
    name: &'static str,
    duration_ns: u64,
}

#[derive(Serialize)]
struct FailureDiagnostic {
    elapsed_ns: u64,
    phases: Option<Vec<PhaseTiming>>,
    status: u16,
    accounting_unavailable: bool,
    approved_hits: usize,
    attacker_hits: Option<usize>,
}

/// Read only the three whitelisted wire fields; never serialize IDs or attributes
/// into an intermediate value. Names must resolve back to the fixed enum.
fn project_phases(spans: &[telemetry::Span]) -> Option<Vec<PhaseTiming>> {
    use serde::ser::{Error, Impossible, SerializeMap, Serializer};

    #[derive(Default)]
    struct Projection {
        name: Option<&'static str>,
        start: Option<u64>,
        end: Option<u64>,
    }
    impl SerializeMap for Projection {
        type Ok = PhaseTiming;
        type Error = serde_json::Error;

        fn serialize_key<T: ?Sized + Serialize>(&mut self, _: &T) -> Result<(), Self::Error> {
            Err(Self::Error::custom("phase projection requires entries"))
        }
        fn serialize_value<T: ?Sized + Serialize>(&mut self, _: &T) -> Result<(), Self::Error> {
            Err(Self::Error::custom("phase projection requires entries"))
        }
        fn serialize_entry<K: ?Sized + Serialize, V: ?Sized + Serialize>(
            &mut self,
            key: &K,
            value: &V,
        ) -> Result<(), Self::Error> {
            let key = serde_json::to_value(key)?;
            match key.as_str() {
                Some("name") => {
                    let value = serde_json::to_value(value)?;
                    self.name = telemetry::SpanName::ALL
                        .iter()
                        .map(|name| name.as_str())
                        .find(|name| Some(*name) == value.as_str());
                }
                Some("startTimeUnixNano" | "endTimeUnixNano") => {
                    let value = serde_json::to_value(value)?;
                    let nanos = value.as_str().and_then(|n| n.parse::<u64>().ok());
                    if key == "startTimeUnixNano" {
                        self.start = nanos;
                    } else {
                        self.end = nanos;
                    }
                }
                _ => {} // Do not visit the value of any non-whitelisted field.
            }
            Ok(())
        }
        fn end(self) -> Result<Self::Ok, Self::Error> {
            let unavailable = || Self::Error::custom("phase projection unavailable");
            Ok(PhaseTiming {
                name: self.name.ok_or_else(unavailable)?,
                duration_ns: self
                    .end
                    .zip(self.start)
                    .and_then(|(end, start)| end.checked_sub(start))
                    .ok_or_else(unavailable)?,
            })
        }
    }

    // Span's wire shape is a map. Reject every other shape rather than falling
    // back to serializing a whole span (including its sensitive metadata).
    macro_rules! reject_shapes {
        ($($method:ident($($arg:ident: $ty:ty),*));* $(;)?) => {
            $(fn $method(self, $(_: $ty),*) -> Result<Self::Ok, Self::Error> {
                Err(Self::Error::custom("phase projection requires a map"))
            })*
        };
    }
    impl Serializer for Projection {
        type Ok = PhaseTiming;
        type Error = serde_json::Error;
        type SerializeSeq = Impossible<Self::Ok, Self::Error>;
        type SerializeTuple = Impossible<Self::Ok, Self::Error>;
        type SerializeTupleStruct = Impossible<Self::Ok, Self::Error>;
        type SerializeTupleVariant = Impossible<Self::Ok, Self::Error>;
        type SerializeMap = Self;
        type SerializeStruct = Impossible<Self::Ok, Self::Error>;
        type SerializeStructVariant = Impossible<Self::Ok, Self::Error>;

        fn serialize_map(self, _: Option<usize>) -> Result<Self, Self::Error> {
            Ok(self)
        }
        reject_shapes! {
            serialize_bool(v: bool);
            serialize_i8(v: i8); serialize_i16(v: i16); serialize_i32(v: i32);
            serialize_i64(v: i64); serialize_i128(v: i128);
            serialize_u8(v: u8); serialize_u16(v: u16); serialize_u32(v: u32);
            serialize_u64(v: u64); serialize_u128(v: u128);
            serialize_f32(v: f32); serialize_f64(v: f64);
            serialize_char(v: char); serialize_str(v: &str); serialize_bytes(v: &[u8]);
            serialize_none(); serialize_unit(); serialize_unit_struct(name: &'static str);
            serialize_unit_variant(name: &'static str, index: u32, variant: &'static str);
        }
        fn serialize_some<T: ?Sized + Serialize>(self, _: &T) -> Result<Self::Ok, Self::Error> {
            Err(Self::Error::custom("phase projection requires a map"))
        }
        fn serialize_newtype_struct<T: ?Sized + Serialize>(
            self,
            _: &'static str,
            _: &T,
        ) -> Result<Self::Ok, Self::Error> {
            Err(Self::Error::custom("phase projection requires a map"))
        }
        fn serialize_newtype_variant<T: ?Sized + Serialize>(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: &T,
        ) -> Result<Self::Ok, Self::Error> {
            Err(Self::Error::custom("phase projection requires a map"))
        }
        fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
            Err(Self::Error::custom("phase projection requires a map"))
        }
        fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Self::Error> {
            Err(Self::Error::custom("phase projection requires a map"))
        }
        fn serialize_tuple_struct(
            self,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeTupleStruct, Self::Error> {
            Err(Self::Error::custom("phase projection requires a map"))
        }
        fn serialize_tuple_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeTupleVariant, Self::Error> {
            Err(Self::Error::custom("phase projection requires a map"))
        }
        fn serialize_struct(
            self,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeStruct, Self::Error> {
            Err(Self::Error::custom("phase projection requires a map"))
        }
        fn serialize_struct_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeStructVariant, Self::Error> {
            Err(Self::Error::custom("phase projection requires a map"))
        }
    }

    if spans.is_empty() {
        return None;
    }
    spans
        .iter()
        .map(|span| span.serialize(Projection::default()).ok())
        .collect()
}

struct Observed {
    status: StatusCode,
    text: String,
    elapsed_ns: u64,
    phases: Option<Vec<PhaseTiming>>,
}

async fn send_observed(app: &Router, request: Request) -> Observed {
    let started = Instant::now();
    let buffer = Arc::new(Mutex::new(telemetry::PhaseBuffer::new(
        [1; 16],
        [1; 8],
        telemetry::now_unix_nanos(),
    )));
    // Engine::new has no recorder: its None phase groups await directly, leaving
    // this outer task-local capture intact. No exporter or eligibility is used.
    // Capture starts before HTTP auth/catalog: the first duration includes that
    // lead-in. These are completed boundaries, not precise phase-queue SLOs;
    // elapsed time or the last boundary cannot identify a timeout's cause.
    let (status, text) = telemetry::with_phases(buffer.clone(), async {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 16 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    })
    .await;
    let elapsed_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    let phases = Arc::try_unwrap(buffer)
        .ok()
        .and_then(|mutex| mutex.into_inner().ok())
        .and_then(|buffer| project_phases(&buffer.take().0));
    Observed {
        status,
        text,
        elapsed_ns,
        phases,
    }
}

async fn send(app: &Router, request: Request) -> (StatusCode, String) {
    let observed = send_observed(app, request).await;
    (observed.status, observed.text)
}

/// `error.code`, else `error.type`, of a gateway error body ("" otherwise).
fn reason(text: &str) -> String {
    let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    value["error"]["code"]
        .as_str()
        .or(value["error"]["type"].as_str())
        .unwrap_or("")
        .to_owned()
}

/// What a steering attempt must do, exactly.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Expect {
    /// Refused with this status and error reason; no upstream connection.
    Rejected(u16, &'static str),
    /// Served as the clean control: the same response and exactly one upstream
    /// request equal to the control's.
    Forwarded,
}
use Expect::{Forwarded, Rejected};

/// The clean request's outcome, which "forwarded" cases must reproduce.
struct Control {
    hit: Hit,
    status: u16,
    reason: String,
}

async fn control(w: &World, label: &str, request: Request) -> Control {
    let (status, text) = send(&w.app, request).await;
    let mut hits = w.approved.take();
    assert_eq!(
        hits.len(),
        1,
        "{label}: the control reached the upstream {} times ({status}: {text})",
        hits.len()
    );
    let hit = hits.remove(0);
    assert!(
        ["authorization", "x-api-key"]
            .iter()
            .any(|h| hit.header(h).is_some_and(
                |v| v.starts_with("server-secret-") || v.starts_with("Bearer server-secret-")
            )),
        "{label}: the upstream credential is not the server's"
    );
    for key in [&w.keys.personal_key.token, &w.keys.team_key.token] {
        assert!(
            !hit.everything().contains(key.as_str()),
            "{label}: the caller's key was forwarded"
        );
    }
    assert_no_attacker(w, label, "control");
    // This fixture needs no 5xx baseline; Anthropic storage errors have no code.
    assert!(
        !status.is_server_error(),
        "{label}: a server error is not a clean control ({})",
        status.as_u16()
    );
    let reason = reason(&text);
    assert_ne!(
        reason, "accounting_unavailable",
        "{label}: an accounting failure is not a clean control ({status})"
    );
    Control {
        hit,
        status: status.as_u16(),
        reason,
    }
}

/// How a forwarded request's body is compared to the control's.
#[derive(Clone, Copy, PartialEq)]
enum Compare {
    Equal,
    /// Multipart: the boundary is random, the length is not.
    SameLength,
}

async fn expect(
    w: &World,
    label: &str,
    case: &str,
    request: Request,
    want: Expect,
    control: &Control,
    compare: Compare,
) {
    let Observed {
        status,
        text,
        elapsed_ns,
        phases,
    } = send_observed(&w.app, request).await;
    let hits = w.approved.take();
    let got = (status.as_u16(), reason(&text));
    let mismatch = match want {
        Rejected(code, why) => got != (code, why.to_owned()),
        Forwarded => got != (control.status, control.reason.clone()),
    };
    if mismatch {
        let attacker_hits = w.attacker.hits.lock().ok().map(|hits| hits.len());
        let mut diagnostic = FailureDiagnostic {
            elapsed_ns,
            phases,
            status: status.as_u16(),
            accounting_unavailable: got.1 == "accounting_unavailable",
            approved_hits: hits.len(),
            attacker_hits,
        };
        let mut entries = w.diagnostics.lock().ok();
        if entries.is_none() || attacker_hits.is_none() {
            diagnostic.phases = None;
        }
        let rendered = serde_json::to_string(&diagnostic).ok().or_else(|| {
            diagnostic.phases = None;
            serde_json::to_string(&diagnostic).ok()
        });
        if let Some(entries) = entries.as_mut() {
            entries.push(diagnostic);
        }
        drop(entries);
        if let Some(rendered) = rendered {
            use std::io::Write;
            let _ = writeln!(std::io::stderr().lock(), "{rendered}");
        }
    }
    match want {
        Rejected(code, why) => {
            assert_eq!(got, (code, why.to_owned()), "{label} {case}");
            assert!(
                hits.is_empty(),
                "{label} {case}: a rejected request reached the upstream"
            );
        }
        Forwarded => {
            assert_eq!(
                got,
                (control.status, control.reason.clone()),
                "{label} {case}: response differs from the control's"
            );
            assert_eq!(
                hits.len(),
                1,
                "{label} {case}: {} upstream requests",
                hits.len()
            );
            assert_clean(label, case, &hits[0], &control.hit, w);
            match compare {
                Compare::Equal => assert_eq!(
                    hits[0].body, control.hit.body,
                    "{label} {case}: the body changed"
                ),
                Compare::SameLength => assert_eq!(
                    hits[0].body.len(),
                    control.hit.body.len(),
                    "{label} {case}: the body changed"
                ),
            }
        }
    }
    assert_no_attacker(w, label, case);
}

/// A JSON protocol: its client path, model and a valid request body.
#[derive(Clone, Copy, PartialEq)]
enum Labels {
    /// No `user` or `metadata`: both are unknown fields.
    No,
    /// OpenAI `user` and string `metadata`: client labels, never forwarded.
    OpenAi,
    /// Anthropic `metadata.user_id` only.
    Anthropic,
}

struct Target {
    label: &'static str,
    path: &'static str,
    model: &'static str,
    body: fn(&str) -> Value,
    labels: Labels,
    /// Headers the protocol itself requires.
    headers: &'static [(&'static str, &'static str)],
}
impl Target {
    fn messages(&self) -> bool {
        self.path == "/v1/messages"
    }
    fn base(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect()
    }
    /// The refusal of a malformed or unknown field, and of an over-long model.
    fn invalid(&self) -> Expect {
        Rejected(
            if self.label.starts_with("systemone") {
                422
            } else {
                400
            },
            "invalid_request_error",
        )
    }
    fn not_found(&self) -> Expect {
        Rejected(
            404,
            if self.messages() {
                "not_found_error"
            } else {
                "model_not_found"
            },
        )
    }
}

fn targets() -> Vec<Target> {
    let chat = |m: &str| json!({"model":m,"messages":[{"role":"user","content":"hi"}],"max_completion_tokens":5});
    let embed = |m: &str| json!({"model":m,"input":"hi"});
    let rerank = |m: &str| json!({"model":m,"query":"cat","documents":["kitten","airplane","dog"],"top_n":2});
    let s1 = |m: &str| json!({"model":m,"state":"hello there","questions":{"is_q":{"type":"noul","instructions":"Is the text a greeting?"}}});
    let speech = |m: &str| json!({"model":m,"input":"Hello","voice":"alloy"});
    let t = |label, path, model, body: fn(&str) -> Value, labels| Target {
        label,
        path,
        model,
        body,
        labels,
        headers: &[],
    };
    let chats = "/v1/chat/completions";
    let embeds = "/v1/embeddings";
    let mut all = vec![
        t("chat/local", chats, "t/local-chat", chat, Labels::OpenAi),
        t("chat/vllm", chats, "t/vllm-chat", chat, Labels::OpenAi),
        t("chat/sglang", chats, "t/sglang-chat", chat, Labels::OpenAi),
        t("chat/ollama", chats, "t/ollama-chat", chat, Labels::OpenAi),
        t("chat/openai", chats, "t/oa-chat", chat, Labels::OpenAi),
        t("chat/anthropic", chats, "t/an-chat", chat, Labels::OpenAi),
        t("chat/openrouter", chats, "t/or-chat", chat, Labels::OpenAi),
        t(
            "responses/openai",
            "/v1/responses",
            "t/oa-responses",
            |m| json!({"model":m,"input":"hi","max_output_tokens":16}),
            Labels::OpenAi,
        ),
        t(
            "embeddings/local",
            embeds,
            "t/local-embed",
            embed,
            Labels::No,
        ),
        t("embeddings/vllm", embeds, "t/vllm-embed", embed, Labels::No),
        t(
            "embeddings/sglang",
            embeds,
            "t/sglang-embed",
            embed,
            Labels::No,
        ),
        t(
            "embeddings/ollama",
            embeds,
            "t/ollama-embed",
            embed,
            Labels::No,
        ),
        t("embeddings/openai", embeds, "t/oa-embed", embed, Labels::No),
        t(
            "embeddings/openrouter",
            embeds,
            "t/or-embed",
            embed,
            Labels::No,
        ),
        t(
            "images/openai",
            "/v1/images/generations",
            "t/oa-images",
            |m| json!({"model":m,"prompt":"cat","n":1,"size":"1024x1024"}),
            Labels::No,
        ),
        t(
            "images/openrouter",
            "/v1/images/generations",
            "t/or-images",
            |m| json!({"model":m,"prompt":"cat","n":1,"size":"1K"}),
            Labels::No,
        ),
        t(
            "speech/openai",
            "/v1/audio/speech",
            "t/oa-speech",
            speech,
            Labels::No,
        ),
        t(
            "speech/openrouter",
            "/v1/audio/speech",
            "t/or-speech",
            speech,
            Labels::No,
        ),
        t(
            "rerank/local",
            "/v1/rerank",
            "t/local-rerank",
            rerank,
            Labels::No,
        ),
        t(
            "rerank/vllm",
            "/v1/rerank",
            "t/vllm-rerank",
            rerank,
            Labels::No,
        ),
        t(
            "rerank/sglang",
            "/v1/rerank",
            "t/sglang-rerank",
            rerank,
            Labels::No,
        ),
        t(
            "rerank/openrouter",
            "/v1/rerank",
            "t/or-rerank",
            rerank,
            Labels::No,
        ),
        t(
            "systemone/local",
            "/v1/systemone",
            "t/local-s1",
            s1,
            Labels::No,
        ),
        t(
            "systemone/ollama",
            "/v1/systemone",
            "t/ollama-s1",
            s1,
            Labels::No,
        ),
        t(
            "systemone/openrouter",
            "/v1/systemone",
            "t/or-s1",
            s1,
            Labels::No,
        ),
    ];
    all.push(Target {
        label: "messages/anthropic",
        path: "/v1/messages",
        model: "t/an-messages",
        body: |m| json!({"model":m,"max_tokens":5,"messages":[{"role":"user","content":"hi"}]}),
        labels: Labels::Anthropic,
        headers: &[("anthropic-version", "2023-06-01")],
    });
    all
}

/// Fields other gateways accept to pick a destination or a credential, and
/// the OpenRouter routing fields (`models`, `route`, `provider`, ...).
const STEERING_FIELDS: &[&str] = &[
    "api_base",
    "base_url",
    "baseURL",
    "endpoint",
    "endpoint_url",
    "api_endpoint",
    "azure_endpoint",
    "api_key",
    "credentials",
    "client_credentials",
    "provider",
    "custom_llm_provider",
    "extra_headers",
    "extra_body",
    "extra_query",
    "headers",
    "vertex_credentials",
    "vertex_project",
    "vertex_location",
    "aws_access_key_id",
    "aws_secret_access_key",
    "aws_region_name",
    "organization",
    "proxy",
    "url",
    "deployment_id",
    "region",
    "model_list",
    "litellm_params",
    "fallbacks",
    "models",
    "route",
    "transforms",
    "plugins",
    "allow_fallbacks",
    "data_collection",
];

/// OpenRouter's documented shapes for the fields above.
fn openrouter_fields(attacker: &Upstream) -> Vec<(&'static str, Value)> {
    let url = format!("http://{}/v1", attacker.authority());
    vec![
        (
            "provider",
            json!({"order":["attacker"],"allow_fallbacks":true,"data_collection":"allow","only":[url]}),
        ),
        (
            "provider",
            json!({"ignore":["openai"],"require_parameters":false}),
        ),
        ("models", json!(["attacker/model", "openai/gpt-4o"])),
        ("route", json!("fallback")),
        ("transforms", json!(["middle-out"])),
        ("plugins", json!([{"id":"web","url":url}])),
    ]
}

/// Steering request headers: none may reach the upstream or move it.
fn steering_headers(attacker: &Upstream) -> Vec<(String, String)> {
    let (host, url) = (
        attacker.authority(),
        format!("http://{}/v1", attacker.authority()),
    );
    let pairs = [
        ("x-forwarded-host", host.clone()),
        ("x-forwarded-server", host.clone()),
        ("x-forwarded-proto", "http".to_owned()),
        ("x-forwarded-for", "169.254.169.254".to_owned()),
        ("forwarded", format!("for=attacker;host={host}")),
        ("host", host.clone()),
        ("x-original-url", url.clone()),
        ("x-rewrite-url", url.clone()),
        ("x-base-url", url.clone()),
        ("x-api-base", url.clone()),
        ("api-base", url.clone()),
        ("x-litellm-api-base", url.clone()),
        ("x-litellm-api-key", "sk-attacker-litellm".to_owned()),
        ("x-upstream-url", url.clone()),
        ("destination", url.clone()),
        ("origin", url.clone()),
        ("referer", url.clone()),
        ("openai-organization", "org-attacker".to_owned()),
        ("openai-project", "proj-attacker".to_owned()),
        ("openai-beta", "attacker=v1".to_owned()),
        ("anthropic-version", "9999-01-01".to_owned()),
        ("anthropic-beta", "attacker-beta".to_owned()),
        ("anthropic-api-key", "sk-ant-attacker".to_owned()),
        ("x-goog-api-key", "attacker-goog".to_owned()),
        ("x-amz-security-token", "attacker-token".to_owned()),
        ("proxy-authorization", "Basic YXR0YWNrZXI6eA==".to_owned()),
        ("x-provider", "attacker".to_owned()),
        ("x-model", "attacker/model".to_owned()),
        ("x-http-method-override", "GET".to_owned()),
        ("x-forwarded-authorization", "Bearer sk-attacker".to_owned()),
        // Client labels the gateway records for Logs and OpenRouter attribution.
        ("x-session-id", "sess-attacker".to_owned()),
        ("x-title", "title-attacker".to_owned()),
        ("http-referer", url),
    ];
    pairs.into_iter().map(|(n, v)| (n.to_owned(), v)).collect()
}

/// A header the protocol itself reads is refused when it is wrong; every other
/// one is ignored.
fn header_expect(t: &Target, name: &str) -> Expect {
    if t.messages() && name == "anthropic-version" {
        return Rejected(400, "invalid_request_error");
    }
    Forwarded
}

fn model_strings(model: &str, attacker: &Upstream) -> Vec<String> {
    let (host, url) = (
        attacker.authority(),
        format!("http://{}/v1", attacker.authority()),
    );
    let swapped = model.replacen('t', "\u{0442}", 1); // Cyrillic te for the Latin t
    let fullwidth = model.replacen('t', "\u{ff54}", 1); // fullwidth t
    vec![
        format!("openai/{url}"),
        format!("openai/{model}@{host}"),
        url.clone(),
        format!("{url}/{model}"),
        format!("{host}/{model}"),
        "../../etc/passwd".to_owned(),
        format!("{model}/../../x"),
        format!("../{model}"),
        format!("{model}@{host}"),
        format!("{model}#@{host}"),
        format!("{model}?api_base={url}"),
        format!("{model}%2F..%2F{host}"),
        model.replace('/', "%2F"),
        model.replace('/', "\\"),
        format!("openai/{model}"),
        format!("anthropic/{model}"),
        format!("{model}:{url}"),
        swapped,
        fullwidth,
        format!("{model}\u{200b}"),
        format!("\u{200b}{model}"),
        format!("{model}\u{202e}"),
        model.to_uppercase(),
        format!(" {model}"),
        format!("{model} "),
        format!("{model}\n"),
        format!("{model}\u{0}"),
        "\u{0}".to_owned(),
        "x".repeat(300),
        format!("{model}\u{0}{url}"),
    ]
}

/// A hit that left the gateway for the approved upstream must be exactly what
/// the clean control produced: same method, path and query, same header names,
/// the server's credential, and nothing a caller chose.
fn assert_clean(label: &str, case: &str, hit: &Hit, control: &Hit, world: &World) {
    let all = hit.everything();
    assert_eq!(hit.method, control.method, "{label} {case}");
    assert_eq!(
        hit.target, control.target,
        "{label} {case}: the upstream path moved"
    );
    assert_eq!(
        hit.names(),
        control.names(),
        "{label} {case}: upstream headers differ"
    );
    assert_eq!(
        hit.header("host"),
        Some(world.approved.authority().as_str()),
        "{label} {case}: Host"
    );
    for credential in ["authorization", "x-api-key"] {
        assert_eq!(
            hit.header(credential),
            control.header(credential),
            "{label} {case}: {credential}"
        );
    }
    let attacker = world.attacker.authority();
    for forbidden in [
        world.keys.personal_key.token.as_str(),
        world.keys.team_key.token.as_str(),
        attacker.as_str(),
        "attacker",
        "169.254.169.254",
    ] {
        assert!(
            !all.contains(forbidden),
            "{label} {case}: upstream request carries a caller value"
        );
    }
}

fn assert_no_attacker(world: &World, label: &str, case: &str) {
    assert_eq!(
        world.attacker.take().len(),
        0,
        "{label} {case}: the attacker was contacted"
    );
}

// ---------------------------------------------------------------- tests ----

/// Holding the caller's only connection must not starve the world's store.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn steering_world_dispatch_is_independent_of_the_provided_pool(pool: PgPool) {
    // No SQLx parent: exhausting this pool cannot consume the shared test pool's
    // capacity or starve unrelated fixtures running alongside this regression.
    let provided = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_millis(100))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let w = world(provided.clone()).await;
    let held = provided.acquire().await.unwrap();
    let (status, text) = send(
        &w.app,
        json_request(
            &w.keys.personal_key.token,
            "/v1/embeddings",
            &[],
            &json!({"model":"t/ollama-embed","input":"hi"}),
        ),
    )
    .await;
    let approved = w.approved.take();
    let attacker = w.attacker.take();

    // Clean up before the outcome assertions, including on the expected RED.
    drop(held);
    w.close().await;
    provided.close().await;

    assert_eq!(
        (status.as_u16(), reason(&text)),
        (400, "upstream_rejected".to_owned()),
        "the clean Ollama embedding dispatch must not depend on the held pool: {text}"
    );
    assert_eq!(approved.len(), 1, "exactly one approved upstream hop");
    assert!(attacker.is_empty(), "the attacker must not be contacted");
}

/// Neither protocol's storage-error envelope is a baseline, even after a real hop.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn steering_control_rejects_accounting_unavailable_after_a_real_upstream_hop(pool: PgPool) {
    use axum::middleware::{Next, from_fn};
    use futures_util::FutureExt;
    use std::panic::AssertUnwindSafe;

    let mut w = world(pool).await;
    let mut outcomes = Vec::new();
    for (target_label, clean_reason, envelope) in [
        (
            "embeddings/ollama",
            "upstream_rejected",
            json!({"error":{"message":InferenceError::Storage.message(),"type":"accounting_unavailable","code":"accounting_unavailable","param":null}}),
        ),
        (
            "messages/anthropic",
            "invalid_request_error",
            // Match messages::error_body(Storage): api_error, without error.code.
            json!({"type":"error","error":{"type":"api_error","message":InferenceError::Storage.message()}}),
        ),
    ] {
        let t = targets()
            .into_iter()
            .find(|t| t.label == target_label)
            .expect("the real protocol target exists");
        let request = || {
            json_request(
                &w.keys.personal_key.token,
                t.path,
                &t.base(),
                &(t.body)(t.model),
            )
        };
        let clean = control(&w, t.label, request()).await;
        assert_eq!(
            (clean.status, clean.reason.as_str()),
            (400, clean_reason),
            "{}: the unwrapped dispatch must be a valid clean control",
            t.label
        );
        let wrapped_request = request();
        let observed = Arc::new(Mutex::new(None));
        let observation = observed.clone();
        let approved_hits = w.approved.hits.clone();
        let attacker_hits = w.attacker.hits.clone();
        let unwrapped = w.app.clone();
        w.app = unwrapped
            .clone()
            .layer(from_fn(move |request: Request, next: Next| {
                let observation = observation.clone();
                let approved_hits = approved_hits.clone();
                let attacker_hits = attacker_hits.clone();
                let envelope = envelope.clone();
                async move {
                    // Run the actual router/admission/adapter before replacing anything.
                    let response = next.run(request).await;
                    let status = response.status();
                    let bytes = to_bytes(response.into_body(), 16 << 20).await.unwrap();
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    // Snapshot both logs before control's assertions can drain either.
                    *observation.lock().unwrap() = Some((
                        status,
                        text,
                        approved_hits.lock().unwrap().clone(),
                        attacker_hits.lock().unwrap().clone(),
                    ));
                    (StatusCode::SERVICE_UNAVAILABLE, axum::Json(envelope)).into_response()
                }
            }));

        let label = format!("{} injected accounting failure", t.label);
        let outcome = AssertUnwindSafe(control(&w, &label, wrapped_request))
            .catch_unwind()
            .await;
        // Remove the wrapper between cases; no spare router may retain the pool.
        w.app = unwrapped;

        // These checks are outside catch_unwind: a broken dispatch or middleware
        // must not be mistaken for the control helper rejecting the injected error.
        let (status, text, hits, attacker) =
            observed.lock().unwrap().take().expect("middleware ran");
        assert_eq!(
            (status.as_u16(), reason(&text)),
            (400, clean_reason.to_owned()),
            "{}: the wrapped dispatch must still reach the real clean outcome: {text}",
            t.label
        );
        assert_eq!(hits.len(), 1, "exactly one approved upstream hop");
        assert_clean(
            t.label,
            "injected accounting failure",
            &hits[0],
            &clean.hit,
            &w,
        );
        assert_eq!(hits[0].body, clean.hit.body, "the clean body is unchanged");
        assert!(
            attacker.is_empty(),
            "{}: the pre-assertion snapshot must contain no attacker requests",
            t.label
        );
        assert_no_attacker(&w, t.label, "injected accounting failure");
        outcomes.push((label, outcome));
    }
    w.close().await;
    for (label, outcome) in outcomes {
        let panic_message = outcome.as_ref().err().and_then(|payload| {
            payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
        });
        let expected = format!("{label}: a server error is not a clean control (503)");
        assert!(
            panic_message == Some(expected.as_str()),
            "{label}: control must fail specifically at the server-error baseline guard"
        );
    }
}

/// Diagnostics project completed spans, never their correlation or usage metadata.
#[test]
fn steering_phase_projection_keeps_only_fixed_names_and_durations() {
    use crate::telemetry::{Attribute, Span, SpanName};

    let canary_id = Uuid::from_bytes(*b"NoSensitiveCanar");
    let mut span = Span::reserved(
        SpanName::AdmissionConnect,
        *canary_id.as_bytes(),
        [2; 8],
        Some([3; 8]),
        1_000,
        1_042,
    )
    .unwrap();
    assert!(span.attribute(Attribute::RequestId(canary_id)));
    assert!(span.attribute(Attribute::InputTokens(987654321)));
    let raw = serde_json::to_value(&span).unwrap();
    assert!(raw.to_string().contains(&canary_id.to_string()));
    assert!(raw.to_string().contains("987654321"));

    let projected = project_phases(&[span]).expect("a completed phase is available");
    assert_eq!(
        serde_json::to_value(projected).unwrap(),
        json!([{"name":"admission.connect","duration_ns":42}]),
        "no IDs, timestamps, attributes, tokens or other span metadata may survive"
    );
}

#[test]
fn steering_phase_projection_reports_missing_capture_as_unavailable() {
    assert!(project_phases(&[]).is_none());
}

/// An injected accounting error must still fail the original strict comparison,
/// but only after a secret-safe diagnostic has captured the real hop's phases.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn steering_expect_records_diagnostics_before_rejecting_injected_503(pool: PgPool) {
    use axum::middleware::{Next, from_fn};
    use futures_util::FutureExt;
    use std::{panic::AssertUnwindSafe, time::Instant};

    let mut w = world(pool).await;
    let t = targets()
        .into_iter()
        .find(|t| t.label == "rerank/sglang")
        .expect("the original failing matrix target exists");
    let body = (t.body)(t.model);
    let clean = control(
        &w,
        t.label,
        json_request(&w.keys.personal_key.token, t.path, &t.base(), &body),
    )
    .await;
    assert_eq!(
        (clean.status, clean.reason.as_str()),
        (400, "upstream_rejected")
    );
    assert!(w.diagnostics.lock().unwrap().is_empty());

    let observed = Arc::new(Mutex::new(None));
    let observation = observed.clone();
    let approved_hits = w.approved.hits.clone();
    let attacker_hits = w.attacker.hits.clone();
    let unwrapped = w.app.clone();
    w.app = unwrapped
        .clone()
        .layer(from_fn(move |request: Request, next: Next| {
            let observation = observation.clone();
            let approved_hits = approved_hits.clone();
            let attacker_hits = attacker_hits.clone();
            async move {
                // No fabricated upstream hit: finish and snapshot the actual dispatch.
                let response = next.run(request).await;
                let status = response.status();
                let bytes = to_bytes(response.into_body(), 16 << 20).await.unwrap();
                *observation.lock().unwrap() = Some((
                    status,
                    String::from_utf8_lossy(&bytes).into_owned(),
                    approved_hits.lock().unwrap().clone(),
                    attacker_hits.lock().unwrap().clone(),
                ));
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [("x-diagnostic-canary", "NoSensitiveCanary")],
                    axum::Json(json!({"error":{
                        "message":"NoSensitiveCanary",
                        "type":"accounting_unavailable",
                        "code":"accounting_unavailable",
                        "param":"NoSensitiveCanary"
                    }})),
                )
                    .into_response()
            }
        }));
    let headers = [
        t.base(),
        vec![("openai-beta".into(), "NoSensitiveCanary".into())],
    ]
    .concat();
    let case = "openai-beta NoSensitiveCanary";
    let started = Instant::now();
    let outcome = AssertUnwindSafe(expect(
        &w,
        t.label,
        case,
        json_request(&w.keys.personal_key.token, t.path, &headers, &body),
        Forwarded,
        &clean,
        Compare::Equal,
    ))
    .catch_unwind()
    .await;
    let elapsed_ns = started.elapsed().as_nanos();
    w.app = unwrapped;

    // Outside catch_unwind: the route and exact wire request must remain valid.
    let (status, text, hits, attacker) = observed.lock().unwrap().take().expect("middleware ran");
    assert_eq!(
        (status.as_u16(), reason(&text)),
        (400, clean.reason.clone())
    );
    assert_eq!(hits.len(), 1, "exactly one approved upstream hop");
    assert_clean(t.label, case, &hits[0], &clean.hit, &w);
    assert_eq!(hits[0].body, clean.hit.body, "the body is unchanged");
    assert!(attacker.is_empty(), "no pre-assertion attacker hop");
    assert_no_attacker(&w, t.label, case);
    let diagnostics = serde_json::to_value(&*w.diagnostics.lock().unwrap()).unwrap();
    w.close().await;

    let panic_message = outcome.as_ref().err().and_then(|payload| {
        payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
    });
    let panic_message = panic_message.expect("the injected 503 must not pass as forwarded");
    assert!(panic_message.contains(&format!(
        "{} {case}: response differs from the control's",
        t.label
    )));
    assert!(panic_message.contains("(503, \"accounting_unavailable\")"));
    assert!(panic_message.contains("(400, \"upstream_rejected\")"));

    let entries = diagnostics.as_array().expect("per-world entries");
    assert_eq!(entries.len(), 1, "record before unwinding");
    let diagnostic = &entries[0];
    let duration = diagnostic["elapsed_ns"]
        .as_u64()
        .expect("monotonic elapsed time");
    assert!(u128::from(duration) <= elapsed_ns);
    let phases = diagnostic["phases"]
        .as_array()
        .expect("real phase capture, not unavailable");
    for name in ["admission.commit", "settlement.commit"] {
        assert!(
            phases.iter().any(|phase| phase["name"] == name),
            "missing {name}"
        );
    }
    for phase in phases {
        assert_eq!(phase.as_object().unwrap().len(), 2);
        assert!(phase["duration_ns"].as_u64().is_some());
    }
    assert_eq!(
        diagnostic,
        &json!({
            "elapsed_ns":duration,
            "phases":phases,
            "status":503,
            "accounting_unavailable":true,
            "approved_hits":1,
            "attacker_hits":0
        })
    );
    let rendered = diagnostics.to_string();
    for forbidden in [
        "NoSensitiveCanary",
        "server-secret-",
        "upstream_rejected",
        "traceId",
        "attributes",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "diagnostic leaked forbidden metadata"
        );
    }
}

/// Reviewed native fields may carry attacker strings as data, never destinations.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn anthropic_fidelity_data_cannot_steer_transport(pool: PgPool) {
    let w = world(pool).await;
    let t = targets()
        .into_iter()
        .find(|t| t.label == "messages/anthropic")
        .unwrap();
    let body = (t.body)(t.model);
    let ctl = control(
        &w,
        t.label,
        json_request(&w.keys.personal_key.token, t.path, &t.base(), &body),
    )
    .await;
    let attacker = format!("http://{}/steal", w.attacker.authority());
    let mut rich = body.clone();
    rich["system"] =
        json!([{"type":"text","text":attacker,"cache_control":{"type":"ephemeral","ttl":"1h"}}]);
    rich["thinking"] = json!({"type":"adaptive"});
    rich["output_config"] = json!({"effort":"high","task_budget":{"type":"tokens","total":64}});
    rich["context_management"] =
        json!({"edits":[{"type":"clear_tool_uses_20250919","exclude_tools":[attacker]}]});
    rich["tools"] = json!([{"name":"lookup","description":attacker,"input_schema":{"type":"object","description":attacker},"defer_loading":true}]);
    rich["metadata"] = json!({"user_id":attacker});
    rich["messages"] = json!([{"role":"assistant","content":[{"type":"thinking","thinking":attacker,"signature":attacker},{"type":"redacted_thinking","data":attacker}]},{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}},{"type":"text","text":attacker}]}]);
    // This request deliberately needs a TTL-aware price. Its original v1
    // control remains valid because it has no cache hints.
    let priced = json!({"status":"priced","microusd_per_million":"2000000"});
    let cache = json!({"read":priced,"write":priced,"write_5m":priced,"write_1h":priced});
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,cache_pricing) SELECT gen_random_uuid(),d.id,1000000,2000000,1000,1000,2,$1 FROM deployments d JOIN models m ON m.id=d.model_id WHERE m.public_name=$2")
        .bind(cache).bind(t.model).execute(&w.pool).await.unwrap();
    let beta = "context-management-2025-06-27";
    let headers = [t.base(), vec![("anthropic-beta".into(), beta.into())]].concat();
    let _ = send(
        &w.app,
        json_request(&w.keys.personal_key.token, t.path, &headers, &rich),
    )
    .await;
    let mut hits = w.approved.take();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].header("anthropic-beta"), Some(beta));
    hits[0].headers.retain(|(n, _)| n != "anthropic-beta");
    let actual: Value = serde_json::from_str(&hits[0].body).unwrap();
    // Compare transport to the control; reviewed body data is asserted exactly
    // below, not exempted from inspection by a general passthrough allowance.
    hits[0].body.clone_from(&ctl.hit.body);
    assert_clean(t.label, "reviewed fidelity fields", &hits[0], &ctl.hit, &w);
    rich.as_object_mut().unwrap().remove("metadata");
    let clean: Value = serde_json::from_str(&ctl.hit.body).unwrap();
    rich["model"] = clean["model"].clone();
    rich["stream"] = json!(false);
    assert_eq!(
        actual, rich,
        "only reviewed fields move; metadata stays local"
    );
    assert_no_attacker(&w, t.label, "reviewed fidelity fields");
    w.close().await;
}

/// Every JSON inference protocol, on every adapter and local profile that serves it.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn no_json_inference_input_can_steer_the_upstream(pool: PgPool) {
    let w = world(pool).await;
    let key = w.keys.personal_key.token.clone();
    let attacker_url = format!("http://{}/v1", w.attacker.authority());
    for t in targets() {
        let body = (t.body)(t.model);
        let base = t.base();
        let post = |headers: &[(String, String)], b: &Value| {
            json_request(&key, t.path, &[base.clone(), headers.to_vec()].concat(), b)
        };
        let ctl = control(&w, t.label, post(&[], &body)).await;
        let check = |case: String, request: Request, want: Expect| {
            let (w, ctl, label) = (&w, &ctl, t.label);
            async move { expect(w, label, &case, request, want, ctl, Compare::Equal).await }
        };

        // Body fields that pick a destination or a credential: rejected.
        for field in STEERING_FIELDS {
            for value in [
                json!(attacker_url),
                json!({"url": attacker_url, "headers": {"authorization": "Bearer sk-attacker"}}),
                json!([attacker_url]),
            ] {
                let mut steered = body.clone();
                steered[*field] = value;
                check((*field).to_owned(), post(&[], &steered), t.invalid()).await;
            }
        }
        for (field, value) in openrouter_fields(&w.attacker) {
            let mut steered = body.clone();
            steered[field] = value;
            check(
                format!("{field} (OpenRouter shape)"),
                post(&[], &steered),
                t.invalid(),
            )
            .await;
        }
        // The same fields nested where a client library puts them.
        for (case, extra) in [
            (
                "extra_body.api_base",
                json!({"extra_body": {"api_base": attacker_url}}),
            ),
            (
                "headers.host",
                json!({"headers": {"Host": w.attacker.authority()}}),
            ),
        ] {
            let mut merged = body.clone();
            merged
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            check(case.to_owned(), post(&[], &merged), t.invalid()).await;
        }

        // Request headers: ignored, except those the protocol itself reads.
        for (name, value) in steering_headers(&w.attacker) {
            if t.messages() && name == "anthropic-beta" {
                // An unfamiliar beta is data in this one reviewed header. It
                // must not move the destination or any other upstream header.
                let (status, text) =
                    send(&w.app, post(&[(name.clone(), value.clone())], &body)).await;
                assert_eq!(
                    (status.as_u16(), reason(&text)),
                    (ctl.status, ctl.reason.clone())
                );
                let mut hits = w.approved.take();
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0].header("anthropic-beta"), Some(value.as_str()));
                hits[0].headers.retain(|(n, _)| n != "anthropic-beta");
                assert_clean(t.label, "bounded verbatim beta", &hits[0], &ctl.hit, &w);
                assert_eq!(hits[0].body, ctl.hit.body);
                assert_no_attacker(&w, t.label, "bounded verbatim beta");
                check(
                    "denied long-context beta".into(),
                    post(&[(name, "context-1m-2025-08-07".into())], &body),
                    Rejected(400, "invalid_request_error"),
                )
                .await;
                continue;
            }
            let want = header_expect(&t, &name);
            check(name.clone(), post(&[(name, value)], &body), want).await;
        }
        // A second credential is ambiguous: refused, never picked between.
        for name in ["x-api-key", "authorization"] {
            check(
                format!("second credential {name}"),
                post(&[(name.to_owned(), "sk-attacker".to_owned())], &body),
                Rejected(401, "authentication_error"),
            )
            .await;
        }
        // All of them at once.
        let all = steering_headers(&w.attacker);
        let want = if t.messages() {
            Rejected(400, "invalid_request_error")
        } else {
            Forwarded
        };
        check("all headers".to_owned(), post(&all, &body), want).await;

        // Model strings: nothing but the exact configured name resolves.
        for model in model_strings(t.model, &w.attacker) {
            let want = if model.len() > 200 {
                t.invalid()
            } else {
                t.not_found()
            };
            check(
                format!("model {model:?}"),
                post(&[], &(t.body)(&model)),
                want,
            )
            .await;
        }

        // User and metadata labels carrying URLs stay local.
        let labelled = |extra: Value| {
            let mut b = body.clone();
            b.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            b
        };
        let cases: Vec<(&str, Value, Expect)> = match t.labels {
            Labels::OpenAi => vec![
                ("user", json!({"user": attacker_url}), Forwarded),
                (
                    "metadata",
                    json!({"metadata": {"session": attacker_url, "user_id": attacker_url}}),
                    Forwarded,
                ),
                (
                    "metadata.api_base",
                    json!({"metadata": {"api_base": attacker_url}}),
                    Forwarded,
                ),
            ],
            Labels::Anthropic => vec![
                (
                    "metadata.user_id",
                    json!({"metadata": {"user_id": attacker_url}}),
                    Forwarded,
                ),
                ("user", json!({"user": attacker_url}), t.invalid()),
                (
                    "metadata.api_base",
                    json!({"metadata": {"api_base": attacker_url}}),
                    t.invalid(),
                ),
            ],
            Labels::No => vec![
                ("user", json!({"user": attacker_url}), t.invalid()),
                (
                    "metadata",
                    json!({"metadata": {"api_base": attacker_url}}),
                    t.invalid(),
                ),
            ],
        };
        for (case, extra, want) in cases {
            check(case.to_owned(), post(&[], &labelled(extra)), want).await;
        }
    }
    w.close().await;
}

const BOUNDARY: &str = "steering-boundary";

fn multipart(fields: &[(&str, &str)], filename: &str, file: &[u8], file_type: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    out.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {file_type}\r\n\r\n").as_bytes(),
    );
    out.extend_from_slice(file);
    out.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    out
}

fn multipart_request(
    key: &str,
    path: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
) -> Request {
    let mut builder = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", format!("Bearer {key}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        );
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder.body(Body::from(body)).unwrap()
}

/// PCM WAV: 16 kHz mono 16-bit silence.
fn wav(ms: u64) -> Vec<u8> {
    let data = (16_000 * 2 * ms / 1000) as u32;
    let mut b = b"RIFF".to_vec();
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    for (v, w) in [
        (16u32, 4),
        (1, 2),
        (1, 2),
        (16_000, 4),
        (32_000, 4),
        (2, 2),
        (16, 2),
    ] {
        b.extend_from_slice(&v.to_le_bytes()[..w]);
    }
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    b.resize(b.len() + data as usize, 0);
    b
}

/// Audio transcription is multipart: form fields, file names and headers
/// cannot steer either, alone or together.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn no_multipart_audio_input_can_steer_the_upstream(pool: PgPool) {
    let w = world(pool).await;
    let key = w.keys.personal_key.token.clone();
    let path = "/v1/audio/transcriptions";
    let attacker_url = format!("http://{}/v1", w.attacker.authority());
    let audio = wav(500);
    let request = |headers: &[(String, String)], fields: &[(&str, &str)], filename: &str| {
        multipart_request(
            &key,
            path,
            headers,
            multipart(fields, filename, &audio, "audio/wav"),
        )
    };
    for (label, model, compare) in [
        ("transcription/openai", "t/oa-trans", Compare::SameLength),
        ("transcription/openrouter", "t/or-trans", Compare::Equal),
    ] {
        let ctl = control(&w, label, request(&[], &[("model", model)], "a.wav")).await;
        let check = |case: String, request: Request, want: Expect| {
            let (w, ctl) = (&w, &ctl);
            async move { expect(w, label, &case, request, want, ctl, compare).await }
        };
        let invalid = Rejected(400, "invalid_request_error");
        for field in STEERING_FIELDS {
            check(
                format!("field {field}"),
                request(
                    &[],
                    &[("model", model), (field, attacker_url.as_str())],
                    "a.wav",
                ),
                invalid,
            )
            .await;
        }
        // File names alone: labels, never destinations or upstream content.
        let hostile = steering_headers(&w.attacker);
        for filename in [
            format!("{attacker_url}/a.wav"),
            "../../a.wav".to_owned(),
            format!("a@{}.wav", w.attacker.authority()),
            "a.wav\\..\\b.wav".to_owned(),
        ] {
            check(
                format!("filename {filename:?}"),
                request(&[], &[("model", model)], &filename),
                Forwarded,
            )
            .await;
            // Then with every hostile header.
            check(
                format!("filename {filename:?} with hostile headers"),
                request(&hostile, &[("model", model)], &filename),
                Forwarded,
            )
            .await;
        }
        for (name, value) in &hostile {
            check(
                format!("header {name}"),
                request(
                    &[(name.clone(), value.clone())],
                    &[("model", model)],
                    "a.wav",
                ),
                Forwarded,
            )
            .await;
        }
        for name in model_strings(model, &w.attacker) {
            let want = if name.len() > 200 {
                invalid
            } else {
                Rejected(404, "model_not_found")
            };
            check(
                format!("model {name:?}"),
                request(&[], &[("model", &name)], "a.wav"),
                want,
            )
            .await;
        }
    }
    w.close().await;
}

async fn upload(w: &World, fields: &[(&str, &str)], name: &str, content: &[u8]) -> (u16, String) {
    let (status, text) = send(
        &w.app,
        multipart_request(
            &w.keys.personal_key.token,
            "/v1/files",
            &[],
            multipart(fields, name, content, "application/octet-stream"),
        ),
    )
    .await;
    (status.as_u16(), text)
}

fn is(got: &(u16, String), status: u16, why: &str, case: &str) {
    assert_eq!(
        (got.0, reason(&got.1).as_str()),
        (status, why),
        "{case}: {}",
        got.1
    );
}

/// Files and Batches: the file store is local and a batch runs only the
/// validated lines of a gateway file on the routes the catalog names.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn files_and_batches_cannot_steer_the_upstream(pool: PgPool) {
    let w = world(pool).await;
    let key = w.keys.personal_key.token.clone();
    let attacker_url = format!("http://{}/v1", w.attacker.authority());
    let line = |extra: Value, url: &str, model: &str| {
        let mut body = json!({"model":model,"messages":[{"role":"user","content":"hi"}],"max_completion_tokens":5});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        format!(
            "{}\n",
            json!({"custom_id":"r1","method":"POST","url":url,"body":body})
        )
    };
    let good = line(json!({}), "/v1/chat/completions", "t/local-chat");
    let silent = |case: &str| {
        assert_eq!(w.approved.take().len(), 0, "{case}");
        assert_no_attacker(&w, "files and batches", case);
    };
    // Files: an upload never leaves the gateway, whatever its name says.
    let accepted = upload(&w, &[("purpose", "batch")], "good.jsonl", good.as_bytes()).await;
    is(&accepted, 200, "", "control upload");
    for name in [
        format!("{attacker_url}/x.jsonl"),
        "../../x.jsonl".to_owned(),
        format!("x@{}.jsonl", w.attacker.authority()),
    ] {
        let got = upload(&w, &[("purpose", "batch")], &name, good.as_bytes()).await;
        is(&got, 200, "", &format!("filename {name:?}"));
        silent(&name);
    }
    for purpose in [attacker_url.as_str(), "../batch", "batch@127.0.0.1"] {
        let got = upload(&w, &[("purpose", purpose)], "x.jsonl", good.as_bytes()).await;
        is(
            &got,
            400,
            "invalid_request_error",
            &format!("purpose {purpose:?}"),
        );
        silent(purpose);
    }
    for field in STEERING_FIELDS {
        let got = upload(
            &w,
            &[("purpose", "batch"), (field, attacker_url.as_str())],
            "x.jsonl",
            good.as_bytes(),
        )
        .await;
        is(
            &got,
            400,
            "invalid_request_error",
            &format!("field {field}"),
        );
        silent(field);
    }
    // Batches: the input file is a gateway file; the endpoint is one of four paths.
    let (status, file) = upload(&w, &[("purpose", "batch")], "good.jsonl", good.as_bytes()).await;
    assert_eq!(status, 200, "{file}");
    let id = serde_json::from_str::<Value>(&file).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let create = |body: Value| json_request(&key, "/v1/batches", &[], &body);
    let post = |body: Value| async {
        let (s, t) = send(&w.app, create(body)).await;
        (s.as_u16(), t)
    };
    let control =
        json!({"input_file_id":id,"endpoint":"/v1/chat/completions","completion_window":"24h"});
    is(&post(control.clone()).await, 200, "", "control batch");
    silent("control batch (no upstream call at creation)");
    for field in STEERING_FIELDS {
        let mut body = control.clone();
        body[*field] = json!(attacker_url);
        is(
            &post(body).await,
            400,
            // `endpoint` is a batch field: a URL there is an unsupported endpoint.
            if *field == "endpoint" {
                "unsupported_capability"
            } else {
                "invalid_request_error"
            },
            &format!("batch field {field}"),
        );
        silent(field);
    }
    for endpoint in [
        format!("{attacker_url}/chat/completions"),
        "/v1/chat/completions/../../x".to_owned(),
        "//attacker.invalid/v1/chat/completions".to_owned(),
        "/v1/chat/completions?api_base=x".to_owned(),
        "/v1/models".to_owned(),
    ] {
        let mut body = control.clone();
        body["endpoint"] = json!(endpoint);
        is(
            &post(body).await,
            400,
            "unsupported_capability",
            &format!("endpoint {endpoint}"),
        );
        silent(&endpoint);
    }
    for input in [
        attacker_url.clone(),
        "../x".to_owned(),
        format!("{id}/../{id}"),
        format!("{id}@{}", w.attacker.authority()),
    ] {
        let mut body = control.clone();
        body["input_file_id"] = json!(input);
        is(
            &post(body).await,
            404,
            "not_found",
            &format!("input_file_id {input}"),
        );
        silent(&input);
    }
    // Lines that carry steering are invalid, so the batch is refused up front.
    for (case, bad) in [
        (
            "url",
            line(
                json!({}),
                &format!("{attacker_url}/chat/completions"),
                "t/local-chat",
            ),
        ),
        (
            "api_base",
            line(
                json!({"api_base": attacker_url}),
                "/v1/chat/completions",
                "t/local-chat",
            ),
        ),
        (
            "extra_body",
            line(
                json!({"extra_body": {"api_base": attacker_url}}),
                "/v1/chat/completions",
                "t/local-chat",
            ),
        ),
        (
            "model url",
            line(
                json!({}),
                "/v1/chat/completions",
                &format!("openai/{attacker_url}"),
            ),
        ),
        (
            "model dots",
            line(json!({}), "/v1/chat/completions", "../../t/local-chat"),
        ),
    ] {
        let (status, file) = upload(&w, &[("purpose", "batch")], "bad.jsonl", bad.as_bytes()).await;
        assert_eq!(status, 200, "{case}: {file}");
        let mut body = control.clone();
        body["input_file_id"] = serde_json::from_str::<Value>(&file).unwrap()["id"].clone();
        is(&post(body).await, 400, "invalid_batch_input", case);
        silent(case);
    }
    w.close().await;
}

/// Model resolution is exact: whatever string a request carries (arbitrary
/// text, near misses of real names, confusables, separators), the candidates
/// are those of a configured public name byte for byte, or none, never an error.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn only_an_exact_configured_name_ever_resolves(pool: PgPool) {
    use crate::inference::repository::InferenceRepository;
    use proptest::{prelude::*, strategy::ValueTree, test_runner::TestRunner};

    let w = world(pool).await;
    let user: Uuid = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    let principal = crate::auth::Principal {
        key_id: w.keys.personal_key.id,
        workspace_id: w.keys.personal_workspace_id,
        user_id: Some(user),
    };
    let store = Store::new(w.pool.clone());
    let configured: Vec<String> = targets().iter().map(|t| t.model.to_owned()).collect();
    // Mixed case, dots, colons: valid names that differ from others only slightly.
    for name in [
        "T/Exact-2",
        "t/x.y:z_w",
        "t/oa-chat2",
        "t/oa-chat.",
        "t/oa-chat:",
    ] {
        let c = connection(&store.pool, "openai", None).await;
        add_route(
            &store.pool,
            name,
            "chat_completions",
            c,
            "u",
            Price::Tokens { output_limit: 10 },
        )
        .await;
    }
    let mut known = configured.clone();
    known.extend(
        [
            "T/Exact-2",
            "t/x.y:z_w",
            "t/oa-chat2",
            "t/oa-chat.",
            "t/oa-chat:",
        ]
        .map(String::from),
    );

    let mutate = |name: &str, at: usize, op: u8, ch: char| -> String {
        let chars: Vec<char> = name.chars().collect();
        let i = at % (chars.len() + 1);
        let mut out = chars.clone();
        match op % 5 {
            0 => out.insert(i, ch),
            1 if i < out.len() => {
                out.remove(i);
            }
            2 if i < out.len() => out[i] = ch,
            3 => out.truncate(i),
            _ => out.extend(['\u{200b}', ch]),
        }
        out.into_iter().collect()
    };
    let mut runner = TestRunner::deterministic();
    let mut samples: Vec<String> = Vec::new();
    let arbitrary = any::<String>();
    let urlish = proptest::string::string_regex("[a-zA-Z0-9/._:@#%?&=\\\\ -]{0,60}").unwrap();
    let near = (0..known.len(), any::<usize>(), any::<u8>(), any::<char>());
    for _ in 0..400 {
        samples.push(arbitrary.new_tree(&mut runner).unwrap().current());
        samples.push(urlish.new_tree(&mut runner).unwrap().current());
        let (n, at, op, ch) = near.new_tree(&mut runner).unwrap().current();
        samples.push(mutate(&known[n], at, op, ch));
    }
    for name in &known {
        samples.push(name.clone());
    }
    let mut resolved = 0;
    for sample in &samples {
        let found = store
            .deployments(&principal, sample)
            .await
            .unwrap_or_else(|e| panic!("{sample:?}: a lookup failed with {e:?}"));
        if known.contains(sample) {
            assert_eq!(found.len(), 1, "{sample:?} is configured");
            resolved += 1;
        } else {
            assert!(
                found.is_empty(),
                "{sample:?} resolved without being a configured name"
            );
        }
    }
    assert!(resolved >= known.len(), "the configured names resolved");
    drop(store);
    w.close().await;
}

/// Names inserted by SQL outside the management charset (a space, `@`, an
/// accented letter) resolved before #196 and must keep doing so; only what can
/// never match and would break the query (empty, control characters, longer
/// than an index key) is turned away before it.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn names_outside_the_management_charset_keep_resolving(pool: PgPool) {
    use crate::inference::repository::InferenceRepository;
    let w = world(pool).await;
    let user: Uuid = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    let principal = crate::auth::Principal {
        key_id: w.keys.personal_key.id,
        workspace_id: w.keys.personal_workspace_id,
        user_id: Some(user),
    };
    let store = Store::new(w.pool.clone());
    let legacy = ["team model", "team@model", "caf\u{e9}", "a#b%2F", "x/../y"];
    for name in legacy {
        let c = connection(&store.pool, "openai", None).await;
        add_route(
            &store.pool,
            name,
            "chat_completions",
            c,
            "u",
            Price::Tokens { output_limit: 10 },
        )
        .await;
    }
    for name in legacy {
        assert_eq!(
            store.deployments(&principal, name).await.unwrap().len(),
            1,
            "{name:?}"
        );
    }
    // Near misses of those names still do not resolve.
    for name in [
        "team  model",
        "Team model",
        "caf\u{65}\u{301}",
        "team model ",
    ] {
        assert!(
            store
                .deployments(&principal, name)
                .await
                .unwrap()
                .is_empty(),
            "{name:?}"
        );
    }
    // What can never match answers "unknown", never a storage error.
    for name in [
        "",
        "\0",
        "a\0b",
        "a\nb",
        "a\u{7f}",
        "a\u{85}",
        &"x".repeat(3000),
    ] {
        assert!(
            store
                .deployments(&principal, name)
                .await
                .unwrap()
                .is_empty(),
            "{name:?}"
        );
    }
    drop(store);
    w.close().await;
}

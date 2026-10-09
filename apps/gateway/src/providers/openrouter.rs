//! OpenRouter wire protocol at the fixed origin `https://openrouter.ai/api/v1`.
//!
//! - The key is a server-held `env:` reference; inference credentials are
//!   never forwarded. Optional `HTTP-Referer`/`X-Title` come from server config.
//! - Redirects, ambient proxies and implicit retries are disabled.
//! - Non-success bodies are never read: OpenRouter error bodies carry the
//!   account `user_id` and embedded provider errors. 200 bodies carrying an
//!   `error` object are mapped by code only.
//! - Every request sets `provider.data_collection` (default `"deny"`). Free
//!   (`:free`) endpoints may train on prompts and are unavailable under deny
//!   (upstream 404 → `upstream_rejected`).
//! - `usage.cost` is captured from the JSON number's source text as exact
//!   micro-USD (rounded up) for `provider_cost_microusd` evidence only; it is
//!   never the gateway charge.
//! - Chat: OpenRouter-only fields (`native_finish_reason`, reasoning traces)
//!   are normalized away; reasoning traces are not returned to clients but
//!   their tokens stay in `completion_tokens`. The final streaming accounting
//!   frame (repeated finish choice with an empty delta plus `usage`) is treated
//!   as usage, not as a second terminal event.
use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{
    Client,
    header::{self, HeaderValue},
};
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};

use super::{
    ProviderAdapter, embeddings, metering, openai,
    secrets::SecretResolver,
    text_workloads::{rerank_results, rerank_usage, systemone_usage},
};
use crate::inference::{error::InferenceError, evidence, types::*};

pub const BASE: &str = "https://openrouter.ai/api/v1";
const BODY_LIMIT: usize = 4 * 1024 * 1024;

type Result<T> = std::result::Result<T, InferenceError>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DataCollection {
    #[default]
    Deny,
    Allow,
}
impl DataCollection {
    /// `GATEWAY_OPENROUTER_DATA_COLLECTION` (`deny` default, or `allow`).
    /// `serve` validates it at startup through `OpenRouterConfig::from_env`.
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var("GATEWAY_OPENROUTER_DATA_COLLECTION")
            .ok()
            .filter(|v| !v.is_empty())
            .as_deref()
        {
            None | Some("deny") => Ok(Self::Deny),
            Some("allow") => Ok(Self::Allow),
            Some(_) => anyhow::bail!("GATEWAY_OPENROUTER_DATA_COLLECTION must be deny or allow"),
        }
    }
    /// The operator override from `GATEWAY_OPENROUTER_DATA_COLLECTION`, when set.
    /// It locks Admin › Settings › Data & privacy. Invalid values fail `serve`
    /// startup; elsewhere they fall back to the deny override (fail closed).
    pub fn env_override() -> Option<Self> {
        static OVERRIDE: std::sync::OnceLock<Option<DataCollection>> = std::sync::OnceLock::new();
        *OVERRIDE.get_or_init(|| {
            std::env::var_os("GATEWAY_OPENROUTER_DATA_COLLECTION")
                .filter(|v| !v.is_empty())
                .map(|_| Self::from_env().unwrap_or_default())
        })
    }
    /// The installation setting (0010 `installation_settings`), refreshed by the
    /// maintenance loop and on save. Deny until first loaded.
    pub fn installation_default() -> Self {
        if INSTALLATION_ALLOWS.load(std::sync::atomic::Ordering::SeqCst) {
            Self::Allow
        } else {
            Self::Deny
        }
    }
    pub fn set_installation_default(value: Self) {
        INSTALLATION_ALLOWS.store(value == Self::Allow, std::sync::atomic::Ordering::SeqCst);
    }
    /// The effective policy (no secrets): the environment override, else the
    /// installation setting.
    pub fn configured() -> Self {
        Self::env_override().unwrap_or_else(Self::installation_default)
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "deny" => Some(Self::Deny),
            "allow" => Some(Self::Allow),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Allow => "allow",
        }
    }
}

static INSTALLATION_ALLOWS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Server configuration; never derived from inference requests.
#[derive(Clone, Default)]
pub struct OpenRouterConfig {
    http_referer: Option<HeaderValue>,
    title: Option<HeaderValue>,
    data_collection: DataCollection,
    /// No environment override: follow the live installation setting.
    follow_installation: bool,
}
impl OpenRouterConfig {
    pub fn new(
        http_referer: Option<&str>,
        title: Option<&str>,
        data_collection: DataCollection,
    ) -> anyhow::Result<Self> {
        let referer = http_referer
            .map(|v| {
                anyhow::ensure!(
                    (v.starts_with("https://") || v.starts_with("http://"))
                        && v.len() <= 512
                        && v.bytes().all(|b| (0x21..=0x7e).contains(&b)),
                    "GATEWAY_OPENROUTER_HTTP_REFERER must be an absolute http(s) URL"
                );
                Ok(HeaderValue::from_str(v)?)
            })
            .transpose()?;
        let title = title
            .map(|v| {
                anyhow::ensure!(
                    !v.trim().is_empty()
                        && v.trim() == v
                        && v.len() <= 128
                        && v.bytes().all(|b| (0x20..=0x7e).contains(&b)),
                    "GATEWAY_OPENROUTER_TITLE must be 1-128 printable ASCII characters"
                );
                Ok(HeaderValue::from_str(v)?)
            })
            .transpose()?;
        Ok(Self {
            http_referer: referer,
            title,
            data_collection,
            follow_installation: false,
        })
    }
    /// `GATEWAY_OPENROUTER_HTTP_REFERER`, `GATEWAY_OPENROUTER_TITLE`,
    /// `GATEWAY_OPENROUTER_DATA_COLLECTION` (`deny` default, or `allow`).
    pub fn from_env() -> anyhow::Result<Self> {
        let get = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let data_collection = DataCollection::from_env()?;
        let mut config = Self::new(
            get("GATEWAY_OPENROUTER_HTTP_REFERER").as_deref(),
            get("GATEWAY_OPENROUTER_TITLE").as_deref(),
            data_collection,
        )?;
        // Unset: Admin › Settings › Data & privacy decides, per request.
        config.follow_installation = get("GATEWAY_OPENROUTER_DATA_COLLECTION").is_none();
        Ok(config)
    }
}

pub struct OpenRouterAdapter {
    client: Client,
    resolver: Arc<dyn SecretResolver>,
    // Only the private test constructor can replace the production origin.
    base: String,
    config: OpenRouterConfig,
}

impl OpenRouterAdapter {
    pub fn new(
        resolver: Arc<dyn SecretResolver>,
        config: OpenRouterConfig,
    ) -> anyhow::Result<Self> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .https_only(true)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| anyhow::anyhow!("Unable to initialize provider transport"))?;
        Ok(Self {
            client,
            resolver,
            base: BASE.into(),
            config,
        })
    }

    #[cfg(test)]
    fn for_test(resolver: Arc<dyn SecretResolver>, config: OpenRouterConfig, base: String) -> Self {
        let url = reqwest::Url::parse(&base).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        let mut adapter = Self::new(resolver, config).unwrap();
        adapter.client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .build()
            .unwrap();
        adapter.base = base;
        adapter
    }

    fn provider_preferences(&self) -> Value {
        let policy = if self.config.follow_installation {
            DataCollection::configured()
        } else {
            self.config.data_collection
        };
        json!({"data_collection": policy.as_str()})
    }

    /// Validate the connection before resolving credentials, then POST.
    async fn post(
        &self,
        target: &Deployment,
        path: &str,
        body: &Value,
    ) -> Result<reqwest::Response> {
        if target.provider != self.id()
            || !target.credential_ref.starts_with("env:")
            || target
                .endpoint
                .as_deref()
                .is_some_and(|v| v != BASE && v != "https://openrouter.ai/api/v1/")
            || target.region.as_deref().is_some_and(|v| !v.is_empty())
            || target.upstream_model.trim().is_empty()
        {
            return Err(InferenceError::Configuration);
        }
        let secret = self.resolver.resolve(&target.credential_ref)?;
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
            .map_err(|_| InferenceError::Configuration)?;
        auth.set_sensitive(true);
        let mut builder = self
            .client
            .post(format!("{}{path}", self.base))
            .header(header::AUTHORIZATION, auth)
            .json(body);
        if let Some(referer) = &self.config.http_referer {
            builder = builder.header("HTTP-Referer", referer.clone());
        }
        if let Some(title) = &self.config.title {
            builder = builder.header("X-Title", title.clone());
        }
        let response = builder.send().await.map_err(transport)?;
        if !response.status().is_success() {
            // Dropping the response without reading it: error bodies are never
            // parsed, logged or forwarded.
            return Err(status_error(response.status().as_u16()));
        }
        Ok(response)
    }
}

fn transport(error: reqwest::Error) -> InferenceError {
    if error.is_timeout() {
        InferenceError::Timeout
    } else {
        InferenceError::UpstreamUnavailable
    }
}

/// Status (or embedded `error.code`) to a sanitized error kind.
fn status_error(status: u16) -> InferenceError {
    match status {
        // Bad key, or the gateway's OpenRouter account lacks credit/limit:
        // operator action, not a client error.
        401 | 402 => InferenceError::Configuration,
        // Moderation/guardrail rejection of this request.
        403 => InferenceError::UpstreamRejected,
        408 | 524 => InferenceError::Timeout,
        429 => InferenceError::Busy,
        // Redirects are never followed.
        300..=399 => InferenceError::Configuration,
        400..=499 => InferenceError::UpstreamRejected,
        500..=599 => InferenceError::UpstreamUnavailable,
        _ => InferenceError::InvalidUpstream,
    }
}

fn empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// A 200 body (or stream frame) carrying `error` is a failure, mapped by code.
fn embedded_error(value: &Value) -> Result<()> {
    if empty(&value["error"]) {
        return Ok(());
    }
    Err(value["error"]["code"]
        .as_u64()
        .and_then(|c| u16::try_from(c).ok())
        .map_or(InferenceError::UpstreamUnavailable, status_error))
}

async fn read(response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|n| n > BODY_LIMIT as u64)
    {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport)?;
        if chunk.len() > BODY_LIMIT - bytes.len() {
            return Err(InferenceError::InvalidUpstream);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[derive(Deserialize)]
struct CostProbe<'a> {
    #[serde(borrow, default)]
    usage: Option<UsageCost<'a>>,
}
#[derive(Deserialize)]
struct UsageCost<'a> {
    #[serde(borrow, default)]
    cost: Option<&'a RawValue>,
}
/// `usage.cost` from the original JSON text (exact; no f64 round trip).
fn provider_cost(bytes: &[u8]) -> Result<Option<i64>> {
    let probe: CostProbe =
        serde_json::from_slice(bytes).map_err(|_| InferenceError::InvalidUpstream)?;
    probe
        .usage
        .and_then(|u| u.cost)
        .map(|raw| metering::usd_text_to_microusd_ceil(raw.get()))
        .transpose()
}

/// JSON object body plus exact provider cost evidence.
fn parse(bytes: &[u8]) -> Result<(Value, Option<i64>)> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| InferenceError::InvalidUpstream)?;
    if !value.is_object() {
        return Err(InferenceError::InvalidUpstream);
    }
    embedded_error(&value)?;
    Ok((value, provider_cost(bytes)?))
}

/// Remove OpenRouter-only choice fields after type-checking them, so the
/// shared OpenAI decoder still rejects every other unknown content field.
fn normalize_choices(value: &mut Value) -> Result<()> {
    let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for choice in choices {
        let Some(choice) = choice.as_object_mut() else {
            return Err(InferenceError::InvalidUpstream);
        };
        if let Some(native) = choice.remove("native_finish_reason")
            && !(native.is_null() || native.is_string())
        {
            return Err(InferenceError::InvalidUpstream);
        }
        for key in ["message", "delta"] {
            if let Some(message) = choice.get_mut(key).and_then(Value::as_object_mut) {
                if let Some(text) = message.remove("reasoning")
                    && !(text.is_null() || text.is_string())
                {
                    return Err(InferenceError::InvalidUpstream);
                }
                if let Some(details) = message.remove("reasoning_details")
                    && !(details.is_null() || details.is_array())
                {
                    return Err(InferenceError::InvalidUpstream);
                }
            }
        }
    }
    Ok(())
}

fn decode_chat(bytes: &[u8]) -> Result<ChatResponse> {
    let (mut value, cost) = parse(bytes)?;
    normalize_choices(&mut value)?;
    let mut response = openai::decode_complete(&value)?;
    response.usage.provider_cost_microusd = cost;
    Ok(response)
}

/// Normalize one SSE data frame. `finish` is the first finish reason seen.
fn normalize_frame(data: &[u8], finish: &mut Option<String>) -> Result<(Vec<u8>, Option<i64>)> {
    if data == b"[DONE]" {
        return Ok((data.to_vec(), None));
    }
    let (mut value, cost) = parse(data)?;
    normalize_choices(&mut value)?;
    if let Some(previous) = finish.as_deref()
        && !value["usage"].is_null()
        && let Some([choice]) = value["choices"].as_array().map(Vec::as_slice)
    {
        let delta = &choice["delta"];
        let accounting = delta.as_object().is_some_and(|d| {
            d.iter().all(|(k, v)| match k.as_str() {
                "content" | "tool_calls" => empty(v),
                "role" => v.is_null() || v == "assistant",
                _ => false,
            })
        }) && (choice["finish_reason"].is_null()
            || choice["finish_reason"].as_str() == Some(previous))
            && choice["index"].as_u64() == Some(0);
        if accounting {
            value["choices"] = json!([]);
        }
    }
    if finish.is_none()
        && let Some(reason) = value["choices"][0]["finish_reason"].as_str()
    {
        *finish = Some(reason.to_owned());
    }
    Ok((
        serde_json::to_vec(&value).map_err(|_| InferenceError::InvalidUpstream)?,
        cost,
    ))
}

fn decode_chat_stream(response: reqwest::Response) -> EventStream {
    Box::pin(async_stream::try_stream! {
        let mut chunks = response.bytes_stream();
        let mut decoder = openai::SseDecoder::new();
        let mut state = openai::StreamState::default();
        let mut finish = None;
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(transport)?;
            for &byte in chunk.iter() {
                if let Some(data) = decoder.push(byte)? {
                    let (frame, cost) = normalize_frame(&data, &mut finish)?;
                    for event in state.decode(&frame)? {
                        let event = match event {
                            ChatEvent::Usage(mut usage) => {
                                usage.provider_cost_microusd = cost;
                                ChatEvent::Usage(usage)
                            }
                            other => other,
                        };
                        let done = matches!(event, ChatEvent::Done);
                        yield event;
                        if done { return; }
                    }
                }
            }
        }
        // Only [DONE] proves completion.
        Err(InferenceError::InvalidUpstream)?;
    })
}

/// Embedding models with a fixed output width that rejects other `dimensions`.
fn fixed_dimensions(model: &str) -> Option<u32> {
    let base = model.strip_suffix(":free").unwrap_or(model);
    (base == "nvidia/nemotron-3-embed-1b").then_some(2048)
}

#[async_trait]
impl ProviderAdapter for OpenRouterAdapter {
    fn id(&self) -> &'static str {
        "openrouter"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: true,
            tools: true,
        }
    }

    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        super::capabilities::serves(self.id(), protocol)
    }

    fn supports_transcription_request(
        &self,
        _target: &Deployment,
        request: &TranscriptionRequest,
    ) -> bool {
        audio::supports_transcription(request)
    }

    fn supports_speech_request(&self, _target: &Deployment, request: &SpeechRequest) -> bool {
        audio::supports_speech(request)
    }

    async fn execute_audio_transcription(
        &self,
        target: &Deployment,
        request: TranscriptionRequest,
    ) -> Result<TranscriptionResponse> {
        self.transcribe(target, request).await
    }

    async fn execute_audio_speech(
        &self,
        target: &Deployment,
        request: SpeechRequest,
    ) -> Result<SpeechResponse> {
        self.speak(target, request).await
    }

    fn supports_image_request(&self, _target: &Deployment, request: &ImageRequest) -> bool {
        images::supports(request)
    }

    async fn execute_images(
        &self,
        target: &Deployment,
        request: ImageRequest,
    ) -> Result<ImageResponse> {
        self.generate_images(target, request).await
    }

    fn supports_embedding_target(&self, target: &Deployment, request: &EmbeddingRequest) -> bool {
        // OpenRouter's catalog advertises no `dimensions` parameter for its
        // embedding models; only a model's fixed native width is accepted.
        match fixed_dimensions(&target.upstream_model) {
            Some(width) => request.dimensions.is_none_or(|d| d == width),
            None => request.dimensions.is_none(),
        }
    }

    async fn execute(&self, target: &Deployment, request: ChatRequest) -> Result<ProviderOutput> {
        if !self.supports_chat_request(&request) {
            return Err(InferenceError::Unsupported);
        }
        let mut body = openai::encode(&target.upstream_model, &request);
        body["provider"] = self.provider_preferences();
        body["usage"] = json!({"include": true});
        let response = self.post(target, "/chat/completions", &body).await?;
        if request.stream {
            super::framing::check_sse(&response)?;
            Ok(ProviderOutput::Stream(decode_chat_stream(response)))
        } else {
            Ok(ProviderOutput::Complete(decode_chat(
                &read(response).await?,
            )?))
        }
    }

    async fn execute_embeddings(
        &self,
        target: &Deployment,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse> {
        embeddings::validate(&request)?;
        if !self.supports_embedding_target(target, &request) {
            return Err(InferenceError::Unsupported);
        }
        let mut body = embeddings::encode(&target.upstream_model, &request);
        body["provider"] = self.provider_preferences();
        let response = self.post(target, "/embeddings", &body).await?;
        let (value, cost) = parse(&read(response).await?)?;
        let mut decoded = embeddings::decode(&value, &request)?;
        if let Some(width) = fixed_dimensions(&target.upstream_model)
            && decoded.embeddings.iter().any(|v| v.len() != width as usize)
        {
            return evidence::preserve(Err(InferenceError::InvalidUpstream), || {
                Some(Ok(decoded.usage))
            });
        }
        decoded.usage.meters = Some(metering::text_workload_meters(Some(0)));
        decoded.usage.provider_cost_microusd = cost;
        Ok(decoded)
    }

    async fn execute_rerank(
        &self,
        target: &Deployment,
        request: RerankRequest,
    ) -> Result<RerankResponse> {
        request.validate()?;
        let mut body = json!({
            "model": target.upstream_model,
            "query": request.query,
            "documents": request.documents,
            "provider": self.provider_preferences(),
        });
        if request.top_n.is_some() {
            body["top_n"] = json!(request.result_limit());
        }
        let response = self.post(target, "/rerank", &body).await?;
        let (value, cost) = parse(&read(response).await?)?;
        let usage = rerank_usage(&value["usage"], cost)?;
        evidence::preserve(
            rerank_results(&value, &request).map(|results| RerankResponse { results, usage }),
            || Some(Ok(usage)),
        )
    }

    async fn execute_systemone(
        &self,
        target: &Deployment,
        request: SystemoneRequest,
    ) -> Result<SystemoneResponse> {
        request.validate()?;
        let mut body = request.wire(&target.upstream_model);
        body["provider"] = self.provider_preferences();
        let response = self.post(target, "/systemone", &body).await?;
        let (value, cost) = parse(&read(response).await?)?;
        let usage = systemone_usage(&value["usage"], cost)?;
        evidence::preserve(
            parse_answers(&value["answers"], &request)
                .map(|answers| SystemoneResponse { answers, usage }),
            || Some(Ok(usage)),
        )
    }
}

mod audio;
mod images;
#[cfg(test)]
mod tests;

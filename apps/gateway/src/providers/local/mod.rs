//! Explicitly approved local profiles: Chat and embeddings on every profile,
//! rerank on the compatible/vLLM/SGLang profiles, System One on the compatible
//! and Ollama profiles (see `workloads`). Ollama uses compatible Chat and
//! native /api/embed with truncation disabled.
use super::{ProviderAdapter, embeddings, framing, openai, secrets::SecretResolver};
use crate::inference::{error::InferenceError, types::*};
use async_trait::async_trait;
use std::sync::Arc;
pub mod endpoints;
mod workloads;
use endpoints::ApprovedEndpoints;
use workloads::RerankWire;
type Result<T> = std::result::Result<T, InferenceError>;
#[derive(Clone, Copy)]
pub enum Profile {
    OpenAiCompatible,
    Vllm,
    Sglang,
    Ollama,
}
impl Profile {
    fn id(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai_compatible",
            Self::Vllm => "vllm",
            Self::Sglang => "sglang",
            Self::Ollama => "ollama",
        }
    }
}
pub struct LocalAdapter {
    profile: Profile,
    resolver: Arc<dyn SecretResolver>,
    approvals: Arc<ApprovedEndpoints>,
}
impl LocalAdapter {
    pub fn new(
        profile: Profile,
        resolver: Arc<dyn SecretResolver>,
        approvals: Arc<ApprovedEndpoints>,
    ) -> Self {
        Self {
            profile,
            resolver,
            approvals,
        }
    }
    fn connection(&self, target: &Deployment) -> Result<(&reqwest::Client, &str)> {
        if target.provider != self.id() || target.region.as_deref().is_some_and(|s| !s.is_empty()) {
            return Err(InferenceError::Configuration);
        }
        let endpoint = target
            .endpoint
            .as_deref()
            .ok_or(InferenceError::Configuration)?;
        self.approvals.validate_connection(
            endpoint,
            &target.credential_ref,
            target.region.as_deref(),
        )?;
        self.approvals.approved(endpoint)
    }
    /// The approved base with its fixed `/v1` suffix removed, keeping any
    /// reverse-proxy prefix (native, non-`/v1` server routes).
    fn prefix(base: &str) -> Result<&str> {
        base.strip_suffix("/v1")
            .ok_or(InferenceError::Configuration)
    }
    /// Rerank wire of this profile; Ollama serves no rerank API.
    fn rerank_wire(&self) -> Option<RerankWire> {
        match self.profile {
            Profile::OpenAiCompatible | Profile::Vllm => Some(RerankWire::Jina),
            Profile::Sglang => Some(RerankWire::Sglang),
            Profile::Ollama => None,
        }
    }
    /// TypeSafe System One: generic compatible servers (OpenJev and the like)
    /// and Ollama v0.35.0+. vLLM and SGLang serve no such route.
    fn serves_systemone(&self) -> bool {
        matches!(self.profile, Profile::OpenAiCompatible | Profile::Ollama)
    }
    async fn post_json(
        &self,
        target: &Deployment,
        url: String,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let (client, _) = self.connection(target)?;
        let response = self
            .authenticate(client.post(url), target)?
            .json(payload)
            .send()
            .await
            .map_err(framing::transport)?;
        framing::status(response.status())?;
        framing::body(response).await
    }
    fn authenticate(
        &self,
        builder: reqwest::RequestBuilder,
        target: &Deployment,
    ) -> Result<reqwest::RequestBuilder> {
        if target.credential_ref == "none" {
            return Ok(builder);
        }
        // Only environment references may cross an approved local connection.
        if !target.credential_ref.starts_with("env:") {
            return Err(InferenceError::Configuration);
        }
        let secret = self.resolver.resolve(&target.credential_ref)?;
        let mut header =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
                .map_err(|_| InferenceError::Configuration)?;
        header.set_sensitive(true);
        Ok(builder.header(reqwest::header::AUTHORIZATION, header))
    }
    fn encode_chat(&self, target: &Deployment, request: &ChatRequest) -> Result<serde_json::Value> {
        if !self.supports_chat_request(request) {
            return Err(InferenceError::Unsupported);
        }
        let mut value = openai::encode(&target.upstream_model, request);
        // Older compatible servers use max_tokens. Every local profile pins that dialect.
        if let Some(tokens) = value
            .as_object_mut()
            .unwrap()
            .remove("max_completion_tokens")
        {
            value["max_tokens"] = tokens;
        }
        if let Some(priority) = self.batch_priority(target) {
            value["priority"] = priority.into();
        }
        Ok(value)
    }
    /// vLLM `priority` of a batch line, only when the batch runner scoped it
    /// to this exact route (its scheduling settings opt in) and the profile
    /// is vLLM-compatible. Never sent otherwise.
    fn batch_priority(&self, target: &Deployment) -> Option<i32> {
        matches!(self.profile, Profile::Vllm | Profile::OpenAiCompatible)
            .then(|| crate::inference::scheduling::priority_for(target.id))
            .flatten()
    }
}
pub fn adapters(
    resolver: Arc<dyn SecretResolver>,
    approvals: ApprovedEndpoints,
) -> Vec<Arc<dyn ProviderAdapter>> {
    let approvals = Arc::new(approvals);
    [
        Profile::OpenAiCompatible,
        Profile::Vllm,
        Profile::Sglang,
        Profile::Ollama,
    ]
    .into_iter()
    .map(|profile| {
        Arc::new(LocalAdapter::new(
            profile,
            resolver.clone(),
            approvals.clone(),
        )) as Arc<dyn ProviderAdapter>
    })
    .collect()
}
#[async_trait]
impl ProviderAdapter for LocalAdapter {
    fn id(&self) -> &'static str {
        self.profile.id()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: true,
            tools: true,
        }
    }
    fn supports_chat_request(&self, request: &ChatRequest) -> bool {
        self.capabilities().supports(request)
            && !request.tools.iter().any(|t| t.strict == Some(true))
            && !(matches!(self.profile, Profile::Ollama) && request.tool_choice.is_some())
            && !(matches!(self.profile, Profile::OpenAiCompatible)
                && matches!(
                    request.tool_choice,
                    Some(ToolChoice::Required | ToolChoice::Function(_))
                ))
    }
    fn supports_embedding_request(&self, request: &EmbeddingRequest) -> bool {
        embeddings::validate(request).is_ok()
            && !(matches!(self.profile, Profile::OpenAiCompatible | Profile::Ollama)
                && request.dimensions.is_some())
    }
    /// The shared capability table (`providers::capabilities`); the rerank
    /// wire and System One support below must agree with it (tested).
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        super::capabilities::serves(self.id(), protocol)
    }
    async fn execute(&self, target: &Deployment, request: ChatRequest) -> Result<ProviderOutput> {
        let payload = self.encode_chat(target, &request)?;
        let (client, base) = self.connection(target)?;
        let response = self
            .authenticate(client.post(format!("{base}/chat/completions")), target)?
            .json(&payload)
            .send()
            .await
            .map_err(framing::transport)?;
        framing::status(response.status())?;
        if request.stream {
            framing::check_sse(&response)?;
            Ok(ProviderOutput::Stream(openai::decode_stream_profile(
                response,
                Some(self.id()),
            )))
        } else {
            let value = framing::body(response).await?;
            let response = openai::decode_complete_profile(&value, Some(self.id()))?;
            Ok(ProviderOutput::Complete(response))
        }
    }
    async fn execute_embeddings(
        &self,
        target: &Deployment,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse> {
        embeddings::validate(&request)?;
        if !self.supports_embedding_request(&request) {
            return Err(InferenceError::Unsupported);
        }
        let (client, base) = self.connection(target)?;
        let native = matches!(self.profile, Profile::Ollama);
        let (url, payload) = if native {
            // The base was canonically approved, including its origin and /v1 suffix.
            // Strip only that fixed suffix, retaining a reverse-proxy path prefix.
            let prefix = Self::prefix(base)?;
            (
                format!("{prefix}/api/embed"),
                embeddings::encode_ollama(&target.upstream_model, &request)?,
            )
        } else {
            let mut payload = embeddings::encode(&target.upstream_model, &request);
            if let Some(priority) = self.batch_priority(target) {
                payload["priority"] = priority.into();
            }
            (format!("{base}/embeddings"), payload)
        };
        let response = self
            .authenticate(client.post(url), target)?
            .json(&payload)
            .send()
            .await
            .map_err(framing::transport)?;
        framing::status(response.status())?;
        let value = framing::body(response).await?;
        if native {
            embeddings::decode_ollama(&value, &request)
        } else {
            embeddings::decode(&value, &request)
        }
    }
    async fn execute_rerank(
        &self,
        target: &Deployment,
        request: RerankRequest,
    ) -> Result<RerankResponse> {
        request.validate()?;
        let wire = self.rerank_wire().ok_or(InferenceError::Unsupported)?;
        let (_, base) = self.connection(target)?;
        // vLLM's canonical route is `/rerank` (its `/v1/rerank` is a deprecated alias).
        let url = match self.profile {
            Profile::Vllm => format!("{}/rerank", Self::prefix(base)?),
            _ => format!("{base}/rerank"),
        };
        let payload = workloads::rerank_body(&target.upstream_model, &request, wire);
        let value = self.post_json(target, url, &payload).await?;
        workloads::decode_rerank(&value, &request, wire)
    }
    async fn execute_systemone(
        &self,
        target: &Deployment,
        request: SystemoneRequest,
    ) -> Result<SystemoneResponse> {
        request.validate()?;
        if !self.serves_systemone() {
            return Err(InferenceError::Unsupported);
        }
        let (_, base) = self.connection(target)?;
        let url = format!("{base}/systemone");
        let payload = request.wire(&target.upstream_model);
        let value = self.post_json(target, url, &payload).await?;
        workloads::decode_systemone(&value, &request)
    }
}
#[cfg(test)]
mod cancellation_tests;
#[cfg(test)]
mod native_tests;
#[cfg(test)]
mod reasoning_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod workload_tests;

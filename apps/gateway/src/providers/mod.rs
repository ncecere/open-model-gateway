use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;

use crate::inference::{
    error::InferenceError,
    types::{
        ApiProtocol, Capabilities, ChatRequest, Deployment, EmbeddingRequest, EmbeddingResponse,
        ImageRequest, ImageResponse, ProviderOutput, RerankRequest, RerankResponse, SpeechRequest,
        SpeechResponse, SystemoneRequest, SystemoneResponse, TranscriptionRequest,
        TranscriptionResponse,
    },
};

pub mod anthropic;
pub(crate) mod audio;
pub mod bedrock;
#[cfg(test)]
pub(crate) mod contract;
pub(crate) mod embeddings;
pub(crate) mod framing;
pub(crate) mod images;
pub mod local;
pub(crate) mod metering;
pub mod openai;
pub mod openai_responses;
pub mod openrouter;
pub mod secrets;

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    fn supports_chat_request(&self, request: &ChatRequest) -> bool {
        self.capabilities().supports(request)
    }
    fn supports_embedding_request(&self, _request: &EmbeddingRequest) -> bool {
        true
    }
    /// Deployment-aware embedding support (e.g. fixed upstream dimensions),
    /// evaluated before admission so unsupported requests never dispatch.
    fn supports_embedding_target(&self, _target: &Deployment, request: &EmbeddingRequest) -> bool {
        self.supports_embedding_request(request)
    }
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        protocol == ApiProtocol::ChatCompletions
    }
    async fn execute_protocol(
        &self,
        target: &Deployment,
        request: ChatRequest,
        protocol: ApiProtocol,
    ) -> Result<ProviderOutput, InferenceError> {
        if !self.supports_protocol(protocol) {
            return Err(InferenceError::Unsupported);
        }
        self.execute(target, request).await
    }
    async fn execute_embeddings(
        &self,
        _target: &Deployment,
        _request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    // Non-generation workloads (see `inference::workload`). Each is gated by
    // `supports_protocol` and defaults to an explicit `Unsupported`. Dropping
    // the future MUST cancel upstream work; never retry internally.
    /// Deployment-aware image support (model family, size/quality/seed),
    /// evaluated before admission so unsupported requests never dispatch.
    fn supports_image_request(&self, _target: &Deployment, _request: &ImageRequest) -> bool {
        false
    }
    async fn execute_images(
        &self,
        _target: &Deployment,
        _request: ImageRequest,
    ) -> Result<ImageResponse, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    /// Request-specific transcription support (e.g. `prompt`), evaluated
    /// before admission. Adapters opt in.
    fn supports_transcription_request(
        &self,
        _target: &Deployment,
        _request: &TranscriptionRequest,
    ) -> bool {
        false
    }
    /// Request-specific speech support (e.g. output format). Adapters opt in.
    fn supports_speech_request(&self, _target: &Deployment, _request: &SpeechRequest) -> bool {
        false
    }
    async fn execute_audio_transcription(
        &self,
        _target: &Deployment,
        _request: TranscriptionRequest,
    ) -> Result<TranscriptionResponse, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    async fn execute_audio_speech(
        &self,
        _target: &Deployment,
        _request: SpeechRequest,
    ) -> Result<SpeechResponse, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    async fn execute_rerank(
        &self,
        _target: &Deployment,
        _request: RerankRequest,
    ) -> Result<RerankResponse, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    async fn execute_systemone(
        &self,
        _target: &Deployment,
        _request: SystemoneRequest,
    ) -> Result<SystemoneResponse, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    /// Dropping this future or the returned stream MUST cancel upstream work.
    /// Do not spawn detached producers. Return only sanitized errors.
    async fn execute(
        &self,
        target: &Deployment,
        request: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError>;
}

#[derive(Clone, Default)]
pub struct ProviderRegistry {
    adapters: BTreeMap<String, Arc<dyn ProviderAdapter>>,
}

impl ProviderRegistry {
    pub fn register(&mut self, adapter: Arc<dyn ProviderAdapter>) -> anyhow::Result<()> {
        let id = adapter.id();
        anyhow::ensure!(
            !id.is_empty()
                && id.len() <= 64
                && id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "invalid provider adapter ID"
        );
        anyhow::ensure!(
            !self.adapters.contains_key(id),
            "duplicate provider adapter ID"
        );
        self.adapters.insert(id.to_owned(), adapter);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn ProviderAdapter>> {
        self.adapters.get(id).cloned()
    }
}

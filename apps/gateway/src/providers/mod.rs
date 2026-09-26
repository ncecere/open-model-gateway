use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;

use crate::inference::{
    error::InferenceError,
    types::{ApiProtocol, Capabilities, ChatRequest, Deployment, ProviderOutput},
};

pub mod anthropic;
pub mod bedrock;
#[cfg(test)]
pub(crate) mod contract;
pub(crate) mod framing;
pub mod openai;
pub mod openai_responses;
pub mod secrets;

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
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

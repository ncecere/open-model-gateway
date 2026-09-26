use std::collections::BTreeSet;

use crate::inference::error::InferenceError;

/// Not Debug, Serialize, or Clone. Adapters may expose a value only to their transport.
pub struct Secret(String);
impl Secret {
    pub fn new(value: String) -> Result<Self, InferenceError> {
        if value.is_empty() || value.bytes().any(|b| b.is_ascii_control()) {
            return Err(InferenceError::Configuration);
        }
        Ok(Self(value))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

pub trait SecretResolver: Send + Sync {
    fn resolve(&self, reference: &str) -> Result<Secret, InferenceError>;
}

/// Platform-controlled allowlist. Connection rows cannot read arbitrary process env.
pub struct EnvSecrets {
    allowed: BTreeSet<String>,
}
impl EnvSecrets {
    pub fn new(names: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowed: names.into_iter().collect(),
        }
    }
}
impl SecretResolver for EnvSecrets {
    fn resolve(&self, reference: &str) -> Result<Secret, InferenceError> {
        let name = reference
            .strip_prefix("env:")
            .filter(|name| self.allowed.contains(*name))
            .ok_or(InferenceError::Configuration)?;
        Secret::new(std::env::var(name).map_err(|_| InferenceError::Configuration)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn denies_non_allowlisted_and_unsupported_references() {
        let resolver = EnvSecrets::new(Vec::new());
        assert!(resolver.resolve("env:PATH").is_err());
        assert!(resolver.resolve("file:/etc/passwd").is_err());
        assert!(Secret::new("key\nheader".into()).is_err());
    }
}

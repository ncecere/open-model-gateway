use std::collections::BTreeSet;

use crate::inference::error::InferenceError;

/// Operator settings that point at gateway-owned secret variables. Keep this
/// inventory alongside any new `GATEWAY_*_ENV` secret indirection.
pub const SECRET_POINTERS: &[&str] = &[
    "GATEWAY_FILE_ENCRYPTION_KEYS_ENV",
    "GATEWAY_S3_ACCESS_KEY_ID_ENV",
    "GATEWAY_S3_SECRET_ACCESS_KEY_ENV",
    "GATEWAY_SCIM_TOKEN_ENV",
    "GATEWAY_AUDIT_WEBHOOK_SECRET_ENV",
    "GATEWAY_AUDIT_OTLP_HEADERS_ENV",
    "GATEWAY_TRACES_OTLP_HEADERS_ENV",
];

/// A gateway-owned variable must never be directed to a provider or SMTP.
/// Looks up pointer names only, never secret values.
pub fn reserved_env_name(name: &str, lookup: &impl Fn(&str) -> Option<String>) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("PG")
        || upper.starts_with("OTEL_")
        || matches!(
            upper.as_str(),
            "HTTP_PROXY" | "HTTPS_PROXY" | "ALL_PROXY" | "NO_PROXY"
        )
        || matches!(name, "DATABASE_URL" | "DATABASE_URL_FILE")
        || name.starts_with("GATEWAY_")
        || name.starts_with("AWS_")
        || (crate::startup::SECRET_NAMES.contains(&name)
            && !matches!(name, "OPENAI_API_KEY" | "ANTHROPIC_API_KEY"))
        || !selecting_secret_pointers(name, lookup).is_empty()
}

/// Names of pointer settings, for safe startup diagnostics (never values).
pub fn selecting_secret_pointers(
    name: &str,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Vec<&'static str> {
    SECRET_POINTERS
        .iter()
        .copied()
        .filter(|setting| lookup(setting).is_some_and(|selected| selected.trim() == name))
        .collect()
}

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

/// Operator-controlled allowlist shared by provider and SMTP reference resolution.
/// Gateway-owned names are never eligible, even if accidentally allowlisted.
#[derive(Clone, Default)]
pub struct EnvSecrets {
    allowed: BTreeSet<String>,
}
impl EnvSecrets {
    pub fn new(names: impl IntoIterator<Item = String>) -> Self {
        Self::from_lookup(names, |name| std::env::var(name).ok())
    }

    pub fn from_lookup(
        names: impl IntoIterator<Item = String>,
        get: impl Fn(&str) -> Option<String>,
    ) -> Self {
        Self {
            allowed: names
                .into_iter()
                .filter(|name| !reserved_env_name(name, &get))
                .collect(),
        }
    }
    /// Validate a reference without resolving or exposing its credential.
    pub fn allows(&self, reference: &str) -> bool {
        reference
            .strip_prefix("env:")
            .is_some_and(|name| self.allowed.contains(name))
    }
}
impl SecretResolver for EnvSecrets {
    fn resolve(&self, reference: &str) -> Result<Secret, InferenceError> {
        let name = reference
            .strip_prefix("env:")
            .filter(|_| self.allows(reference))
            .ok_or(InferenceError::Configuration)?;
        Secret::new(std::env::var(name).map_err(|_| InferenceError::Configuration)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gateway_secrets_are_refused_even_when_allowlisted() {
        for name in [
            "DATABASE_URL",
            "DATABASE_URL_FILE",
            "PGPASSWORD",
            "PGSSLKEY",
            "PGPASSFILE",
            "PGFUTURE_OPTION",
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
            "OTEL_EXPORTER_OTLP_HEADERS",
            "AWS_SECRET_ACCESS_KEY",
            "GATEWAY_OIDC_CLIENT_SECRET",
            "GATEWAY_REPORTING_DATABASE_URL",
            "GATEWAY_FILE_ENCRYPTION_KEYS_ENV",
            "GATEWAY_AUDIT_WEBHOOK_SECRET_ENV",
            "GATEWAY_SCIM_TOKEN_ENV",
        ] {
            let resolver = EnvSecrets::new([name.to_owned()]);
            let reference = format!("env:{name}");
            assert!(
                !resolver.allows(&reference),
                "gateway-owned variable accepted"
            );
            assert!(resolver.resolve(&reference).is_err());
        }
    }

    #[test]
    fn indirect_gateway_secrets_are_refused_without_reading_their_values() {
        let settings = [
            "GATEWAY_FILE_ENCRYPTION_KEYS_ENV",
            "GATEWAY_S3_ACCESS_KEY_ID_ENV",
            "GATEWAY_S3_SECRET_ACCESS_KEY_ENV",
            "GATEWAY_SCIM_TOKEN_ENV",
            "GATEWAY_AUDIT_WEBHOOK_SECRET_ENV",
            "GATEWAY_AUDIT_OTLP_HEADERS_ENV",
        ];
        for setting in settings {
            let resolver = EnvSecrets::from_lookup(
                ["PRIVATE_MATERIAL".to_owned(), "OPENAI_API_KEY".to_owned()],
                |name| {
                    assert!(settings.contains(&name), "must not resolve secret values");
                    (name == setting).then(|| "PRIVATE_MATERIAL".to_owned())
                },
            );
            assert!(!resolver.allows("env:PRIVATE_MATERIAL"));
            assert!(resolver.resolve("env:PRIVATE_MATERIAL").is_err());
            assert!(resolver.allows("env:OPENAI_API_KEY"));
        }
    }

    #[test]
    fn denies_non_allowlisted_and_unsupported_references() {
        let resolver = EnvSecrets::new(Vec::new());
        assert!(resolver.resolve("env:PATH").is_err());
        assert!(resolver.resolve("file:/etc/passwd").is_err());
        assert!(Secret::new("key\nheader".into()).is_err());
    }
}

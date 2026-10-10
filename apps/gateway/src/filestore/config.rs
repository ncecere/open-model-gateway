//! Startup configuration (`GATEWAY_FILE_STORE=off|local|s3`) and the runtime handle.
//! Invalid configuration fails startup with a clear, secret-free error.
use std::{path::PathBuf, sync::Arc};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Encrypted, FileStore, KeyRing, s3::S3Settings};

pub const BACKEND_ENV: &str = "GATEWAY_FILE_STORE";
pub const DIR_ENV: &str = "GATEWAY_FILE_STORE_DIR";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackendKind {
    Local,
    S3,
    /// Tests only.
    Memory,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::S3 => "s3",
            Self::Memory => "memory",
        }
    }
}

enum Backend {
    Local(PathBuf),
    S3(Box<S3Settings>),
}

/// Parsed, validated configuration. No Debug (may hold static credentials).
pub struct FileStoreConfig {
    backend: Option<Backend>,
    keys: Option<KeyRing>,
}

impl FileStoreConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(&|name| std::env::var(name).ok())
    }

    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let get = |name: &str| {
            lookup(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let backend = match get(BACKEND_ENV).as_deref() {
            None | Some("off") => None,
            Some("local") => {
                let dir = get(DIR_ENV).ok_or_else(|| {
                    anyhow::anyhow!("{DIR_ENV} is required with {BACKEND_ENV}=local")
                })?;
                Some(Backend::Local(PathBuf::from(dir)))
            }
            Some("s3") => Some(Backend::S3(Box::new(S3Settings::from_lookup(lookup)?))),
            Some(_) => anyhow::bail!("{BACKEND_ENV} must be off, local or s3"),
        };
        // Keys are required when a backend is on, and validated whenever named.
        let keys = if backend.is_some() || get(super::crypto::KEYS_ENV).is_some() {
            Some(KeyRing::from_lookup(lookup)?)
        } else {
            None
        };
        Ok(Self { backend, keys })
    }
}

/// Non-secret description of the configured store (Admin › Settings › Storage).
#[derive(Clone)]
struct Location {
    json: Value,
    /// Stable fingerprint of backend + location, so a health result from an
    /// older configuration is not shown as current.
    fingerprint: String,
}

struct Active {
    store: Arc<dyn FileStore>,
    kind: BackendKind,
    key_id: String,
    key_ids: Vec<String>,
    location: Location,
}

/// The configured file store, or off. Cheap to clone; pass it to consumers.
#[derive(Clone, Default)]
pub struct FileStoreRuntime {
    active: Option<Arc<Active>>,
}

fn fingerprint(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    hex::encode(h.finalize())
}

impl FileStoreRuntime {
    pub fn off() -> Self {
        Self::default()
    }

    /// Builds the backend. Performs no network I/O (S3 credentials resolve lazily);
    /// the local backend creates/validates its directory.
    pub async fn build(config: FileStoreConfig) -> anyhow::Result<Self> {
        let Some(backend) = config.backend else {
            return Ok(Self::off());
        };
        let keys = Arc::new(
            config
                .keys
                .ok_or_else(|| anyhow::anyhow!("{} is required", super::crypto::KEYS_ENV))?,
        );
        let key_id = keys.active_id().to_owned();
        let key_ids = keys.ids().map(str::to_owned).collect();
        let (store, kind, location): (Arc<dyn FileStore>, _, _) = match backend {
            #[cfg(unix)]
            Backend::Local(dir) => {
                let local = super::local::LocalBackend::open(&dir)?;
                (
                    Arc::new(Encrypted::new(local, keys)),
                    BackendKind::Local,
                    Location {
                        json: json!({"kind": "local"}),
                        fingerprint: fingerprint(&["local", &dir.to_string_lossy()]),
                    },
                )
            }
            #[cfg(not(unix))]
            Backend::Local(_) => anyhow::bail!("the local file store requires a Unix host"),
            Backend::S3(settings) => {
                let location = Location {
                    json: json!({
                        "kind": "s3",
                        "bucket": settings.bucket(),
                        "region": settings.region(),
                        "endpoint_host": settings.endpoint_host(),
                        "endpoint_tls": settings.endpoint_tls(),
                        "path_style": settings.path_style(),
                        "prefix_set": settings.prefix().is_some(),
                        "auth": settings.auth_mode(),
                    }),
                    fingerprint: fingerprint(&[
                        "s3",
                        settings.bucket(),
                        settings.region(),
                        settings.endpoint.as_deref().unwrap_or(""),
                        settings.prefix().unwrap_or(""),
                    ]),
                };
                let backend = super::s3::S3Backend::new(&settings).await?;
                (
                    Arc::new(Encrypted::new(backend, keys)),
                    BackendKind::S3,
                    location,
                )
            }
        };
        Ok(Self {
            active: Some(Arc::new(Active {
                store,
                kind,
                key_id,
                key_ids,
                location,
            })),
        })
    }

    /// Wraps an existing store (tests, e.g. [`super::memory_store`]).
    pub fn from_store(store: Arc<dyn FileStore>, kind: BackendKind, key_ids: Vec<String>) -> Self {
        let key_id = key_ids.first().cloned().unwrap_or_else(|| "memory".into());
        Self {
            active: Some(Arc::new(Active {
                store,
                kind,
                key_id,
                key_ids,
                location: Location {
                    json: json!({"kind": kind.as_str()}),
                    fingerprint: fingerprint(&[kind.as_str()]),
                },
            })),
        }
    }

    /// An encrypted in-memory runtime (tests).
    #[cfg(any(test, feature = "integration-tests"))]
    pub fn memory() -> Self {
        Self::from_store(
            super::memory_store(),
            BackendKind::Memory,
            vec!["memory".into()],
        )
    }

    pub fn store(&self) -> Option<Arc<dyn FileStore>> {
        self.active.as_ref().map(|a| a.store.clone())
    }
    pub fn backend(&self) -> Option<BackendKind> {
        self.active.as_ref().map(|a| a.kind)
    }
    pub fn backend_name(&self) -> &'static str {
        self.backend().map_or("off", BackendKind::as_str)
    }
    pub fn active_key_id(&self) -> Option<&str> {
        self.active.as_ref().map(|a| a.key_id.as_str())
    }
    pub fn knows_key(&self, id: &str) -> bool {
        self.active
            .as_ref()
            .is_some_and(|a| a.key_ids.iter().any(|k| k == id))
    }
    pub fn decrypt_only_keys(&self) -> usize {
        self.active
            .as_ref()
            .map_or(0, |a| a.key_ids.len().saturating_sub(1))
    }
    pub fn location(&self) -> Option<Value> {
        self.active.as_ref().map(|a| a.location.json.clone())
    }
    pub fn fingerprint(&self) -> Option<&str> {
        self.active
            .as_ref()
            .map(|a| a.location.fingerprint.as_str())
    }
}

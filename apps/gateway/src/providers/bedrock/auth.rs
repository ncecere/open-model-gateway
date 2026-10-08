//! Bedrock connection identity, endpoint and route policy.
//!
//! A Bedrock connection stores one of three credential references, never a secret:
//!
//! - `aws:default`: the server's default AWS credential chain.
//! - `aws:profile:<name>`: one named profile from the server's AWS config files. The name
//!   must be on `GATEWAY_AWS_PROFILE_ALLOWLIST`.
//! - `aws:role:<role-arn>[;external_id=<id>][;session_name=<name>]`: STS AssumeRole,
//!   called with the server's default chain. The optional parts are stored in this fixed
//!   order; neither the ARN, external ID nor session name charset contains `;`.
//!
//! An optional HTTPS endpoint (VPC interface endpoint/PrivateLink) replaces the regional
//! Bedrock Runtime endpoint only when it exactly matches `GATEWAY_BEDROCK_ENDPOINT_ALLOWLIST`.
//! Validation here is pure. Constructing credential providers performs no I/O; AWS resolves
//! credentials lazily when the first signed request needs them.
//!
//! Nothing here implements Debug: references can carry an external ID.
use std::{collections::BTreeSet, time::Duration};

use aws_config::{
    BehaviorVersion, Region, SdkConfig, default_provider::credentials::DefaultCredentialsChain,
    profile::ProfileFileCredentialsProvider, provider_config::ProviderConfig,
    sts::AssumeRoleProvider,
};
use aws_sdk_bedrockruntime::config::{
    SharedCredentialsProvider, retry::RetryConfig, timeout::TimeoutConfig,
};

use crate::inference::types::Deployment;

pub(crate) const PROFILE_ALLOWLIST_ENV: &str = "GATEWAY_AWS_PROFILE_ALLOWLIST";
pub(crate) const ENDPOINT_ALLOWLIST_ENV: &str = "GATEWAY_BEDROCK_ENDPOINT_ALLOWLIST";
/// STS `RoleSessionName` when the connection sets none (visible in the role owner's CloudTrail).
pub(crate) const DEFAULT_SESSION_NAME: &str = "open-model-gateway";

/// How a Bedrock connection obtains AWS credentials.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum AwsAuth {
    Default,
    Profile(String),
    Role {
        arn: String,
        external_id: Option<String>,
        session_name: Option<String>,
    },
}

impl AwsAuth {
    /// Parses a stored reference. Anything noncanonical is rejected.
    pub(crate) fn parse(reference: &str) -> Option<Self> {
        if reference == "aws:default" {
            return Some(Self::Default);
        }
        if let Some(name) = reference.strip_prefix("aws:profile:") {
            return valid_profile_name(name).then(|| Self::Profile(name.to_owned()));
        }
        let mut parts = reference.strip_prefix("aws:role:")?.split(';');
        let arn = parts.next()?;
        let (mut external_id, mut session_name) = (None, None);
        for part in parts {
            if let Some(id) = part.strip_prefix("external_id=")
                && external_id.is_none()
                && session_name.is_none()
            {
                external_id = Some(id);
            } else if let Some(name) = part.strip_prefix("session_name=")
                && session_name.is_none()
            {
                session_name = Some(name);
            } else {
                return None;
            }
        }
        Self::role(arn, external_id, session_name)
    }

    /// Builds an assume-role reference from separately supplied management fields.
    pub(crate) fn role(
        arn: &str,
        external_id: Option<&str>,
        session_name: Option<&str>,
    ) -> Option<Self> {
        (valid_role_arn(arn)
            && external_id.is_none_or(valid_external_id)
            && session_name.is_none_or(valid_session_name))
        .then(|| Self::Role {
            arn: arn.to_owned(),
            external_id: external_id.map(str::to_owned),
            session_name: session_name.map(str::to_owned),
        })
    }

    /// Management input: a base reference without `;` options plus optional role fields.
    /// External ID and session name are accepted only with `aws:role:`.
    pub(crate) fn from_parts(
        reference: &str,
        external_id: Option<&str>,
        session_name: Option<&str>,
    ) -> Option<Self> {
        if reference.contains(';') {
            return None;
        }
        match reference.strip_prefix("aws:role:") {
            Some(arn) => Self::role(arn, external_id, session_name),
            None if external_id.is_none() && session_name.is_none() => Self::parse(reference),
            None => None,
        }
    }

    /// The canonical stored reference.
    pub(crate) fn reference(&self) -> String {
        match self {
            Self::Default => "aws:default".into(),
            Self::Profile(name) => format!("aws:profile:{name}"),
            Self::Role {
                arn,
                external_id,
                session_name,
            } => {
                let mut value = format!("aws:role:{arn}");
                if let Some(id) = external_id {
                    value.push_str(";external_id=");
                    value.push_str(id);
                }
                if let Some(name) = session_name {
                    value.push_str(";session_name=");
                    value.push_str(name);
                }
                value
            }
        }
    }
}

/// Server-controlled Bedrock allowlists. Invalid entries never match anything.
#[derive(Clone, Default)]
pub(crate) struct Policy {
    profiles: BTreeSet<String>,
    endpoints: BTreeSet<String>,
}

impl Policy {
    pub(crate) fn new<'a>(
        profiles: impl IntoIterator<Item = &'a str>,
        endpoints: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        Self {
            profiles: profiles
                .into_iter()
                .map(str::trim)
                .filter(|p| valid_profile_name(p))
                .map(str::to_owned)
                .collect(),
            endpoints: endpoints
                .into_iter()
                .filter_map(|e| canonical_endpoint(e.trim()))
                .collect(),
        }
    }

    /// Comma-separated `GATEWAY_AWS_PROFILE_ALLOWLIST` and `GATEWAY_BEDROCK_ENDPOINT_ALLOWLIST`.
    /// Both default to empty: only the server default identity and the regional endpoint.
    pub(crate) fn from_env() -> Self {
        let profiles = std::env::var(PROFILE_ALLOWLIST_ENV).unwrap_or_default();
        let endpoints = std::env::var(ENDPOINT_ALLOWLIST_ENV).unwrap_or_default();
        Self::new(profiles.split(','), endpoints.split(','))
    }

    fn allows(&self, auth: &AwsAuth) -> bool {
        match auth {
            AwsAuth::Profile(name) => self.profiles.contains(name),
            AwsAuth::Default | AwsAuth::Role { .. } => true,
        }
    }

    /// The canonical endpoint when it is valid and allowlisted.
    fn endpoint(&self, endpoint: &str) -> Option<String> {
        canonical_endpoint(endpoint).filter(|e| self.endpoints.contains(e))
    }
}

/// Everything needed to build one Bedrock Runtime client.
#[derive(Clone)]
pub(super) struct Plan {
    pub(super) region: String,
    pub(super) auth: AwsAuth,
    pub(super) endpoint: Option<String>,
}

impl Plan {
    /// Validates a deployment target against the server policy, before any I/O.
    pub(super) fn new(target: &Deployment, policy: &Policy) -> Option<Self> {
        let region = target.region.as_deref().filter(|r| valid_region(r))?;
        let auth = AwsAuth::parse(&target.credential_ref).filter(|a| policy.allows(a))?;
        let endpoint = match target.endpoint.as_deref() {
            None => None,
            Some(endpoint) => Some(policy.endpoint(endpoint)?),
        };
        (target.provider == "bedrock" && valid_upstream_model(&target.upstream_model, region)).then(
            || Self {
                region: region.to_owned(),
                auth,
                endpoint,
            },
        )
    }

    /// Client cache key. Kept in memory only; never logged.
    pub(super) fn key(&self) -> String {
        format!(
            "{}\n{}\n{}",
            self.region,
            self.auth.reference(),
            self.endpoint.as_deref().unwrap_or("")
        )
    }
}

/// Management validation for a Bedrock connection (create or update).
pub(crate) fn connection_valid(
    policy: &Policy,
    credential_ref: &str,
    endpoint: Option<&str>,
    region: Option<&str>,
) -> bool {
    region.is_some_and(valid_region)
        && AwsAuth::parse(credential_ref).is_some_and(|a| policy.allows(&a))
        && endpoint.is_none_or(|e| policy.endpoint(e).as_deref() == Some(e))
}

// `ProfileFiles` is aws-config's deprecated alias of aws-runtime's `EnvConfigFiles`; the
// alias avoids a direct aws-runtime dependency. Only tests pass explicit files.
#[allow(deprecated)]
pub(super) type ProfileFiles = aws_config::profile::profile_file::ProfileFiles;

/// The profile's credentials only: unlike the default chain, environment credentials
/// cannot take precedence over the named profile.
pub(super) fn profile_provider(
    name: &str,
    region: &str,
    files: Option<ProfileFiles>,
) -> ProfileFileCredentialsProvider {
    let mut builder = ProfileFileCredentialsProvider::builder()
        .configure(&ProviderConfig::default().with_region(Some(Region::new(region.to_owned()))))
        .profile_name(name);
    if let Some(files) = files {
        builder = builder.profile_files(files);
    }
    builder.build()
}

/// STS client configuration for AssumeRole: the server's identity, the connection's region
/// and the regional STS endpoint. Built explicitly, so shared AWS configuration (including
/// endpoint overrides) is not loaded. One attempt and bounded timeouts.
pub(super) fn sts_config(region: &str, base: SharedCredentialsProvider) -> SdkConfig {
    let mut builder = SdkConfig::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(region.to_owned()))
        .credentials_provider(base)
        .time_source(aws_smithy_async::time::SystemTimeSource::new())
        .retry_config(RetryConfig::standard().with_max_attempts(1))
        .timeout_config(
            TimeoutConfig::builder()
                .connect_timeout(Duration::from_secs(10))
                .operation_timeout(Duration::from_secs(30))
                .build(),
        );
    builder.set_sleep_impl(aws_smithy_async::rt::sleep::default_async_sleep());
    builder.build()
}

pub(super) async fn assume_role_provider(
    arn: &str,
    external_id: Option<&str>,
    session_name: Option<&str>,
    sts: &SdkConfig,
) -> AssumeRoleProvider {
    let mut builder = AssumeRoleProvider::builder(arn)
        .session_name(session_name.unwrap_or(DEFAULT_SESSION_NAME))
        .configure(sts);
    if let Some(id) = external_id {
        builder = builder.external_id(id);
    }
    builder.build().await
}

/// Credentials for a plan. No network I/O until the SDK first needs an identity.
pub(super) async fn credentials(auth: &AwsAuth, region: &str) -> SharedCredentialsProvider {
    let server = || {
        DefaultCredentialsChain::builder()
            .region(Region::new(region.to_owned()))
            .build()
    };
    match auth {
        AwsAuth::Default => SharedCredentialsProvider::new(server().await),
        AwsAuth::Profile(name) => {
            SharedCredentialsProvider::new(profile_provider(name, region, None))
        }
        AwsAuth::Role {
            arn,
            external_id,
            session_name,
        } => {
            let sts = sts_config(region, SharedCredentialsProvider::new(server().await));
            SharedCredentialsProvider::new(
                assume_role_provider(arn, external_id.as_deref(), session_name.as_deref(), &sts)
                    .await,
            )
        }
    }
}

fn charset(value: &str, extra: &[u8]) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
}

/// AWS region code such as `us-east-1`, `eu-central-2` or `us-gov-west-1`.
pub(crate) fn valid_region(region: &str) -> bool {
    let parts: Vec<&str> = region.split('-').collect();
    region.len() <= 32
        && parts.len() >= 3
        && parts[0].len() == 2
        && parts[..parts.len() - 1]
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase()))
        && parts
            .last()
            .is_some_and(|p| (1..=2).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
}

/// A name from the server's AWS config files (conservative charset).
pub(crate) fn valid_profile_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && charset(name, b"_.+-") && !name.starts_with('-')
}

fn valid_partition(partition: &str) -> bool {
    partition == "aws"
        || partition.strip_prefix("aws-").is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .split('-')
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase()))
        })
}

fn valid_account(account: &str) -> bool {
    account.len() == 12 && account.bytes().all(|b| b.is_ascii_digit())
}

/// `arn:<partition>:iam::<account>:role/<optional/path/>name`.
pub(crate) fn valid_role_arn(arn: &str) -> bool {
    let fields: Vec<&str> = arn.splitn(6, ':').collect();
    let [prefix, partition, service, region, account, resource] = fields[..] else {
        return false;
    };
    let Some(path_and_name) = resource.strip_prefix("role/") else {
        return false;
    };
    let (path, name) = path_and_name
        .rsplit_once('/')
        .map_or(("", path_and_name), |(p, n)| (p, n));
    arn.len() <= 2048
        && prefix == "arn"
        && valid_partition(partition)
        && service == "iam"
        && region.is_empty()
        && valid_account(account)
        && (1..=64).contains(&name.len())
        && charset(name, b"_+=,.@-")
        && path.len() <= 510
        && (path.is_empty()
            || path
                .split('/')
                .all(|s| !s.is_empty() && charset(s, b"_+=,.@-")))
}

/// STS `ExternalId`: 2-1224 characters of `[\w+=,.@:/-]`.
pub(crate) fn valid_external_id(id: &str) -> bool {
    (2..=1224).contains(&id.len()) && charset(id, b"_+=,.@:/-")
}

/// STS `RoleSessionName`: 2-64 characters of `[\w+=,.@-]`.
pub(crate) fn valid_session_name(name: &str) -> bool {
    (2..=64).contains(&name.len()) && charset(name, b"_+=,.@-")
}

/// Exact HTTPS origin without credentials, path, query or fragment, in canonical form.
fn canonical_endpoint(endpoint: &str) -> Option<String> {
    let url = reqwest::Url::parse(endpoint).ok()?;
    let canonical = url.as_str().trim_end_matches('/').to_owned();
    (url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url
            .host_str()
            .is_some_and(|h| !h.is_empty() && charset(h, b".-"))
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && (endpoint == canonical || endpoint == format!("{canonical}/")))
    .then_some(canonical)
}

/// Bedrock Runtime `modelId` for a route on a connection in `region`: a model ID, a
/// (cross-region) inference profile ID such as `us.anthropic.…`, or a Bedrock ARN in the
/// connection's own region (inference profiles must be invoked from their source region).
pub(crate) fn valid_upstream_model(model: &str, region: &str) -> bool {
    if model.starts_with("arn:") {
        return valid_model_arn(model, region);
    }
    (1..=256).contains(&model.len())
        && model.as_bytes()[0].is_ascii_alphanumeric()
        && charset(model, b"._:-")
}

fn valid_model_arn(arn: &str, region: &str) -> bool {
    let fields: Vec<&str> = arn.splitn(6, ':').collect();
    let [_, partition, service, arn_region, account, resource] = fields[..] else {
        return false;
    };
    let Some((kind, id)) = resource.split_once('/') else {
        return false;
    };
    let kind_ok = matches!(
        kind,
        "foundation-model"
            | "inference-profile"
            | "application-inference-profile"
            | "provisioned-model"
            | "custom-model-deployment"
            | "imported-model"
            | "prompt"
    );
    arn.len() <= 2048
        && valid_partition(partition)
        && service == "bedrock"
        && arn_region == region
        && (valid_account(account) || kind == "foundation-model" && account.is_empty())
        && kind_ok
        && (1..=256).contains(&id.len())
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && charset(id, b"._:/-")
}

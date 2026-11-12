//! Sign-in configuration: `IdentityConfig` from the environment and identity URL validation.
use super::*;

/// Intentionally not Debug: contains a confidential-client secret.
#[derive(Clone)]
pub struct IdentityConfig {
    pub(super) public_origin: String,
    pub(super) issuer: IssuerUrl,
    pub(super) client_id: String,
    pub(super) client_secret: Option<String>,
    pub(super) allow_loopback_http: bool,
    pub(super) secure_cookies: bool,
    pub(super) groups_claim: String,
    pub(super) trusted_proxies: throttle::TrustedProxies,
    pub(super) login_rate_per_minute: u32,
    pub(super) pending_login_cap: i64,
    pub(super) pending_cap_latch: Duration,
    pub(super) session_policy: SessionPolicy,
}

impl IdentityConfig {
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        fn read(name: &str) -> anyhow::Result<Option<String>> {
            match env::var(name) {
                Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
                Ok(_) => anyhow::bail!("{name} must not be empty"),
                Err(env::VarError::NotPresent) => Ok(None),
                Err(_) => anyhow::bail!("{name} is not valid Unicode"),
            }
        }
        let public = read("GATEWAY_PUBLIC_URL")?;
        let issuer = read("GATEWAY_OIDC_ISSUER")?;
        let client_id = read("GATEWAY_OIDC_CLIENT_ID")?;
        let secret = read("GATEWAY_OIDC_CLIENT_SECRET")?;
        // Checked even without OIDC, so a typo never goes unnoticed.
        let session_policy = SessionPolicy::parse(
            read("GATEWAY_SESSION_LIFETIME")?.as_deref(),
            read("GATEWAY_SESSION_IDLE_TIMEOUT")?.as_deref(),
        )?;
        if issuer.is_none() && client_id.is_none() && secret.is_none() {
            // A public URL may also be used by non-identity application features.
            if let Some(public) = public {
                Self::validate_origin(
                    &public,
                    env::var("GATEWAY_ENV").as_deref() == Ok("development"),
                )?;
            }
            return Ok(None);
        }
        let mut config = Self::parse(
            public.ok_or_else(|| anyhow::anyhow!("GATEWAY_PUBLIC_URL is required for OIDC"))?,
            issuer.ok_or_else(|| anyhow::anyhow!("GATEWAY_OIDC_ISSUER is required for OIDC"))?,
            client_id
                .ok_or_else(|| anyhow::anyhow!("GATEWAY_OIDC_CLIENT_ID is required for OIDC"))?,
            secret,
            env::var("GATEWAY_ENV").as_deref() == Ok("development"),
        )?;
        if let Some(path) = read("GATEWAY_OIDC_GROUPS_CLAIM")? {
            anyhow::ensure!(
                path.len() <= 512 && !path.chars().any(|c| c.is_whitespace() || c.is_control()),
                "Invalid OIDC groups claim path"
            );
            config.groups_claim = path;
        }
        if let Some(value) = read("GATEWAY_TRUSTED_PROXIES")? {
            config.trusted_proxies = throttle::TrustedProxies::parse(&value)?;
        }
        config.session_policy = session_policy;
        if let Some(value) = read("GATEWAY_LOGIN_RATE_PER_MINUTE")? {
            config.login_rate_per_minute = value
                .parse::<u32>()
                .ok()
                .filter(|n| (1..=10_000).contains(n))
                .ok_or_else(|| {
                    anyhow::anyhow!("GATEWAY_LOGIN_RATE_PER_MINUTE must be 1 to 10000")
                })?;
        }
        Ok(Some(config))
    }

    pub(crate) fn validate_origin(value: &str, development: bool) -> anyhow::Result<Url> {
        let url = validate_url(value, development)?;
        anyhow::ensure!(
            url.path() == "/" && url.query().is_none(),
            "GATEWAY_PUBLIC_URL must be an origin without a path or query"
        );
        anyhow::ensure!(
            value == url.origin().ascii_serialization()
                || value == format!("{}/", url.origin().ascii_serialization()),
            "GATEWAY_PUBLIC_URL must be a canonical origin"
        );
        Ok(url)
    }

    pub(super) fn parse(
        public: String,
        issuer: String,
        client_id: String,
        client_secret: Option<String>,
        development: bool,
    ) -> anyhow::Result<Self> {
        let public = Self::validate_origin(&public, development)?;
        let issuer_url = validate_url(&issuer, development)?;
        anyhow::ensure!(
            issuer_url.query().is_none(),
            "OIDC issuer must not contain a query"
        );
        anyhow::ensure!(
            !client_id.trim().is_empty(),
            "OIDC client ID must not be empty"
        );
        Ok(Self {
            public_origin: public.origin().ascii_serialization(),
            issuer: IssuerUrl::new(issuer).map_err(|_| anyhow::anyhow!("Invalid OIDC issuer"))?,
            client_id,
            client_secret,
            allow_loopback_http: development,
            secure_cookies: public.scheme() == "https",
            groups_claim: "groups".into(),
            trusted_proxies: throttle::TrustedProxies::default(),
            login_rate_per_minute: LOGIN_RATE_PER_MINUTE,
            pending_login_cap: PENDING_LOGIN_CAP,
            pending_cap_latch: Duration::from_secs(5),
            session_policy: SessionPolicy::default(),
        })
    }
}

pub(super) fn validate_url(value: &str, allow_loopback_http: bool) -> anyhow::Result<Url> {
    let url = Url::parse(value).map_err(|_| anyhow::anyhow!("Invalid identity URL"))?;
    anyhow::ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "Identity URL must have a host and no credentials or fragment"
    );
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    anyhow::ensure!(
        url.scheme() == "https" || (allow_loopback_http && loopback && url.scheme() == "http"),
        "Identity URLs require HTTPS (HTTP is limited to development loopback)"
    );
    Ok(url)
}

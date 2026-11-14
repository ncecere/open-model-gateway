//! The OIDC provider: signed claims, the policy-bound HTTP client, the discovered
//! provider and ID-token verification.
use super::*;

/// Unknown claims are captured inside the signature-verified ID token, never from userinfo.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(super) struct SignedClaims {
    #[serde(flatten)]
    values: std::collections::BTreeMap<String, serde_json::Value>,
}
impl AdditionalClaims for SignedClaims {}
type EnterpriseTokenFields = IdTokenFields<
    SignedClaims,
    EmptyExtraTokenFields,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJwsSigningAlgorithm,
>;
pub(super) type EnterpriseClient<A = EndpointNotSet, T = EndpointNotSet, U = EndpointNotSet> =
    Client<
        SignedClaims,
        CoreAuthDisplay,
        CoreGenderClaim,
        CoreJweContentEncryptionAlgorithm,
        CoreJsonWebKey,
        CoreAuthPrompt,
        StandardErrorResponse<CoreErrorResponseType>,
        StandardTokenResponse<EnterpriseTokenFields, CoreTokenType>,
        CoreTokenIntrospectionResponse,
        CoreRevocableToken,
        CoreRevocationErrorResponse,
        A,
        EndpointNotSet,
        EndpointNotSet,
        EndpointNotSet,
        T,
        U,
    >;
type DiscoveredClient = EnterpriseClient<EndpointSet, EndpointMaybeSet, EndpointMaybeSet>;
pub(super) type EnterpriseIdToken = IdToken<
    SignedClaims,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJwsSigningAlgorithm,
>;
type EnterpriseClaims = IdTokenClaims<SignedClaims, CoreGenderClaim>;

/// ID tokens must use an asymmetric signature advertised by the issuer. `none`
/// and shared-secret MACs (HS*) are never accepted, even if advertised.
pub(super) const ASYMMETRIC_ALGS: [CoreJwsSigningAlgorithm; 9] = [
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha384,
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha512,
    CoreJwsSigningAlgorithm::RsaSsaPssSha256,
    CoreJwsSigningAlgorithm::RsaSsaPssSha384,
    CoreJwsSigningAlgorithm::RsaSsaPssSha512,
    CoreJwsSigningAlgorithm::EcdsaP256Sha256,
    CoreJwsSigningAlgorithm::EcdsaP384Sha384,
    CoreJwsSigningAlgorithm::EdDsa,
];

impl SignedClaims {
    pub(super) fn groups(&self, path: &str) -> Option<Vec<String>> {
        // Prefer the literal claim name (URI and dot-containing claim names are legal).
        let value = if let Some(value) = self.values.get(path) {
            value
        } else {
            let mut parts = path.split('.');
            let mut value = self.values.get(parts.next()?)?;
            for part in parts {
                value = value.as_object()?.get(part)?;
            }
            value
        };
        let values = value.as_array()?;
        if values.len() > 2048 {
            return None;
        }
        values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    // #428: a control character would reach PostgreSQL as a
                    // bound group value; refuse the claim like any malformed one.
                    .filter(|s| {
                        !s.is_empty()
                            && crate::text::chars_at_most(s, 1024)
                            && !s.chars().any(char::is_control)
                    })
                    .map(str::to_owned)
            })
            .collect()
    }
}

#[derive(Clone)]
pub(super) struct OidcHttp {
    client: reqwest::Client,
    allow_loopback_http: bool,
}
impl OidcHttp {
    pub(super) fn new(allow_loopback_http: bool) -> anyhow::Result<Self> {
        Ok(Self {
            // No ambient HTTP(S)_PROXY/ALL_PROXY and no implicit retries,
            // like every provider client (#144): identity traffic goes
            // straight to the issuer, once.
            // Discovery, token and JWKS addresses pass the egress policy
            // (#33). `allow_loopback_http` is GATEWAY_ENV=development, as
            // in the global policy; passing it keeps tests (strict global
            // default) and production identical.
            client: crate::egress::client_builder(crate::egress::resolver_with(
                crate::egress::Purpose::Oidc,
                crate::egress::policy().permit_loopback(allow_loopback_http),
            ))
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            // No idle connections (#417): a provider closes them on
            // its own schedule, and a request written to one it is closing
            // is lost. With no retry that fails the sign-in after its
            // one-time attempt is consumed. Sign-in is rare; each request
            // pays for its own connection.
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|_| anyhow::anyhow!("Cannot initialize OIDC HTTP client"))?,
            allow_loopback_http,
        })
    }

    pub(super) async fn send(self, request: HttpRequest) -> Result<HttpResponse, std::io::Error> {
        // Apply the transport policy BEFORE every request, including the discovered JWKS URL.
        let fail = || std::io::Error::other("OIDC HTTP request failed");
        let url = request.uri().to_string();
        validate_url(&url, self.allow_loopback_http).map_err(|_| fail())?;
        // IP-literal hosts never reach the resolver: check them here (#33).
        let parsed = Url::parse(&url).map_err(|_| fail())?;
        crate::egress::check_url(
            &crate::egress::policy().permit_loopback(self.allow_loopback_http),
            crate::egress::Purpose::Oidc,
            &parsed,
        )
        .map_err(|_| fail())?;
        let request = reqwest::Request::try_from(request).map_err(|_| fail())?;
        let mut response = self.client.execute(request).await.map_err(|_| fail())?;
        let mut builder = axum::http::Response::builder().status(response.status());
        *builder.headers_mut().ok_or_else(fail)? = response.headers().clone();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| fail())? {
            if body.len() + chunk.len() > 1024 * 1024 {
                return Err(fail());
            }
            body.extend_from_slice(&chunk);
        }
        builder.body(body).map_err(|_| fail())
    }

    /// One bounded JWKS fetch under the same transport policy, with its Cache-Control max-age.
    pub(super) async fn fetch_jwks(self, uri: String) -> Result<jwks::Fetched, ()> {
        let request = axum::http::Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(header::ACCEPT, "application/json")
            .body(Vec::new())
            .map_err(|_| ())?;
        let response = self.send(request).await.map_err(|_| ())?;
        if response.status() != StatusCode::OK {
            return Err(());
        }
        let max_age = jwks::max_age(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
        );
        let keys: jwks::Keys = serde_json::from_slice(response.body()).map_err(|_| ())?;
        Ok(jwks::Fetched { keys, max_age })
    }
}

pub(super) struct Provider {
    pub(super) config: IdentityConfig,
    pub(super) client: DiscoveredClient,
    pub(super) http: OidcHttp,
    pub(super) jwks: JwksCache,
    pub(super) algs: Vec<CoreJwsSigningAlgorithm>,
    /// Per-replica sign-in start and callback buckets (#78).
    pub(super) login_limiter: throttle::Limiter,
    pub(super) callback_limiter: throttle::Limiter,
    /// Until when this replica treats the pending-attempt cap as reached,
    /// so refusals during a flood don't each count the table.
    pub(super) login_full_until: std::sync::Mutex<Option<std::time::Instant>>,
}

impl Provider {
    fn verifier(&self, keys: jwks::Keys) -> IdTokenVerifier<'static, CoreJsonWebKey> {
        // Always a public-client verifier: the client secret is never an ID-token key.
        IdTokenVerifier::new_public_client(
            ClientId::new(self.config.client_id.clone()),
            self.config.issuer.clone(),
            keys,
        )
        .set_allowed_algs(self.algs.iter().cloned())
    }
}

/// Signature, issuer, audience, expiry, nonce and algorithm policy. A token naming an
/// unknown key triggers one rate-limited, single-flight JWKS refetch and one retry.
pub(super) async fn verify_id_token<'t>(
    provider: &Provider,
    token: &'t EnterpriseIdToken,
    nonce: &Nonce,
) -> Result<&'t EnterpriseClaims, AuthError> {
    let unavailable = || AuthError(StatusCode::SERVICE_UNAVAILABLE, None);
    let keys = provider.jwks.current().await.ok_or_else(unavailable)?;
    match token.claims(&provider.verifier(keys), nonce) {
        Ok(claims) => Ok(claims),
        Err(ClaimsVerificationError::SignatureVerification(
            SignatureVerificationError::NoMatchingKey,
        )) => {
            let keys = provider
                .jwks
                .refresh_for_unknown_kid()
                .await
                .ok_or_else(unavailable)?;
            token
                .claims(&provider.verifier(keys), nonce)
                .map_err(|_| invalid())
        }
        Err(_) => Err(invalid()),
    }
}

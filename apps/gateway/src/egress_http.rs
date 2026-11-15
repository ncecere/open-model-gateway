//! The only place a reqwest client is built or held (#509, #569).
//!
//! Production code holds an outbound client only as [`HttpClient`], built
//! from [`client_builder`] or [`pinned_client_builder`] through
//! [`HttpClientBuilder`]. Neither type implements `Default` (asserted below),
//! and neither has a public constructor from a reqwest value, so a
//! `Default::default()`, `mem::take`, `unwrap_or_default()`, `or_default()`
//! or `#[derive(Default)]` that would produce one does not compile. A
//! reqwest client from its `Default` impl follows redirects, uses ambient
//! proxies and the system resolver.
//!
//! Clippy's `disallowed-types` (`apps/gateway/clippy.toml`) refuses
//! `reqwest::Client` and `reqwest::ClientBuilder` by name everywhere else, so
//! no field, parameter, annotation or `Client::default()` path can hold or
//! make a raw one; this module is the one sanctioned site.
#![expect(
    clippy::disallowed_types,
    reason = "the egress client newtype is the one place a reqwest client is held (#569)"
)]

use std::{sync::Arc, time::Duration};

use super::Resolver;

/// An outbound HTTP client with the egress safeguards (#509): no redirects,
/// no implicit retries, no ambient proxy, and the egress resolver (or, for an
/// approved local endpoint, only its pinned addresses). Cloning shares the
/// connection pool, as with reqwest.
#[derive(Clone, Debug)]
pub struct HttpClient(reqwest::Client);

/// A builder from [`client_builder`] or [`pinned_client_builder`]. It
/// exposes only settings that cannot change the safeguards: timeouts, the
/// idle pool, HTTPS-only and (through [`add_root_certificates`]) extra trust
/// anchors.
#[derive(Debug)]
#[must_use]
pub(crate) struct HttpClientBuilder(reqwest::ClientBuilder);

/// Compile-time proof that neither type implements `Default` (#569): if one
/// did, `some_item` would resolve through both impls and this would not
/// compile.
trait AmbiguousIfDefault<A> {
    fn some_item() {}
}
impl<T: ?Sized> AmbiguousIfDefault<()> for T {}
impl<T: Default> AmbiguousIfDefault<u8> for T {}
const _: fn() = || {
    let _ = <HttpClient as AmbiguousIfDefault<_>>::some_item;
    let _ = <HttpClientBuilder as AmbiguousIfDefault<_>>::some_item;
};

impl HttpClient {
    pub fn get<U: reqwest::IntoUrl>(&self, url: U) -> reqwest::RequestBuilder {
        self.0.get(url)
    }

    pub fn post<U: reqwest::IntoUrl>(&self, url: U) -> reqwest::RequestBuilder {
        self.0.post(url)
    }

    pub fn delete<U: reqwest::IntoUrl>(&self, url: U) -> reqwest::RequestBuilder {
        self.0.delete(url)
    }

    pub async fn execute(&self, request: reqwest::Request) -> reqwest::Result<reqwest::Response> {
        self.0.execute(request).await
    }

    pub fn request<U: reqwest::IntoUrl>(
        &self,
        method: reqwest::Method,
        url: U,
    ) -> reqwest::RequestBuilder {
        self.0.request(method, url)
    }

    /// Wraps a test's own client (a loopback test server without the
    /// resolver). Test builds only.
    #[cfg(test)]
    pub(crate) fn from_test_client(client: reqwest::Client) -> Self {
        Self(client)
    }
}

impl HttpClientBuilder {
    pub fn timeout(self, timeout: Duration) -> Self {
        Self(self.0.timeout(timeout))
    }

    pub fn connect_timeout(self, timeout: Duration) -> Self {
        Self(self.0.connect_timeout(timeout))
    }

    pub fn read_timeout(self, timeout: Duration) -> Self {
        Self(self.0.read_timeout(timeout))
    }

    pub fn pool_max_idle_per_host(self, max: usize) -> Self {
        Self(self.0.pool_max_idle_per_host(max))
    }

    /// How long the pool keeps an unused connection. A pool idle timeout
    /// below the upstream's closes the window in which a request is written
    /// to a connection the upstream is closing (#483). It changes no
    /// redirect, retry, proxy or resolver safeguard.
    pub fn pool_idle_timeout(self, timeout: Duration) -> Self {
        Self(self.0.pool_idle_timeout(timeout))
    }

    pub fn https_only(self, enabled: bool) -> Self {
        Self(self.0.https_only(enabled))
    }

    pub fn build(self) -> reqwest::Result<HttpClient> {
        self.0.build().map(HttpClient)
    }
}

/// The one way to start a reqwest client (#509): redirects, implicit retries
/// and ambient proxies (`HTTP(S)_PROXY`/`ALL_PROXY`) off, and every resolved
/// address checked by `resolver` (from [`super::resolver`] or
/// [`super::resolver_with`]). Callers add only their own timeouts, pool and
/// TLS-mode settings. Clippy's `disallowed-methods` and `disallowed-types`
/// (`apps/gateway/clippy.toml`) reject every other constructor and every
/// method that could change these safeguards outside this module.
#[expect(
    clippy::disallowed_methods,
    reason = "the egress-guarded constructor (#509)"
)]
pub(crate) fn client_builder(resolver: Arc<Resolver>) -> HttpClientBuilder {
    HttpClientBuilder(
        reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(resolver)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never()),
    )
}

/// [`client_builder`]'s counterpart for an approved local endpoint: the
/// approval's host connects only to its pinned addresses, with the same
/// redirect, retry and proxy safeguards. No DNS lookup happens for that host.
/// Only `providers::local::endpoints` can construct
/// [`ApprovedDestinations`](crate::providers::local::endpoints::ApprovedDestinations),
/// after its approval checks, so nothing else can pin an address here.
#[expect(
    clippy::disallowed_methods,
    reason = "the egress-guarded constructor (#509)"
)]
pub(crate) fn pinned_client_builder(
    approved: &crate::providers::local::endpoints::ApprovedDestinations,
) -> HttpClientBuilder {
    HttpClientBuilder(
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .resolve_to_addrs(approved.host(), approved.addresses()),
    )
}

/// Adds operator-configured trust anchors (`GATEWAY_S3_CA_FILE`) to a builder
/// from [`client_builder`]. The caller chooses the certificates; keeping the
/// call here means a new trust-anchor site has to use this named helper, not
/// `add_root_certificate` directly (#509).
#[expect(
    clippy::disallowed_methods,
    reason = "the egress-guarded constructor (#509)"
)]
pub(crate) fn add_root_certificates(
    builder: HttpClientBuilder,
    certificates: impl IntoIterator<Item = reqwest::Certificate>,
) -> HttpClientBuilder {
    let mut builder = builder.0;
    for certificate in certificates {
        builder = builder.add_root_certificate(certificate);
    }
    HttpClientBuilder(builder)
}

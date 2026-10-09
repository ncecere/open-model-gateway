//! Issuer JWKS cache.
//!
//! - TTL comes from `Cache-Control: max-age`, clamped to `[min_ttl, max_ttl]`
//!   (default one hour when the issuer sends none).
//! - Refresh is single-flight: concurrent callers wait for one fetch.
//! - An unknown `kid` triggers at most one refetch per `unknown_kid_interval`
//!   (installation-wide), so random key IDs cannot cause a fetch stampede.
//! - A failed fetch keeps the last good keys for at most `grace` after they
//!   expire, retrying no more often than `failure_backoff`. After the grace
//!   period sign-in fails closed.
//!
//! Algorithm policy (no `none`, no shared-secret MACs) is enforced by the
//! verifier in `identity.rs`, not here.
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use openidconnect::{JsonWebKey, JsonWebKeySet, core::CoreJsonWebKey};
use tokio::sync::Mutex;

pub(crate) type Keys = JsonWebKeySet<CoreJsonWebKey>;
pub(crate) type FetchFuture = Pin<Box<dyn Future<Output = Result<Fetched, ()>> + Send>>;
pub(crate) type Fetcher = Arc<dyn Fn() -> FetchFuture + Send + Sync>;
pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

pub(crate) struct Fetched {
    pub keys: Keys,
    pub max_age: Option<Duration>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct JwksPolicy {
    pub default_ttl: Duration,
    pub min_ttl: Duration,
    pub max_ttl: Duration,
    pub unknown_kid_interval: Duration,
    pub failure_backoff: Duration,
    pub grace: Duration,
}

impl Default for JwksPolicy {
    fn default() -> Self {
        Self {
            default_ttl: Duration::from_secs(60 * 60),
            min_ttl: Duration::from_secs(5 * 60),
            max_ttl: Duration::from_secs(24 * 60 * 60),
            unknown_kid_interval: Duration::from_secs(60),
            failure_backoff: Duration::from_secs(30),
            grace: Duration::from_secs(6 * 60 * 60),
        }
    }
}

impl JwksPolicy {
    pub(crate) fn ttl(&self, max_age: Option<Duration>) -> Duration {
        max_age
            .unwrap_or(self.default_ttl)
            .clamp(self.min_ttl, self.max_ttl)
    }
}

/// `max-age` from a Cache-Control header. `no-store`/`no-cache` without a
/// max-age mean "as short as allowed" (the policy minimum still applies).
pub(crate) fn max_age(header: Option<&str>) -> Option<Duration> {
    let header = header?;
    let mut uncached = false;
    for directive in header.split(',') {
        let directive = directive.trim();
        let lower = directive.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("max-age=") {
            return value
                .trim_matches('"')
                .parse::<u64>()
                .ok()
                .map(Duration::from_secs);
        }
        if lower == "no-store" || lower == "no-cache" {
            uncached = true;
        }
    }
    uncached.then_some(Duration::ZERO)
}

struct State {
    keys: Keys,
    /// Monotonic expiry of the current keys.
    expires: Instant,
    /// Last fetch attempt (successful or not).
    last_attempt: Instant,
    refreshed_at: DateTime<Utc>,
    fresh_until: DateTime<Utc>,
    last_failure_at: Option<DateTime<Utc>>,
    consecutive_failures: u32,
    /// Bumped on every attempt; lets waiters skip a fetch someone else just made.
    generation: u64,
}

pub(crate) struct JwksCache {
    fetch: Fetcher,
    policy: JwksPolicy,
    now: Clock,
    state: RwLock<State>,
    flight: Mutex<()>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Freshness {
    Fresh,
    /// Expired; last good keys kept because refresh failed (within grace).
    Stale,
    /// Past the grace period: sign-in fails closed until a refresh succeeds.
    Unavailable,
}

#[derive(Debug, Clone)]
pub(crate) struct JwksStatus {
    pub keys: usize,
    pub refreshed_at: DateTime<Utc>,
    pub fresh_until: DateTime<Utc>,
    pub last_failure_at: Option<DateTime<Utc>>,
    pub state: Freshness,
}

fn usable(keys: &Keys) -> bool {
    !keys.keys().is_empty()
        && keys.keys().len() <= 100
        && keys
            .keys()
            .iter()
            .all(|key| key.key_id().is_none_or(|kid| kid.len() <= 256))
}

impl JwksCache {
    /// Startup fetch must succeed: configured OIDC fails closed.
    pub(crate) async fn load(fetch: Fetcher, policy: JwksPolicy, now: Clock) -> Result<Self, ()> {
        let fetched = fetch().await?;
        if !usable(&fetched.keys) {
            return Err(());
        }
        let ttl = policy.ttl(fetched.max_age);
        let at = now();
        let wall = Utc::now();
        Ok(Self {
            state: RwLock::new(State {
                keys: fetched.keys,
                expires: at + ttl,
                last_attempt: at,
                refreshed_at: wall,
                fresh_until: wall + chrono::Duration::from_std(ttl).unwrap_or_default(),
                last_failure_at: None,
                consecutive_failures: 0,
                generation: 0,
            }),
            fetch,
            policy,
            now,
            flight: Mutex::new(()),
        })
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Keys usable for verification right now, refreshing first if expired.
    /// `None` only after the grace period with no successful refresh.
    pub(crate) async fn current(&self) -> Option<Keys> {
        let (expired, generation) = {
            let state = self.read();
            ((self.now)() >= state.expires, state.generation)
        };
        if expired {
            self.refresh(generation, false).await;
        }
        self.usable_keys()
    }

    /// Called when a token names a key we do not have. Refetches at most once
    /// per `unknown_kid_interval`; concurrent callers share that one fetch.
    pub(crate) async fn refresh_for_unknown_kid(&self) -> Option<Keys> {
        let generation = self.read().generation;
        self.refresh(generation, true).await;
        self.usable_keys()
    }

    fn usable_keys(&self) -> Option<Keys> {
        let state = self.read();
        ((self.now)() < state.expires + self.policy.grace).then(|| state.keys.clone())
    }

    async fn refresh(&self, observed: u64, unknown_kid: bool) {
        let _flight = self.flight.lock().await;
        {
            let state = self.read();
            let now = (self.now)();
            let since = now.saturating_duration_since(state.last_attempt);
            if state.generation != observed
                || (!unknown_kid && now < state.expires)
                || (unknown_kid && since < self.policy.unknown_kid_interval)
                || (state.consecutive_failures > 0 && since < self.policy.failure_backoff)
            {
                return;
            }
        }
        let result = (self.fetch)().await;
        let now = (self.now)();
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        state.generation += 1;
        state.last_attempt = now;
        match result {
            Ok(fetched) if usable(&fetched.keys) => {
                let ttl = self.policy.ttl(fetched.max_age);
                let wall = Utc::now();
                state.keys = fetched.keys;
                state.expires = now + ttl;
                state.refreshed_at = wall;
                state.fresh_until = wall + chrono::Duration::from_std(ttl).unwrap_or_default();
                state.consecutive_failures = 0;
            }
            _ => {
                state.last_failure_at = Some(Utc::now());
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                tracing::warn!(
                    failures = state.consecutive_failures,
                    "OIDC JWKS refresh failed; keeping last good keys within the grace period"
                );
            }
        }
    }

    pub(crate) fn status(&self) -> JwksStatus {
        let state = self.read();
        let now = (self.now)();
        JwksStatus {
            keys: state.keys.keys().len(),
            refreshed_at: state.refreshed_at,
            fresh_until: state.fresh_until,
            last_failure_at: state.last_failure_at,
            state: if now < state.expires {
                Freshness::Fresh
            } else if now < state.expires + self.policy.grace {
                Freshness::Stale
            } else {
                Freshness::Unavailable
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    #[derive(Clone)]
    struct ManualClock {
        base: Instant,
        offset: Arc<AtomicU64>,
    }
    impl ManualClock {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                offset: Arc::new(AtomicU64::new(0)),
            }
        }
        fn advance(&self, seconds: u64) {
            self.offset.fetch_add(seconds, Ordering::SeqCst);
        }
        fn clock(&self) -> Clock {
            let this = self.clone();
            Arc::new(move || this.base + Duration::from_secs(this.offset.load(Ordering::SeqCst)))
        }
    }

    fn key_set(kids: &[&str]) -> Keys {
        serde_json::from_value(serde_json::json!({
            "keys": kids.iter().map(|kid| serde_json::json!({
                "kty":"OKP","crv":"Ed25519","use":"sig","kid":kid,
                "x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"
            })).collect::<Vec<_>>()
        }))
        .unwrap()
    }

    struct Mock {
        hits: Arc<AtomicUsize>,
        fail: Arc<AtomicBool>,
        kids: Arc<std::sync::Mutex<Vec<&'static str>>>,
        max_age: Arc<std::sync::Mutex<Option<Duration>>>,
        delay: Duration,
    }
    impl Mock {
        fn new(delay: Duration) -> Self {
            Self {
                hits: Arc::default(),
                fail: Arc::default(),
                kids: Arc::new(std::sync::Mutex::new(vec!["a"])),
                max_age: Arc::default(),
                delay,
            }
        }
        fn fetcher(&self) -> Fetcher {
            let (hits, fail, kids, max_age, delay) = (
                self.hits.clone(),
                self.fail.clone(),
                self.kids.clone(),
                self.max_age.clone(),
                self.delay,
            );
            Arc::new(move || {
                let (hits, fail, kids, max_age) =
                    (hits.clone(), fail.clone(), kids.clone(), max_age.clone());
                Box::pin(async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    if fail.load(Ordering::SeqCst) {
                        return Err(());
                    }
                    let kids = kids.lock().unwrap().clone();
                    Ok(Fetched {
                        keys: key_set(&kids),
                        max_age: *max_age.lock().unwrap(),
                    })
                })
            })
        }
    }

    fn kids(keys: &Keys) -> Vec<String> {
        keys.keys()
            .iter()
            .filter_map(|k| k.key_id().map(|k| k.to_string()))
            .collect()
    }

    #[test]
    fn cache_control_is_parsed_and_bounded() {
        let policy = JwksPolicy::default();
        assert_eq!(
            max_age(Some("public, max-age=600")),
            Some(Duration::from_secs(600))
        );
        assert_eq!(
            max_age(Some("Max-Age=\"90\"")),
            Some(Duration::from_secs(90))
        );
        assert_eq!(max_age(Some("no-store")), Some(Duration::ZERO));
        assert_eq!(max_age(Some("max-age=nope")), None);
        assert_eq!(max_age(Some("public")), None);
        assert_eq!(max_age(None), None);
        assert_eq!(policy.ttl(None), Duration::from_secs(3600));
        assert_eq!(
            policy.ttl(Some(Duration::from_secs(1))),
            Duration::from_secs(300)
        );
        assert_eq!(
            policy.ttl(Some(Duration::from_secs(10_000_000))),
            Duration::from_secs(86_400)
        );
        assert_eq!(
            policy.ttl(Some(Duration::from_secs(900))),
            Duration::from_secs(900)
        );
    }

    #[tokio::test]
    async fn expiry_unknown_kid_rate_limit_and_grace() {
        let clock = ManualClock::new();
        let mock = Mock::new(Duration::ZERO);
        *mock.max_age.lock().unwrap() = Some(Duration::from_secs(600));
        let cache = JwksCache::load(mock.fetcher(), JwksPolicy::default(), clock.clock())
            .await
            .unwrap();
        assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
        // Within max-age: no refetch.
        clock.advance(599);
        assert_eq!(kids(&cache.current().await.unwrap()), ["a"]);
        assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
        // Rotation: unknown kid refetches (60 s since last attempt have passed).
        *mock.kids.lock().unwrap() = vec!["a", "b"];
        assert_eq!(
            kids(&cache.refresh_for_unknown_kid().await.unwrap()),
            ["a", "b"]
        );
        assert_eq!(mock.hits.load(Ordering::SeqCst), 2);
        // Rate limited: a second unknown kid within 60 s does not refetch.
        *mock.kids.lock().unwrap() = vec!["c"];
        clock.advance(59);
        assert_eq!(
            kids(&cache.refresh_for_unknown_kid().await.unwrap()),
            ["a", "b"]
        );
        assert_eq!(mock.hits.load(Ordering::SeqCst), 2);
        clock.advance(1);
        assert_eq!(kids(&cache.refresh_for_unknown_kid().await.unwrap()), ["c"]);
        assert_eq!(mock.hits.load(Ordering::SeqCst), 3);
        assert_eq!(cache.status().state, Freshness::Fresh);

        // Failure after expiry: last good keys within grace, with retry backoff.
        mock.fail.store(true, Ordering::SeqCst);
        clock.advance(601);
        assert_eq!(kids(&cache.current().await.unwrap()), ["c"]);
        assert_eq!(mock.hits.load(Ordering::SeqCst), 4);
        assert_eq!(cache.status().state, Freshness::Stale);
        assert!(cache.status().last_failure_at.is_some());
        clock.advance(29);
        assert!(cache.current().await.is_some());
        assert!(cache.refresh_for_unknown_kid().await.is_some());
        assert_eq!(mock.hits.load(Ordering::SeqCst), 4, "backoff after failure");
        clock.advance(1);
        assert!(cache.current().await.is_some());
        assert_eq!(mock.hits.load(Ordering::SeqCst), 5);
        // Past the grace period: fail closed.
        clock.advance(6 * 60 * 60);
        assert!(cache.current().await.is_none());
        assert_eq!(cache.status().state, Freshness::Unavailable);
        // Recovery.
        mock.fail.store(false, Ordering::SeqCst);
        *mock.kids.lock().unwrap() = vec!["d"];
        clock.advance(30);
        assert_eq!(kids(&cache.current().await.unwrap()), ["d"]);
        assert_eq!(cache.status().state, Freshness::Fresh);
    }

    #[tokio::test]
    async fn empty_key_sets_are_failures_not_replacements() {
        let clock = ManualClock::new();
        let mock = Mock::new(Duration::ZERO);
        let cache = JwksCache::load(mock.fetcher(), JwksPolicy::default(), clock.clock())
            .await
            .unwrap();
        mock.kids.lock().unwrap().clear();
        clock.advance(3600);
        assert_eq!(kids(&cache.current().await.unwrap()), ["a"]);
        assert_eq!(cache.status().state, Freshness::Stale);
        let empty = Mock::new(Duration::ZERO);
        empty.kids.lock().unwrap().clear();
        assert!(
            JwksCache::load(empty.fetcher(), JwksPolicy::default(), clock.clock())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn concurrent_refreshes_are_single_flight() {
        let clock = ManualClock::new();
        let mock = Mock::new(Duration::from_millis(50));
        let cache = Arc::new(
            JwksCache::load(mock.fetcher(), JwksPolicy::default(), clock.clock())
                .await
                .unwrap(),
        );
        clock.advance(3600);
        *mock.kids.lock().unwrap() = vec!["b"];
        let tasks: Vec<_> = (0..32)
            .map(|i| {
                let cache = cache.clone();
                tokio::spawn(async move {
                    if i % 2 == 0 {
                        cache.current().await
                    } else {
                        cache.refresh_for_unknown_kid().await
                    }
                })
            })
            .collect();
        for task in tasks {
            assert_eq!(kids(&task.await.unwrap().unwrap()), ["b"]);
        }
        assert_eq!(
            mock.hits.load(Ordering::SeqCst),
            2,
            "one startup + one refresh"
        );
    }
}

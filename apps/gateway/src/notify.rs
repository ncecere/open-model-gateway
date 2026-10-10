//! Configuration change notifications (migration 0028, scale plan P4).
//!
//! Configuration writes bump a per-domain version in `config_versions` and
//! `NOTIFY omg_config, '<topic>:<version>'` at commit. Each replica learns the
//! current versions two ways:
//!
//! - a LISTEN task on one dedicated session connection
//!   (`GATEWAY_LISTEN_DATABASE_URL`, default `DATABASE_URL`). LISTEN needs a
//!   session: point it at PostgreSQL (or a session-mode pooler), never at a
//!   transaction-mode PgBouncer. It reconnects with backoff and flushes every
//!   cache on each (re)connect;
//! - a poll of `config_versions` every second through the normal pool, which
//!   bounds staleness when notifications are lost or the listener is down.
//!
//! A cache entry ([`crate::cache`]) is stamped with the versions of its topics
//! when it is loaded (versions are read *before* the database read, so the
//! read sees at least that state) and is valid only while they are current.
//! Fail closed: until the first poll succeeds, and whenever no poll has
//! succeeded for [`FRESHNESS`], [`ConfigVersions::stamp`] returns `None` and
//! every cache is bypassed (live reads). Admission never reads a cache; it
//! re-checks authorization, catalog eligibility and policies live.
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use sqlx::PgPool;

/// The NOTIFY channel of migration 0028.
pub const CHANNEL: &str = "omg_config";
/// Version poll interval (also the staleness bound without notifications).
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Caches are bypassed when no version poll succeeded for this long.
pub const FRESHNESS: Duration = Duration::from_secs(3);

/// A configuration domain (`config_versions.topic`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Topic {
    /// Memberships, platform grants, user/workspace/service-account state and
    /// workspace model entitlements (grants, catalog overrides).
    Access,
    /// Models, deployments, provider connections, prices, routing, catalogs
    /// and workspace-type catalogs.
    Catalog,
    /// API key state and per-key model restrictions.
    Keys,
    /// Limits and budgets (admission reads them live; display only).
    Policy,
    /// Installation settings.
    Settings,
}
impl Topic {
    pub const ALL: [Self; 5] = [
        Self::Access,
        Self::Catalog,
        Self::Keys,
        Self::Policy,
        Self::Settings,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Access => "access",
            Self::Catalog => "catalog",
            Self::Keys => "keys",
            Self::Policy => "policy",
            Self::Settings => "settings",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == value)
    }
    fn index(self) -> usize {
        self as usize
    }
}

/// Parse a notification payload `<topic>:<version>`.
pub fn parse_payload(payload: &str) -> Option<(Topic, u64)> {
    let (topic, version) = payload.split_once(':')?;
    Some((Topic::parse(topic)?, version.parse().ok()?))
}

/// The configuration versions a cache entry was loaded under: the reconnect
/// generation and the versions of the cache's topics (others are zero).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    generation: u64,
    versions: [u64; 5],
}

/// This replica's view of the configuration versions.
pub struct ConfigVersions {
    versions: [AtomicU64; 5],
    /// Raised on every listener (re)connect and on enable: entries stamped
    /// under an older generation are invalid.
    generation: AtomicU64,
    /// Milliseconds since `origin` of the last successful poll (0: never).
    synced_at: AtomicU64,
    origin: Instant,
    enabled: AtomicBool,
    listener_up: AtomicBool,
}

impl Default for ConfigVersions {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigVersions {
    /// Disabled until [`start`] (or [`ConfigVersions::enable`] in tests):
    /// every cache is bypassed.
    pub fn new() -> Self {
        Self {
            versions: Default::default(),
            generation: AtomicU64::new(0),
            synced_at: AtomicU64::new(0),
            origin: Instant::now(),
            enabled: AtomicBool::new(false),
            listener_up: AtomicBool::new(false),
        }
    }

    pub fn enable(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.enabled.store(true, Ordering::Release);
    }

    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Whether the versions were confirmed by a poll within [`FRESHNESS`].
    pub fn fresh(&self) -> bool {
        if !self.enabled() {
            return false;
        }
        let synced = self.synced_at.load(Ordering::Acquire);
        synced != 0 && self.millis().saturating_sub(synced) <= FRESHNESS.as_millis() as u64
    }

    /// The stamp of `topics` now, or `None` (bypass every cache) when the
    /// versions are not fresh.
    pub fn stamp(&self, topics: &[Topic]) -> Option<Stamp> {
        if !self.fresh() {
            return None;
        }
        let mut versions = [0u64; 5];
        for topic in topics {
            versions[topic.index()] = self.versions[topic.index()].load(Ordering::Acquire);
        }
        Some(Stamp {
            generation: self.generation.load(Ordering::Acquire),
            versions,
        })
    }

    pub fn version(&self, topic: Topic) -> u64 {
        self.versions[topic.index()].load(Ordering::Acquire)
    }

    /// Record a committed version of `topic`; returns whether it was newer.
    pub fn observe(&self, topic: Topic, version: u64) -> bool {
        let previous = self.versions[topic.index()].fetch_max(version, Ordering::AcqRel);
        if previous < version {
            crate::metrics::METRICS.observe_config_change(topic.as_str());
            true
        } else {
            false
        }
    }

    /// Invalidate every cache entry (listener reconnect).
    pub fn flush(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn listener_up(&self) -> bool {
        self.listener_up.load(Ordering::Acquire)
    }

    /// Milliseconds since `origin`, offset by a day so a backdated sync
    /// (tests) never reaches 0 ("never synced").
    fn millis(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64 + 86_400_000
    }

    fn mark_synced(&self) {
        self.synced_at.store(self.millis(), Ordering::Release);
    }

    /// Tests: pretend the last successful poll happened `ago`.
    #[cfg(any(test, feature = "integration-tests"))]
    pub fn backdate_sync(&self, ago: Duration) {
        let at = self.millis().saturating_sub(ago.as_millis() as u64).max(1);
        self.synced_at.store(at, Ordering::Release);
    }
}

/// Read every topic's committed version (one query through the pool).
pub async fn poll_once(pool: &PgPool, versions: &ConfigVersions) -> Result<(), sqlx::Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT topic,version FROM config_versions")
        .fetch_all(pool)
        .await?;
    for (topic, version) in rows {
        if let Some(topic) = Topic::parse(&topic) {
            versions.observe(topic, version.max(0) as u64);
        }
    }
    versions.mark_synced();
    Ok(())
}

/// Enable `versions` and run the poll and LISTEN loops until aborted.
/// `listen` is the dedicated LISTEN pool (one session connection), `None` to
/// rely on polling alone.
pub fn start(
    pool: PgPool,
    listen: Option<PgPool>,
    versions: Arc<ConfigVersions>,
) -> tokio::task::JoinHandle<()> {
    versions.enable();
    tokio::spawn(async move {
        tokio::join!(
            poll_loop(pool, versions.clone()),
            listen_loop(listen, versions)
        );
    })
}

async fn poll_loop(pool: PgPool, versions: Arc<ConfigVersions>) {
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let polled =
            tokio::time::timeout(Duration::from_secs(2), poll_once(&pool, &versions)).await;
        if !matches!(polled, Ok(Ok(()))) {
            crate::metrics::METRICS.observe_config_poll_failure();
            if !versions.fresh() {
                tracing::warn!(
                    "configuration versions unconfirmed; caches bypassed until the next successful poll"
                );
            }
        }
    }
}

async fn listen_loop(listen: Option<PgPool>, versions: Arc<ConfigVersions>) {
    let Some(pool) = listen else {
        return std::future::pending().await;
    };
    let mut backoff = Duration::from_millis(250);
    loop {
        if let Ok(mut listener) = sqlx::postgres::PgListener::connect_with(&pool).await
            && listener.listen(CHANNEL).await.is_ok()
        {
            // Anything missed while disconnected: start over.
            versions.flush();
            versions.listener_up.store(true, Ordering::Release);
            crate::metrics::METRICS.set_config_listener(true);
            backoff = Duration::from_millis(250);
            // Ok(None): the connection was lost (sqlx would reconnect on the
            // next call; reconnect explicitly instead, flushing the caches).
            while let Ok(Some(notification)) = listener.try_recv().await {
                if let Some((topic, version)) = parse_payload(notification.payload()) {
                    versions.observe(topic, version);
                }
            }
            versions.listener_up.store(false, Ordering::Release);
            crate::metrics::METRICS.set_config_listener(false);
            versions.flush();
            tracing::warn!("configuration change listener disconnected; reconnecting");
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_parse_strictly() {
        assert_eq!(parse_payload("keys:7"), Some((Topic::Keys, 7)));
        assert_eq!(parse_payload("catalog:0"), Some((Topic::Catalog, 0)));
        for bad in ["", "keys", "keys:", "nope:1", "keys:-1", "keys:1:2", ":1"] {
            assert_eq!(parse_payload(bad), None, "{bad}");
        }
        for topic in Topic::ALL {
            assert_eq!(Topic::parse(topic.as_str()), Some(topic));
        }
    }

    #[test]
    fn stamps_fail_closed_and_track_versions() {
        let v = ConfigVersions::new();
        assert!(v.stamp(&[Topic::Keys]).is_none(), "disabled");
        v.enable();
        assert!(v.stamp(&[Topic::Keys]).is_none(), "never synced");
        v.mark_synced();
        let keys = v.stamp(&[Topic::Keys]).unwrap();
        // Other topics do not change a stamp.
        assert!(v.observe(Topic::Policy, 3));
        assert_eq!(v.stamp(&[Topic::Keys]), Some(keys));
        // Its own topic does; versions never move backwards.
        assert!(v.observe(Topic::Keys, 2));
        assert!(!v.observe(Topic::Keys, 1));
        assert_eq!(v.version(Topic::Keys), 2);
        let newer = v.stamp(&[Topic::Keys]).unwrap();
        assert_ne!(newer, keys);
        // A reconnect invalidates everything.
        v.flush();
        assert_ne!(v.stamp(&[Topic::Keys]), Some(newer));
        // Stale versions: bypass.
        v.backdate_sync(FRESHNESS + Duration::from_millis(50));
        assert!(v.stamp(&[Topic::Keys]).is_none());
        v.mark_synced();
        assert!(v.stamp(&[Topic::Keys]).is_some());
        v.disable();
        assert!(v.stamp(&[Topic::Keys]).is_none());
    }
}

//! Per-replica caches of pre-admission reads (scale plan P4, migration 0028).
//!
//! What is cached, and why it is safe:
//!
//! | Cache | Key | Topics | Used by |
//! |---|---|---|---|
//! | `keys` | SHA-256 of the presented token | keys, access | inference/workload authentication only |
//! | `candidates` | workspace, key, public model | catalog, access, keys | candidate deployments (`InferenceRepository::deployments`) |
//! | `routes` | workspace, public model | catalog, access | routing policy and per-deployment routing rows |
//! | `health` | deployment | TTL 1 s | passive circuit state for planning |
//! | prices (`governance`) | price id, deployment | immutable | admission and settlement valuation |
//!
//! Key entries hold the key id, workspace, issuing user and expiry: never the
//! token or its secret. Only *positive* results are cached (a successful live
//! authentication or revalidation), and only after the live read; the stamp is
//! taken before that read ([`crate::notify`]).
//!
//! **Admission never reads these caches.** It re-checks the key (revoked,
//! disabled, expired), the user, membership/service account, workspace state,
//! catalog eligibility, key restrictions, the latest price and every policy
//! live under its scope locks. A stale entry can therefore only route a
//! request to an admission that refuses it, or make a refusal arrive a few
//! milliseconds later; revocation still blocks new upstream work
//! immediately. A key the live re-check finds no longer valid is refused
//! exactly as authentication refuses it (`InferenceError::Unauthenticated`,
//! HTTP 401), and [`Caches::forget_key`] drops its entries on this replica. Key-authenticated routes that return workspace data or start
//! work outside the engine (files, batches, videos, realtime) authenticate
//! live. When the versions cannot be confirmed every cache is bypassed.
use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    auth::Principal,
    inference::types::Deployment,
    notify::{ConfigVersions, Stamp, Topic},
};

/// Result of a cache lookup.
pub enum Lookup<V> {
    Hit(V),
    /// Not cached (or stale): read live, then insert under this stamp.
    Miss(Stamp),
    /// Versions unconfirmed (or caches disabled): read live, do not insert.
    Bypass,
}

/// A bounded map whose entries are valid only under the current versions of
/// its topics.
pub struct VersionedCache<K, V> {
    name: &'static str,
    topics: &'static [Topic],
    capacity: usize,
    entries: Mutex<HashMap<K, (V, Stamp)>>,
}

impl<K: Eq + Hash + Clone, V: Clone> VersionedCache<K, V> {
    pub fn new(name: &'static str, topics: &'static [Topic], capacity: usize) -> Self {
        Self {
            name,
            topics,
            capacity: capacity.max(16),
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, versions: &ConfigVersions, key: &K) -> Lookup<V> {
        let Some(stamp) = versions.stamp(self.topics) else {
            crate::metrics::METRICS.observe_cache(self.name, "bypass");
            return Lookup::Bypass;
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match entries.get(key) {
            Some((value, at)) if *at == stamp => {
                crate::metrics::METRICS.observe_cache(self.name, "hit");
                Lookup::Hit(value.clone())
            }
            Some(_) => {
                entries.remove(key);
                crate::metrics::METRICS.observe_cache(self.name, "miss");
                Lookup::Miss(stamp)
            }
            None => {
                crate::metrics::METRICS.observe_cache(self.name, "miss");
                Lookup::Miss(stamp)
            }
        }
    }

    /// Insert a value read live after the lookup that returned `stamp`.
    pub fn insert(&self, key: K, value: V, stamp: Stamp) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() >= self.capacity && !entries.contains_key(&key) {
            // Entries of an older stamp first, then an arbitrary eighth.
            entries.retain(|_, (_, at)| *at == stamp);
            if entries.len() >= self.capacity {
                let drop: Vec<K> = entries
                    .keys()
                    .take(self.capacity / 8 + 1)
                    .cloned()
                    .collect();
                for k in drop {
                    entries.remove(&k);
                }
            }
        }
        entries.insert(key, (value, stamp));
    }

    pub fn remove(&self, key: &K) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }

    /// Drop every entry for which `remove` holds (a scan: for rare events
    /// such as a key found invalid by a live check).
    pub fn remove_where(&self, mut remove: impl FnMut(&K, &V) -> bool) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k, (v, _)| !remove(k, v));
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Key metadata of a successful live authentication (no secret material).
#[derive(Clone, Copy, Debug)]
pub struct KeyEntry {
    pub principal: Principal,
    /// Checked locally on every hit; admission checks it again with the
    /// database clock.
    pub expires_at: Option<DateTime<Utc>>,
}

/// Routing configuration of one public model as seen by one workspace:
/// the routing policy and, per eligible deployment, its routing row.
#[derive(Clone, Debug)]
pub struct RouteSnapshot {
    pub policy: Option<crate::routing::RoutingPolicy>,
    /// (priority, weight, residency) of each eligible deployment.
    pub rows: HashMap<Uuid, (i32, i32, String)>,
}

/// Passive health of one deployment as last read (advisory).
#[derive(Clone, Copy, Debug)]
pub struct HealthEntry {
    pub loaded: Instant,
    /// When the open circuit closes (local clock), if open.
    pub open_until: Option<Instant>,
    pub consecutive_failures: i32,
    /// Age of `last_observed_at` when loaded (`None`: never observed).
    pub observed_age: Option<Duration>,
}

/// Health entries are reused for this long (cross-replica circuit changes
/// become visible within it; this replica's own results invalidate at once).
pub const HEALTH_TTL: Duration = Duration::from_secs(1);

const KEY_TOPICS: &[Topic] = &[Topic::Keys, Topic::Access];
const CANDIDATE_TOPICS: &[Topic] = &[Topic::Catalog, Topic::Access, Topic::Keys];
const ROUTE_TOPICS: &[Topic] = &[Topic::Catalog, Topic::Access];

/// This replica's caches (shared by every clone of a `Store`).
pub struct Caches {
    pub versions: Arc<ConfigVersions>,
    pub(crate) keys: VersionedCache<[u8; 32], KeyEntry>,
    pub(crate) candidates: VersionedCache<(Uuid, Uuid, String), Arc<Vec<Deployment>>>,
    pub(crate) routes: VersionedCache<(Uuid, String), Arc<RouteSnapshot>>,
    health: Mutex<HashMap<Uuid, HealthEntry>>,
}

impl Default for Caches {
    fn default() -> Self {
        Self::new()
    }
}

impl Caches {
    pub fn new() -> Self {
        Self {
            versions: Arc::new(ConfigVersions::new()),
            keys: VersionedCache::new("keys", KEY_TOPICS, 250_000),
            candidates: VersionedCache::new("candidates", CANDIDATE_TOPICS, 100_000),
            routes: VersionedCache::new("routes", ROUTE_TOPICS, 50_000),
            health: Mutex::new(HashMap::new()),
        }
    }

    /// Whether caching is on for this replica (serve with
    /// `GATEWAY_CONFIG_CACHE=on`); the health cache follows it.
    pub fn enabled(&self) -> bool {
        self.versions.enabled()
    }

    /// A live check found the key `key_id` no longer valid: forget its
    /// cached authentication and candidates on this replica at once (the
    /// version bump of the change that invalidated it may not have arrived
    /// yet), so its next request authenticates live.
    pub(crate) fn forget_key(&self, key_id: Uuid) {
        self.keys
            .remove_where(|_, entry| entry.principal.key_id == key_id);
        self.candidates
            .remove_where(|(_, key, _), _| *key == key_id);
    }

    /// Fresh health entries of `ids`; the rest must be read.
    pub(crate) fn health(&self, ids: &[Uuid]) -> (HashMap<Uuid, HealthEntry>, Vec<Uuid>) {
        let entries = self.health.lock().unwrap_or_else(|e| e.into_inner());
        let mut hit = HashMap::new();
        let mut missing = Vec::new();
        for id in ids {
            match entries.get(id) {
                Some(e) if e.loaded.elapsed() < HEALTH_TTL => {
                    hit.insert(*id, *e);
                }
                _ => missing.push(*id),
            }
        }
        crate::metrics::METRICS.observe_cache_n("health", "hit", hit.len() as u64);
        crate::metrics::METRICS.observe_cache_n("health", "miss", missing.len() as u64);
        (hit, missing)
    }

    pub(crate) fn health_entry(&self, id: Uuid) -> Option<HealthEntry> {
        let entries = self.health.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .get(&id)
            .filter(|e| e.loaded.elapsed() < HEALTH_TTL)
            .copied()
    }

    pub(crate) fn store_health(&self, loaded: impl IntoIterator<Item = (Uuid, HealthEntry)>) {
        let mut entries = self.health.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() > 10_000 {
            entries.retain(|_, e| e.loaded.elapsed() < HEALTH_TTL);
        }
        entries.extend(loaded);
    }

    /// This replica recorded a result for `id`: forget its entry.
    pub(crate) fn forget_health(&self, id: Uuid) {
        self.health
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
}

/// An immutable-row cache (append-only data such as prices): entries never
/// need invalidation, only bounding.
pub struct ImmutableCache<K, V> {
    name: &'static str,
    capacity: usize,
    entries: Mutex<HashMap<K, V>>,
}

impl<K: Eq + Hash + Clone, V: Clone> ImmutableCache<K, V> {
    pub fn new(name: &'static str, capacity: usize) -> Self {
        Self {
            name,
            capacity: capacity.max(16),
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: &K) -> Option<V> {
        let value = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned();
        crate::metrics::METRICS
            .observe_cache(self.name, if value.is_some() { "hit" } else { "miss" });
        value
    }

    pub fn insert(&self, key: K, value: V) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() >= self.capacity && !entries.contains_key(&key) {
            let drop: Vec<K> = entries
                .keys()
                .take(self.capacity / 8 + 1)
                .cloned()
                .collect();
            for k in drop {
                entries.remove(&k);
            }
        }
        entries.insert(key, value);
    }
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "cache/invalidation_tests.rs"]
mod invalidation_tests;

#[cfg(all(test, feature = "integration-tests"))]
#[path = "cache/stale_auth_tests.rs"]
mod stale_auth_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_follow_their_topics_and_bound_their_size() {
        let versions = ConfigVersions::new();
        let cache: VersionedCache<u32, u32> = VersionedCache::new("test", &[Topic::Keys], 16);
        assert!(matches!(cache.get(&versions, &1), Lookup::Bypass));
        versions.enable();
        versions.backdate_sync(Duration::ZERO);
        let Lookup::Miss(stamp) = cache.get(&versions, &1) else {
            panic!("miss expected")
        };
        cache.insert(1, 10, stamp);
        assert!(matches!(cache.get(&versions, &1), Lookup::Hit(10)));
        versions.observe(Topic::Catalog, 5);
        assert!(matches!(cache.get(&versions, &1), Lookup::Hit(10)));
        versions.observe(Topic::Keys, 1);
        assert!(matches!(cache.get(&versions, &1), Lookup::Miss(_)));
        assert!(cache.is_empty(), "stale entries are dropped");
        // An entry loaded under an old stamp never becomes valid.
        cache.insert(2, 20, stamp);
        assert!(matches!(cache.get(&versions, &2), Lookup::Miss(_)));
        let Lookup::Miss(now) = cache.get(&versions, &3) else {
            panic!()
        };
        for k in 0..100 {
            cache.insert(k, k, now);
        }
        assert!(cache.len() <= 16);
        versions.backdate_sync(crate::notify::FRESHNESS * 2);
        assert!(matches!(cache.get(&versions, &99), Lookup::Bypass));
    }

    #[test]
    fn immutable_entries_are_bounded() {
        let cache: ImmutableCache<u32, u32> = ImmutableCache::new("test", 16);
        assert_eq!(cache.get(&1), None);
        for k in 0..100 {
            cache.insert(k, k);
        }
        assert_eq!(cache.get(&99), Some(99));
        assert!(cache.entries.lock().unwrap().len() <= 16);
    }
}

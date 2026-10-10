//! Canonical lock order of scoped admission (scale plan P3, migration 0027).
//!
//! Every transaction that admits, settles or changes something admission
//! depends on acquires a subset of these locks, always in this order, which
//! makes the protocol deadlock-free:
//!
//! 1. the catalog advisory lock [`CATALOG_LOCK`]: shared for admission and
//!    ordinary management, exclusive for global catalog changes;
//! 2. management only: the singleton installation row, for
//!    management-versus-management serialization (SCIM, sign-in grants,
//!    last-admin checks). Never taken by scoped admission or settlement;
//! 3. authority advisory locks ([`Scope`]): workspace type, workspace, user,
//!    key lineage, each ascending by key. Shared in admission, exclusive in a
//!    management change of that scope ([`exclusive`]);
//! 4. an existing reservation row (`FOR UPDATE`);
//! 5. the `budget_totals`, then `rate_minute_counters`, then
//!    `inflight_counters` rows the write will change, in primary-key order
//!    ([`rows`]); the 0015/0024 triggers then only touch rows already held;
//! 6. new rows (execution, reservation, ledger).
//!
//! Database triggers (0027) also take the exclusive authority lock on every
//! row change admission depends on, so no code path can forget it; the
//! gateway takes the same locks up front so they are never acquired out of
//! order. `GATEWAY_ADMISSION_MODE=global` restores the former protocol (the
//! installation row lock in admission and settlement) for one release.
use std::cell::Cell;

use chrono::{DateTime, Utc};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

/// The catalog advisory lock (single 64-bit key space).
pub const CATALOG_LOCK: i64 = 72419502;

/// Admission protocol (`GATEWAY_ADMISSION_MODE`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AdmissionMode {
    /// Scoped authority locks and totals/counter row locks (default).
    #[default]
    Scoped,
    /// The former protocol: admission and settlement serialize on the
    /// installation row (operational rollback for one release).
    Global,
}
impl AdmissionMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "scoped" => Some(Self::Scoped),
            "global" => Some(Self::Global),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scoped => "scoped",
            Self::Global => "global",
        }
    }
    /// `GATEWAY_ADMISSION_MODE` (unset: scoped). Invalid values are errors.
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var("GATEWAY_ADMISSION_MODE") {
            Err(std::env::VarError::NotPresent) => Ok(Self::default()),
            Ok(v) => Self::parse(v.trim())
                .ok_or_else(|| anyhow::anyhow!("GATEWAY_ADMISSION_MODE must be scoped or global")),
            Err(_) => anyhow::bail!("GATEWAY_ADMISSION_MODE must be scoped or global"),
        }
    }
}

/// An authority scope. The derived order is the canonical lock order
/// (variant order = class order), refined by [`Scope::key`] within a class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// A workspace type (`personal`, `team`, `project`): type defaults
    /// (policies, budgets, type catalogs) apply to every workspace of it.
    Type(WorkspaceType),
    Workspace(Uuid),
    User(Uuid),
    /// A key lineage (`api_keys.governance_key_id`).
    Lineage(Uuid),
}

/// Workspace kinds (immutable per workspace).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WorkspaceType {
    Personal,
    Team,
    Project,
}
impl WorkspaceType {
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "personal" => Some(Self::Personal),
            "team" => Some(Self::Team),
            "project" => Some(Self::Project),
            _ => None,
        }
    }
    pub const ALL: [Self; 3] = [Self::Personal, Self::Team, Self::Project];
}

impl Scope {
    /// Advisory lock class (first key of the two-int key space); ascending
    /// class is the canonical order between scope kinds.
    pub const fn class(self) -> i32 {
        match self {
            Self::Type(_) => 72419510,
            Self::Workspace(_) => 72419511,
            Self::User(_) => 72419512,
            Self::Lineage(_) => 72419513,
        }
    }
    /// Second advisory key: 1/2/3 for types, the first 32 bits of the id
    /// otherwise (`omg_scope_key` computes the same value in SQL).
    pub fn key(self) -> i32 {
        match self {
            Self::Type(WorkspaceType::Personal) => 1,
            Self::Type(WorkspaceType::Team) => 2,
            Self::Type(WorkspaceType::Project) => 3,
            Self::Workspace(id) | Self::User(id) | Self::Lineage(id) => {
                let b = id.as_bytes();
                i32::from_be_bytes([b[0], b[1], b[2], b[3]])
            }
        }
    }
    /// A type scope from a stored `workspaces.kind`.
    pub fn of_kind(kind: &str) -> Option<Self> {
        WorkspaceType::parse(kind).map(Self::Type)
    }
}

/// `scopes` in canonical order, one entry per advisory key (two scopes whose
/// keys collide share one lock).
pub fn canonical(scopes: impl IntoIterator<Item = Scope>) -> Vec<(i32, i32)> {
    let mut keys: Vec<(i32, i32)> = scopes.into_iter().map(|s| (s.class(), s.key())).collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Take exclusive authority locks for a management change of `scopes`, in
/// canonical order and in one round trip. Call it once per transaction,
/// after the catalog and installation locks and before the first write
/// (also before writing reservations, jobs or counters), with every scope the
/// transaction will change: a later call for an earlier class would run out
/// of order (refused in audit mode).
pub async fn exclusive(
    tx: &mut Transaction<'_, Postgres>,
    scopes: impl IntoIterator<Item = Scope>,
) -> Result<(), sqlx::Error> {
    acquire(tx, scopes, true).await
}

/// Shared authority locks (an operation that must not run concurrently
/// with a change of these scopes, like admission), in canonical order.
pub(crate) async fn shared(
    tx: &mut Transaction<'_, Postgres>,
    scopes: impl IntoIterator<Item = Scope>,
) -> Result<(), sqlx::Error> {
    acquire(tx, scopes, false).await
}

async fn acquire(
    tx: &mut Transaction<'_, Postgres>,
    scopes: impl IntoIterator<Item = Scope>,
    exclusive: bool,
) -> Result<(), sqlx::Error> {
    let keys = canonical(scopes);
    if keys.is_empty() {
        return Ok(());
    }
    let (classes, keys): (Vec<i32>, Vec<i32>) = keys.into_iter().unzip();
    sqlx::query("SELECT omg_lock_scopes($1,$2,$3)")
        .bind(&classes)
        .bind(&keys)
        .bind(exclusive)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Lineages of the keys of `workspace` matching `filter` (a SQL condition on
/// `api_keys` with `$1` the workspace and `$2` an optional id), read without
/// row locks (`governance_key_id` is immutable): the scopes a management
/// change of those keys must lock before writing.
pub(crate) async fn key_lineages(
    tx: &mut Transaction<'_, Postgres>,
    filter: &str,
    workspace: Option<Uuid>,
    id: Option<Uuid>,
) -> Result<Vec<Scope>, sqlx::Error> {
    let ids: Vec<Uuid> = sqlx::query_scalar(&format!(
        "SELECT DISTINCT governance_key_id FROM api_keys WHERE ($1::uuid IS NULL OR workspace_id=$1) AND ({filter})"
    ))
    .bind(workspace)
    .bind(id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(ids.into_iter().map(Scope::Lineage).collect())
}

/// Admission's lock prefix: the shared catalog lock, then the shared
/// authority locks of the key's workspace type, workspace, issuing user and
/// lineage (one round trip, ordered server-side). Returns the key lineage, or
/// `None` for an unknown workspace/key (live revalidation then refuses it).
pub(crate) async fn admission(
    tx: &mut Transaction<'_, Postgres>,
    principal: &crate::auth::Principal,
) -> Result<Option<Uuid>, sqlx::Error> {
    let (lineage, _kind): (Option<Uuid>, Option<String>) =
        sqlx::query_as("SELECT lineage,kind FROM omg_admission_locks($1,$2,$3)")
            .bind(principal.workspace_id)
            .bind(principal.user_id)
            .bind(principal.key_id)
            .fetch_one(&mut **tx)
            .await?;
    Ok(lineage)
}

/// One reservation/execution write: the scopes (workspace, the key's
/// lineage) and the admission instant whose buckets it changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Touch {
    pub workspace: Uuid,
    pub api_key: Uuid,
    pub at: DateTime<Utc>,
}

/// Lock, in canonical order, every `budget_totals` row (and with `counters`
/// every minute and in-flight counter row) the writes `touches` will change,
/// creating missing rows as zeros. The next statement reads the latest
/// committed values of the held rows.
pub(crate) async fn rows(
    tx: &mut Transaction<'_, Postgres>,
    touches: &[Touch],
    counters: bool,
) -> Result<(), sqlx::Error> {
    if touches.is_empty() {
        return Ok(());
    }
    let workspaces: Vec<Uuid> = touches.iter().map(|t| t.workspace).collect();
    let keys: Vec<Uuid> = touches.iter().map(|t| t.api_key).collect();
    let ats: Vec<DateTime<Utc>> = touches.iter().map(|t| t.at).collect();
    sqlx::query("SELECT omg_lock_scope_rows($1,$2,$3,$4)")
        .bind(&workspaces)
        .bind(&keys)
        .bind(&ats)
        .bind(counters)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Whether `error` is a deadlock the database resolved by aborting this
/// transaction (SQLSTATE 40P01).
pub(crate) fn is_deadlock(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(e) if e.code().as_deref() == Some("40P01"))
}

tokio::task_local! {
    /// Set when a statement of the current governance attempt failed with a
    /// deadlock (see [`retry_deadlocks`]).
    static DEADLOCKED: Cell<bool>;
}

/// Record a storage error of the running governance attempt.
pub(crate) fn note(error: &sqlx::Error) {
    if is_deadlock(error) {
        let _ = DEADLOCKED.try_with(|flag| flag.set(true));
    }
}

/// Attempts of one governance transaction when the database aborts it as a
/// deadlock victim (two retries).
pub(crate) const DEADLOCK_ATTEMPTS: u32 = 3;

/// Run one database-only governance transaction (`$attempt`, an expression
/// awaiting it), re-running it (at most [`DEADLOCK_ATTEMPTS`] times) only
/// when it failed because the database aborted it as a deadlock victim
/// (40P01). The failed attempt rolled back entirely; callers run before
/// dispatch (admission) or after upstream work ended (settlement), so no
/// upstream work is ever repeated. The canonical order makes deadlocks
/// impossible between gateway transactions; this is a safety net for
/// out-of-order acquisition elsewhere (e.g. a hand-written SQL session).
macro_rules! retry_deadlocks {
    ($path:expr, $attempt:expr) => {{
        let mut tries = 0u32;
        loop {
            tries += 1;
            let (result, deadlocked) = $crate::governance::locks::watch(async { $attempt }).await;
            if result.is_err() && deadlocked && tries < $crate::governance::locks::DEADLOCK_ATTEMPTS
            {
                $crate::metrics::METRICS.observe_deadlock_retry($path);
                continue;
            }
            break result;
        }
    }};
}
pub(crate) use retry_deadlocks;

/// Run `attempt` and report whether one of its statements failed with a
/// deadlock (recorded by [`note`]).
pub(crate) async fn watch<T>(attempt: impl std::future::Future<Output = T>) -> (T, bool) {
    DEADLOCKED
        .scope(Cell::new(false), async {
            let result = attempt.await;
            (result, DEADLOCKED.with(Cell::get))
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(rank: u8, id: Uuid) -> Scope {
        match rank % 4 {
            0 => Scope::Type(WorkspaceType::ALL[(id.as_bytes()[5] % 3) as usize]),
            1 => Scope::Workspace(id),
            2 => Scope::User(id),
            _ => Scope::Lineage(id),
        }
    }

    #[test]
    fn keys_match_the_sql_helper() {
        // omg_scope_key: the first 32 bits of the uuid, two's complement.
        let id = Uuid::parse_str("ffffffff-0000-4000-8000-000000000000").unwrap();
        assert_eq!(Scope::Workspace(id).key(), -1);
        let id = Uuid::parse_str("00000001-ffff-4fff-bfff-ffffffffffff").unwrap();
        assert_eq!(Scope::Lineage(id).key(), 1);
        assert_eq!(Scope::Type(WorkspaceType::Personal).key(), 1);
        assert_eq!(Scope::Type(WorkspaceType::Project).key(), 3);
        assert!(Scope::Type(WorkspaceType::Team).class() < Scope::Workspace(id).class());
        assert!(Scope::Workspace(id).class() < Scope::User(id).class());
        assert!(Scope::User(id).class() < Scope::Lineage(id).class());
    }

    /// Property: whatever order scopes are requested in, the acquisition
    /// sequence is sorted by (class, key) without duplicates, so any two
    /// transactions acquire their common locks in the same relative order (no
    /// wait-for cycle between them).
    #[test]
    fn canonical_order_is_a_total_order_independent_of_request_order() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // A small id pool so collisions and duplicates are frequent.
        let pool: Vec<Uuid> = (0..6)
            .map(|_| {
                let mut b = [0u8; 16];
                b[..8].copy_from_slice(&next().to_be_bytes());
                b[8..].copy_from_slice(&next().to_be_bytes());
                Uuid::from_bytes(b)
            })
            .collect();
        for _ in 0..2_000 {
            let n = (next() % 9) as usize;
            let scopes: Vec<Scope> = (0..n)
                .map(|_| scope(next() as u8, pool[(next() % 6) as usize]))
                .collect();
            let order = canonical(scopes.clone());
            assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
            let mut shuffled = scopes.clone();
            shuffled.reverse();
            shuffled.rotate_left(n / 2);
            assert_eq!(canonical(shuffled), order);
            // Every requested scope is covered.
            for s in &scopes {
                assert!(order.contains(&(s.class(), s.key())));
            }
            // Two transactions: their common locks appear in the same
            // relative order in both acquisition sequences.
            let other = canonical(
                (0..(next() % 9) as usize)
                    .map(|_| scope(next() as u8, pool[(next() % 6) as usize])),
            );
            let common_a: Vec<_> = order.iter().filter(|k| other.contains(k)).collect();
            let common_b: Vec<_> = other.iter().filter(|k| order.contains(k)).collect();
            assert_eq!(common_a, common_b);
        }
    }

    #[test]
    fn admission_lock_prefix_is_canonical() {
        // Admission's shared prefix (type, workspace, user, lineage) is
        // already in canonical order for any ids.
        let mut seed = 7u64;
        for _ in 0..1_000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let ids: Vec<Uuid> = (0..3)
                .map(|i| {
                    Uuid::from_u128(u128::from(seed.rotate_left(i * 7)) << 64 | u128::from(seed))
                })
                .collect();
            let prefix = [
                Scope::Type(WorkspaceType::Team),
                Scope::Workspace(ids[0]),
                Scope::User(ids[1]),
                Scope::Lineage(ids[2]),
            ];
            let keys: Vec<_> = prefix.iter().map(|s| (s.class(), s.key())).collect();
            assert_eq!(canonical(prefix), keys);
        }
    }

    #[test]
    fn modes_parse() {
        assert_eq!(AdmissionMode::parse("scoped"), Some(AdmissionMode::Scoped));
        assert_eq!(AdmissionMode::parse("global"), Some(AdmissionMode::Global));
        assert_eq!(AdmissionMode::parse("serialized"), None);
        assert_eq!(AdmissionMode::default(), AdmissionMode::Scoped);
    }
}

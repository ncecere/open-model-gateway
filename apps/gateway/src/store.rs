use sqlx::{PgConnection, PgPool, migrate::Migrator};

pub static MIGRATOR: Migrator = sqlx::migrate!("./enterprise_migrations");
/// The one reviewed runtime-role allowlist; `grants apply`, staging and the Helm Job all run it.
pub const RUNTIME_GRANTS_SQL: &str = include_str!("../../../deploy/staging/runtime-grants.sql");
/// ACL assertions and rollback-only runtime probes run by `grants verify`.
pub const VERIFY_PRIVILEGES_SQL: &str =
    include_str!("../../../deploy/staging/verify-privileges.sql");

/// `/health/ready` inputs; `Default` is "database unreachable".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Readiness {
    pub database: bool,
    /// Migration family/lineage/checksums exactly match this binary.
    pub schema: bool,
}

#[derive(Clone)]
pub struct Store {
    pub(crate) pool: PgPool,
    /// Immutable operator policy, injected once at startup; empty by default.
    pub(crate) provider_secrets: std::sync::Arc<crate::providers::secrets::EnvSecrets>,
    pub(crate) smtp_secrets: std::sync::Arc<crate::providers::secrets::EnvSecrets>,
    pub(crate) alert_webhooks: std::sync::Arc<crate::config::AlertWebhookPolicy>,
    /// In-process queues in front of the installation lock (see
    /// `governance::LockGate`): waiters queue here instead of holding pooled
    /// connections, so authentication and settlement are not starved.
    pub(crate) lock_gates: std::sync::Arc<crate::governance::LockGates>,
    /// Bounded per-process alert dispatch history, shared across ticks/clones.
    /// Advisory scheduling only: no recipient or durable lease authority.
    pub(crate) alert_dispatch:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<uuid::Uuid, std::time::Instant>>>,
    /// Admission protocol (`GATEWAY_ADMISSION_MODE`, see
    /// `governance::locks`): scoped locks (default) or the former
    /// installation row lock.
    pub(crate) admission_mode: crate::governance::locks::AdmissionMode,
    /// Optional reporting replica (`GATEWAY_REPORTING_DATABASE_URL`) for
    /// reports, usage and logs only; see [`crate::reporting`].
    pub(crate) reporting: Option<PgPool>,
    /// Replay lag beyond which the reporting replica is skipped.
    pub(crate) reporting_max_lag: std::time::Duration,
    /// Per-replica caches of pre-admission reads (`crate::cache`), shared by
    /// every clone; bypassed until change notifications are started.
    pub(crate) caches: std::sync::Arc<crate::cache::Caches>,
    /// This replica's background work leases (`crate::leases`); `None`
    /// outside `serve` (CLI one-shot commands and tests are not leased).
    pub(crate) leases: Option<std::sync::Arc<crate::leases::Leases>>,
    /// Audit export (#106), when the operator configured a destination.
    pub(crate) audit_export: Option<std::sync::Arc<crate::audit_export::Exporter>>,
    /// Subject exports building at once on this replica (#74).
    pub(crate) export_slots: std::sync::Arc<tokio::sync::Semaphore>,
    /// Bounded trace recorder (`Store::with_traces`, #98); `None` disables
    /// tracing, including the independent background roots this store's own
    /// maintenance jobs (`rate_prune`, `lock_wait_prune`) record.
    pub(crate) traces: Option<crate::telemetry::Recorder>,
    /// Test-only pinned admission instant shared by every clone of this store.
    /// Production builds have no override: admission always reads the
    /// database clock (see [`Store::admission_now`]).
    #[cfg(any(test, feature = "integration-tests"))]
    admission_clock: std::sync::Arc<std::sync::OnceLock<chrono::DateTime<chrono::Utc>>>,
}

// Keep this inventory in step with future enterprise migrations. Preflight rejects
// unrelated public relations; checksum validation is lineage validation, not a
// claim that every function/index/constraint is tamper-proof schema attestation.
const ENTERPRISE_RELATIONS: &[&str] = &[
    "_sqlx_migrations",
    "installation",
    "users",
    "oidc_identities",
    "platform_role_grants",
    "effective_platform_roles",
    "cost_centers",
    "workspaces",
    "oidc_group_mappings",
    "workspace_membership_grants",
    "effective_workspace_memberships",
    "oidc_login_attempts",
    "browser_sessions",
    "workspace_invitations",
    "service_accounts",
    "api_keys",
    "provider_connections",
    "models",
    "deployments",
    "catalogs",
    "catalog_models",
    "workspace_type_catalogs",
    "workspace_catalog_overrides",
    "workspace_catalog_override_items",
    "workspace_model_grants",
    "key_model_restrictions",
    "key_model_selections",
    "deployment_prices",
    "workspace_type_policies",
    "workspace_platform_policy_overrides",
    "workspace_local_policies",
    "key_policies",
    "policy_budgets",
    "inference_executions",
    "governance_reservations",
    "monetary_ledger",
    "routing_policies",
    "deployment_routing",
    "deployment_health",
    "audit_events",
    "installation_settings",
    // 0011 alerts
    "alert_rules",
    "alert_events",
    "alert_deliveries",
    "alert_reads",
    "alert_recipient_outbox",
    // 0057 platform alert webhooks (#45)
    "alert_webhook_destinations",
    "alert_webhook_outbox",
    // 0014 SCIM
    "scim_users",
    "scim_groups",
    "scim_group_members",
    "scim_state",
    // 0015 budget totals
    "budget_totals",
    // 0016 async jobs
    "async_jobs",
    "async_job_files",
    // 0017 realtime
    "realtime_responses",
    // 0019 file store
    "stored_files",
    // 0020 Files API storage usage
    "storage_usage_hours",
    "storage_usage_progress",
    // 0021 batch engine
    "batch_lines",
    "batch_segments",
    // 0022 batch scheduling
    "deployment_batch_scheduling",
    "deployment_batch_signals",
    "batch_route_waits",
    // 0024 rate counters
    "rate_minute_counters",
    "inflight_counters",
    // 0027 scoped admission adds functions and triggers only.
    // 0028 change notifications (per-replica cache invalidation)
    "config_versions",
    // 0029 work leases (singleton background jobs)
    "work_leases",
    // 0030/0031 monthly history partitions: the registry (partitions
    // themselves are accepted through PARTITIONED_RELATIONS)
    "history_partitions",
    // 0032 hourly usage rollups
    "usage_rollups_hourly",
    "usage_rollup_hours",
    "usage_rollup_progress",
    "usage_rollup_dirty",
    // 0033 archived history months
    "archived_partitions",
    "archived_budget_contributions",
    // 0034 history parent checks add triggers only; 0035 sets storage
    // parameters only.
    // 0036 per-scope lock waits (admission_ceiling alerts)
    "admission_lock_waits",
    // 0044 audit export (#106)
    "audit_export_state",
    "audit_export_queue",
    // 0045 legal holds (#75)
    "legal_holds",
    // 0051 admin API tokens, storage and lifecycle (#37 PR1)
    "admin_api_tokens",
    // 0052 admin API token authentication and audit (#37 PR2)
    "admin_token_attempts",
];
/// Partitioned parents (0030/0031). Any partition of one of these in
/// `public` (`pg_class.relispartition`; month partitions are created ahead of
/// time by `omg_ensure_partitions`) is an enterprise relation. Detached
/// archived partitions live in schema `omg_archive`, which preflight ignores.
pub const PARTITIONED_RELATIONS: &[&str] = &[
    "inference_executions",
    "governance_reservations",
    "monetary_ledger",
    "audit_events",
    "storage_usage_hours",
];
/// Relations that a later migration drops: accepted only before an explicit
/// upgrade (`migrate`), never by readiness or serve on a current schema.
/// `installation_policy` is dropped by 0026 (installation limits removed).
const RETIRED_RELATIONS: &[&str] = &["installation_policy"];

/// Takes the session lock that serializes `migrate` and `grants apply`, polling
/// `pg_try_advisory_lock` instead of blocking in `pg_advisory_lock`. A blocked
/// lock statement is a running transaction, and a migration that builds an index
/// CONCURRENTLY (0054) waits for every transaction older than it: a second
/// migrator parked in a blocking lock call would wait on the first, which waits
/// on it, a deadlock. A poller holds no transaction between attempts.
async fn migration_lock(connection: &mut PgConnection) -> sqlx::Result<()> {
    loop {
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(72419503)")
            .fetch_one(&mut *connection)
            .await?;
        if acquired {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Indexes that a no-transaction migration builds with `CREATE INDEX CONCURRENTLY
/// IF NOT EXISTS` (the migration file is their only definition). A failed
/// no-transaction migration is not recorded, so `migrate` reruns it, but an
/// interrupted build leaves an INVALID index of the same name that `IF NOT
/// EXISTS` would skip. [`drop_invalid_concurrent_indexes`] removes such a
/// leftover first.
const CONCURRENT_INDEXES: &[&str] = &[
    "api_keys_workspace_created",
    "alert_deliveries_pending_bucket",
];

/// Before the migrations run, drops any [`CONCURRENT_INDEXES`] entry that is
/// invalid, with a standalone `DROP INDEX CONCURRENTLY` (never inside a
/// transaction and never an ACCESS EXCLUSIVE lock on the table, unlike plain
/// `DROP INDEX`: key authentication reads `api_keys` on every request). Like the
/// concurrent build it waits for older transactions. The caller holds the
/// migration lock.
async fn drop_invalid_concurrent_indexes(connection: &mut PgConnection) -> sqlx::Result<()> {
    for name in CONCURRENT_INDEXES {
        let invalid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relname=$1 AND NOT i.indisvalid)")
            .bind(name)
            .fetch_one(&mut *connection)
            .await?;
        if invalid {
            tracing::warn!(
                index = name,
                "dropping an invalid index left by an interrupted concurrent build"
            );
            // One statement, simple protocol: it cannot run inside a transaction block.
            sqlx::raw_sql(&format!("DROP INDEX CONCURRENTLY IF EXISTS public.{name}"))
                .execute(&mut *connection)
                .await?;
        }
    }
    Ok(())
}

fn lineage_matches(
    applied: &[(i64, Vec<u8>, bool)],
    expected: &[(i64, &[u8])],
    full: bool,
) -> bool {
    !applied.is_empty()
        && applied.len() <= expected.len()
        && (!full || applied.len() == expected.len())
        && applied
            .iter()
            .zip(expected)
            .all(|(row, migration)| row.0 == migration.0 && row.1 == migration.1 && row.2)
}

async fn preflight(connection: &mut PgConnection, initializing: bool) -> anyhow::Result<()> {
    // Read-only detection precedes even creation of SQLx's migration tracking table.
    // A partition is identified by its root parent (also in `public`).
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT c.relname,CASE WHEN c.relispartition THEN (SELECT r.relname FROM pg_class r
           WHERE r.oid=pg_partition_root(c.oid) AND r.relnamespace=c.relnamespace) END
         FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname='public' AND c.relkind IN ('r','p','v','m','S','f') ORDER BY c.relname",
    )
    .fetch_all(&mut *connection)
    .await?;
    let relations: Vec<String> = rows
        .into_iter()
        .filter(|(_, root)| {
            !root
                .as_deref()
                .is_some_and(|r| PARTITIONED_RELATIONS.contains(&r))
        })
        .map(|(name, _)| name)
        .collect();
    if relations.is_empty() {
        anyhow::ensure!(
            initializing,
            "Enterprise installation is not initialized; run the explicit migrate command"
        );
        return Ok(());
    }
    anyhow::ensure!(
        relations.iter().all(|n| {
            ENTERPRISE_RELATIONS.contains(&n.as_str())
                || (initializing && RETIRED_RELATIONS.contains(&n.as_str()))
        }),
        "Unexpected public relations; refusing an unrelated or legacy database before DDL"
    );
    anyhow::ensure!(
        relations.iter().any(|n| n == "installation")
            && !relations.iter().any(|n| n == "organizations"),
        "Database is not a fresh enterprise installation; legacy/nonempty databases are rejected before DDL"
    );
    let family: Vec<String> =
        sqlx::query_scalar("SELECT schema_family FROM installation WHERE singleton")
            .fetch_all(&mut *connection)
            .await?;
    anyhow::ensure!(
        family == ["enterprise_v1"],
        "Unexpected installation schema family"
    );
    let applied = sqlx::query_as::<_, (i64, Vec<u8>, bool)>(
        "SELECT version,checksum,success FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&mut *connection)
    .await?;
    anyhow::ensure!(
        lineage_matches(
            &applied,
            &MIGRATOR
                .iter()
                .map(|m| (m.version, m.checksum.as_ref()))
                .collect::<Vec<_>>(),
            !initializing
        ),
        "Enterprise migration lineage is missing, dirty, changed, or unexpected"
    );
    Ok(())
}

impl Store {
    pub fn new(pool: PgPool) -> Self {
        // Tests run under either protocol (`GATEWAY_ADMISSION_MODE`); serve
        // sets the validated configuration with `with_admission_mode`.
        #[cfg(any(test, feature = "integration-tests"))]
        let mode = crate::governance::locks::AdmissionMode::from_env()
            .expect("GATEWAY_ADMISSION_MODE must be scoped or global");
        #[cfg(not(any(test, feature = "integration-tests")))]
        let mode = crate::governance::locks::AdmissionMode::default();
        let gates =
            crate::governance::LockGates::for_pool(pool.options().get_max_connections(), mode);
        Self {
            pool,
            provider_secrets: Default::default(),
            smtp_secrets: Default::default(),
            alert_webhooks: Default::default(),
            lock_gates: std::sync::Arc::new(gates),
            alert_dispatch: Default::default(),
            admission_mode: mode,
            reporting: None,
            reporting_max_lag: std::time::Duration::from_secs(30),
            caches: std::sync::Arc::new(crate::cache::Caches::new()),
            leases: None,
            audit_export: None,
            export_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
            traces: None,
            #[cfg(any(test, feature = "integration-tests"))]
            admission_clock: Default::default(),
        }
    }

    /// Attach the startup-validated reference allowlists. The resolver keeps
    /// its reserved-name filter even for callers that bypass startup validation.
    pub fn with_secret_allowlists(
        mut self,
        providers: crate::providers::secrets::EnvSecrets,
        smtp: crate::providers::secrets::EnvSecrets,
    ) -> Self {
        self.provider_secrets = std::sync::Arc::new(providers);
        self.smtp_secrets = std::sync::Arc::new(smtp);
        self
    }

    /// Dedicated operator-owned webhook approval and signing-reference policy.
    pub fn with_alert_webhooks(mut self, policy: crate::config::AlertWebhookPolicy) -> Self {
        self.alert_webhooks = std::sync::Arc::new(policy);
        self
    }

    /// Select the admission protocol (`GATEWAY_ADMISSION_MODE`).
    pub fn with_admission_mode(mut self, mode: crate::governance::locks::AdmissionMode) -> Self {
        let gates =
            crate::governance::LockGates::for_pool(self.pool.options().get_max_connections(), mode);
        self.lock_gates = std::sync::Arc::new(gates);
        self.admission_mode = mode;
        self
    }

    /// The admission protocol in use.
    pub fn admission_mode(&self) -> crate::governance::locks::AdmissionMode {
        self.admission_mode
    }

    /// This replica's caches (`crate::cache`).
    pub fn caches(&self) -> &crate::cache::Caches {
        &self.caches
    }

    /// Enable the per-replica caches: poll `config_versions` every second
    /// through the pool and LISTEN on `listen` (a dedicated session pool;
    /// `None` polls only). Runs until the returned task is aborted.
    pub fn start_change_notifications(
        &self,
        listen: Option<PgPool>,
    ) -> tokio::task::JoinHandle<()> {
        crate::notify::start(self.pool.clone(), listen, self.caches.versions.clone())
    }

    /// Run singleton background jobs under `leases` (serve).
    pub fn with_leases(mut self, leases: std::sync::Arc<crate::leases::Leases>) -> Self {
        self.leases = Some(leases);
        self
    }

    /// Attach the configured audit exporter (#106).
    pub fn with_audit_export(
        mut self,
        exporter: std::sync::Arc<crate::audit_export::Exporter>,
    ) -> Self {
        self.audit_export = Some(exporter);
        self
    }

    /// The configured audit exporter, if any.
    pub fn audit_export(&self) -> Option<&std::sync::Arc<crate::audit_export::Exporter>> {
        self.audit_export.as_ref()
    }

    /// Attach the bounded trace recorder (#98). `None` disables tracing,
    /// including this store's own background-root spans.
    pub fn with_traces(mut self, traces: Option<crate::telemetry::Recorder>) -> Self {
        self.traces = traces;
        self
    }

    /// The configured trace recorder, if any.
    pub fn traces(&self) -> Option<&crate::telemetry::Recorder> {
        self.traces.as_ref()
    }

    /// This replica's work leases, if background work is leased (serve).
    pub fn leases(&self) -> Option<&std::sync::Arc<crate::leases::Leases>> {
        self.leases.as_ref()
    }

    /// The database pool (tests and tools).
    #[cfg(any(test, feature = "integration-tests"))]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The instant admission evaluates per-minute rate windows, budget
    /// periods and leases at: the database clock, so every replica agrees.
    pub(crate) async fn admission_now(
        &self,
        conn: &mut PgConnection,
    ) -> sqlx::Result<chrono::DateTime<chrono::Utc>> {
        #[cfg(any(test, feature = "integration-tests"))]
        if let Some(at) = self.admission_clock.get() {
            return Ok(*at);
        }
        sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(conn)
            .await
    }

    /// The pinned test admission instant, if any (never in production
    /// builds): admission reads the database clock in one of its own
    /// statements and lets this override it, like [`Store::admission_now`].
    pub(crate) fn admission_override(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        #[cfg(any(test, feature = "integration-tests"))]
        if let Some(at) = self.admission_clock.get() {
            return Some(*at);
        }
        None
    }

    /// Tests only: freeze admission time (for this store and all its clones)
    /// at the current database time and return it. Every later admission
    /// then evaluates the same UTC minute however slowly the test runs, so
    /// per-minute rate assertions cannot straddle a minute boundary. Leases
    /// and budget periods use the same instant. Freezing twice is an error.
    #[cfg(any(test, feature = "integration-tests"))]
    pub async fn freeze_admission_clock(&self) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
        let now = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&self.pool)
            .await?;
        self.admission_clock
            .set(now)
            .map_err(|_| anyhow::anyhow!("admission clock is already frozen"))?;
        Ok(now)
    }

    /// Tests only: pin admission time (for this store and all its clones) at
    /// `at`, e.g. either side of a minute boundary. Pinning twice is an error.
    #[cfg(any(test, feature = "integration-tests"))]
    pub fn pin_admission_clock(&self, at: chrono::DateTime<chrono::Utc>) -> anyhow::Result<()> {
        self.admission_clock
            .set(at)
            .map_err(|_| anyhow::anyhow!("admission clock is already frozen"))
    }

    /// Require a fully initialized current installation, including on serve/bootstrap.
    /// This read-only check neither initializes nor upgrades anything.
    pub async fn preflight_enterprise(&self) -> anyhow::Result<()> {
        preflight(&mut *self.pool.acquire().await?, false).await
    }

    /// Read-only: an initialized installation whose lineage is a recognized
    /// prefix of this binary's (before an explicit upgrade), or current.
    pub async fn preflight_upgrade(&self) -> anyhow::Result<()> {
        let mut connection = self.pool.acquire().await?;
        let initialized: bool =
            sqlx::query_scalar("SELECT to_regclass('public.installation') IS NOT NULL")
                .fetch_one(&mut *connection)
                .await?;
        anyhow::ensure!(initialized, "Enterprise installation is not initialized");
        preflight(&mut connection, true).await
    }

    /// Explicit operator initialization/upgrade only. A matching nonempty enterprise
    /// migration prefix may upgrade; dirty/changed/unrecognized/legacy lineages may not.
    /// Serialize preflight and DDL on the same connection.
    pub async fn migrate_enterprise(&self) -> anyhow::Result<()> {
        // Detached connection closes on drop, including cancellation/panic. Neither our
        // session lock nor SQLx's migration lock can leak back into the runtime pool.
        let mut connection = self.pool.acquire().await?.detach();
        migration_lock(&mut connection).await?;
        let result = async {
            preflight(&mut connection, true).await?;
            drop_invalid_concurrent_indexes(&mut connection).await?;
            MIGRATOR.run(&mut connection).await?;
            Ok(())
        }
        .await;
        sqlx::query("SELECT pg_advisory_unlock(72419503)")
            .execute(&mut connection)
            .await?;
        result
    }

    /// Applies the reviewed runtime-role allowlist (`deploy/staging/runtime-grants.sql`,
    /// embedded in the binary) as the migrator, after every migration. Serialized with
    /// `migrate` by the same session lock; an error closes the detached connection,
    /// which releases the lock and rolls back the file's single transaction.
    /// Requires the exact schema of this binary: an older binary's allowlist would
    /// silently revoke runtime access to newer tables.
    pub async fn apply_runtime_grants(&self) -> anyhow::Result<()> {
        let mut connection = self.pool.acquire().await?.detach();
        migration_lock(&mut connection).await?;
        preflight(&mut connection, false).await?;
        sqlx::raw_sql(RUNTIME_GRANTS_SQL)
            .execute(&mut connection)
            .await?;
        sqlx::query("SELECT pg_advisory_unlock(72419503)")
            .execute(&mut connection)
            .await?;
        Ok(())
    }

    /// Runs `deploy/staging/verify-privileges.sql`: ACL assertions plus rollback-only
    /// probes as `gateway_runtime` (the connecting role must be allowed to `SET ROLE` to it).
    pub async fn verify_runtime_grants(&self) -> anyhow::Result<()> {
        let mut connection = self.pool.acquire().await?.detach();
        sqlx::raw_sql(VERIFY_PRIVILEGES_SQL)
            .execute(&mut connection)
            .await?;
        Ok(())
    }

    /// Readiness checks the family AND exact checksums; it cannot initialize anything.
    pub async fn is_ready(&self) -> bool {
        let checks = self.readiness().await;
        checks.database && checks.schema
    }

    /// Database reachability and exact schema lineage, reported separately.
    pub async fn readiness(&self) -> Readiness {
        let Ok(mut connection) = self.pool.acquire().await else {
            return Readiness::default();
        };
        let Ok(installed) =
            sqlx::query_scalar::<_, bool>("SELECT to_regclass('public.installation') IS NOT NULL")
                .fetch_one(&mut *connection)
                .await
        else {
            return Readiness::default();
        };
        Readiness {
            database: true,
            schema: installed && preflight(&mut connection, false).await.is_ok(),
        }
    }
}

#[cfg(test)]
mod lineage_tests {
    use super::*;
    #[test]
    fn upgrades_accept_only_a_recognized_nonempty_successful_prefix() {
        let expected: &[(i64, &[u8])] = &[(1, b"first"), (2, b"second")];
        let prefix = vec![(1, b"first".to_vec(), true)];
        assert!(lineage_matches(&prefix, expected, false));
        assert!(!lineage_matches(&prefix, expected, true));
        let full = vec![(1, b"first".to_vec(), true), (2, b"second".to_vec(), true)];
        assert!(lineage_matches(&full, expected, false));
        assert!(lineage_matches(&full, expected, true));
        for bad in [
            vec![],
            vec![(2, b"second".to_vec(), true)],
            vec![(1, b"changed".to_vec(), true)],
            vec![(1, b"first".to_vec(), false)],
            vec![
                (1, b"first".to_vec(), true),
                (2, b"second".to_vec(), true),
                (3, b"future".to_vec(), true),
            ],
        ] {
            assert!(!lineage_matches(&bad, expected, false));
            assert!(!lineage_matches(&bad, expected, true));
        }
    }
}

#[cfg(all(test, feature = "integration-tests"))]
mod tests {
    use super::*;

    #[sqlx::test(migrations = false)]
    async fn legacy_rejection_is_before_ddl_and_unchanged(pool: PgPool) {
        sqlx::raw_sql("CREATE TABLE organizations(id integer PRIMARY KEY, name text); INSERT INTO organizations VALUES(1,'legacy evidence')")
            .execute(&pool).await.unwrap();
        let store = Store::new(pool.clone());
        assert!(store.preflight_enterprise().await.is_err());
        assert!(store.migrate_enterprise().await.is_err());
        let objects: Vec<String> = sqlx::query_scalar(
            "SELECT tablename FROM pg_tables WHERE schemaname='public' ORDER BY tablename",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(objects, ["organizations"]);
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT name FROM organizations WHERE id=1")
                .fetch_one(&pool)
                .await
                .unwrap(),
            "legacy evidence"
        );
    }

    /// Deadlock regression (0054): two migrators started together must both succeed.
    /// The second waits on the migration session lock while the first builds an
    /// index CONCURRENTLY, which waits for every older transaction; a second
    /// migrator blocked inside `pg_advisory_lock` is such a transaction. The lock
    /// is therefore polled (`migration_lock`), never waited for in a statement.
    #[sqlx::test(migrations = false)]
    async fn fresh_initialization_is_idempotent_and_checks_readiness(pool: PgPool) {
        let store = Store::new(pool.clone());
        assert!(!store.is_ready().await);
        assert!(store.preflight_enterprise().await.is_err());
        let (first, second) = tokio::join!(store.migrate_enterprise(), store.migrate_enterprise());
        first.unwrap();
        second.unwrap();
        // The current schema (including 0066) must survive a fresh migration,
        // startup preflight and a later migrate, not just SQLx's test setup.
        for relation in ["access_requests", "access_request_applications"] {
            assert!(
                sqlx::query_scalar::<_, bool>("SELECT to_regclass($1) IS NOT NULL")
                    .bind(relation)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            );
        }
        store.preflight_enterprise().await.unwrap();
        store.migrate_enterprise().await.unwrap();
        assert!(store.is_ready().await);
        sqlx::query("UPDATE _sqlx_migrations SET checksum='\\x00'::bytea")
            .execute(&pool)
            .await
            .unwrap();
        assert!(!store.is_ready().await);
        assert!(store.migrate_enterprise().await.is_err());
    }

    #[sqlx::test(migrations = false)]
    async fn explicit_upgrade_from_0001_keeps_history_and_rejects_mixed_workloads(pool: PgPool) {
        let first = Migrator {
            migrations: std::borrow::Cow::Owned(MIGRATOR.iter().take(1).cloned().collect()),
            ignore_missing: false,
            locking: true,
            no_tx: false,
        };
        first.run(&pool).await.unwrap();
        let (model, mixed, connection, deployment, price) = (
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
        );
        sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,'chat',ARRAY['chat_completions','responses']),($2,'mixed',ARRAY['chat_completions','embeddings'])").bind(model).bind(mixed).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Cloud','openai','env:KEY')").bind(connection).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'m')").bind(deployment).bind(model).bind(connection).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,cache_pricing) VALUES($1,$2,1,2,100,10,2,'{\"read\":{\"status\":\"unknown\"},\"write\":{\"status\":\"unknown\"},\"write_5m\":{\"status\":\"unknown\"},\"write_1h\":{\"status\":\"unknown\"}}')").bind(price).bind(deployment).execute(&pool).await.unwrap();
        let store = Store::new(pool.clone());
        assert!(!store.is_ready().await, "a lineage prefix is not ready");
        // Revalidation fails closed and atomically; nothing is recorded or changed.
        assert!(store.migrate_enterprise().await.is_err());
        let versions: Vec<i64> =
            sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(versions, [1]);
        sqlx::query("UPDATE models SET supported_protocols=ARRAY['embeddings'] WHERE id=$1")
            .bind(mixed)
            .execute(&pool)
            .await
            .unwrap();
        store.migrate_enterprise().await.unwrap();
        assert!(store.is_ready().await);
        let kept: (i16, Option<i64>) = sqlx::query_as(
            "SELECT pricing_version,input_microusd_per_million FROM deployment_prices WHERE id=$1",
        )
        .bind(price)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kept, (2, Some(1)));
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,100,10,3,'[{\"meter\":\"requests\",\"not_applicable\":true}]','{}')").bind(uuid::Uuid::new_v4()).bind(deployment).execute(&pool).await.unwrap();
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn unrelated_public_relations_are_rejected_without_modification(pool: PgPool) {
        let store = Store::new(pool.clone());
        assert!(store.is_ready().await);
        sqlx::query("CREATE TABLE unrelated_data(value text)")
            .execute(&pool)
            .await
            .unwrap();
        assert!(store.preflight_enterprise().await.is_err());
        assert!(store.migrate_enterprise().await.is_err());
        let exists: bool =
            sqlx::query_scalar("SELECT to_regclass('public.unrelated_data') IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(exists);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn complete_legacy_lineage_is_rejected_without_schema_or_checksum_changes(pool: PgPool) {
        let store = Store::new(pool.clone());
        let id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO organizations(id,slug,name) VALUES($1,'untouched','Historical installation')").bind(id).execute(&pool).await.unwrap();
        let before: Vec<(i64, Vec<u8>, bool)> = sqlx::query_as(
            "SELECT version,checksum,success FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let objects_before: Vec<(String,String)>=sqlx::query_as("SELECT table_name,column_name FROM information_schema.columns WHERE table_schema='public' ORDER BY table_name,ordinal_position").fetch_all(&pool).await.unwrap();
        assert!(store.preflight_enterprise().await.is_err());
        assert!(store.migrate_enterprise().await.is_err());
        assert!(!store.is_ready().await);
        let after: Vec<(i64, Vec<u8>, bool)> = sqlx::query_as(
            "SELECT version,checksum,success FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let objects_after: Vec<(String,String)>=sqlx::query_as("SELECT table_name,column_name FROM information_schema.columns WHERE table_schema='public' ORDER BY table_name,ordinal_position").fetch_all(&pool).await.unwrap();
        assert_eq!(before, after);
        assert_eq!(objects_before, objects_after);
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT name FROM organizations WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap(),
            "Historical installation"
        );
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn capabilities_cache_shapes_and_provider_auth_modes_are_database_invariants(
        pool: PgPool,
    ) {
        let legacy_columns: i64=sqlx::query_scalar("SELECT count(*) FROM information_schema.columns WHERE table_schema='public' AND column_name='organization_id'").fetch_one(&pool).await.unwrap();
        assert_eq!(legacy_columns, 0);
        for protocols in [
            vec![],
            vec!["chat_completions", "chat_completions"],
            vec!["chat_completions", "embeddings"],
            vec!["images", "rerank"],
            vec!["messages", "audio_speech"],
            vec!["embeddings", "bogus"],
        ] {
            assert!(
                sqlx::query(
                    "INSERT INTO models(id,public_name,supported_protocols) VALUES($1,$2,$3)"
                )
                .bind(uuid::Uuid::new_v4())
                .bind(uuid::Uuid::new_v4().to_string())
                .bind(protocols)
                .execute(&pool)
                .await
                .is_err()
            );
        }
        for (protocols, ok) in [
            ("{chat_completions,responses,messages}", true),
            ("{chat_completions,responses,messages,embeddings}", false),
            ("{images}", true),
            ("{audio_transcriptions}", true),
            ("{audio_speech}", true),
            ("{rerank}", true),
            ("{systemone}", true),
            ("{embeddings}", true),
            ("{systemone,rerank}", false),
        ] {
            let valid: bool = sqlx::query_scalar("SELECT valid_model_protocols($1::text[])")
                .bind(protocols)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(valid, ok, "{protocols}");
        }
        let invalid: bool =
            sqlx::query_scalar("SELECT valid_model_protocols(ARRAY['chat_completions',NULL])")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!invalid);
        for (provider, auth, okay) in [
            ("bedrock", "aws:default", true),
            ("openai", "aws:default", false),
            ("bedrock", "env:KEY", false),
            ("openai", "none", false),
            ("ollama", "none", true),
            ("anthropic", "env:KEY", true),
        ] {
            let result=sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Test',$2,$3)").bind(uuid::Uuid::new_v4()).bind(provider).bind(auth).execute(&pool).await;
            assert_eq!(result.is_ok(), okay, "{provider}/{auth}");
        }
        let good = serde_json::json!({"read":{"status":"priced","microusd_per_million":"0"},"write":{"status":"unknown"},"write_5m":{"status":"not_applicable"},"write_1h":{"status":"priced","microusd_per_million":"9223372036854775807"}});
        let valid: bool = sqlx::query_scalar("SELECT valid_cache_pricing($1)")
            .bind(&good)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(valid);
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"read":{"status":"unknown"}}),
            serde_json::json!(null),
        ] {
            let valid: bool = sqlx::query_scalar("SELECT valid_cache_pricing($1)")
                .bind(bad)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert!(!valid);
        }
        let mut bad = good.clone();
        bad["read"]["microusd_per_million"] = serde_json::json!(0);
        let valid: bool = sqlx::query_scalar("SELECT valid_cache_pricing($1)")
            .bind(bad)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!valid);
        let good_usage = serde_json::json!({"total_input_tokens":"30","uncached_input_tokens":"10","cache_read_input_tokens":"5","cache_write_input_tokens":"15","cache_write_default_input_tokens":"3","cache_write_5m_input_tokens":"4","cache_write_1h_input_tokens":"8"});
        let valid: bool = sqlx::query_scalar("SELECT valid_billing_usage($1)")
            .bind(&good_usage)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(valid);
        for bad in [
            serde_json::json!({"total_input_tokens":0}),
            serde_json::json!({"total_input_tokens":"-1"}),
            serde_json::json!({"total_input_tokens":"9223372036854775808"}),
            serde_json::json!({"junk":"1"}),
            serde_json::json!({"total_input_tokens":"1","cache_write_5m_input_tokens":"2"}),
        ] {
            let valid: bool = sqlx::query_scalar("SELECT valid_billing_usage($1)")
                .bind(bad)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert!(!valid);
        }
        let mut bad = good_usage;
        bad["cache_write_input_tokens"] = serde_json::json!("14");
        let valid: bool = sqlx::query_scalar("SELECT valid_billing_usage($1)")
            .bind(bad)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!valid);
    }
    async fn index_valid(pool: &PgPool) -> Option<bool> {
        sqlx::query_scalar("SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relname='api_keys_workspace_created'")
            .fetch_optional(pool)
            .await
            .unwrap()
    }
    async fn applied(pool: &PgPool, version: i64) -> bool {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT FROM _sqlx_migrations WHERE version=$1 AND success)",
        )
        .bind(version)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// A database migrated up to 0053: the state `migrate` finds when the build of
    /// 0054's index was interrupted (a failed no-transaction migration is not
    /// recorded). Independent of any later migration.
    async fn migrated_to_0053(pool: &PgPool) {
        sqlx::migrate::Migrator {
            migrations: std::borrow::Cow::Owned(
                MIGRATOR
                    .iter()
                    .filter(|m| m.version <= 53)
                    .cloned()
                    .collect(),
            ),
            ignore_missing: false,
            locking: true,
            no_tx: false,
        }
        .run(pool)
        .await
        .unwrap();
        assert_eq!(index_valid(pool).await, None);
    }
    /// What an interrupted CREATE INDEX CONCURRENTLY leaves: an INVALID index of the name.
    async fn leave_an_invalid_index(pool: &PgPool) {
        sqlx::raw_sql("CREATE INDEX api_keys_workspace_created ON api_keys(workspace_id, created_at); UPDATE pg_index SET indisvalid=false WHERE indexrelid='public.api_keys_workspace_created'::regclass")
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(index_valid(pool).await, Some(false));
    }

    /// An interrupted CREATE INDEX CONCURRENTLY leaves an INVALID index of its
    /// name, which the rerun's IF NOT EXISTS would skip. `migrate` drops it
    /// (concurrently) first, then 0054 builds the index.
    #[sqlx::test(migrations = false)]
    async fn migrate_recovers_from_an_interrupted_concurrent_index_build(pool: PgPool) {
        assert!(MIGRATOR.iter().any(|m| m.version == 54 && m.no_tx));
        migrated_to_0053(&pool).await;
        leave_an_invalid_index(&pool).await;
        let store = Store::new(pool.clone());
        store.migrate_enterprise().await.unwrap();
        assert_eq!(index_valid(&pool).await, Some(true));
        assert!(applied(&pool, 54).await && store.is_ready().await);
        // Nothing to recover: a rerun changes nothing.
        let oid = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT 'public.api_keys_workspace_created'::regclass::oid::bigint",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        let before = oid().await;
        store.migrate_enterprise().await.unwrap();
        assert_eq!(oid().await, before, "a valid index is never rebuilt");
    }

    /// Interrupted after the drop and before the rebuild (the index is gone and
    /// 0054 is not recorded, the state of a database at 0053): still recoverable.
    #[sqlx::test(migrations = false)]
    async fn migrate_builds_the_index_when_interrupted_between_the_drop_and_the_rebuild(
        pool: PgPool,
    ) {
        migrated_to_0053(&pool).await;
        let store = Store::new(pool.clone());
        store.migrate_enterprise().await.unwrap();
        assert_eq!(index_valid(&pool).await, Some(true));
        assert!(applied(&pool, 54).await && store.is_ready().await);
    }

    /// Recovery waits for older transactions (a concurrent drop and build do), but
    /// takes no lock that blocks others: while it waits, key authentication reads
    /// and key writes of api_keys proceed.
    #[sqlx::test(migrations = false)]
    async fn recovery_never_blocks_reads_or_writes_of_api_keys(pool: PgPool) {
        migrated_to_0053(&pool).await;
        leave_an_invalid_index(&pool).await;
        let store = Store::new(pool.clone());
        // An older transaction: the recovery's concurrent drop must wait for it.
        let mut old = pool.begin().await.unwrap();
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM api_keys")
            .fetch_one(&mut *old)
            .await
            .unwrap();
        let during = async {
            // Wait until the drop is actually waiting for `old`.
            let mut waiting = false;
            for _ in 0..200 {
                waiting = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pg_stat_activity WHERE query LIKE 'DROP INDEX CONCURRENTLY%' AND wait_event_type='Lock'")
                    .fetch_one(&pool)
                    .await
                    .unwrap()
                    > 0;
                if waiting {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert!(
                waiting,
                "the recovery's drop never waited for the older transaction"
            );
            assert_eq!(index_valid(&pool).await, Some(false), "still recovering");
            let bound = std::time::Duration::from_secs(3);
            tokio::time::timeout(
                bound,
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM api_keys").fetch_one(&pool),
            )
            .await
            .expect("a read of api_keys was blocked by the recovery")
            .unwrap();
            let (user, ws) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
            tokio::time::timeout(bound, async {
                sqlx::query("INSERT INTO users(id,email) VALUES($1,'recover@example.test')").bind(user).execute(&pool).await?;
                sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Recover','team')").bind(ws).execute(&pool).await?;
                sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES(gen_random_uuid(),$1,$2,'k',decode(repeat('ab',32),'hex'))").bind(ws).bind(user).execute(&pool).await
            })
            .await
            .expect("a write to api_keys was blocked by the recovery")
            .unwrap();
            old.commit().await.unwrap();
        };
        let (recovery, ()) = tokio::join!(store.migrate_enterprise(), during);
        recovery.unwrap();
        assert_eq!(index_valid(&pool).await, Some(true));
    }

    /// `grants apply` and `migrate` queue behind a running migrator by polling
    /// `pg_try_advisory_lock`, never by blocking in `pg_advisory_lock`, so a
    /// waiter holds no transaction a concurrent index build would wait for.
    #[sqlx::test(migrations = false)]
    async fn waiting_migrators_poll_the_lock_and_hold_no_transaction(pool: PgPool) {
        let store = Store::new(pool.clone());
        store.migrate_enterprise().await.unwrap();
        // A running migrator, simulated by its session lock.
        let mut holder = pool.acquire().await.unwrap().detach();
        sqlx::query("SELECT pg_advisory_lock(72419503)")
            .execute(&mut holder)
            .await
            .unwrap();
        let driver = async {
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            let (blocked, open): (i64, i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE wait_event='advisory'),count(*) FILTER(WHERE state='idle in transaction') FROM pg_stat_activity WHERE datname=current_database()")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(
                (blocked, open),
                (0, 0),
                "a waiter blocked in pg_advisory_lock or held a transaction open"
            );
            sqlx::query("SELECT pg_advisory_unlock(72419503)")
                .execute(&mut holder)
                .await
                .unwrap();
        };
        // Both wait for the lock until the driver releases it (the first
        // sleep proves they queued: neither can finish before 600 ms).
        let started = std::time::Instant::now();
        let (_grants, migrate, ()) = tokio::join!(
            store.apply_runtime_grants(),
            store.migrate_enterprise(),
            driver
        );
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(600),
            "they did not wait for the lock"
        );
        // `migrate` of a migrated database succeeds; `grants apply` may need a role this test database lacks.
        migrate.unwrap();
    }
}

//! Alerts (docs/alerts.md): a bounded background evaluator turns alert rules
//! into incidents (`alert_events`) and emails recipients through the
//! installation's SMTP relay.
//!
//! - Rule kinds: budget thresholds (% of any applicable stacked workspace or
//!   key budget), installation spend thresholds (% of a reference amount per
//!   period; never blocks anything: there are no installation-wide limits),
//!   spend spikes (last hour vs the trailing 7-day hourly average), error rate
//!   and failing provider connections. Personal workspaces get built-in budget
//!   alerts for their owner only.
//! - Exact integer micro-USD math. Spend is settled actual plus in-flight
//!   pending holds (as admission counts them); unknown-cost reservations are
//!   never counted as zero or as spend: they raise a separate flag/count.
//! - Idempotent: at most one open incident per rule and subject (a partial
//!   unique index), so repeated evaluation never fires twice. An incident
//!   resolves once when its condition clears; a changed threshold level
//!   supersedes it with a new one.
//! - One replica evaluates at a time (transaction advisory lock); no
//!   installation lock is taken, so admission is never blocked. Each rule runs
//!   in its own savepoint: one failing rule cannot resolve or block others.
//! - Email is sent after commit, never under a lock on shared state; outcomes
//!   are recorded as counts and a category. Failures never crash the loop.
//! - No prompt data, key names or owner identities appear in incidents.
use std::time::Duration;

use chrono::{DateTime, DurationRound, TimeDelta, Utc};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

mod outbox;
pub(crate) mod webhooks;
pub use outbox::deliver_pending;
mod aged_hold;
pub(crate) use aged_hold::{aged_hold_tick, aged_hold_visible};
mod evaluation;
mod incidents;
mod key_expiry;
mod message;
pub use evaluation::{BATCH_FAILED_HOURS, CEILING_MIN_REQUESTS, p95_at_least_ms};
use evaluation::{budget_conditions, evaluate_rule};
pub use incidents::Changes;
pub(crate) use incidents::{fire_scim_last_admin, lock_delivery_buckets, resolve, retire_inactive};
use incidents::{reconcile, resolve_scim_last_admin};
use key_expiry::KEY_EXPIRY_DELIVERABLE;
pub(crate) use key_expiry::{key_expiring_tick, key_expiry_visible};
pub use message::{EmailFacts, email_text};
use message::{Pending, email_message, relay};

use crate::{
    email::{SmtpSettings, TlsMode},
    governance::BudgetPeriod,
    store::Store,
};

pub const INTERVAL_VAR: &str = "GATEWAY_ALERT_INTERVAL_SECONDS";
pub const DEFAULT_INTERVAL_SECONDS: u64 = 60;
/// Transaction advisory lock: one evaluating replica at a time.
const EVALUATION_LOCK: i64 = 72419507;
/// Built-in personal budget alert thresholds (owner only).
pub const PERSONAL_THRESHOLDS: [i32; 2] = [80, 100];
pub const PERSONAL_BUILTIN: &str = "personal_budget";
/// Built-in installation incident (0018): SCIM tried to remove platform
/// access from the last active Platform Admin and was refused. Also its kind
/// and subject: at most one is open at a time.
pub const SCIM_LAST_ADMIN: &str = "scim_last_admin";
pub const SCIM_LAST_ADMIN_SUMMARY: &str = "SCIM tried to remove the last Platform Admin";
/// Built-in installation incident: a history table has fewer than
/// `crate::partitions::ALERT_BELOW_MONTHS` future month partitions (0030).
pub const PARTITIONS_MISSING: &str = "partitions_missing";
/// Built-in installation incident: `history verify` found history rows whose
/// parents are missing or belong to another scope (0034).
pub const HISTORY_ORPHANS: &str = "history_orphans";
/// Built-in installation incidents (no rule; Platform Admins are notified).
pub const INSTALLATION_BUILTINS: [&str; 3] = [SCIM_LAST_ADMIN, PARTITIONS_MISSING, HISTORY_ORPHANS];
/// Attempt failures that indicate the upstream (not the request) is failing.
pub const UPSTREAM_FAILURES: [&str; 4] = [
    "upstream_unavailable",
    "timeout_error",
    "invalid_upstream_response",
    "provider_configuration_error",
];
/// The spend-spike baseline: the 7 days (168 hours) before the last hour.
pub const BASELINE_HOURS: i128 = 168;
/// Evaluation output bounds (per rule) and delivery bounds (per tick).
const MAX_CONDITIONS: usize = 5000;
const MAX_RULES: i64 = 2000;
const DELIVERIES_PER_TICK: i64 = 25;
const MAX_RECIPIENTS: i64 = 50;
/// #100: fixed 7-day and 1-day key-expiry reminders. No new rule/configuration
/// surface; not a user-selectable value anywhere.
pub const KEY_EXPIRY_URGENT_DAYS: i32 = 1;
pub const KEY_EXPIRY_WARNING_DAYS: i32 = 7;
/// Per-tick bound on new key-expiry reminders, and separately on retired
/// (superseded/cleared) ones: at most 500 of each, indexed and
/// expiry/fired-ordered, so already-processed rows never starve later
/// candidates.
const KEY_EXPIRY_PROGRESS: i64 = 500;
/// New incidents only; deferred tuples remain eligible on a later tick/band.
const KEY_EXPIRY_WORKSPACE_24H: i64 = 100;

/// `GATEWAY_ALERT_INTERVAL_SECONDS`: default 60, `0` disables, at most 86400.
pub fn interval_from_env() -> anyhow::Result<Option<Duration>> {
    parse_interval(std::env::var(INTERVAL_VAR).ok().as_deref())
}
pub fn parse_interval(value: Option<&str>) -> anyhow::Result<Option<Duration>> {
    let Some(value) = value else {
        return Ok(Some(Duration::from_secs(DEFAULT_INTERVAL_SECONDS)));
    };
    let seconds: u64 = value
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid {INTERVAL_VAR}"))?;
    anyhow::ensure!(seconds <= 86_400, "{INTERVAL_VAR} must be 0..86400");
    Ok((seconds > 0).then(|| Duration::from_secs(seconds)))
}

// ---------- Exact arithmetic (unit-tested) ----------

/// Highest threshold reached (`used/amount >= t/100`), exactly. Zero budgets
/// (deny-all) never alert.
pub fn threshold_level(used: i128, amount: i128, thresholds: &[i32]) -> Option<i32> {
    if amount <= 0 {
        return None;
    }
    thresholds
        .iter()
        .copied()
        .filter(|t| used * 100 >= amount * i128::from(*t))
        .max()
}
/// Whole percent used, rounded down.
pub fn percent_floor(used: i128, amount: i128) -> Option<i128> {
    (amount > 0).then(|| used.max(0) * 100 / amount)
}
/// Last-hour spend reaches `factor_percent/100` times the trailing hourly
/// average (`baseline_total / 168`) and the absolute floor.
pub fn spike_firing(
    current: i128,
    baseline_total: i128,
    factor_percent: i64,
    min_spend: i64,
) -> bool {
    current >= i128::from(min_spend)
        && current * BASELINE_HOURS * 100 >= baseline_total * i128::from(factor_percent)
}
/// `failed/finished >= rate%` with at least `min_requests` finished attempts.
pub fn rate_firing(failed: i64, finished: i64, rate_percent: i32, min_requests: i32) -> bool {
    finished > 0
        && finished >= i64::from(min_requests)
        && i128::from(failed) * 100 >= i128::from(finished) * i128::from(rate_percent)
}
/// Exact dollars: at least two decimals, at most six, never rounded.
pub fn usd(micro: i128) -> String {
    let micro = micro.max(0);
    let mut fraction = format!("{:06}", micro % 1_000_000);
    while fraction.len() > 2 && fraction.ends_with('0') {
        fraction.pop();
    }
    format!("${}.{fraction}", micro / 1_000_000)
}
/// "4.2" (tenths, rounded down) for last hour / hourly average, if any baseline.
pub fn spike_ratio(current: i128, baseline_total: i128) -> Option<String> {
    (baseline_total > 0).then(|| {
        let tenths = current.max(0) * BASELINE_HOURS * 10 / baseline_total;
        format!("{}.{}", tenths / 10, tenths % 10)
    })
}

// ---------- Rules ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Budget,
    Spike,
    ErrorRate,
    Provider,
    /// A batch ended failed or expired (0021).
    BatchFailed,
    /// An unfinished batch made no progress for `window_minutes` (0021).
    BatchStalled,
    /// Installation-wide spend in a period reaches a share of a reference
    /// amount (0026). A notification only: nothing is ever denied by it.
    Spend,
    /// One workspace or key lineage near its scoped-admission ceiling:
    /// sustained admission rate or lock-wait p95 (0036; installation only).
    AdmissionCeiling,
    /// #100: an authorized API key expires within 7 or 1 days. Built-in
    /// (no `alert_rules` row, like `Source::Builtin`'s personal budget
    /// alerts): never a rule kind, never accepted by rule validation.
    KeyExpiring,
    /// Fixed 24h/168h unknown-hold review; built in, never a rule.
    AgedUnknownHold,
}
impl Kind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "budget_threshold" => Some(Self::Budget),
            "spend_spike" => Some(Self::Spike),
            "error_rate" => Some(Self::ErrorRate),
            "provider_failing" => Some(Self::Provider),
            "batch_failed" => Some(Self::BatchFailed),
            "batch_stalled" => Some(Self::BatchStalled),
            "spend_threshold" => Some(Self::Spend),
            "admission_ceiling" => Some(Self::AdmissionCeiling),
            "key_expiring" => Some(Self::KeyExpiring),
            "aged_unknown_hold" => Some(Self::AgedUnknownHold),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Budget => "budget_threshold",
            Self::Spike => "spend_spike",
            Self::ErrorRate => "error_rate",
            Self::Provider => "provider_failing",
            Self::BatchFailed => "batch_failed",
            Self::BatchStalled => "batch_stalled",
            Self::Spend => "spend_threshold",
            Self::AdmissionCeiling => "admission_ceiling",
            Self::KeyExpiring => "key_expiring",
            Self::AgedUnknownHold => "aged_unknown_hold",
        }
    }
}
/// Stacked-budget layers a budget rule can watch. There is no installation
/// layer (0026); installation spend has its own rule kind ([`Kind::Spend`]).
pub const BUDGET_LAYERS: [&str; 4] = ["type", "override", "local", "key"];
fn layer_label(layer: &str) -> &'static str {
    match layer {
        "type" => "Type default",
        "override" => "Platform override",
        "local" => "Workspace",
        _ => "API key",
    }
}
fn period_word(period: &str) -> &'static str {
    match period {
        "day" => "daily",
        "week" => "weekly",
        "month" => "monthly",
        _ => "lifetime",
    }
}

#[derive(sqlx::FromRow, Clone, Debug)]
struct Rule {
    id: Uuid,
    workspace_id: Option<Uuid>,
    kind: String,
    budget_layers: Option<Vec<String>>,
    thresholds: Option<Vec<i32>>,
    spike_factor_percent: Option<i32>,
    min_spend_microusd: Option<i64>,
    window_minutes: Option<i32>,
    error_rate_percent: Option<i32>,
    min_requests: Option<i32>,
    consecutive_failures: Option<i32>,
    provider_connection_id: Option<Uuid>,
    spend_period: Option<String>,
    spend_amount_microusd: Option<i64>,
    ceiling_requests_per_second: Option<i32>,
    ceiling_lock_wait_ms: Option<i32>,
}

/// One firing condition produced by evaluation.
#[derive(Clone, Debug)]
pub(crate) struct Condition {
    pub(crate) subject: String,
    pub(crate) level: i32,
    pub(crate) critical: bool,
    pub(crate) workspace_id: Option<Uuid>,
    pub(crate) connection_id: Option<Uuid>,
    pub(crate) summary: String,
    pub(crate) details: Value,
}

#[derive(Clone, Copy)]
enum Source {
    Rule(Uuid),
    Builtin,
}
impl Source {
    fn rule(self) -> Option<Uuid> {
        match self {
            Self::Rule(id) => Some(id),
            Self::Builtin => None,
        }
    }
    fn builtin(self) -> Option<&'static str> {
        match self {
            Self::Rule(_) => None,
            Self::Builtin => Some(PERSONAL_BUILTIN),
        }
    }
}

/// Which workspaces a budget evaluation covers.
#[derive(Clone, Copy)]
enum Workspaces {
    /// Every enabled Team/Project (installation rules).
    Shared,
    /// Every enabled personal workspace (built-in alerts).
    Personal,
    One(Uuid),
}

/// Settled spend of the last hour ($2..$3) and of the baseline ($1..$2), and
/// the last hour's unknown-cost attempts, optionally of one workspace ($4).
/// The 7-day baseline reads hourly rollups (exact: `omg_usage_rows` uses an
/// hour's rollup only when it matches raw history in this snapshot).
pub(crate) const SPIKE: &str = "SELECT coalesce(sum(known_cost_microusd) FILTER(WHERE w='current' AND accounting_state='settled'),0)::text,coalesce(sum(known_cost_microusd) FILTER(WHERE w='baseline' AND accounting_state='settled'),0)::text,coalesce(sum(attempts) FILTER(WHERE w='current' AND accounting_state='unknown'),0)::bigint FROM (SELECT 'baseline' w,u.* FROM omg_usage_rows($1,$2) u UNION ALL SELECT 'current',u.* FROM omg_usage_rows($2,$3) u) x WHERE ($4::uuid IS NULL OR workspace_id=$4)";
/// The former raw-history spike query (parity oracle for [`SPIKE`]).
#[cfg(test)]
pub(crate) const SPIKE_RAW: &str = "SELECT coalesce(sum(actual_microusd) FILTER(WHERE admitted_at>=$2 AND state='settled'),0)::text,coalesce(sum(actual_microusd) FILTER(WHERE admitted_at<$2 AND state='settled'),0)::text,count(*) FILTER(WHERE admitted_at>=$2 AND state='unknown') FROM governance_reservations WHERE admitted_at>=$1 AND admitted_at<$3 AND ($4::uuid IS NULL OR workspace_id=$4)";

/// What one evaluation did. `None` from [`evaluate_once`] means another replica
/// is evaluating right now.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub rules: usize,
    pub fired: usize,
    pub resolved: usize,
    pub failed_rules: usize,
}

/// One bounded evaluation of every enabled rule and the built-in personal budget alerts.
///
/// A short claim transaction takes the evaluation lock with `try` (another
/// replica evaluating means this tick does nothing), retires inactive
/// incidents and lists the rules. Each rule is then evaluated and reconciled
/// in its own short transaction that waits for the same lock, so no snapshot
/// spans the whole tick (vacuum is not held back) and one rule's incidents
/// are never reconciled concurrently. Budget rules read maintained totals.
pub async fn evaluate_once(store: &Store) -> anyhow::Result<Option<Report>> {
    Ok(evaluate_once_fenced(store, None).await?)
}

/// [`evaluate_once`] as the `alerts` lease holder: the claim and every rule
/// transaction are fenced to the term (a replica whose term ended commits
/// nothing more). The tick lock still excludes a concurrent operator
/// `alerts evaluate --once`.
#[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
pub async fn evaluate_once_fenced(
    store: &Store,
    fence: Option<&crate::leases::Fence>,
) -> Result<Option<Report>, sqlx::Error> {
    let mut tx = crate::db::begin(&store.pool).await?;
    crate::leases::fence(&mut tx, fence).await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(EVALUATION_LOCK)
        .fetch_one(&mut *tx)
        .await?;
    if !locked {
        return Ok(None);
    }
    sqlx::query("SET LOCAL statement_timeout='10s'")
        .execute(&mut *tx)
        .await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    // Outbox cleanup deliberately does not run here. The delivery worker
    // already runs it in its own short transaction on every tick (and the
    // CLI's one-shot `deliver_pending` call does too), so running it again
    // in this longer transaction (which also fetches the rule list before
    // committing) would only risk blocking a concurrent complete()'s
    // `FOR UPDATE` on the same delivery row until this transaction commits,
    // for no benefit: dropping the call is simpler than giving cleanup its
    // own committed transaction here, and just as correct.
    let mut report = Report {
        resolved: retire_inactive(&mut tx, None).await? as usize
            + resolve_scim_last_admin(&mut tx).await?,
        ..Report::default()
    };
    let rules: Vec<Rule> = sqlx::query_as("SELECT r.id,r.workspace_id,r.kind,r.budget_layers,r.thresholds,r.spike_factor_percent,r.min_spend_microusd,r.window_minutes,r.error_rate_percent,r.min_requests,r.consecutive_failures,r.provider_connection_id,r.spend_period,r.spend_amount_microusd,r.ceiling_requests_per_second,r.ceiling_lock_wait_ms FROM alert_rules r LEFT JOIN workspaces w ON w.id=r.workspace_id WHERE r.enabled AND r.deleted_at IS NULL AND (r.workspace_id IS NULL OR w.disabled_at IS NULL) ORDER BY r.created_at,r.id LIMIT $1")
        .bind(MAX_RULES)
        .fetch_all(&mut *tx)
        .await?;
    let mut jobs: Vec<(Source, Kind, Option<Rule>)> = rules
        .into_iter()
        .filter_map(|r| Some((Source::Rule(r.id), Kind::parse(&r.kind)?, Some(r))))
        .collect();
    jobs.push((Source::Builtin, Kind::Budget, None));
    tx.commit().await?;
    for (source, kind, rule) in jobs {
        report.rules += 1;
        let mut tx = crate::db::begin(&store.pool).await?;
        crate::leases::fence(&mut tx, fence).await?;
        let outcome = async {
            sqlx::query("SET LOCAL statement_timeout='10s'")
                .execute(&mut *tx)
                .await?;
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(EVALUATION_LOCK)
                .execute(&mut *tx)
                .await?;
            let conditions = match &rule {
                Some(rule) => evaluate_rule(&mut tx, rule, now).await?,
                None => {
                    let layers = ["type", "override", "local", "key"].map(String::from);
                    budget_conditions(
                        &mut tx,
                        Workspaces::Personal,
                        &layers,
                        &PERSONAL_THRESHOLDS,
                        now,
                    )
                    .await?
                }
            };
            reconcile(&mut tx, source, kind, conditions).await
        }
        .await;
        match outcome {
            Ok(changes) => {
                tx.commit().await?;
                report.fired += changes.fired;
                report.resolved += changes.resolved;
            }
            Err(_) => {
                tx.rollback().await?;
                report.failed_rules += 1;
            }
        }
    }
    // Built-ins each get their OWN fenced transaction and post-lock DB
    // timestamp. A missing migration fails only this job, not the loop.
    for kind in [Kind::KeyExpiring, Kind::AgedUnknownHold] {
        report.rules += 1;
        let mut tx = crate::db::begin(&store.pool).await?;
        crate::leases::fence(&mut tx, fence).await?;
        let outcome = async {
            sqlx::query("SET LOCAL statement_timeout='10s'")
                .execute(&mut *tx)
                .await?;
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(EVALUATION_LOCK)
                .execute(&mut *tx)
                .await?;
            // Earlier rules may have crossed an expiry/band boundary or
            // waited on locks. Use database time in this job, after its lock.
            let now = sqlx::query_scalar("SELECT clock_timestamp()")
                .fetch_one(&mut *tx)
                .await?;
            match kind {
                Kind::KeyExpiring => key_expiring_tick(&mut tx, now)
                    .await
                    .map(|changes| (changes, aged_hold::AgedHoldCounts::default())),
                Kind::AgedUnknownHold => aged_hold_tick(&mut tx, now).await,
                _ => unreachable!(),
            }
        }
        .await;
        match outcome {
            Ok((changes, counts)) => {
                tx.commit().await?;
                if kind == Kind::AgedUnknownHold {
                    counts.record(&crate::metrics::METRICS);
                }
                report.fired += changes.fired;
                report.resolved += changes.resolved;
            }
            Err(_) => {
                tx.rollback().await?;
                report.failed_rules += 1;
            }
        }
    }
    Ok(Some(report))
}

/// Background evaluator for `serve`.
pub fn start(store: Store, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            // With leases (serve), only the `alerts` lease holder evaluates:
            // before P5 each replica evaluated whenever its own tick found the
            // tick lock free, so N replicas evaluated about N times per interval.
            let evaluated = match store.leases() {
                Some(leases) => match leases.held(crate::leases::Lease::Alerts) {
                    Some(fence) => {
                        tokio::time::timeout(
                            Duration::from_secs(60),
                            evaluate_once_fenced(&store, Some(&fence)),
                        )
                        .await
                    }
                    None => Ok(Ok(None)),
                },
                None => {
                    tokio::time::timeout(
                        Duration::from_secs(60),
                        evaluate_once_fenced(&store, None),
                    )
                    .await
                }
            };
            match evaluated {
                Ok(Err(error)) if crate::leases::is_fenced(&error) => {
                    crate::metrics::METRICS.observe_alert_run("skipped", 0)
                }
                Ok(Ok(Some(report))) => {
                    crate::metrics::METRICS.observe_alert_run("ok", report.failed_rules);
                    if report.fired + report.resolved + report.failed_rules > 0 {
                        tracing::info!(
                            fired = report.fired,
                            resolved = report.resolved,
                            failed_rules = report.failed_rules,
                            "alerts evaluated"
                        );
                    }
                }
                Ok(Ok(None)) => crate::metrics::METRICS.observe_alert_run("skipped", 0),
                _ => {
                    crate::metrics::METRICS.observe_alert_run("failed", 0);
                    tracing::warn!("alert evaluation incomplete; retrying next interval")
                }
            }
            webhooks::deliver(&store).await;
            // `deliver_pending_within` stops claiming once a further send could
            // overrun SEND_BUDGET, and its cleanup has its own bound. The worst
            // case is cleanup, then one send started just inside the budget, so
            // the outer timeout is only a backstop above that.
            const SEND_BUDGET: Duration = Duration::from_secs(60);
            let backstop = SEND_BUDGET + outbox::SEND_DEADLINE + outbox::CLEANUP_BOUND;
            if tokio::time::timeout(
                backstop,
                outbox::deliver_pending_within(&store, DELIVERIES_PER_TICK, SEND_BUDGET),
            )
            .await
            .is_err()
            {
                tracing::warn!(
                    "alert email delivery timed out; pending emails retry next interval"
                );
            }
        }
    })
}

#[cfg(all(test, feature = "integration-tests"))]
use evaluation::{BudgetRow, WORKSPACE_BUDGETS, windows};
#[cfg(test)]
use message::email_message_with_link;
#[cfg(all(test, feature = "integration-tests"))]
use message::public_url;
#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn thresholds_are_exact_integer_math() {
        let t = [50, 80, 100];
        assert_eq!(threshold_level(0, 100, &t), None);
        assert_eq!(threshold_level(49, 100, &t), None);
        assert_eq!(threshold_level(50, 100, &t), Some(50));
        // 79.999999% is not 80%.
        assert_eq!(threshold_level(7_999_999, 10_000_000, &t), Some(50));
        assert_eq!(threshold_level(8_000_000, 10_000_000, &t), Some(80));
        assert_eq!(threshold_level(10_000_000, 10_000_000, &t), Some(100));
        assert_eq!(threshold_level(30_000_000, 10_000_000, &t), Some(100));
        // Deny-all (zero) budgets never alert.
        assert_eq!(threshold_level(5, 0, &t), None);
        // Large values do not overflow.
        assert_eq!(
            threshold_level(i128::from(i64::MAX), i128::from(i64::MAX), &t),
            Some(100)
        );
        assert_eq!(percent_floor(1, 3), Some(33));
        assert_eq!(percent_floor(1, 0), None);
    }

    #[test]
    fn spikes_compare_against_the_hourly_average_and_a_floor() {
        // Baseline $16.80 over 168 hours = $0.10/hour; 3x = $0.30.
        let baseline = 16_800_000;
        assert!(!spike_firing(299_999, baseline, 300, 1));
        assert!(spike_firing(300_000, baseline, 300, 1));
        // The absolute floor gates small amounts, including with no baseline.
        assert!(!spike_firing(999_999, 0, 300, 1_000_000));
        assert!(spike_firing(1_000_000, 0, 300, 1_000_000));
        assert_eq!(spike_ratio(300_000, baseline).as_deref(), Some("3.0"));
        assert_eq!(spike_ratio(1, 0), None);
    }

    #[test]
    fn error_rates_need_volume() {
        assert!(!rate_firing(5, 9, 20, 10));
        assert!(rate_firing(2, 10, 20, 10));
        assert!(!rate_firing(1, 10, 20, 10));
        assert!(!rate_firing(0, 0, 1, 1));
    }

    #[test]
    fn money_is_exact() {
        assert_eq!(usd(0), "$0.00");
        assert_eq!(usd(8_100), "$0.0081");
        assert_eq!(usd(12_400_000), "$12.40");
        assert_eq!(usd(1_234_567), "$1.234567");
    }

    #[test]
    fn interval_parsing() {
        assert_eq!(parse_interval(None).unwrap(), Some(Duration::from_secs(60)));
        assert_eq!(parse_interval(Some("0")).unwrap(), None);
        assert_eq!(
            parse_interval(Some(" 15 ")).unwrap(),
            Some(Duration::from_secs(15))
        );
        assert!(parse_interval(Some("-1")).is_err());
        assert!(parse_interval(Some("86401")).is_err());
        assert!(parse_interval(Some("soon")).is_err());
    }

    #[test]
    fn aged_hold_email_copy_and_absolute_or_relative_navigation_are_explicit() {
        for link in [None, Some("https://gateway.example.test")] {
            for (severity, hours) in [("warning", 24), ("critical", 168)] {
                let row = Pending {
                    transition: "fired".into(),
                    summary: format!("Holds with unknown cost are over {hours} hours old"),
                    severity: severity.into(),
                    fired_at: DateTime::<Utc>::UNIX_EPOCH,
                    resolved_at: None,
                    rule_name: None,
                    workspace_name: None,
                    workspace_kind: None,
                    connection_name: None,
                    builtin: None,
                    kind: "aged_unknown_hold".into(),
                    key_expires_at: None,
                };
                let (subject, body) = email_message_with_link(row, "Enterprise".into(), link);
                assert!(subject.contains("Aged unknown hold:"));
                assert!(subject.contains("Installation"));
                assert!(body.contains("Built-in aged unknown-hold alert"));
                assert!(!body.contains("Built-in budget alert"));
                assert!(body.contains("Retained floors are not upper bounds."));
                assert!(body.contains("Nothing is refunded or released here"));
                let base = link.unwrap_or_default();
                assert!(body.contains(&format!("Notifications: {base}/notifications")));
                assert!(body.contains(&format!("Open holds: {base}/admin/costs?tab=holds")));
            }
        }
    }

    #[test]
    fn emails_carry_only_scope_labels_and_strip_controls() {
        let facts = EmailFacts {
            installation: "Acme AI".into(),
            summary: "Workspace monthly budget reached 80%".into(),
            resolved: false,
            critical: false,
            scope: "Team\r\nBcc: x@example.test".into(),
            rule: None,
            at: DateTime::<Utc>::UNIX_EPOCH,
        };
        let (subject, body) = email_text(&facts, Some("https://gw.example.test"));
        assert!(!subject.contains('\n') && !subject.contains('\r'));
        assert!(subject.starts_with("[Acme AI] Alert: Workspace monthly budget reached 80%"));
        assert!(body.contains("Built-in budget alert"));
        assert!(body.contains("https://gw.example.test/notifications"));
        let (subject, _) = email_text(
            &EmailFacts {
                resolved: true,
                ..facts
            },
            None,
        );
        assert!(subject.contains("Resolved:"));
    }
}

#[cfg(all(test, feature = "integration-tests"))]
mod tests;

//! Budget periods and the applicable policy and budget rows of an admission.
use super::*;
/// Budget window of one policy budget. Each scope (type default, workspace
/// override, workspace local, key lineage) may hold at most
/// one budget per period; every applicable budget is enforced over its own
/// current UTC window. Changing budgets never resets or rewrites history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BudgetPeriod {
    /// UTC calendar day.
    Day,
    /// ISO week: Monday 00:00 UTC to the next Monday.
    Week,
    /// UTC calendar month.
    Month,
    /// All time since the scope was created.
    Lifetime,
}
impl BudgetPeriod {
    pub const ALL: [BudgetPeriod; 4] = [
        BudgetPeriod::Day,
        BudgetPeriod::Week,
        BudgetPeriod::Month,
        BudgetPeriod::Lifetime,
    ];
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "day" => Some(Self::Day),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            "lifetime" => Some(Self::Lifetime),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Lifetime => "lifetime",
        }
    }
    /// Half-open UTC window `[start, end)` containing `at`. A lifetime budget
    /// spans every admission (no consumption predates its scope).
    pub fn window(self, at: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        use chrono::{Datelike, NaiveDate, TimeDelta};
        let date = at.date_naive();
        let midnight = |d: NaiveDate| d.and_time(chrono::NaiveTime::MIN).and_utc();
        match self {
            Self::Day => {
                let start = midnight(date);
                (start, start + TimeDelta::days(1))
            }
            Self::Week => {
                let monday =
                    date - TimeDelta::days(i64::from(date.weekday().num_days_from_monday()));
                let start = midnight(monday);
                (start, start + TimeDelta::days(7))
            }
            Self::Month => {
                let first = date.with_day(1).unwrap_or(date);
                let next = if first.month() == 12 {
                    NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)
                } else {
                    NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)
                }
                .unwrap_or(first);
                (midnight(first), midnight(next))
            }
            Self::Lifetime => (
                DateTime::<Utc>::UNIX_EPOCH,
                NaiveDate::from_ymd_opt(9999, 1, 1)
                    .map(midnight)
                    .unwrap_or(DateTime::<Utc>::MAX_UTC),
            ),
        }
    }
}
/// One applicable policy layer of the admitting workspace: the workspace
/// layers (type default or platform override, then local) or, with
/// `api_key_id`, the key lineage layer. There is no installation layer
/// (removed in 0026): admission reads no installation-wide limit data.
#[derive(sqlx::FromRow)]
pub(super) struct Policy {
    pub(super) api_key_id: Option<Uuid>,
    pub(super) requests_per_minute: Option<i64>,
    pub(super) tokens_per_minute: Option<i64>,
    pub(super) concurrent_requests: Option<i64>,
    /// "Jobs at once" (0018): active async jobs (video + batch).
    pub(super) concurrent_jobs: Option<i64>,
}
/// One applicable budget of the admitting workspace (`api_key_id`: the key
/// lineage layer).
#[derive(sqlx::FromRow)]
pub(super) struct Budget {
    pub(super) api_key_id: Option<Uuid>,
    pub(super) period: String,
    pub(super) amount_microusd: i64,
}
// A replacement HEADER, including all-null, replaces the type default. Local/key
// policies compose, never coalesce away a stricter parent. Type limits are per workspace.
pub(super) const POLICIES: &str = "SELECT $1::uuid workspace_id,NULL::uuid api_key_id,p.requests_per_minute,p.tokens_per_minute,p.concurrent_requests,p.concurrent_jobs FROM workspace_platform_policy_overrides p WHERE workspace_id=$1
 UNION ALL SELECT $1::uuid,NULL::uuid,p.requests_per_minute,p.tokens_per_minute,p.concurrent_requests,p.concurrent_jobs FROM workspace_type_policies p JOIN workspaces w ON w.kind=p.kind WHERE w.id=$1 AND NOT EXISTS(SELECT 1 FROM workspace_platform_policy_overrides WHERE workspace_id=$1)
 UNION ALL SELECT $1::uuid,NULL::uuid,requests_per_minute,tokens_per_minute,concurrent_requests,concurrent_jobs FROM workspace_local_policies WHERE workspace_id=$1
 UNION ALL SELECT $1::uuid,$2::uuid,requests_per_minute,tokens_per_minute,concurrent_requests,concurrent_jobs FROM key_policies WHERE workspace_id=$1 AND governance_key_id=$2";
// Rate/concurrency/job accounting reads the maintained counters of
// `rates` (migration 0024): the current UTC minute and live leases only,
// O(scopes), exactly the former per-layer scan (kept as `rates::SCAN`):
// - Job reservations never count toward requests/tokens per minute.
// - A job holds a "requests at once" slot only while its submission runs
//   (no `async_jobs` row yet); afterwards it holds a job slot until the job
//   is terminal, cancel was requested, or the lease expired.
// - Gateway-run batch lines (`batch_job_id`, 0021) count toward nothing:
//   their batch holds the job slot.
/// Every applicable budget (one per scope and period). Override budgets apply
/// only while the replacement header exists; type budgets only without one.
/// Each budget is checked over its own window; a child can never loosen a
/// parent because the parent's own window check still applies.
pub(crate) const BUDGETS: &str = "SELECT $1::uuid workspace_id,NULL::uuid api_key_id,b.period,b.amount_microusd FROM policy_budgets b JOIN workspace_platform_policy_overrides o ON o.workspace_id=b.workspace_id WHERE b.layer='override' AND b.workspace_id=$1
 UNION ALL SELECT $1::uuid,NULL::uuid,b.period,b.amount_microusd FROM policy_budgets b JOIN workspaces w ON w.kind=b.kind WHERE b.layer='type' AND w.id=$1 AND NOT EXISTS(SELECT 1 FROM workspace_platform_policy_overrides WHERE workspace_id=$1)
 UNION ALL SELECT $1::uuid,NULL::uuid,period,amount_microusd FROM policy_budgets WHERE layer='local' AND workspace_id=$1
 UNION ALL SELECT $1::uuid,$2::uuid,period,amount_microusd FROM policy_budgets WHERE layer='key' AND workspace_id=$1 AND governance_key_id=$2";

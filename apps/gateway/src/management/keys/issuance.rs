//! Bounded key issuance (#253, migration 0053).
//!
//! Every new `api_keys` row (a create or a rotation) counts in a rolling
//! 24-hour window against two budgets: the holder's (`issued_to_user_id` or
//! `service_account_id`, inside the workspace) and the workspace's (all rows).
//! Both are read from the append-only `api_keys` table, so there are no counter
//! rows, no new lock and no hot row.
//!
//! Serialization is the workspace row lock that `resources::workspace_tx`
//! already takes (after the installation row) for every issuance; [`check`]
//! takes it again explicitly so the guarantee does not depend on a future
//! refactor of that helper. The count must be issued after that lock, at READ
//! COMMITTED: each statement then sees every issuance that committed before the
//! lock was granted (a REPEATABLE READ snapshot taken earlier would not). That
//! is verified on every call (`check` fails closed otherwise), not assumed.
//!
//! One database timestamp is read after the lock and bound into every window
//! predicate of the call (`created_at > now - 24h`), so the counts are a range
//! scan of `api_keys_workspace_created` (a volatile `clock_timestamp()` in the
//! predicate could not bound the index) and every figure of one call agrees on
//! "now". New keys are stamped `clock_timestamp()` at insert (migration 0053),
//! so a key that waited behind the installation lock counts from when it exists.
//! Limit edits are serialized with issuance too: type and override edits hold
//! the installation row, local edits the workspace row, so an issuance sees the
//! old or the new limit atomically.
//!
//! Effective limit = `least(coalesce(platform, 10000), coalesce(local, 10000))`,
//! where platform is `coalesce(override, type default)`: an override field that
//! is set replaces the type default, and one that is NULL inherits it (unlike
//! `filestore::files::QUOTA_SQL`, where a NULL override means no cap). That
//! makes override rows written without these columns by a replica that predates
//! migration 0053 harmless. A missing value is never unlimited.
use super::*;
use chrono::{DateTime, Utc};

pub(super) const WINDOW_SECONDS: i64 = 86_400;
const REFUSED: &str = "key_issuance_limit_exceeded";
const MAX: i64 = super::governance::MAX_KEY_ISSUANCES;

#[derive(Clone, Copy)]
pub(in crate::management) enum Operation {
    Create,
    Rotate,
}
impl Operation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Rotate => "rotate",
        }
    }
}
/// Whose budget a new key counts against, besides the workspace's.
#[derive(Clone, Copy)]
pub(in crate::management) enum Holder {
    User(Uuid),
    Service(Uuid),
}
impl Holder {
    fn column(self) -> &'static str {
        match self {
            Self::User(_) => "issued_to_user_id",
            Self::Service(_) => "service_account_id",
        }
    }
    fn id(self) -> Uuid {
        match self {
            Self::User(id) | Self::Service(id) => id,
        }
    }
}

/// Limits and rolling-window counts of one holder and its workspace.
pub(in crate::management) struct Usage {
    pub(in crate::management) holder_limit: i64,
    pub(in crate::management) workspace_limit: i64,
    pub(in crate::management) holder_used: i64,
    pub(in crate::management) workspace_used: i64,
}
/// The database's current time: read once per call, after the workspace lock.
async fn db_now(tx: &mut Transaction<'_, Postgres>) -> Result<DateTime<Utc>, ApiError> {
    Ok(sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?)
}
/// Start of the rolling window that ends at `now`.
fn window_start(now: DateTime<Utc>) -> DateTime<Utc> {
    now - chrono::Duration::seconds(WINDOW_SECONDS)
}
/// The effective limits and the counts, in one statement (so one snapshot).
/// `$1` workspace, `$2` holder, `$3` start of the window.
pub(in crate::management) fn usage_sql(holder: Holder) -> String {
    let col = holder.column();
    // The override's value replaces the type default; an empty override field
    // inherits it (so a row written by a pre-0053 replica never loosens it).
    let platform = |c: &str| format!("coalesce(o.{c},t.{c})");
    let (ph, pw) = (
        platform("key_issuances_per_holder"),
        platform("key_issuances_per_workspace"),
    );
    format!(
        "SELECT least(coalesce({ph},{MAX}),coalesce(l.key_issuances_per_holder,{MAX})),\
         least(coalesce({pw},{MAX}),coalesce(l.key_issuances_per_workspace,{MAX})),\
         (SELECT count(*) FROM api_keys k WHERE k.workspace_id=w.id AND k.{col}=$2 AND k.created_at>$3),\
         (SELECT count(*) FROM api_keys k WHERE k.workspace_id=w.id AND k.created_at>$3) \
         FROM workspaces w LEFT JOIN workspace_platform_policy_overrides o ON o.workspace_id=w.id LEFT JOIN workspace_type_policies t ON t.kind=w.kind LEFT JOIN workspace_local_policies l ON l.workspace_id=w.id WHERE w.id=$1"
    )
}
pub(in crate::management) async fn usage(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    holder: Holder,
    since: DateTime<Utc>,
) -> Result<Usage, ApiError> {
    let (holder_limit, workspace_limit, holder_used, workspace_used): (i64, i64, i64, i64) =
        sqlx::query_as(&usage_sql(holder))
            .bind(ws)
            .bind(holder.id())
            .bind(since)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(missing)?;
    Ok(Usage {
        holder_limit,
        workspace_limit,
        holder_used,
        workspace_used,
    })
}
/// When a budget at or over its limit frees up: the instant the
/// `used - limit + 1`-th oldest counted row leaves the window, and the seconds
/// from `now` until then (at least 1). `None` while there is room.
async fn frees_up(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    holder: Option<Holder>,
    used: i64,
    limit: i64,
    now: DateTime<Utc>,
) -> Result<Option<(DateTime<Utc>, i64)>, ApiError> {
    if used < limit {
        return Ok(None);
    }
    let since = window_start(now);
    let oldest: Option<DateTime<Utc>> = match holder {
        Some(h) => {
            sqlx::query_scalar(&format!(
                "SELECT k.created_at FROM api_keys k WHERE k.workspace_id=$1 AND k.{}=$3 AND k.created_at>$4 ORDER BY k.created_at OFFSET $2 LIMIT 1",
                h.column()
            ))
            .bind(ws)
            .bind(used - limit)
            .bind(h.id())
            .bind(since)
            .fetch_optional(&mut **tx)
            .await?
        }
        None => {
            sqlx::query_scalar("SELECT k.created_at FROM api_keys k WHERE k.workspace_id=$1 AND k.created_at>$3 ORDER BY k.created_at OFFSET $2 LIMIT 1")
                .bind(ws)
                .bind(used - limit)
                .bind(since)
                .fetch_optional(&mut **tx)
                .await?
        }
    };
    // Rows only age out of the window; if the row vanished from it since the
    // count, the budget has room now.
    let at = oldest.map_or(now, |created| {
        created + chrono::Duration::seconds(WINDOW_SECONDS)
    });
    let seconds = ((at - now).num_milliseconds() + 999)
        .div_euclid(1000)
        .max(1);
    Ok(Some((at, seconds)))
}

/// Refuses (429) when this issuance would exceed the holder's or the
/// workspace's budget. Call after authority and validation, before inserting
/// the key, in the transaction that holds the workspace lock; a refusal rolls
/// that transaction back and leaves nothing behind. Fails closed (503) unless
/// the transaction is READ COMMITTED.
pub(in crate::management) async fn check(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    holder: Holder,
    operation: Operation,
) -> Result<(), ManagementError> {
    // Same-transaction no-op today; explicit so the count below can never run
    // without the lock.
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE id=$1 FOR NO KEY UPDATE")
        .bind(ws)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)?;
    // The count is exact only if a statement sees everything committed before
    // the lock was granted: READ COMMITTED. Never admit on a weaker level.
    let isolation: String = sqlx::query_scalar("SELECT current_setting('transaction_isolation')")
        .fetch_one(&mut **tx)
        .await?;
    if isolation != "read committed" {
        // The level only: no workspace, holder or key data.
        tracing::error!(
            isolation,
            "key issuance refused: transactions must run at READ COMMITTED"
        );
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Management storage unavailable",
        )
        .into());
    }
    let now = db_now(tx).await?;
    let since = window_start(now);
    let u = usage(tx, ws, holder, since).await?;
    // The holder's own budget is named first, so a member is never told about
    // the workspace's when theirs is what stops them.
    for (scope, used, limit, who) in [
        ("holder", u.holder_used, u.holder_limit, Some(holder)),
        ("workspace", u.workspace_used, u.workspace_limit, None),
    ] {
        if let Some((_, retry)) = frees_up(tx, ws, who, used, limit, now).await? {
            crate::metrics::METRICS.key_issuance_refused(scope, operation.as_str());
            // The workspace id only: no holder, key name or amounts.
            tracing::warn!(workspace_id = %ws, scope, operation = operation.as_str(), "key issuance limit reached");
            return Err(ManagementError::detailed(
                StatusCode::TOO_MANY_REQUESTS,
                REFUSED,
                "Key issuance limit reached; try again later".to_owned(),
                json!({"scope":scope,"operation":operation.as_str(),"window_seconds":WINDOW_SECONDS,"retry_after_seconds":retry}),
            ));
        }
    }
    Ok(())
}

fn block(used: i64, limit: i64, next: Option<(DateTime<Utc>, i64)>) -> Value {
    json!({"used":used,"limit":limit,"next_available_at":next.map(|(at, _)| at)})
}
/// `GET /workspaces/{ws}/key-issuance`: the caller's own budget, and for
/// workspace admins the workspace's (members see their own activity only).
pub(in crate::management) async fn key_issuance(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_snapshot_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    let holder = Holder::User(u.user_id);
    // One "now" for the whole view, in the same snapshot as the counts.
    let now = db_now(&mut tx).await?;
    let usage = usage(&mut tx, ws, holder, window_start(now)).await?;
    let mine = frees_up(
        &mut tx,
        ws,
        Some(holder),
        usage.holder_used,
        usage.holder_limit,
        now,
    )
    .await?;
    let mut out = json!({"window_seconds":WINDOW_SECONDS,"holder":block(usage.holder_used,usage.holder_limit,mine)});
    if a.admin {
        let all = frees_up(
            &mut tx,
            ws,
            None,
            usage.workspace_used,
            usage.workspace_limit,
            now,
        )
        .await?;
        out["workspace"] = block(usage.workspace_used, usage.workspace_limit, all);
    }
    tx.commit().await?;
    Ok(Json(out))
}

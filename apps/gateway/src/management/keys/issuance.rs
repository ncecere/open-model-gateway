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
//! lock was granted (a REPEATABLE READ snapshot taken earlier would not).
//! Limit edits are serialized with issuance too: type and override edits hold
//! the installation row, local edits the workspace row, so an issuance sees the
//! old or the new limit atomically.
//!
//! Effective limit = `least(coalesce(platform, 10000), coalesce(local, 10000))`,
//! where platform is the override row if one exists, else the type default (as
//! `filestore::files::QUOTA_SQL` does). A missing value is never unlimited.
use super::*;
use chrono::{DateTime, Utc};

pub(super) const WINDOW_SECONDS: i64 = 86_400;
const REFUSED: &str = "key_issuance_limit_exceeded";
const MAX: i64 = super::governance::MAX_KEY_ISSUANCES;

#[derive(Clone, Copy)]
pub(super) enum Operation {
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
pub(super) enum Holder {
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
struct Usage {
    holder_limit: i64,
    workspace_limit: i64,
    holder_used: i64,
    workspace_used: i64,
}
/// The effective limits and the counts, in one statement (so one snapshot).
async fn usage(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    holder: Holder,
) -> Result<Usage, ApiError> {
    let col = holder.column();
    let platform =
        |c: &str| format!("CASE WHEN o.workspace_id IS NOT NULL THEN o.{c} ELSE t.{c} END");
    let (ph, pw) = (
        platform("key_issuances_per_holder"),
        platform("key_issuances_per_workspace"),
    );
    let (holder_limit, workspace_limit, holder_used, workspace_used): (i64, i64, i64, i64) = sqlx::query_as(&format!(
        "SELECT least(coalesce({ph},{MAX}),coalesce(l.key_issuances_per_holder,{MAX})),\
         least(coalesce({pw},{MAX}),coalesce(l.key_issuances_per_workspace,{MAX})),\
         (SELECT count(*) FROM api_keys k WHERE k.workspace_id=w.id AND k.{col}=$2 AND k.created_at>clock_timestamp()-make_interval(secs=>{WINDOW_SECONDS})),\
         (SELECT count(*) FROM api_keys k WHERE k.workspace_id=w.id AND k.created_at>clock_timestamp()-make_interval(secs=>{WINDOW_SECONDS})) \
         FROM workspaces w LEFT JOIN workspace_platform_policy_overrides o ON o.workspace_id=w.id LEFT JOIN workspace_type_policies t ON t.kind=w.kind LEFT JOIN workspace_local_policies l ON l.workspace_id=w.id WHERE w.id=$1"
    ))
    .bind(ws)
    .bind(holder.id())
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
/// until then (at least 1). `None` while there is room.
async fn frees_up(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    holder: Option<Holder>,
    used: i64,
    limit: i64,
) -> Result<Option<(DateTime<Utc>, i64)>, ApiError> {
    if used < limit {
        return Ok(None);
    }
    let filter = holder.map_or(String::new(), |h| format!("AND k.{}=$3", h.column()));
    let row: Option<(DateTime<Utc>, i64)> = sqlx::query_as(&format!(
        "SELECT k.created_at+make_interval(secs=>{WINDOW_SECONDS}),greatest(1,ceil(extract(epoch FROM k.created_at+make_interval(secs=>{WINDOW_SECONDS})-clock_timestamp()))::bigint) \
         FROM api_keys k WHERE k.workspace_id=$1 {filter} AND k.created_at>clock_timestamp()-make_interval(secs=>{WINDOW_SECONDS}) ORDER BY k.created_at OFFSET $2 LIMIT 1"
    ))
    .bind(ws)
    .bind(used - limit)
    .bind(holder.map_or_else(Uuid::nil, Holder::id))
    .fetch_optional(&mut **tx)
    .await?;
    // Rows only age out of the window; if the row vanished from the window
    // since the count, the budget has room now.
    Ok(Some(row.unwrap_or_else(|| (Utc::now(), 1))))
}

/// Refuses (429) when this issuance would exceed the holder's or the
/// workspace's budget. Call after authority and validation, before inserting
/// the key, in the transaction that holds the workspace lock; a refusal rolls
/// that transaction back and leaves nothing behind.
pub(super) async fn check(
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
    let u = usage(tx, ws, holder).await?;
    // The holder's own budget is named first, so a member is never told about
    // the workspace's when theirs is what stops them.
    for (scope, used, limit, who) in [
        ("holder", u.holder_used, u.holder_limit, Some(holder)),
        ("workspace", u.workspace_used, u.workspace_limit, None),
    ] {
        if let Some((_, retry)) = frees_up(tx, ws, who, used, limit).await? {
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
    let usage = usage(&mut tx, ws, holder).await?;
    let mut out = json!({"window_seconds":WINDOW_SECONDS,"holder":block(usage.holder_used,usage.holder_limit,frees_up(&mut tx,ws,Some(holder),usage.holder_used,usage.holder_limit).await?)});
    if a.admin {
        out["workspace"] = block(
            usage.workspace_used,
            usage.workspace_limit,
            frees_up(
                &mut tx,
                ws,
                None,
                usage.workspace_used,
                usage.workspace_limit,
            )
            .await?,
        );
    }
    tx.commit().await?;
    Ok(Json(out))
}

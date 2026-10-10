//! Own-scope Home endpoints: only the caller's human keys and activity, plus
//! workspace-wide budget windows where the caller administers the workspace.
use super::*;
use crate::governance::BudgetPeriod;
use chrono::{DateTime, Months, Utc};

/// Usage of the caller's human keys, as decimal strings. Token sums are the
/// observed values (a lower bound when `unknown_token_attempts` is nonzero).
const USAGE: &str = "jsonb_build_object('known_cost_microusd',coalesce(sum(r.actual_microusd),0)::text,'held_microusd',coalesce(sum(r.held_microusd) FILTER(WHERE r.actual_microusd IS NULL AND r.state IN('pending','unknown')),0)::text,'requests',count(DISTINCT e.root_request_id)::text,'attempts',count(e.id)::text,'unresolved_attempts',count(e.id) FILTER(WHERE r.actual_microusd IS NULL)::text,'input_tokens',coalesce(sum(e.input_tokens),0)::text,'output_tokens',coalesce(sum(e.output_tokens),0)::text,'tokens',(coalesce(sum(e.input_tokens),0)+coalesce(sum(e.output_tokens),0))::text,'unknown_token_attempts',count(e.id) FILTER(WHERE e.input_tokens IS NULL OR e.output_tokens IS NULL)::text)";
const OWN: &str = "FROM inference_executions e JOIN api_keys k ON k.id=e.api_key_id AND k.workspace_id=e.workspace_id LEFT JOIN governance_reservations r ON r.execution_id=e.id WHERE k.issued_to_user_id=$1 AND e.workspace_id=ANY($2)";
fn empty_usage() -> Value {
    json!({"known_cost_microusd":"0","held_microusd":"0","requests":"0","attempts":"0","unresolved_attempts":"0","input_tokens":"0","output_tokens":"0","tokens":"0","unknown_token_attempts":"0"})
}
/// A lock-free primary snapshot (`crate::reporting`; never the reporting
/// replica): the live platform-role check, the caller's workspaces and their
/// own activity all come from one snapshot, without the catalog or
/// installation lock.
async fn me_tx(
    s: &Store,
    u: &BrowserPrincipal,
) -> Result<(Transaction<'static, Postgres>, Vec<WorkspaceContextRow>), ApiError> {
    let mut tx = s.snapshot().await?;
    resources::platform_role_snapshot(&mut tx, u.user_id).await?;
    let rows = my_workspaces(&mut tx, u.user_id).await?;
    Ok((tx, rows))
}
pub(super) async fn summary(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
) -> ApiResult {
    let (mut tx, rows) = me_tx(&s, &u).await?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let (cur_start, cur_end) = BudgetPeriod::Month.window(now);
    let prev_start = cur_start
        .checked_sub_months(Months::new(1))
        .ok_or_else(invalid)?;
    let grouped: Vec<(Uuid, bool, Value)> = sqlx::query_as(&format!("SELECT e.workspace_id,e.started_at>=$3,{USAGE} {OWN} AND e.started_at>=$4 AND e.started_at<$5 GROUP BY 1,2")).bind(u.user_id).bind(&ids).bind(cur_start).bind(prev_start).bind(cur_end).fetch_all(&mut *tx).await?;
    let totals: Vec<(bool, Value)> = sqlx::query_as(&format!(
        "SELECT e.started_at>=$3,{USAGE} {OWN} AND e.started_at>=$4 AND e.started_at<$5 GROUP BY 1"
    ))
    .bind(u.user_id)
    .bind(&ids)
    .bind(cur_start)
    .bind(prev_start)
    .bind(cur_end)
    .fetch_all(&mut *tx)
    .await?;
    let active: Vec<(Uuid, i64)> = sqlx::query_as("SELECT workspace_id,count(*) FROM api_keys WHERE issued_to_user_id=$1 AND workspace_id=ANY($2) AND revoked_at IS NULL AND disabled_at IS NULL AND (expires_at IS NULL OR expires_at>now()) GROUP BY 1").bind(u.user_id).bind(&ids).fetch_all(&mut *tx).await?;
    let pick = |ws: Option<Uuid>, current: bool| {
        match ws {
            Some(ws) => grouped
                .iter()
                .find(|(w, c, _)| *w == ws && *c == current)
                .map(|g| g.2.clone()),
            None => totals
                .iter()
                .find(|(c, _)| *c == current)
                .map(|g| g.1.clone()),
        }
        .unwrap_or_else(empty_usage)
    };
    let mut workspaces = Vec::new();
    for (id, name, kind, _, membership, _) in &rows {
        let role = if kind == "personal" {
            Some("owner")
        } else {
            membership.as_deref()
        };
        // Workspace-wide budget use only where the caller administers the workspace.
        let budgets = if matches!(role, Some("owner" | "admin")) {
            Some(governance::workspace_budget_windows(&mut tx, *id).await?)
        } else {
            None
        };
        workspaces.push(json!({"workspace_id":id,"name":name,"kind":kind,"role":role,"active_keys":active.iter().find(|a|a.0==*id).map_or(0,|a|a.1).to_string(),"current":pick(Some(*id),true),"previous":pick(Some(*id),false),"budgets":budgets}));
    }
    tx.commit().await?;
    let date = |d: DateTime<Utc>| d.date_naive().to_string();
    Ok(Json(
        json!({"currency":"USD","basis":"configured_rate_estimate","observed_at":now,"period":{"current":{"start_date":date(cur_start),"end_date":date(cur_end)},"previous":{"start_date":date(prev_start),"end_date":date(cur_start)}},"totals":{"current":pick(None,true),"previous":pick(None,false)},"workspaces":workspaces}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct KeyQuery {
    status: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}
pub(super) async fn my_keys(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<KeyQuery>,
) -> ApiResult {
    let status = match p.status.as_deref() {
        None | Some("all") => None,
        Some(v @ ("active" | "disabled" | "revoked" | "expired")) => Some(v.to_owned()),
        Some(_) => return Err(invalid()),
    };
    let (l, o) = Page {
        limit: p.limit,
        offset: p.offset,
    }
    .bounds()?;
    let (mut tx, rows) = me_tx(&s, &u).await?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    let status_sql = keys::KEY_STATUS;
    let last_used = keys::LAST_USED;
    let mut data: Vec<Value> = sqlx::query_scalar(&format!("SELECT jsonb_build_object('id',k.id,'name',k.name,'workspace',jsonb_build_object('id',w.id,'name',w.name,'kind',w.kind),'status',{status_sql},'created_at',k.created_at,'expires_at',k.expires_at,'revoked_at',k.revoked_at,'disabled_at',k.disabled_at,'last_used_at',{last_used},'lineage_id',k.governance_key_id,'model_ids',CASE WHEN r.governance_key_id IS NULL THEN NULL ELSE ARRAY(SELECT s.model_id FROM key_model_selections s WHERE s.workspace_id=k.workspace_id AND s.governance_key_id=k.governance_key_id ORDER BY s.model_id) END) FROM api_keys k JOIN workspaces w ON w.id=k.workspace_id LEFT JOIN key_model_restrictions r ON r.workspace_id=k.workspace_id AND r.governance_key_id=k.governance_key_id WHERE k.issued_to_user_id=$1 AND k.workspace_id=ANY($2) AND ($3::text IS NULL OR {status_sql}=$3) ORDER BY k.created_at DESC,k.id LIMIT $4 OFFSET $5")).bind(u.user_id).bind(&ids).bind(status).bind(l + 1).bind(o).fetch_all(&mut *tx).await?;
    let more = data.len() > l as usize;
    data.truncate(l as usize);
    keys::with_usage(&mut tx, &mut data, Some("workspace"), Uuid::nil()).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data,"has_more":more})))
}

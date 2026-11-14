//! Admin visibility of batch scheduling: a route's status, a batch's waits
//! per route, and the settings document of the management API. Metadata only.
use serde_json::{Value, json};
use uuid::Uuid;

use super::{PRIORITY_PROFILES, RouteSettings, WAIT_FRESH_SECONDS};

/// Admin › route: queue depth, batch lines running, the pause reason and
/// the last server metrics reading.
pub async fn route_status<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    deployment: Uuid,
) -> Result<Value, sqlx::Error> {
    sqlx::query_scalar(&format!("SELECT jsonb_build_object(
 'queued_lines',(SELECT coalesce(sum(waiting_lines),0) FROM batch_route_waits WHERE deployment_id=$1 AND updated_at>clock_timestamp()-make_interval(secs=>{WAIT_FRESH_SECONDS})),
 'waiting_batches',(SELECT count(*) FROM batch_route_waits WHERE deployment_id=$1 AND updated_at>clock_timestamp()-make_interval(secs=>{WAIT_FRESH_SECONDS})),
 'running_lines',(SELECT count(*) FROM batch_lines WHERE deployment_id=$1 AND state='running'),
 'paused_reason',s.paused_reason,'checked_at',s.checked_at,'live_in_flight',s.live_in_flight,
 'metrics',CASE WHEN s.metrics_checked_at IS NULL THEN NULL ELSE jsonb_build_object('checked_at',s.metrics_checked_at,'ok',s.metrics_ok,'waiting',s.metrics_waiting,'running',s.metrics_running,'kv_cache_permille',s.metrics_kv_cache_permille,'error',s.metrics_error) END)
 FROM (SELECT 1) one LEFT JOIN deployment_batch_signals s ON s.deployment_id=$1"))
        .bind(deployment)
        .fetch_one(executor)
        .await
}

/// A batch's waits per route (model name, reason, lines waiting and running,
/// queue position and length on the route). Metadata only.
pub async fn batch_waits<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    job: Uuid,
) -> Result<Vec<Value>, sqlx::Error> {
    sqlx::query_scalar(&format!("WITH mine AS (SELECT deployment_id FROM batch_route_waits WHERE job_id=$1 AND updated_at>clock_timestamp()-make_interval(secs=>{WAIT_FRESH_SECONDS})),
 w AS (SELECT b.* FROM batch_route_waits b WHERE b.deployment_id IN (SELECT deployment_id FROM mine) AND b.updated_at>clock_timestamp()-make_interval(secs=>{WAIT_FRESH_SECONDS})),
 jr AS (SELECT deployment_id,job_id,count(*) n FROM batch_lines WHERE state='running' AND deployment_id IN (SELECT deployment_id FROM mine) GROUP BY 1,2),
 wr AS (SELECT deployment_id,workspace_id,count(*) n FROM batch_lines WHERE state='running' AND deployment_id IN (SELECT deployment_id FROM mine) GROUP BY 1,2),
 x AS (SELECT w.*,coalesce(jr.n,0) jn,coalesce(wr.n,0) wn,max(w.last_claim_at) OVER (PARTITION BY w.deployment_id,w.workspace_id) wl FROM w LEFT JOIN jr ON jr.deployment_id=w.deployment_id AND jr.job_id=w.job_id LEFT JOIN wr ON wr.deployment_id=w.deployment_id AND wr.workspace_id=w.workspace_id),
 r AS (SELECT x.*,row_number() OVER (PARTITION BY deployment_id ORDER BY wn,wl NULLS FIRST,jn,last_claim_at NULLS FIRST,since,job_id) pos,count(*) OVER (PARTITION BY deployment_id) queue FROM x)
 SELECT jsonb_build_object('model',m.public_name,'reason',r.reason,'waiting_lines',r.waiting_lines,'running_lines',r.jn,'position',r.pos,'queue',r.queue,'since',r.since)
 FROM r JOIN deployments d ON d.id=r.deployment_id JOIN models m ON m.id=d.model_id WHERE r.job_id=$1 ORDER BY m.public_name,r.deployment_id"))
        .bind(job)
        .fetch_all(executor)
        .await
}

/// The settings document of the management API (settings, defaults and
/// what the route supports).
pub fn settings_document(settings: &RouteSettings, provider: &str, status: Value) -> Value {
    json!({
        "settings": settings,
        "defaults": RouteSettings::default(),
        "priority_supported": PRIORITY_PROFILES.contains(&provider),
        "status": status,
    })
}

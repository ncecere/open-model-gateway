//! Report-owned envelopes in the shared email outbox. No SMTP or retry loop.
use crate::{
    governance::locks::{self, Scope},
    identity::IdentityConfig,
};
use chrono::{DateTime, Utc};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

/// Publication-time identity/address snapshot, not dispatcher-time expansion.
/// No plaintext address is retained. A repeated enqueue never reopens a row.
pub(crate) async fn enqueue(tx: &mut Transaction<'_, Postgres>, run: Uuid) -> sqlx::Result<()> {
    let id: Option<Uuid> = sqlx::query_scalar("INSERT INTO alert_deliveries(id,report_run_id,transition) VALUES(gen_random_uuid(),$1,'fired') ON CONFLICT(report_run_id) DO NOTHING RETURNING id").bind(run).fetch_optional(&mut **tx).await?;
    if let Some(id) = id {
        sqlx::query("INSERT INTO alert_recipient_outbox(delivery_id,fingerprint,internal_user_id) SELECT $1,sha256(uuid_send($1)||convert_to(':omg.alert.recipient.v1:','UTF8')||convert_to(lower(btrim(u.email)),'UTF8')),u.id FROM report_runs r JOIN users u ON r.configuration->'recipient_user_ids' @> jsonb_build_array(u.id) WHERE r.id=$2 AND u.email IS NOT NULL ORDER BY u.id")
            .bind(id).bind(run).execute(&mut **tx).await?;
        sqlx::query("WITH totals AS (SELECT count(*)::integer n,min(next_attempt_at) due FROM alert_recipient_outbox WHERE delivery_id=$1) UPDATE alert_deliveries SET initialized=true,recipients=t.n,waiting=t.n,next_attempt_at=t.due,status=CASE WHEN t.n>0 THEN 'report_pending' ELSE 'no_recipients' END,completed_at=CASE WHEN t.n=0 THEN clock_timestamp() END FROM totals t WHERE id=$1").bind(id).execute(&mut **tx).await?;
    }
    Ok(())
}
/// Canonical authority locks end with the claim transaction, never with SMTP.
/// A revocation committed after that authorized claim can race that one send.
pub(crate) async fn lock_recipients(
    tx: &mut Transaction<'_, Postgres>,
    run: Uuid,
) -> sqlx::Result<()> {
    let (ws, users): (Option<Uuid>, Vec<Uuid>) = sqlx::query_as("SELECT s.workspace_id,ARRAY(SELECT q.internal_user_id FROM alert_deliveries d JOIN alert_recipient_outbox q ON q.delivery_id=d.id WHERE d.report_run_id=r.id ORDER BY q.internal_user_id) FROM report_runs r JOIN report_schedules s ON s.id=r.schedule_id WHERE r.id=$1").bind(run).fetch_one(&mut **tx).await?;
    locks::shared(
        tx,
        ws.map(Scope::Workspace)
            .into_iter()
            .chain(users.into_iter().map(Scope::User)),
    )
    .await?;
    // Serialize schedule pause/delete/recipient replacement with this claim.
    sqlx::query("SELECT s.id FROM report_runs r JOIN report_schedules s ON s.id=r.schedule_id WHERE r.id=$1 FOR SHARE OF s").bind(run).execute(&mut **tx).await?;
    Ok(())
}
/// Internal identities only, intersected with the immutable occurrence audience,
/// current recipient set, entitlement, account/workspace/schedule state and the
/// original address fingerprint. Changed email cannot redirect even a retry.
pub(crate) const RECIPIENTS: &str = "SELECT DISTINCT lower(btrim(u.email)) address,u.id internal_user_id FROM alert_deliveries d JOIN report_runs r ON r.id=d.report_run_id JOIN report_snapshots snapshot ON snapshot.run_id=r.id JOIN report_schedules s ON s.id=r.schedule_id JOIN report_schedule_recipients selected ON selected.schedule_id=s.id JOIN users u ON u.id=selected.user_id JOIN effective_platform_roles p ON p.user_id=u.id JOIN alert_recipient_outbox q ON q.delivery_id=d.id AND q.internal_user_id=u.id LEFT JOIN workspaces w ON w.id=s.workspace_id WHERE d.id=$1 AND s.state='active' AND s.deleted_at IS NULL AND snapshot.expires_at>clock_timestamp() AND u.disabled_at IS NULL AND u.cleaned_at IS NULL AND u.email IS NOT NULL AND r.configuration->'recipient_user_ids' @> jsonb_build_array(u.id) AND q.fingerprint=sha256(uuid_send(d.id)||convert_to(':omg.alert.recipient.v1:','UTF8')||convert_to(lower(btrim(u.email)),'UTF8')) AND ((s.scope_kind IN ('installation','cost_center') AND p.role IN ('admin','auditor')) OR (w.disabled_at IS NULL AND ((s.scope_kind='personal' AND w.kind='personal' AND w.owner_user_id=u.id) OR (s.scope_kind='workspace' AND w.kind IN ('team','project') AND EXISTS(SELECT FROM effective_workspace_memberships m WHERE m.workspace_id=w.id AND m.user_id=u.id AND m.role IN ('owner','admin')))))) ORDER BY 1 LIMIT 50";

pub(crate) fn public_origin() -> Option<String> {
    let value = std::env::var("GATEWAY_PUBLIC_URL").ok()?;
    IdentityConfig::validate_origin(
        &value,
        std::env::var("GATEWAY_ENV").as_deref() == Ok("development"),
    )
    .ok()
    .map(|u| u.origin().ascii_serialization())
    .filter(|s| s.len() <= 512)
}
/// Browser deep link, following locations.ts's /workspaces/{ws} and /admin
/// scope conventions. Slice D renders this route and loads the separately
/// authorized JSON occurrence endpoint; link possession grants no authority.
pub(crate) fn report_path(workspace: Option<Uuid>, schedule: Uuid, run: Uuid) -> String {
    let scope = workspace.map_or_else(|| "admin".to_owned(), |ws| format!("workspaces/{ws}"));
    format!("/{scope}/report-schedules/{schedule}/runs/{run}")
}

pub(crate) async fn message(
    tx: &mut Transaction<'_, Postgres>,
    run: Uuid,
    origin: &str,
) -> sqlx::Result<(String, String)> {
    let (schedule, ws, start, end): (Uuid,Option<Uuid>,DateTime<Utc>,DateTime<Utc>) = sqlx::query_as("SELECT r.schedule_id,s.workspace_id,r.period_start,r.period_end FROM report_runs r JOIN report_schedules s ON s.id=r.schedule_id WHERE r.id=$1").bind(run).fetch_one(&mut **tx).await?;
    let path = report_path(ws, schedule, run);
    Ok((
        "Scheduled report available".into(),
        format!(
            "A scheduled report is available.\r\nPeriod: {} to {} UTC (end exclusive).\r\n\r\nSign in to view the report: {origin}{path}\r\nAccess is checked when you open the report; this link grants no access.\r\n",
            start.format("%Y-%m-%d"),
            end.format("%Y-%m-%d")
        ),
    ))
}
/// Called by the common outbox only after fenced state transitions. Typed
/// identifiers/counts/categories only: no fingerprints, addresses or SMTP text.
pub(crate) async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    delivery: Uuid,
    category: &str,
    count: i64,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO audit_events(id,workspace_id,action,resource_type,resource_id,metadata) SELECT gen_random_uuid(),s.workspace_id,'report.delivery_outcome','report_schedule',s.id,jsonb_build_object('run_id',r.id,'category',$2::text,'count',$3::bigint) FROM alert_deliveries d JOIN report_runs r ON r.id=d.report_run_id JOIN report_schedules s ON s.id=r.schedule_id WHERE d.id=$1").bind(delivery).bind(category).bind(count).execute(&mut **tx).await?;
    Ok(())
}

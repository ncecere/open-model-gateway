//! #102B: bounded primary-only aggregate generation. No delivery or SMTP.
//! Every write transaction is lease-fenced; generation runs outside those
//! transactions in a bounded REPEATABLE READ READ ONLY primary snapshot.
pub(crate) mod calendar;
use crate::{
    governance::locks::{self, Scope},
    leases::{Fence, Lease},
    store::Store,
};
use calendar::{Calendar, Occurrence};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use std::time::Duration;
use uuid::Uuid;
const PER_TICK: i64 = 8;
const DEADLINE: Duration = Duration::from_secs(10);
#[derive(Clone, sqlx::FromRow)]
struct Schedule {
    id: Uuid,
    workspace_id: Option<Uuid>,
    cost_center_id: Option<Uuid>,
    scope_kind: String,
    created_by: Uuid,
    cadence: String,
    timezone: String,
    delivery_time: String,
    configuration_revision: i64,
    next_period_end: Option<DateTime<Utc>>,
    last_period_end: Option<DateTime<Utc>>,
    next_run_at: Option<DateTime<Utc>>,
}
const COLUMNS: &str = "id,workspace_id,cost_center_id,scope_kind,created_by,cadence,timezone,left(delivery_time::text,5) delivery_time,configuration_revision,next_period_end,last_period_end,next_run_at";
impl Schedule {
    fn calendar(&self) -> Result<Calendar, sqlx::Error> {
        Calendar::new(&self.cadence, &self.timezone, &self.delivery_time)
    }
    fn scopes(&self) -> Vec<Scope> {
        self.workspace_id
            .map(Scope::Workspace)
            .into_iter()
            .chain(std::iter::once(Scope::User(self.created_by)))
            .collect()
    }
}
#[derive(Clone)]
pub(crate) struct Claim {
    schedule: Schedule,
    token: Uuid,
    period: Occurrence,
    skipped: i64,
}
async fn authorized(tx: &mut Transaction<'_, Postgres>, s: &Schedule) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT FROM effective_platform_roles p WHERE p.user_id=$1 AND (($2 IN ('installation','cost_center') AND p.role='admin') OR EXISTS(SELECT FROM workspaces w WHERE w.id=$3 AND w.disabled_at IS NULL AND (($2='personal' AND w.kind='personal' AND w.owner_user_id=$1) OR ($2='workspace' AND w.kind IN ('team','project') AND EXISTS(SELECT FROM effective_workspace_memberships m WHERE m.workspace_id=w.id AND m.user_id=$1 AND m.role IN ('owner','admin')))))))")
        .bind(s.created_by).bind(&s.scope_kind).bind(s.workspace_id).fetch_one(&mut **tx).await
}
async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    s: &Schedule,
    action: &str,
    metadata: Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO audit_events(id,workspace_id,action,resource_type,resource_id,metadata) VALUES($1,$2,$3,'report_schedule',$4,$5)")
        .bind(Uuid::now_v7()).bind(s.workspace_id).bind(action).bind(s.id).bind(metadata).execute(&mut **tx).await?;
    Ok(())
}
async fn pause(tx: &mut Transaction<'_, Postgres>, s: &Schedule) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE report_schedules SET state='paused' WHERE id=$1 AND state='active' AND deleted_at IS NULL").bind(s.id).execute(&mut **tx).await?;
    audit(
        tx,
        s,
        "report_schedule.authority_paused",
        json!({"category":"creator_authority_lost"}),
    )
    .await
}
/// Called inside CRUD after its configuration write and by bounded worker
/// initialization for pre-upgrade NULL cursors. Database time, no backfill.
pub(crate) async fn initialize(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<(), sqlx::Error> {
    let s:Option<Schedule>=sqlx::query_as(&format!("SELECT {COLUMNS} FROM report_schedules WHERE id=$1 AND deleted_at IS NULL AND next_run_at IS NULL FOR UPDATE")).bind(id).fetch_optional(&mut **tx).await?;
    if let Some(s) = s {
        let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut **tx)
            .await?;
        let o = s.calendar()?.next_after(now, s.last_period_end)?;
        sqlx::query("UPDATE report_schedules SET next_run_at=$2,next_period_end=$3 WHERE id=$1")
            .bind(id)
            .bind(o.due)
            .bind(o.end)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}
pub(crate) async fn claim(
    store: &Store,
    fence: &Fence,
    id: Uuid,
) -> Result<Option<Claim>, sqlx::Error> {
    let mut tx = crate::db::begin(&store.pool).await?;
    fence.check(&mut tx).await?;
    let Some(before)=sqlx::query_as::<_,Schedule>(&format!("SELECT {COLUMNS} FROM report_schedules WHERE id=$1 AND state='active' AND deleted_at IS NULL")).bind(id).fetch_optional(&mut *tx).await? else {return Ok(None)};
    locks::shared(&mut tx, before.scopes()).await?;
    let Some(s)=sqlx::query_as::<_,Schedule>(&format!("SELECT {COLUMNS} FROM report_schedules WHERE id=$1 AND state='active' AND deleted_at IS NULL FOR UPDATE SKIP LOCKED")).bind(id).fetch_optional(&mut *tx).await? else {return Ok(None)};
    if !authorized(&mut tx, &s).await? {
        pause(&mut tx, &s).await?;
        tx.commit().await?;
        return Ok(None);
    }
    if s.next_run_at.is_none() || s.next_period_end.is_none() {
        initialize(&mut tx, id).await?;
        tx.commit().await?;
        return Ok(None);
    }
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    if s.next_run_at.is_some_and(|due| due > now) {
        return Ok(None);
    }
    let c = s.calendar()?;
    let period = c.latest_complete(now)?;
    // Never emit an older period just before the latest complete period's
    // delivery preference. Preserve the original cursor for skipped counting.
    if period.due > now {
        sqlx::query("UPDATE report_schedules SET next_run_at=$2 WHERE id=$1")
            .bind(id)
            .bind(period.due)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(None);
    }
    if s.next_period_end.is_some_and(|end| period.end < end)
        || s.last_period_end.is_some_and(|end| period.end <= end)
    {
        return Ok(None);
    }
    let token = Uuid::new_v4();
    let skipped = c.skipped(s.next_period_end.unwrap(), period.end);
    let won=sqlx::query("INSERT INTO report_generation_claims(schedule_id,token,configuration_revision,period_start,period_end,skipped_intervals,expires_at) VALUES($1,$2,$3,$4,$5,$6,clock_timestamp()+interval '30 seconds') ON CONFLICT(schedule_id) DO UPDATE SET token=excluded.token,configuration_revision=excluded.configuration_revision,period_start=excluded.period_start,period_end=excluded.period_end,skipped_intervals=excluded.skipped_intervals,expires_at=excluded.expires_at WHERE report_generation_claims.expires_at<=clock_timestamp()")
        .bind(id).bind(token).bind(s.configuration_revision).bind(period.start).bind(period.end).bind(skipped).execute(&mut *tx).await?.rows_affected()==1;
    tx.commit().await?;
    Ok(won.then_some(Claim {
        schedule: s,
        token,
        period,
        skipped,
    }))
}
pub(crate) async fn snapshot(store: &Store, c: &Claim) -> Result<Value, sqlx::Error> {
    tokio::time::timeout(DEADLINE,async {
        let mut tx=store.snapshot().await?; // deliberately NOT Store::reporting
        if !authorized(&mut tx,&c.schedule).await? {return Err(sqlx::Error::Protocol("report creator authority lost".into()));}
        let current:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM report_schedules WHERE id=$1 AND configuration_revision=$2 AND state='active' AND deleted_at IS NULL)").bind(c.schedule.id).bind(c.schedule.configuration_revision).fetch_one(&mut *tx).await?;
        if !current {return Err(sqlx::Error::Protocol("report configuration changed".into()));}
        let mut body=crate::management::governance::reports::scheduled_snapshot(&mut tx,c.period.start,c.period.end,c.schedule.workspace_id,c.schedule.cost_center_id,c.schedule.workspace_id.is_none()).await?;
        body["configuration_revision"]=json!(c.schedule.configuration_revision.to_string());
        if serde_json::to_vec(&body).map_err(|_|sqlx::Error::Protocol("report serialization".into()))?.len()>1_048_576 {return Err(sqlx::Error::Protocol("report snapshot exceeds limit".into()));}
        tx.commit().await?;
        Ok(body)
    }).await.unwrap_or(Err(sqlx::Error::PoolTimedOut))
}
pub(crate) async fn complete(
    store: &Store,
    fence: &Fence,
    c: &Claim,
    body: &Value,
) -> Result<bool, sqlx::Error> {
    let mut tx = crate::db::begin(&store.pool).await?;
    fence.check(&mut tx).await?;
    locks::shared(&mut tx, c.schedule.scopes()).await?;
    let Some(s)=sqlx::query_as::<_,Schedule>(&format!("SELECT {COLUMNS} FROM report_schedules WHERE id=$1 AND state='active' AND deleted_at IS NULL FOR UPDATE")).bind(c.schedule.id).fetch_optional(&mut *tx).await? else {return Ok(false)};
    if !authorized(&mut tx, &s).await? {
        pause(&mut tx, &s).await?;
        tx.commit().await?;
        return Ok(false);
    }
    if s.configuration_revision != c.schedule.configuration_revision {
        return Ok(false);
    }
    let current:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM report_generation_claims WHERE schedule_id=$1 AND token=$2 AND configuration_revision=$3 AND expires_at>clock_timestamp())").bind(s.id).bind(c.token).bind(s.configuration_revision).fetch_one(&mut *tx).await?;
    if !current {
        return Ok(false);
    }
    let observed: DateTime<Utc> = body["observed_at"]
        .as_str()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| sqlx::Error::Protocol("invalid report observation".into()))?;
    let run:Option<Uuid>=sqlx::query_scalar("INSERT INTO report_runs(schedule_id,period_start,period_end,configuration_revision,claim_token,lease_holder,lease_epoch,skipped_intervals,observed_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(schedule_id,period_start,period_end) DO NOTHING RETURNING id")
        .bind(s.id).bind(c.period.start).bind(c.period.end).bind(s.configuration_revision).bind(c.token).bind(fence.holder).bind(fence.epoch).bind(c.skipped).bind(observed).fetch_optional(&mut *tx).await?;
    if let Some(run) = run {
        sqlx::query("INSERT INTO report_snapshots(run_id,body) VALUES($1,$2)")
            .bind(run)
            .bind(body)
            .execute(&mut *tx)
            .await?;
        audit(&mut tx,&s,"report.generated",json!({"run_id":run,"configuration_revision":s.configuration_revision.to_string(),"skipped_intervals":c.skipped})).await?;
    }
    let next = s.calendar()?.next_after(c.period.due, Some(c.period.end))?;
    sqlx::query("UPDATE report_schedules SET last_period_end=$2,next_period_end=$3,next_run_at=$4 WHERE id=$1").bind(s.id).bind(c.period.end).bind(next.end).bind(next.due).execute(&mut *tx).await?;
    sqlx::query("UPDATE report_generation_claims SET expires_at=clock_timestamp() WHERE schedule_id=$1 AND token=$2").bind(s.id).bind(c.token).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(run.is_some())
}
/// One bounded tick, also used by real-PostgreSQL tests. Only the seeded report
/// lease is accepted; no unfenced operator mode that can publish occurrences.
pub async fn tick(store: &Store, fence: &Fence) -> Result<usize, sqlx::Error> {
    if fence.lease != Lease::ReportScheduler {
        return Err(sqlx::Error::Protocol("wrong report lease".into()));
    }
    // Upgrade-era paused schedules also get real future calendar values.
    let pending: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM report_schedules WHERE state='paused' AND deleted_at IS NULL AND next_run_at IS NULL ORDER BY id LIMIT $1")
        .bind(PER_TICK).fetch_all(&store.pool).await?;
    for id in pending {
        let mut tx = crate::db::begin(&store.pool).await?;
        fence.check(&mut tx).await?;
        initialize(&mut tx, id).await?;
        tx.commit().await?;
    }
    // Prioritize never-attempted schedules, then oldest expired claims. A
    // permanently oversized report must not starve other due schedules.
    let ids:Vec<Uuid>=sqlx::query_scalar("SELECT s.id FROM report_schedules s LEFT JOIN report_generation_claims c ON c.schedule_id=s.id WHERE s.state='active' AND s.deleted_at IS NULL AND (s.next_run_at IS NULL OR s.next_run_at<=clock_timestamp()) AND (c.schedule_id IS NULL OR c.expires_at<=clock_timestamp()) ORDER BY c.expires_at NULLS FIRST,s.next_run_at NULLS FIRST,s.id LIMIT $1").bind(PER_TICK).fetch_all(&store.pool).await?;
    let mut generated = 0;
    for id in ids {
        let result = tokio::time::timeout(DEADLINE, async {
            let Some(c) = claim(store, fence, id).await? else {
                return Ok(false);
            };
            let body = snapshot(store, &c).await?;
            complete(store, fence, &c, &body).await
        })
        .await
        .unwrap_or(Err(sqlx::Error::PoolTimedOut));
        match result {
            Ok(done) => generated += usize::from(done),
            Err(e) if crate::leases::is_fenced(&e) => return Err(e),
            Err(_) => {
                // Categories only, never SQL/upstream replies or payloads.
                let mut tx = crate::db::begin(&store.pool).await?;
                fence.check(&mut tx).await?;
                let s: Schedule = sqlx::query_as(&format!(
                    "SELECT {COLUMNS} FROM report_schedules WHERE id=$1"
                ))
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
                locks::shared(&mut tx, s.scopes()).await?;
                if !authorized(&mut tx,&s).await? && sqlx::query_scalar::<_,bool>("SELECT state='active' AND deleted_at IS NULL FROM report_schedules WHERE id=$1 FOR UPDATE").bind(id).fetch_one(&mut *tx).await? {pause(&mut tx,&s).await?;}
                audit(
                    &mut tx,
                    &s,
                    "report.generation_failed",
                    json!({"category":"generation_failed"}),
                )
                .await?;
                tx.commit().await?;
                tracing::warn!(schedule_id=%id,"scheduled report generation failed; retry after claim expiry");
            }
        }
    }
    let mut tx = crate::db::begin(&store.pool).await?;
    fence.check(&mut tx).await?;
    sqlx::query("SELECT omg_report_prune_snapshots(100)")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(generated)
}
pub fn start(store: Store) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(Duration::from_secs(30));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            if let Some(leases) = store.leases() {
                crate::leases::run_singleton(
                    leases,
                    Lease::ReportScheduler,
                    "report_scheduler",
                    Duration::from_secs(90),
                    |fence| {
                        let store = &store;
                        async move { tick(store, &fence).await }
                    },
                )
                .await;
            }
        }
    })
}

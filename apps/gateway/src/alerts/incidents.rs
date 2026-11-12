//! Incident storage and state transitions: firing, resolving and superseding
//! `alert_events` rows and enqueueing their deliveries, under the delivery
//! bucket locks. Includes the built-in SCIM last-admin incident.
use super::*;

/// Counts of one reconciliation.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Changes {
    pub fired: usize,
    pub resolved: usize,
}

/// Prelock the complete bucket union before any incident row locks/writes.
/// SQL sorts actual signed advisory keys (UUID order is not lock-key order).
/// All multi-bucket producers (rules, key expiry and aged holds) use this
/// before incident writes, never incrementally per condition.
pub(crate) async fn lock_delivery_buckets(
    tx: &mut Transaction<'_, Postgres>,
    buckets: &[Uuid],
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT alert_delivery_lock_buckets($1)")
        .bind(buckets)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Apply `conditions` to the open incidents of `source`: fire new subjects,
/// resolve cleared ones, supersede changed levels. Idempotent.
pub(super) async fn reconcile(
    tx: &mut Transaction<'_, Postgres>,
    source: Source,
    kind: Kind,
    conditions: Vec<Condition>,
) -> Result<Changes, sqlx::Error> {
    // Include cleared subjects as well as new conditions; sorting conditions
    // alone would miss a bucket whose only transition this tick is clearing.
    let mut buckets: Vec<Uuid> = sqlx::query_scalar("SELECT coalesce(workspace_id,'00000000-0000-0000-0000-000000000000'::uuid) FROM alert_events WHERE resolved_at IS NULL AND rule_id IS NOT DISTINCT FROM $1 AND builtin IS NOT DISTINCT FROM $2")
        .bind(source.rule()).bind(source.builtin()).fetch_all(&mut **tx).await?;
    buckets.extend(
        conditions
            .iter()
            .map(|c| c.workspace_id.unwrap_or(Uuid::nil())),
    );
    lock_delivery_buckets(tx, &buckets).await?;
    let open: Vec<(Uuid, String, i32)> = sqlx::query_as("SELECT id,subject_key,level FROM alert_events WHERE resolved_at IS NULL AND rule_id IS NOT DISTINCT FROM $1 AND builtin IS NOT DISTINCT FROM $2 FOR UPDATE")
        .bind(source.rule())
        .bind(source.builtin())
        .fetch_all(&mut **tx)
        .await?;
    let mut changes = Changes::default();
    for (id, subject, _) in &open {
        if !conditions.iter().any(|c| &c.subject == subject) {
            resolve(tx, *id, "cleared").await?;
            changes.resolved += 1;
        }
    }
    for c in conditions {
        match open.iter().find(|(_, subject, _)| *subject == c.subject) {
            Some((_, _, level)) if *level == c.level => continue,
            Some((id, ..)) => resolve(tx, *id, "superseded").await?,
            None => {}
        }
        let id = Uuid::new_v4();
        // Never fire for a rule that was disabled or deleted meanwhile.
        let inserted = sqlx::query("INSERT INTO alert_events(id,rule_id,builtin,kind,subject_key,level,severity,workspace_id,provider_connection_id,summary,details) SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11 WHERE $2::uuid IS NULL OR EXISTS(SELECT 1 FROM alert_rules WHERE id=$2 AND enabled AND deleted_at IS NULL) ON CONFLICT DO NOTHING")
            .bind(id)
            .bind(source.rule())
            .bind(source.builtin())
            .bind(kind.as_str())
            .bind(&c.subject)
            .bind(c.level)
            .bind(if c.critical { "critical" } else { "warning" })
            .bind(c.workspace_id)
            .bind(c.connection_id)
            .bind(&c.summary)
            .bind(&c.details)
            .execute(&mut **tx)
            .await?
            .rows_affected();
        if inserted == 1 {
            sqlx::query(
                "INSERT INTO alert_deliveries(id,event_id,transition) VALUES($1,$2,'fired')",
            )
            .bind(Uuid::new_v4())
            .bind(id)
            .execute(&mut **tx)
            .await?;
            changes.fired += 1;
        }
    }
    Ok(changes)
}

pub(crate) async fn resolve(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    resolution: &str,
) -> Result<(), sqlx::Error> {
    let changed = sqlx::query("UPDATE alert_events SET resolved_at=clock_timestamp(),resolution=$2 WHERE id=$1 AND resolved_at IS NULL")
        .bind(id)
        .bind(resolution)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    if changed == 1 && resolution == "cleared" {
        sqlx::query(
            "INSERT INTO alert_deliveries(id,event_id,transition) VALUES($1,$2,'resolved')",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Resolve open incidents of disabled/deleted rules, or rules whose workspace
/// was disabled, without email. Shared with management (disable/delete).
pub(crate) async fn retire_inactive(
    tx: &mut Transaction<'_, Postgres>,
    rule: Option<Uuid>,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("UPDATE alert_events e SET resolved_at=clock_timestamp(),resolution='rule_disabled' FROM alert_rules r WHERE e.rule_id=r.id AND e.resolved_at IS NULL AND ($1::uuid IS NULL OR r.id=$1) AND (NOT r.enabled OR r.deleted_at IS NOT NULL OR EXISTS(SELECT 1 FROM workspaces w WHERE w.id=r.workspace_id AND w.disabled_at IS NOT NULL))")
        .bind(rule)
        .execute(&mut **tx)
        .await?
        .rows_affected())
}

/// Raise (or keep) the SCIM last-admin incident. `resource` is `user` or
/// `group` only: no names, emails or ids. Idempotent while one is open.
pub(crate) async fn fire_scim_last_admin(
    tx: &mut Transaction<'_, Postgres>,
    resource: &str,
) -> Result<bool, sqlx::Error> {
    lock_delivery_buckets(tx, &[Uuid::nil()]).await?;
    let id = Uuid::new_v4();
    let inserted = sqlx::query("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary,details) VALUES($1,$2,$2,$2,1,'critical',$3,jsonb_build_object('resource',$4::text)) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(SCIM_LAST_ADMIN)
        .bind(SCIM_LAST_ADMIN_SUMMARY)
        .bind(resource)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    if inserted == 1 {
        sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition) VALUES($1,$2,'fired')")
            .bind(Uuid::new_v4())
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(inserted == 1)
}

/// The SCIM last-admin incident clears once a second active Platform Admin
/// exists (the installation is no longer one SCIM change away from lockout).
pub(super) async fn resolve_scim_last_admin(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<usize, sqlx::Error> {
    lock_delivery_buckets(tx, &[Uuid::nil()]).await?;
    let open: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM alert_events WHERE builtin=$1 AND resolved_at IS NULL AND (SELECT count(*) FROM effective_platform_roles WHERE role='admin')>=2 FOR UPDATE")
        .bind(SCIM_LAST_ADMIN)
        .fetch_all(&mut **tx)
        .await?;
    for id in &open {
        resolve(tx, *id, "cleared").await?;
    }
    Ok(open.len())
}

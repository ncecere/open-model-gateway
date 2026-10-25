//! Readable allowlisted snapshots, authorized on every read. Never reconstruct
//! legacy values from live configuration. IDs survive target removal/renaming.
use super::*;
use chrono::{DateTime, Utc};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HistoryQuery {
    limit: Option<i64>,
    cursor: Option<String>,
}

// The two indexed branches include only deployment enablement/creation events.
// Snapshot IDs are their audit IDs; the composite identity prevents duplicate
// legacy rows. A logical link preserves audit partition archival/detachment.
const HISTORY: &str = "WITH entries AS (
 SELECT c.id,c.created_at,c.deployment_id,c.schema_version,c.revision,c.before_enabled,c.after_enabled,true values_available,
 a.actor_user_id,a.admin_token_id,CASE WHEN c.before_enabled IS NULL THEN 'deployment.created' ELSE 'configuration.updated' END action
 FROM configuration_changes c LEFT JOIN audit_events a ON (a.id,a.created_at)=(c.audit_event_id,c.audit_created_at)
 WHERE c.deployment_id=$1 AND ($2::timestamptz IS NULL OR (c.created_at,c.id)<($2,$3)) AND ($4::uuid IS NULL OR c.id=$4)
 UNION ALL
 SELECT a.id,a.created_at,a.resource_id,NULL::smallint,NULL::bigint,NULL::boolean,NULL::boolean,false,
 a.actor_user_id,a.admin_token_id,a.action
 FROM audit_events a WHERE a.resource_id=$1 AND a.workspace_id IS NULL
 AND a.resource_type IN ('deployment','deployments') AND a.action IN ('deployment.created','configuration.updated')
 AND ($2::timestamptz IS NULL OR (a.created_at,a.id)<($2,$3)) AND ($4::uuid IS NULL OR a.id=$4)
 AND NOT EXISTS(SELECT FROM configuration_changes c WHERE (c.audit_event_id,c.audit_created_at)=(a.id,a.created_at))
), bounded AS (SELECT * FROM entries ORDER BY created_at DESC,id DESC LIMIT $5)
SELECT jsonb_build_object(
 'id',e.id,'created_at',e.created_at,'target',jsonb_build_object('kind','deployment','id',e.deployment_id),
 'action',e.action,'actor',jsonb_build_object('id',e.actor_user_id,'admin_token_id',e.admin_token_id,
 'label',CASE WHEN e.actor_user_id IS NULL THEN CASE WHEN e.values_available AND NOT EXISTS(SELECT FROM audit_events a WHERE (a.id,a.created_at)=(e.id,e.created_at)) THEN 'Actor unavailable' ELSE 'System' END
 ELSE coalesce(nullif(left(regexp_replace(u.display_name,'[[:cntrl:]]','','g'),160),''),nullif(left(regexp_replace(CASE WHEN u.cleaned_at IS NULL THEN u.email END,'[[:cntrl:]]','','g'),160),''),'User '||e.actor_user_id::text) END),
 'schema_version',e.schema_version,'revision',e.revision::text,'values_available',e.values_available,
 'before',CASE WHEN e.values_available AND e.before_enabled IS NOT NULL THEN jsonb_build_object('enabled',e.before_enabled) ELSE NULL END,
 'after',CASE WHEN e.values_available THEN jsonb_build_object('enabled',e.after_enabled) ELSE NULL END),e.created_at,e.id
 FROM bounded e LEFT JOIN users u ON u.id=e.actor_user_id ORDER BY e.created_at DESC,e.id DESC";

async fn read(
    s: &Store,
    u: &ManagementPrincipal,
    id: Uuid,
    cursor: Option<(DateTime<Utc>, Uuid)>,
    change: Option<Uuid>,
    limit: i64,
) -> Result<Vec<(Value, DateTime<Utc>, Uuid)>, ApiError> {
    if u.token_id().is_some() {
        return Err(denied());
    }
    let mut tx = resources::platform_read_tx(s, u).await?;
    // No live-target existence predicate: authorized immutable identities remain
    // readable even when the target no longer exists. No names are invented.
    let rows = sqlx::query_as(HISTORY)
        .bind(id)
        .bind(cursor.map(|c| c.0))
        .bind(cursor.map(|c| c.1))
        .bind(change)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows)
}

pub(super) async fn list(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult {
    let limit = q.limit.unwrap_or(25);
    if !(1..=100).contains(&limit) {
        return Err(invalid());
    }
    let cursor = q.cursor.as_deref().map(logs::cursor_parts).transpose()?;
    let rows = read(&s, &u, id, cursor, None, limit + 1).await?;
    let next = (rows.len() > limit as usize).then(|| {
        let (_, time, id) = &rows[limit as usize - 1];
        format!("{}_{id}", time.timestamp_micros())
    });
    let data: Vec<_> = rows.into_iter().take(limit as usize).map(|r| r.0).collect();
    Ok(Json(json!({"data":data,"next_cursor":next})))
}

pub(super) async fn detail(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path((id, change)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    let row = read(&s, &u, id, None, Some(change), 1)
        .await?
        .into_iter()
        .next()
        .ok_or_else(missing)?;
    Ok(Json(row.0))
}

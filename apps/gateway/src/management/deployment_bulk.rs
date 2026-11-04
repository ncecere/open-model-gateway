//! #88: explicit-target bulk deployment disablement. Each target is applied
//! in its own transaction with live authorization and its reviewed revision;
//! its outcome is recorded in that same transaction. One operation per actor
//! and idempotency key: a replay returns the recorded outcomes and never
//! applies anything again.
//!
//! The request applying an operation holds a lease, renewed before each
//! target. The operation completes exactly once, with one outcome audit
//! event: when the request finishes, or, after an interruption, when a replay
//! or status read finds the lease lapsed. Targets still undecided then are
//! recorded `unattempted`; an item being applied holds its row, so it is
//! either decided by its own transaction or never applied.
use super::*;
use resources::Enabled;
use sha2::{Digest, Sha256};

const MAX_ITEMS: usize = 50;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BulkInput {
    action: String,
    idempotency_key: String,
    items: Vec<BulkItem>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BulkItem {
    id: Uuid,
    expected_revision: String,
}

fn key_valid(key: &str) -> bool {
    (1..=128).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}
/// Validated targets in request order, and the digest of their canonical form
/// (ids and revisions as the server reads them).
type Targets = Vec<(Uuid, i64)>;
const LEASE: &str = "30 seconds";

fn validate(input: &BulkInput) -> Result<(Targets, Vec<u8>), ApiError> {
    if input.action != "disable"
        || !key_valid(&input.idempotency_key)
        || input.items.is_empty()
        || input.items.len() > MAX_ITEMS
    {
        return Err(invalid());
    }
    let mut targets = Vec::with_capacity(MAX_ITEMS);
    for item in &input.items {
        let revision = super::deployment_configuration::revision(&item.expected_revision)?;
        if targets.iter().any(|(id, _)| *id == item.id) {
            return Err(invalid());
        }
        targets.push((item.id, revision));
    }
    let canonical: Vec<String> = targets.iter().map(|(id, r)| format!("{id}:{r}")).collect();
    let digest = Sha256::digest(format!("deployment.disable\n{}", canonical.join("\n")).as_bytes());
    Ok((targets, digest.to_vec()))
}

#[derive(sqlx::FromRow)]
struct Operation {
    id: Uuid,
    payload_digest: Vec<u8>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The recorded operation, as the caller may read it. `outcome` is null only
/// while the operation is incomplete: that target has not been decided yet.
async fn report(s: &Store, operation: Uuid, replayed: bool) -> Result<Value, ApiError> {
    // One snapshot: the completion flag and the outcomes agree.
    let mut tx = crate::db::begin_with_setup(
        &s.pool,
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY",
    )
    .await?;
    let (key, completed_at): (String, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT idempotency_key,completed_at FROM configuration_bulk_operations WHERE id=$1",
    )
    .bind(operation)
    .fetch_one(&mut *tx)
    .await?;
    let items: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',deployment_id,'expected_revision',expected_revision::text,'outcome',outcome,'reason',reason,'revision',revision::text,'change_id',change_id) FROM configuration_bulk_items WHERE operation_id=$1 ORDER BY position",
    )
    .bind(operation)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(
        json!({"operation_id":operation,"idempotency_key":key,"completed":completed_at.is_some(),"replayed":replayed,"items":items}),
    )
}

async fn record(
    tx: &mut Transaction<'_, Postgres>,
    operation: Uuid,
    position: usize,
    outcome: &str,
    reason: Option<&str>,
    revision: Option<i64>,
    change: Option<Uuid>,
) -> Result<(), ApiError> {
    let recorded = sqlx::query("UPDATE configuration_bulk_items SET outcome=$3,reason=$4,revision=$5,change_id=$6,decided_at=clock_timestamp() WHERE operation_id=$1 AND position=$2 AND outcome IS NULL")
        .bind(operation)
        .bind(position as i16)
        .bind(outcome)
        .bind(reason)
        .bind(revision)
        .bind(change)
        .execute(&mut **tx)
        .await?;
    // The caller holds the undecided item: anything else rolls back.
    if recorded.rows_affected() != 1 {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "bulk item outcome was not recorded",
        ));
    }
    Ok(())
}
/// Lock the item; false when it was already decided (the operation was
/// completed after an interruption), so nothing may be applied.
async fn claim(
    tx: &mut Transaction<'_, Postgres>,
    operation: Uuid,
    position: usize,
) -> Result<bool, ApiError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT outcome IS NULL FROM configuration_bulk_items WHERE operation_id=$1 AND position=$2 FOR UPDATE",
    )
    .bind(operation)
    .bind(position as i16)
    .fetch_one(&mut **tx)
    .await?)
}
/// Renew the lease before the next target; false once the operation has been
/// completed (by a status read after the lease lapsed).
async fn renew(s: &Store, operation: Uuid) -> Result<bool, ApiError> {
    Ok(sqlx::query(
        "UPDATE configuration_bulk_operations SET lease_until=clock_timestamp()+$2::interval WHERE id=$1 AND completed_at IS NULL",
    )
    .bind(operation)
    .bind(LEASE)
    .execute(&s.pool)
    .await?
    .rows_affected()
        == 1)
}
/// A refusal that happens before (or instead of) the target's transaction.
async fn refuse(
    s: &Store,
    operation: Uuid,
    position: usize,
    reason: &str,
) -> Result<bool, ApiError> {
    let mut tx = crate::db::begin(&s.pool).await?;
    if !claim(&mut tx, operation, position).await? {
        return Ok(false);
    }
    record(
        &mut tx,
        operation,
        position,
        "refused",
        Some(reason),
        None,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Complete the operation exactly once: under its row lock, when every item
/// is decided, its lease has lapsed, or its own request is finishing (`owner`).
/// Undecided items are recorded `unattempted` and one outcome event is
/// audited. False when it is still in progress.
async fn finalize(
    s: &Store,
    u: &ManagementPrincipal,
    operation: Uuid,
    owner: bool,
) -> Result<bool, ApiError> {
    let mut tx = crate::db::begin(&s.pool).await?;
    let (completed, lapsed, pending): (bool, bool, bool) = sqlx::query_as(
        "SELECT completed_at IS NOT NULL,lease_until<clock_timestamp(),EXISTS(SELECT FROM configuration_bulk_items i WHERE i.operation_id=o.id AND i.outcome IS NULL) FROM configuration_bulk_operations o WHERE id=$1 FOR UPDATE",
    )
    .bind(operation)
    .fetch_one(&mut *tx)
    .await?;
    if completed {
        return Ok(true);
    }
    if pending && !lapsed && !owner {
        return Ok(false);
    }
    // An item being applied holds its row: this waits for it and skips it
    // once decided.
    sqlx::query("UPDATE configuration_bulk_items SET outcome='unattempted',decided_at=clock_timestamp() WHERE operation_id=$1 AND outcome IS NULL")
        .bind(operation)
        .execute(&mut *tx)
        .await?;
    let outcomes: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',deployment_id,'outcome',outcome) FROM configuration_bulk_items WHERE operation_id=$1 ORDER BY position",
    )
    .bind(operation)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE configuration_bulk_operations SET completed_at=clock_timestamp() WHERE id=$1",
    )
    .bind(operation)
    .execute(&mut *tx)
    .await?;
    resources::audit(
        &mut tx,
        u,
        None,
        "configuration.bulk_disabled",
        "deployment_bulk_operations",
        Some(operation),
        json!({"count":outcomes.len(),"items":outcomes}),
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// One target, one transaction: live authority, the reviewed revision, then
/// the disablement and its recorded outcome commit together.
async fn apply_one(
    s: &Store,
    u: &ManagementPrincipal,
    operation: Uuid,
    position: usize,
    id: Uuid,
    expected: i64,
) -> Result<bool, ManagementError> {
    // Lock waits (the catalog lock, this item) end well within the lease, so
    // a slow target fails and stops the operation instead of outliving it.
    let mut tx = match resources::catalog_tx_bounded(s, u, "SET LOCAL lock_timeout='10s'").await {
        Ok(tx) => tx,
        Err(e) if e.0 == StatusCode::FORBIDDEN => {
            return Ok(refuse(s, operation, position, "forbidden").await?);
        }
        Err(e) => return Err(e.into()),
    };
    if !claim(&mut tx, operation, position).await? {
        return Ok(false);
    }
    resources::configuration_actor(&mut tx, u).await?;
    let current: Option<(bool, i64)> =
        sqlx::query_as("SELECT enabled,revision FROM deployments WHERE id=$1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((enabled, revision)) = current else {
        record(
            &mut tx,
            operation,
            position,
            "refused",
            Some("not_found"),
            None,
            None,
        )
        .await?;
        tx.commit().await?;
        return Ok(true);
    };
    if revision != expected {
        record(
            &mut tx,
            operation,
            position,
            "conflict",
            Some("revision_conflict"),
            Some(revision),
            None,
        )
        .await?;
    } else if !enabled {
        record(
            &mut tx,
            operation,
            position,
            "unchanged",
            None,
            Some(revision),
            None,
        )
        .await?;
    } else {
        // The same validation as every disable (disabling is always allowed).
        super::deployment_configuration::DeploymentChange::prepare(
            &mut tx,
            u,
            id,
            &Enabled { enabled: false },
        )
        .await?;
        sqlx::query("SELECT set_config('omg.configuration_bulk',$1,true)")
            .bind(operation.to_string())
            .execute(&mut *tx)
            .await?;
        // The revision was read under the exclusive catalog lock, so the
        // guarded write cannot miss; if it ever does, nothing commits.
        let revision: i64 = sqlx::query_scalar(
            "UPDATE deployments SET enabled=false WHERE id=$1 AND revision=$2 RETURNING revision",
        )
        .bind(id)
        .bind(expected)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "the deployment changed during the operation",
        ))?;
        let change: Uuid = sqlx::query_scalar(
            "SELECT id FROM configuration_changes WHERE deployment_id=$1 AND revision=$2",
        )
        .bind(id)
        .bind(revision)
        .fetch_one(&mut *tx)
        .await?;
        record(
            &mut tx,
            operation,
            position,
            "committed",
            None,
            Some(revision),
            Some(change),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// `POST /api/v1/platform/deployment-bulk-operations`.
pub(super) async fn create(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Json(input): Json<BulkInput>,
) -> DetailedResult {
    if u.token_id().is_some() {
        return Err(denied().into());
    }
    let (targets, digest) = validate(&input)?;
    let mut tx = crate::db::begin(&s.pool).await?;
    resources::platform_write(&mut tx, &u).await?;
    let created: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO configuration_bulk_operations(id,actor_user_id,idempotency_key,payload_digest,action,item_count) VALUES($1,$2,$3,$4,'deployment.disable',$5) ON CONFLICT (actor_user_id,idempotency_key) DO NOTHING RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(u.user_id())
    .bind(&input.idempotency_key)
    .bind(&digest)
    .bind(targets.len() as i16)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(operation) = created else {
        let existing: Operation = sqlx::query_as(
            "SELECT id,payload_digest,completed_at FROM configuration_bulk_operations WHERE actor_user_id=$1 AND idempotency_key=$2",
        )
        .bind(u.user_id())
        .bind(&input.idempotency_key)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        if existing.payload_digest != digest {
            return Err(ManagementError::detailed(
                StatusCode::CONFLICT,
                "idempotency_key_reused",
                "This idempotency key was used for different targets; use a new key.".into(),
                json!({}),
            ));
        }
        // Never applied again: an interrupted operation is completed (its
        // undecided targets unattempted) once its lease lapses.
        if existing.completed_at.is_none() && !finalize(&s, &u, existing.id, false).await? {
            return Err(ManagementError::detailed(
                StatusCode::CONFLICT,
                "bulk_incomplete",
                "This operation is still in progress; read its status.".into(),
                json!({}),
            ));
        }
        return Ok(Json(report(&s, existing.id, true).await?));
    };
    for (position, (id, _)) in targets.iter().enumerate() {
        sqlx::query("INSERT INTO configuration_bulk_items(operation_id,position,deployment_id,expected_revision) VALUES($1,$2,$3,$4)")
            .bind(operation)
            .bind(position as i16)
            .bind(id)
            .bind(targets[position].1)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    // Sequential, independent targets. Dropping this request stops the
    // unstarted ones (they become unattempted once the lease lapses);
    // committed targets stay committed and recorded. A failure stops the
    // remaining targets, which are recorded unattempted.
    for (position, (id, expected)) in targets.iter().enumerate() {
        let applied = match renew(&s, operation).await {
            Ok(true) => apply_one(&s, &u, operation, position, *id, *expected).await,
            Ok(false) => Ok(false),
            Err(e) => Err(e.into()),
        };
        match applied {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                let status = match &e {
                    ManagementError::Api(e) => e.0,
                    ManagementError::Detailed { status, .. } => *status,
                };
                tracing::warn!(operation = %operation, position, status = %status, "bulk target failed; the remaining targets are unattempted");
                break;
            }
        }
    }
    finalize(&s, &u, operation, true).await?;
    Ok(Json(report(&s, operation, false).await?))
}

/// `GET /api/v1/platform/deployment-bulk-operations/{idempotency_key}`: the caller's own
/// operation with this key (another actor's key is unknown).
pub(super) async fn status(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(key): Path<String>,
) -> ApiResult {
    if u.token_id().is_some() {
        return Err(denied());
    }
    if !key_valid(&key) {
        return Err(invalid());
    }
    let mut tx = crate::db::begin(&s.pool).await?;
    resources::platform_write(&mut tx, &u).await?;
    let operation: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT id,completed_at IS NOT NULL FROM configuration_bulk_operations WHERE actor_user_id=$1 AND idempotency_key=$2",
    )
    .bind(u.user_id())
    .bind(&key)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let (operation, completed) = operation.ok_or_else(missing)?;
    // Only an incomplete operation is locked (and perhaps completed).
    if !completed {
        finalize(&s, &u, operation, false).await?;
    }
    Ok(Json(report(&s, operation, true).await?))
}

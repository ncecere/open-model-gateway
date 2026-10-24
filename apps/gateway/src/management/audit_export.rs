//! Admin › Settings › Audit export (#106): read-only status and a test send.
//! The destination is the operator's (`GATEWAY_AUDIT_WEBHOOK_URL`,
//! `GATEWAY_AUDIT_SYSLOG_URL` or `GATEWAY_AUDIT_OTLP_URL`); nothing here can
//! change or disable it.

use super::*;

const TESTS_PER_MINUTE: i64 = 2;
const TESTS_PER_HOUR: i64 = 20;
const TEST_LIMIT: &str = "Too many test deliveries; wait a minute and try again";
const NOT_CONFIGURED: &str = "Audit export isn't configured on this server";

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/platform/settings/audit-export", get(status))
        .route("/api/v1/platform/settings/audit-export/test", post(test))
}

/// Platform Admins and Auditors.
async fn status(State(s): State<Store>, Extension(u): Extension<ManagementPrincipal>) -> ApiResult {
    let mut tx = crate::db::begin(&s.pool).await?;
    // #37 PR2 review: catalog shared lock first, in canonical order, ahead
    // of `platform_read`'s own possible User-scope lock
    // (`resources::revalidate_admin_token`, for a token actor) -- a plain
    // read-only snapshot otherwise, so this adds no contention beyond the
    // existing shared catalog lock every other management read already
    // takes.
    resources::catalog_lock(&mut tx, false).await?;
    resources::platform_read(&mut tx, &u).await?;
    tx.commit().await?;
    let mut v = crate::audit_export::status(&s).await?;
    // Whether the replica answering has a destination (all should agree).
    v["configured"] = json!(s.audit_export().is_some());
    // `webhook`, `syslog` (#107) or `otlp` (#108); null when this replica has no destination.
    v["transport"] = json!(s.audit_export().map(|e| e.transport()));
    Ok(Json(v))
}

/// Platform Admins: one signed test delivery with no events. Audited, and
/// rate-limited from the audit log (replica-safe).
async fn test(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    body: axum::body::Bytes,
) -> ApiResult {
    // Authority first: only a Platform Admin learns whether export is set up.
    let mut tx = super::settings::write_tx(&s, &u).await?;
    super::settings::no_configuration(&body)?;
    let exporter = s
        .audit_export()
        .cloned()
        .ok_or(ApiError(StatusCode::CONFLICT, NOT_CONFIGURED))?;
    let (mine, all): (i64, i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE actor_user_id=$1 AND created_at>now()-interval '1 minute'),count(*) FROM audit_events WHERE action='audit_export.tested' AND created_at>now()-interval '1 hour'")
        .bind(u.user_id())
        .fetch_one(&mut *tx)
        .await?;
    if mine >= TESTS_PER_MINUTE || all >= TESTS_PER_HOUR {
        return Err(ApiError(StatusCode::TOO_MANY_REQUESTS, TEST_LIMIT));
    }
    audit(
        &mut tx,
        &u,
        None,
        "audit_export.tested",
        "installation",
        None,
        json!({}),
    )
    .await?;
    // Never hold the installation lock across network I/O.
    tx.commit().await?;
    let result = crate::audit_export::send_test(&s, &exporter).await;
    Ok(Json(json!({ "ok": result.is_ok(), "error": result.err() })))
}

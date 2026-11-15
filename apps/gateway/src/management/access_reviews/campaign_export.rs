//! The campaign CSV export: `GET /api/v1/platform/access-reviews/{id}/export`.
use super::*;

/// Columns of [`export`], in order.
const EXPORT_COLUMNS: [&str; 22] = [
    "item_id",
    "subject_kind",
    "workspace_id",
    "workspace_name",
    "user_id",
    "email",
    "display_name",
    "role",
    "source",
    "granted_at",
    "active",
    "reviewable",
    "decision",
    "decided_by",
    "decided_by_email",
    "decided_at",
    "reviewer_user_id",
    "reviewer_email",
    "api_key_id",
    "holder_kind",
    "expires_at",
    "last_used_at",
];

/// `GET /api/v1/platform/access-reviews/{id}/export`: every item of a
/// campaign with its latest decision, decider and reviewer (key items as the
/// platform item list shows them: no key name or holder), as CSV (cells a
/// spreadsheet would run as a formula are neutralized). Platform Admins and
/// Auditors; audited with the item count. At most 5,000 rows (a campaign's
/// cap).
pub(super) async fn export(
    method: axum::http::Method,
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> Result<Response, ManagementError> {
    // A HEAD would run the export and record it without sending anything.
    if method == axum::http::Method::HEAD {
        return Err(ApiError(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed").into());
    }
    // Read in a snapshot (no row locks), then record the export on its own.
    let mut tx = resources::platform_read_tx(&s, &u).await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM access_review_campaigns WHERE id=$1)")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if !exists {
        return Err(missing().into());
    }
    let sets = reviewer_sets("$1");
    let item = platform_item_json();
    let rows: Vec<Value> = sqlx::query_scalar(&format!("WITH {sets}, x AS (SELECT {item} AS j, d.decided_by, i.subject_kind AS ok, i.workspace_id AS ow, CASE WHEN i.subject_kind='workspace_key' THEN NULL ELSE i.user_id END AS ou, i.id AS oi FROM {ITEM_FROM} {LATEST_DECISION} WHERE i.campaign_id=$1) SELECT jsonb_build_array(j->'id',j->'subject_kind',j->'workspace_id',j->'workspace_name',j->'user_id',j->'email',j->'display_name',j->'role',j->'source',j->'granted_at',j->'active',j->'reviewable',j->'decision',j->'decided_by',(SELECT x2.email FROM users x2 WHERE x2.id=x.decided_by),j->'decided_at',j->'reviewer_user_id',(SELECT y.email FROM users y WHERE y.id=(j->>'reviewer_user_id')::uuid),j->'api_key_id',j->'holder_kind',j->'expires_at',j->'last_used_at') FROM x ORDER BY x.ok DESC, x.ow, x.ou, x.oi"))
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut tx = crate::db::begin(&s.pool).await?;
    resources::platform_read(&mut tx, &u).await?;
    audit(
        &mut tx,
        &u,
        None,
        "access_review.exported",
        "access_review",
        Some(id),
        json!({"count":rows.len()}),
    )
    .await?;
    tx.commit().await?;
    let mut csv = EXPORT_COLUMNS.join(",");
    csv.push_str("\r\n");
    for row in &rows {
        let cells: Vec<String> = row
            .as_array()
            .into_iter()
            .flatten()
            .map(|v| {
                governance::reports::csv_cell(&match v {
                    Value::Null => String::new(),
                    Value::String(s) => s.clone(),
                    v => v.to_string(),
                })
            })
            .collect();
        csv.push_str(&cells.join(","));
        csv.push_str("\r\n");
    }
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/csv; charset=utf-8".to_owned(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"access-review.csv\"".to_owned(),
            ),
            (axum::http::header::CACHE_CONTROL, "no-store".to_owned()),
            (
                axum::http::header::X_CONTENT_TYPE_OPTIONS,
                "nosniff".to_owned(),
            ),
        ],
        csv,
    )
        .into_response())
}

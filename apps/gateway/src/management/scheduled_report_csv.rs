//! One immutable aggregate export, never stitched live request pages.
use super::*;
use axum::http::header;
const MAX_ROWS: usize = 1000;
fn cell(value: &Value) -> String {
    governance::reports::csv_cell(&match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    })
}
pub(super) fn download(run: Value) -> Response {
    let headers = [
        (header::CACHE_CONTROL, "no-store"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    ];
    if run["expired"] == true || run["snapshot"].is_null() {
        return (
            headers,
            ApiError(StatusCode::GONE, "Report expired after 90 days"),
        )
            .into_response();
    }
    let body = &run["snapshot"];
    let mut rows = vec![("total", Value::Null, Value::Null, &body["totals"])];
    for dimension in ["workspaces", "cost_centers", "models", "providers"] {
        if let Some(entries) = body["breakdowns"][dimension].as_array() {
            if rows.len().saturating_add(entries.len()) > MAX_ROWS {
                return (
                    headers,
                    ApiError(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "Aggregate CSV exceeds 1000 rows",
                    ),
                )
                    .into_response();
            }
            rows.extend(
                entries
                    .iter()
                    .map(|r| (dimension, r["id"].clone(), r["name"].clone(), &r["totals"])),
            );
        }
    }
    let mut csv = "dimension,id,name,known_cost_microusd,held_microusd,attempts,unresolved_attempts,root_requests,period_start,period_end,observed_at,configuration_revision,breakdowns_truncated,truncated_dimensions,unavailable,coverage,held_is_floor\r\n".to_owned();
    for (dimension, id, name, totals) in &rows {
        let fields = [
            json!(dimension),
            id.clone(),
            name.clone(),
            totals["known_cost_microusd"].clone(),
            totals["held_microusd"].clone(),
            totals["attempts"].clone(),
            totals["unresolved_attempts"].clone(),
            totals["root_requests"].clone(),
            run["period_start"].clone(),
            run["period_end"].clone(),
            run["observed_at"].clone(),
            run["configuration_revision"].clone(),
            body["breakdowns_truncated"].clone(),
            body["truncated_dimensions"].clone(),
            body["unavailable"].clone(),
            body["coverage"].clone(),
            body["held_is_floor"].clone(),
        ];
        csv.push_str(&fields.iter().map(cell).collect::<Vec<_>>().join(","));
        csv.push_str("\r\n");
    }
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"scheduled-report.csv\"".to_owned(),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
            (
                header::HeaderName::from_static("x-export-limit"),
                MAX_ROWS.to_string(),
            ),
            (
                header::HeaderName::from_static("x-export-rows"),
                rows.len().to_string(),
            ),
        ],
        csv,
    )
        .into_response()
}

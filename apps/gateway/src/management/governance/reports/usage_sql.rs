//! SQL fragments of the cost report's usage aggregates (`super::report`).
use super::*;

/// The JSON object body of the `cost_components` aggregate: v2 and v3 token
/// components, v3 meter components, and the legacy total.
pub(super) fn cost_components_sql() -> Result<String, ApiError> {
    let components = CostComponents::default();
    let component_fields = serde_json::to_value(components)
        .map_err(|_| invalid())?
        .as_object()
        .ok_or_else(invalid)?
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let mut meter_component_fields =
        serde_json::to_value(crate::billing::MeterCostComponents::default())
            .map_err(|_| invalid())?
            .as_object()
            .ok_or_else(invalid)?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
    // Serialized only when nonzero (async video jobs, 0016); always reported.
    meter_component_fields.push("output_video_microusd".to_owned());
    // Token components come from v2 and v3 settlements; meter components only from v3.
    let mut component_sql=component_fields.iter().map(|key|format!("'{key}',coalesce(sum((cost_components->>'{key}')::numeric) FILTER(WHERE accounting_state='settled' AND pricing_version IN(2,3)),0)::text")).chain(meter_component_fields.iter().map(|key|format!("'{key}',coalesce(sum((cost_components->>'{key}')::numeric) FILTER(WHERE accounting_state='settled' AND pricing_version=3),0)::text"))).collect::<Vec<_>>().join(",");
    component_sql.push_str(
        ",'legacy_microusd',coalesce(sum(actual_microusd) FILTER(WHERE pricing_version=1),0)::text",
    );
    Ok(component_sql)
}
/// Observed meter totals; null when nothing was observed (unknown, not zero).
pub(super) fn meter_usage_sql() -> String {
    crate::billing::MeterUsage::KEYS
        .iter()
        .map(|key| format!("'{key}',sum((meter_usage->>'{key}')::numeric)::text"))
        .collect::<Vec<_>>()
        .join(",")
}
/// Attempts whose workload can produce the meter (unless the pinned v3
/// price marks it not applicable).
fn meter_relevant(key: &str) -> String {
    format!(
        "workload_kind IN({}) AND NOT coalesce(price_lines @> '[{{\"meter\":\"{key}\",\"not_applicable\":true}}]'::jsonb,false)",
        meter_workloads(key)
    )
}
/// Attempts whose workload can produce each meter (unless the pinned v3
/// price marks it not applicable). Decimal strings.
pub(super) fn meter_relevant_counts_sql() -> String {
    crate::billing::MeterUsage::KEYS
        .iter()
        .map(|key| {
            format!(
                "'{key}',count(*) FILTER(WHERE {})::text",
                meter_relevant(key)
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}
/// Those relevant attempts where the meter was not observed. A settled
/// failure (a free pre-processing rejection) used nothing. Decimal strings;
/// the meter total is a lower bound whenever its unknown count is nonzero.
pub(super) fn meter_unknown_counts_sql() -> String {
    crate::billing::MeterUsage::KEYS
        .iter()
        .map(|key| {
            format!(
                "'{key}',count(*) FILTER(WHERE {} AND (meter_usage->>'{key}') IS NULL AND NOT (state='failed' AND accounting_state='settled'))::text",
                meter_relevant(key)
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}
pub(super) fn billing_usage_sql() -> String {
    let billing_fields = [
        "total_input_tokens",
        "uncached_input_tokens",
        "cache_read_input_tokens",
        "cache_write_input_tokens",
        "cache_write_default_input_tokens",
        "cache_write_5m_input_tokens",
        "cache_write_1h_input_tokens",
    ];
    billing_fields
        .iter()
        .map(|key| format!("'{key}',sum((billing_usage->>'{key}')::numeric)::text"))
        .collect::<Vec<_>>()
        .join(",")
}

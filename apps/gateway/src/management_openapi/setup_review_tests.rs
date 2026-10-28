//! Slice 3 boundary controls supplement (never replace) executed handler fixtures.
use super::responses::{artifact, validate_value};
use serde_json::json;

#[test]
fn management_openapi_slice3_privacy_and_money_controls() {
    let provider_schema = json!({"$ref":"#/components/schemas/SetupProvider"});
    let mut provider = json!({"id":uuid::Uuid::nil(),"name":"Reference","provider":"openrouter","endpoint":null,"region":null,"enabled":true,"auth_mode":"credential","aws_auth":null,"model_count":1});
    validate_value(artifact(), &provider_schema, &provider).unwrap();
    provider["credential_ref"] = json!("env:PRIVATE_REFERENCE");
    assert!(validate_value(artifact(), &provider_schema, &provider).is_err());

    let window_schema = json!({"$ref":"#/components/schemas/SetupRestrictedBudgetWindow"});
    let mut window = json!({"layer":"local","period":"month","amount_microusd":"9007199254740993","monthly_budget_microusd":"9007199254740993","budget_period":"month","window_start":"2026-10-01T00:00:00Z","window_end":"2026-11-01T00:00:00Z","usage_visible":false,"used_microusd":null,"unresolved_usage":null,"exhausted":null,"settled_microusd":null,"pending_held_microusd":null,"unknown_held_microusd":null,"unknown_cost_requests":null,"remaining_microusd":null,"headroom_status":"restricted"});
    validate_value(artifact(), &window_schema, &window).unwrap();
    window["used_microusd"] = json!("0");
    assert!(validate_value(artifact(), &window_schema, &window).is_err());
    window["used_microusd"] = json!(null);
    window["amount_microusd"] = json!(9007199254740993u64);
    assert!(validate_value(artifact(), &window_schema, &window).is_err());

    // SQL min rates remain integer micro-USD strings: validated token meters
    // always use batches of exactly one million, including pricing v3.
    let catalog_schema = json!({"$ref":"#/components/schemas/SetupWorkspaceCatalogModel"});
    let row = json!({"model_id":uuid::Uuid::nil(),"public_name":"contract","display_name":"Contract","description":null,"protocols":["chat_completions"],"workload":"generation","eligibility":"direct","reason":"Assigned","min_input_microusd_per_million":"9007199254740993","min_output_microusd_per_million":null,"routes":1,"priced_routes":1,"created_at":"2026-10-28T00:00:00Z"});
    validate_value(artifact(), &catalog_schema, &row).unwrap();
    for invalid in [json!(1.5), json!("1.5")] {
        let mut bad = row.clone();
        bad["min_input_microusd_per_million"] = invalid;
        assert!(validate_value(artifact(), &catalog_schema, &bad).is_err());
    }
}

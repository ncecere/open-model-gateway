//! Money-specific adversarial schema controls supplement executed fixtures.
use super::responses::{artifact, validate_value};
use serde_json::{Value, json};
fn check(name: &str, value: &Value) -> bool {
    validate_value(
        artifact(),
        &json!({"$ref":format!("#/components/schemas/{name}")}),
        value,
    )
    .is_ok()
}
#[test]
fn management_openapi_money_unknown_is_not_zero_and_floors_are_not_upper_bounds() {
    let mut estimate = json!({"settled_estimate_microusd":"9007199254740993","held_floor_microusd":"10","holds_upper_bound_microusd":null,"attempts":"2","unresolved_attempts":"1","unbounded_holds":"1","complete":false});
    assert!(check("MoneyProviderEstimate", &estimate));
    for wrong in [json!(0), json!("0"), json!("10")] {
        estimate["holds_upper_bound_microusd"] = wrong;
        assert!(!check("MoneyProviderEstimate", &estimate));
    }
    let summary = json!({"count":"1","known_floor_microusd":null,"unbounded_count":"1","oldest_age_hours":72});
    assert!(check("MoneyHoldTotals", &summary));
    for wrong in [json!(0), json!(-1), json!(0.25), json!("-1"), json!("1.25")] {
        let mut v = summary.clone();
        v["known_floor_microusd"] = wrong;
        assert!(!check("MoneyHoldTotals", &v));
    }
}
#[test]
fn management_openapi_money_personal_totals_cannot_grow_sensitive_fields() {
    let summary =
        json!({"count":"1","known_floor_microusd":"7","unbounded_count":"0","oldest_age_hours":72});
    for name in [
        "workspace_id",
        "owner_user_id",
        "api_key_id",
        "execution_id",
        "name",
        "email",
    ] {
        let mut v = summary.clone();
        v[name] = json!("must-not-leak");
        assert!(!check("MoneyHoldTotals", &v), "sensitive {name}");
    }
    let mut row = json!({"id":null,"name":"Personal workspace keys","spend_microusd":"7","requests":"1","tokens":"2","share":"1","blended_microusd_per_million":null});
    assert!(check("MoneyPlatformRedactedKeyTotals", &row));
    row["id"] = json!(uuid::Uuid::nil());
    assert!(!check("MoneyPlatformRedactedKeyTotals", &row));
}
#[test]
fn management_openapi_money_signed_credits_and_nested_billing_boundaries() {
    let dimensions = json!({"currency":"USD","workspace_id":null,"description":null,"model":null,"service_tier":null,"context_window":null,"cost_type":"credits","token_type":null,"inference_geo":null,"speed":null});
    let good = json!({"line_number":1,"source_page":0,"source_pointer":"/data/0/results/0","bucket_start":"2025-08-01T00:00:00Z","bucket_end":"2025-08-02T00:00:00Z","dimensions":dimensions,"original_amount":"-2.00015","amount_microusd":"-20002","conversion_residual_microusd":"0.5"});
    assert!(check("MoneyProviderLine", &good));
    for wrong in [json!(-20002), json!("-2.25"), Value::Null] {
        let mut v = good.clone();
        v["amount_microusd"] = wrong;
        assert!(!check("MoneyProviderLine", &v));
    }
    let counters = serde_json::to_value(crate::billing::BillingUsage::default()).unwrap();
    assert!(check("MoneyBillingUsageInput", &counters));
    for text in [
        "0",
        "000",
        "9223372036854775807",
        "9007199254740993",
        "9223372036854775808",
        "9999999999999999999",
        "-1",
        "1.0",
    ] {
        let mut v = counters.clone();
        v["total_input_tokens"] = json!(text);
        assert_eq!(
            check("MoneyBillingUsageInput", &v),
            serde_json::from_value::<crate::billing::BillingUsage>(v).is_ok(),
            "{text}"
        );
    }
    let mut omitted = counters.clone();
    omitted
        .as_object_mut()
        .unwrap()
        .remove("cache_read_input_tokens");
    assert!(!check("MoneyBillingUsageInput", &omitted));
}

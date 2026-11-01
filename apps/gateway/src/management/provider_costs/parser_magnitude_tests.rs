//! DB-free: `cents()` against the artifact's amount and residual schemas at
//! the i128 magnitude boundary. The schema's `not` pattern encodes the whole
//! part limit; the handler additionally refuses the boundary value when its
//! first four fractional digits exceed 5727 or the rounding carries past
//! i128::MAX. Failure messages name only the case label, never values.
use super::cents;
use crate::management::openapi::responses::{artifact, validate_value};
use serde_json::{Value, json};

/// i128::MAX / 10_000: the largest whole part cents() converts (35 digits).
const B: &str = "17014118346046923173168730371588410";
const B_PLUS_ONE: &str = "17014118346046923173168730371588411";

fn component(name: &str) -> &'static Value {
    &artifact()["components"]["schemas"][name]
}
/// Every location of the contract that states the amount rule.
fn amount_schemas() -> [(&'static str, &'static Value); 3] {
    let item = component("MoneyProviderItem");
    [
        (
            "MoneyProviderItem.amount",
            &item["oneOf"][0]["properties"]["amount"],
        ),
        (
            "MoneyProviderItem positional amount",
            &item["oneOf"][1]["prefixItems"][0],
        ),
        (
            "MoneyProviderLine.original_amount",
            &component("MoneyProviderLine")["properties"]["original_amount"],
        ),
    ]
}
fn schema_accepts(schema: &Value, text: &str) -> bool {
    validate_value(artifact(), schema, &json!(text)).is_ok()
}

/// (label, text, schema verdict, handler verdict). The two verdicts are both
/// stated, so a known divergence breaks the test when either side changes.
fn cases() -> Vec<(String, String, bool, bool)> {
    let nines = |n: usize| "9".repeat(n);
    let mut out: Vec<(String, String, bool, bool)> = Vec::new();
    let mut add = |label: &str, text: String, schema: bool, handler: bool| {
        // Signed variants: the schema and the handler both take a leading '-'.
        out.push((
            format!("{label} (negative)"),
            format!("-{text}"),
            schema,
            handler,
        ));
        out.push((label.to_owned(), text, schema, handler));
    };
    add("33 whole digits", nines(33), true, true);
    add("34 whole digits", nines(34), true, true);
    add("35 whole digits, above the limit", nines(35), false, false);
    add(
        "35 whole digits, below the limit",
        format!("1{}", "0".repeat(34)),
        true,
        true,
    );
    add(
        "36 whole digits",
        format!("1{}", "0".repeat(35)),
        false,
        false,
    );
    add("the limit", B.to_owned(), true, true);
    add("the limit plus one", B_PLUS_ONE.to_owned(), false, false);
    add(
        "the limit with fraction .5727",
        format!("{B}.5727"),
        true,
        true,
    );
    // Documented divergence: the schema accepts, the handler refuses.
    add(
        "the limit with fraction .5728",
        format!("{B}.5728"),
        true,
        false,
    );
    add(
        "the limit with fraction .57274",
        format!("{B}.57274"),
        true,
        true,
    );
    add(
        "the limit with a tie rounding up past i128::MAX",
        format!("{B}.57275"),
        true,
        false,
    );
    add(
        "the limit plus one with a fraction",
        format!("{B_PLUS_ONE}.0"),
        false,
        false,
    );
    add(
        "leading zeros, 34 digits",
        format!("{}{}", "0".repeat(10), nines(34)),
        true,
        true,
    );
    add(
        "leading zeros, the limit",
        format!("{}{B}", "0".repeat(20)),
        true,
        true,
    );
    add(
        "leading zeros, 35 digits above the limit",
        format!("00{}", nines(35)),
        false,
        false,
    );
    add(
        "leading zeros, the limit plus one",
        format!("{}{B_PLUS_ONE}", "0".repeat(20)),
        false,
        false,
    );
    add("zeros only", "0".repeat(40), true, true);
    out
}

#[test]
fn management_openapi_money_amount_magnitude_matches_cents() {
    for (label, text, schema, handler) in cases() {
        assert_eq!(cents(&text).is_ok(), handler, "handler verdict: {label}");
        for (location, definition) in amount_schemas() {
            assert_eq!(
                schema_accepts(definition, &text),
                schema,
                "schema verdict at {location}: {label}"
            );
        }
        // Wherever the handler accepts, the schema must too (no value the
        // handler takes may be undocumented).
        if handler {
            assert!(schema, "handler accepts but the schema refuses: {label}");
        }
    }
    // The 64-character limit counts the sign and the zero padding: 29 zeros
    // before the limit is 64 characters, one more is 65.
    let at_limit = format!("{}{B}", "0".repeat(29));
    let over = format!("{}{B}", "0".repeat(30));
    assert!(cents(&at_limit).is_ok() && cents(&over).is_err());
    for (location, definition) in amount_schemas() {
        assert!(
            schema_accepts(definition, &at_limit),
            "{location}: 64 characters"
        );
        assert!(
            !schema_accepts(definition, &over),
            "{location}: 65 characters"
        );
    }
}

#[test]
fn management_openapi_money_amount_shapes_the_handler_refuses_are_refused_by_the_schema() {
    for text in [
        "",
        "-",
        "1.",
        ".5",
        "1e3",
        "+1",
        " 1",
        "1 ",
        "--1",
        "1.0000000000000000001",
        "NaN",
        "0x10",
    ] {
        assert!(cents(text).is_err(), "handler: shape {text:?}");
        for (location, definition) in amount_schemas() {
            assert!(
                !schema_accepts(definition, text),
                "{location}: shape {text:?}"
            );
        }
    }
}

/// The residual is "0" or a signed decimal fraction below one without trailing
/// zeros. Compare the producer with the schema, then pin negative controls.
#[test]
fn management_openapi_money_residual_matches_cents_and_refuses_non_residuals() {
    let residual = &component("MoneyProviderLine")["properties"]["conversion_residual_microusd"];
    for (text, expected) in [
        ("0", "0"),
        ("1", "0"),
        ("-1.25", "0"),
        ("0.00005", "0.5"),
        ("0.00015", "-0.5"),
        ("-0.00005", "-0.5"),
        ("-0.00015", "0.5"),
        ("123.78912", "0.2"),
        ("1.000049", "0.49"),
        ("-1.000049", "-0.49"),
        ("1.0000501", "-0.499"),
        ("1.00000000000001", "0.0000000001"),
    ] {
        let (_, produced) = cents(text).unwrap();
        assert_eq!(produced, expected, "producer: residual of case {expected}");
        assert!(
            schema_accepts(residual, &produced),
            "schema refuses a produced residual: case {expected}"
        );
    }
    for accepted in ["0", "0.5", "-0.25"] {
        assert!(
            schema_accepts(residual, accepted),
            "accepted residual {accepted}"
        );
    }
    for refused in [
        "1.5", "0.50", "-0", "1", "-1", "0.0", "", "00.5", "0.", ".5", "+0.5",
    ] {
        assert!(
            !schema_accepts(residual, refused),
            "refused residual {refused:?}"
        );
    }
}

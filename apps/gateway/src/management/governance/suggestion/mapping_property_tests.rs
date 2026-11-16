//! Property tests of the OpenRouter catalog price mapper (#593): `microusd`
//! (a decimal price string per unit to micro-USD per batch) and `map` /
//! `map_with_endpoints` (one catalog model to a pricing-v3 draft).
//!
//! The contract these tests state, read from the code:
//!
//! * `microusd(price, units)` is `price x units x 10^6`, rounded UP to a whole
//!   micro-USD, returned with whether it was exact. It is `None` (a refusal,
//!   never a zero) for: a leading `-`, an empty or over-64-byte string, any
//!   character other than ASCII digits, one `.`, and one `e`/`E` exponent in
//!   -40..=40 (so `NaN`, `inf`, `+1`, `0x10`, `1_0`, fullwidth digits and a bare
//!   `.` are refused), and for any amount that does not fit an `i64` or whose
//!   intermediate `u128` arithmetic would overflow. Exponent form is accepted.
//! * `map` never produces a price from a missing, unparseable or non-string
//!   rate: such a line has a `null` amount, `needs_review: true` and an
//!   explicit `unknown` draft line. The one deliberate zero is a missing
//!   per-request fee (`absent pricing.request`); every other `"0"` traces back
//!   to an explicit zero rate in the catalog.
//! * A rate that does not divide into whole micro-USD is rounded up, never
//!   down, and flagged for review.
//!
//! The reference below is written independently of `microusd`: decimal digit
//! vectors, no `u128`, no `pow`.
use super::*;
use crate::proptest_support_tests::{apply_all, arb_json, config, hostile, mutation};
use proptest::prelude::*;

// ---- independent reference -------------------------------------------------

/// `(digits, scale)`: the value is `digits x 10^scale`; `None` is a syntax
/// refusal. Written from the grammar in the module comment.
fn reference_parse(raw: &str) -> Option<(Vec<u8>, i32)> {
    let t = raw.trim();
    if t.is_empty() || t.starts_with('-') || t.len() > 64 {
        return None;
    }
    let (mantissa, exponent) = match t.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (t, None),
    };
    let exp = match exponent {
        None => 0,
        Some(e) => {
            let (negative, digits) = match e.as_bytes().first() {
                Some(b'-') => (true, &e[1..]),
                Some(b'+') => (false, &e[1..]),
                _ => (false, e),
            };
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let digits = digits.trim_start_matches('0');
            if digits.len() > 2 {
                return None;
            }
            let magnitude: i32 = if digits.is_empty() {
                0
            } else {
                digits.parse().unwrap()
            };
            let value = if negative { -magnitude } else { magnitude };
            if !(-40..=40).contains(&value) {
                return None;
            }
            value
        }
    };
    let (whole, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if (whole.is_empty() && frac.is_empty())
        || !whole
            .bytes()
            .chain(frac.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits = whole
        .bytes()
        .chain(frac.bytes())
        .map(|b| b - b'0')
        .collect();
    Some((digits, exp - frac.len() as i32))
}

/// Big-endian decimal digits without leading zeros (empty is zero).
fn strip(mut digits: Vec<u8>) -> Vec<u8> {
    let zeros = digits.iter().take_while(|d| **d == 0).count();
    digits.drain(..zeros);
    digits
}

fn multiply(digits: &[u8], m: u64) -> Vec<u8> {
    let mut out = vec![];
    let mut carry: u128 = 0;
    for d in digits.iter().rev() {
        let cur = u128::from(*d) * u128::from(m) + carry;
        out.push((cur % 10) as u8);
        carry = cur / 10;
    }
    while carry > 0 {
        out.push((carry % 10) as u8);
        carry /= 10;
    }
    out.reverse();
    strip(out)
}

fn increment(digits: Vec<u8>) -> Vec<u8> {
    let mut out = digits;
    for d in out.iter_mut().rev() {
        if *d == 9 {
            *d = 0;
        } else {
            *d += 1;
            return out;
        }
    }
    out.insert(0, 1);
    out
}

struct Reference {
    /// The micro-USD amount rounded up, `None` when it does not fit an `i64`.
    amount: Option<i64>,
    exact: bool,
    /// The production arithmetic is `u128`: a value this large may be refused
    /// for overflow although it would fit an `i64`.
    wide: bool,
    zero: bool,
}

fn reference(raw: &str, units: u64) -> Option<Reference> {
    let (digits, scale) = reference_parse(raw)?;
    let value = strip(digits);
    let product = multiply(&value, units);
    let k = scale + 6;
    let (whole, exact) = if k >= 0 {
        let mut v = product.clone();
        if !v.is_empty() {
            v.extend(std::iter::repeat_n(0, k as usize));
        }
        (v, true)
    } else {
        let cut = k.unsigned_abs() as usize;
        if cut >= product.len() {
            (vec![], product.is_empty())
        } else {
            let (head, tail) = product.split_at(product.len() - cut);
            (head.to_vec(), tail.iter().all(|d| *d == 0))
        }
    };
    let rounded = if exact { whole } else { increment(whole) };
    let text: String = if rounded.is_empty() {
        "0".into()
    } else {
        rounded.iter().map(|d| char::from(b'0' + d)).collect()
    };
    let wide = value.len() > 38
        || product.len() > 38
        || k.unsigned_abs() > 38
        || (k > 0 && !product.is_empty() && product.len() + k as usize > 38);
    Some(Reference {
        amount: text.parse::<i64>().ok(),
        exact,
        wide,
        zero: value.is_empty(),
    })
}

// ---- generators ------------------------------------------------------------

fn ws() -> impl Strategy<Value = &'static str> {
    prop_oneof![8 => Just(""), 1 => Just(" "), 1 => Just("\t")]
}

/// Strings shaped like prices, hostile ones and plain noise.
fn decimal_text() -> impl Strategy<Value = String> {
    let structured = (
        ws(),
        "[0-9]{0,22}",
        prop::option::of("[0-9]{0,22}"),
        prop::option::of((
            prop::sample::select(vec!['e', 'E']),
            prop::sample::select(vec!["", "+", "-"]),
            "[0-9]{0,3}",
        )),
        ws(),
    )
        .prop_map(|(lead, whole, frac, exp, trail)| {
            let mut s = format!("{lead}{whole}");
            if let Some(frac) = frac {
                s.push('.');
                s.push_str(&frac);
            }
            if let Some((e, sign, digits)) = exp {
                s.push(e);
                s.push_str(sign);
                s.push_str(&digits);
            }
            s.push_str(trail);
            s
        });
    let fixed = prop::sample::select(vec![
        "NaN",
        "nan",
        "inf",
        "-inf",
        "Infinity",
        "-1",
        "-0",
        "-0.5",
        "+1",
        "1,5",
        "1_0",
        "0x1p3",
        "١٢٣",
        "１２３",
        ".",
        "e5",
        "1e",
        "1e+",
        "1e5.5",
        "1.2.3",
        "--1",
        "",
        "   ",
        "0",
        "0.0",
        "1e40",
        "1e41",
        "1e-40",
        "1e-41",
        "0.000001",
        "0.0000001",
        "9223372036854775807",
        "9223372036854775808",
    ])
    .prop_map(str::to_owned);
    prop_oneof![
        6 => structured,
        3 => "[0-9eE.+\\- \t]{0,70}",
        1 => fixed,
        1 => "[0-9]{60,90}",
        1 => "0\\.0{55,70}1",
    ]
}

fn units() -> impl Strategy<Value = u64> {
    prop_oneof![
        prop::sample::select(vec![1_u64, 60, 1_000, 3_600, 60_000, 1_000_000]),
        (1..=u64::MAX),
        (1..=10_000_000_000_u64),
    ]
}

/// A price string as OpenRouter prints them: plain decimals, sometimes `e`.
fn valid_price() -> impl Strategy<Value = String> {
    prop_oneof![
        8 => ("[0-9]{1,2}", "[0-9]{0,12}").prop_map(|(w, f)| format!("{w}.{f}")),
        2 => ("[0-9]{1,2}", "[0-9]{0,6}", -9..=2_i32).prop_map(|(w, f, e)| format!("{w}.{f}e{e}")),
        1 => Just("0".to_owned()),
        1 => Just("0.0000001".to_owned()),
    ]
}

fn price_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        8 => valid_price().prop_map(Value::String),
        3 => decimal_text().prop_map(Value::String),
        2 => hostile(),
        1 => Just(Value::Null),
    ]
}

const PRICING_KEYS: [&str; 14] = [
    "prompt",
    "completion",
    "input_cache_read",
    "input_cache_write",
    "input_cache_write_1h",
    "request",
    "internal_reasoning",
    "web_search",
    "image",
    "audio",
    "audio_output",
    "input_audio_cache",
    "image_output",
    "image_token",
];

fn threshold() -> impl Strategy<Value = Value> {
    prop_oneof![
        8 => (1..=300_000_u64).prop_map(Value::from),
        2 => hostile(),
    ]
}

fn override_entry() -> impl Strategy<Value = Value> {
    (
        prop::option::of(threshold()),
        prop::collection::vec(prop::option::of(price_value()), 6),
        prop::option::of(price_value()),
        any::<bool>(),
    )
        .prop_map(|(min, prices, other, utc)| {
            let mut o = serde_json::Map::new();
            if let Some(min) = min {
                o.insert("min_prompt_tokens".into(), min);
            }
            for (key, v) in [
                "prompt",
                "completion",
                "input_cache_read",
                "input_cache_write",
                "input_cache_write_1h",
                "web_search",
            ]
            .into_iter()
            .zip(prices)
            {
                if let Some(v) = v {
                    o.insert(key.into(), v);
                }
            }
            if let Some(v) = other {
                o.insert("some unknown/key!".into(), v);
            }
            if utc {
                o.insert("utc_start_hour".into(), json!(3));
            }
            Value::Object(o)
        })
}

fn pricing_object() -> impl Strategy<Value = Value> {
    (
        prop::collection::vec(prop::option::of(price_value()), PRICING_KEYS.len()),
        prop::option::of(prop_oneof![
            Just(json!(0)),
            Just(json!(0.0)),
            Just(json!("0")),
            Just(json!("0.0")),
            Just(json!(0.1)),
            Just(json!("0.25")),
            Just(json!("abc")),
            hostile(),
        ]),
        prop::option::of(prop::collection::vec(override_entry(), 0..4)),
    )
        .prop_map(|(prices, discount, overrides)| {
            let mut p = serde_json::Map::new();
            for (key, v) in PRICING_KEYS.into_iter().zip(prices) {
                if let Some(v) = v {
                    p.insert(key.into(), v);
                }
            }
            if let Some(d) = discount {
                p.insert("discount".into(), d);
            }
            if let Some(o) = overrides {
                p.insert("overrides".into(), Value::Array(o));
            }
            Value::Object(p)
        })
}

fn limit() -> impl Strategy<Value = Option<Value>> {
    prop::option::of(prop_oneof![
        8 => (1..=3_000_000_u64).prop_map(Value::from),
        2 => hostile(),
    ])
}

/// A catalog entry with the documented fields, then a few mutations of it.
fn catalog_model() -> impl Strategy<Value = Value> {
    (
        pricing_object(),
        limit(),
        limit(),
        prop::option::of(hostile()),
        prop::collection::vec(mutation(&["model"]), 0..3),
    )
        .prop_map(|(pricing, context, completion, provider, mutations)| {
            let mut model = json!({"id": "vendor/model-1", "name": "Model", "pricing": pricing,
                "top_provider": {"is_moderated": false}, "architecture": {"modality": "text->text"}});
            if let Some(c) = context {
                model["context_length"] = c;
            }
            if let Some(c) = completion {
                model["top_provider"]["max_completion_tokens"] = c;
            }
            if let Some(p) = provider {
                model["top_provider"] = p;
            }
            let mut events = vec![model];
            apply_all(&mut events, &mutations);
            events.pop().unwrap_or(Value::Null)
        })
}

fn any_model() -> impl Strategy<Value = Value> {
    prop_oneof![9 => catalog_model(), 1 => arb_json()]
}

fn workload() -> impl Strategy<Value = WorkloadKind> {
    prop::sample::select(vec![
        WorkloadKind::Generation,
        WorkloadKind::Embeddings,
        WorkloadKind::Images,
        WorkloadKind::AudioTranscriptions,
        WorkloadKind::AudioSpeech,
        WorkloadKind::Rerank,
        WorkloadKind::Systemone,
        WorkloadKind::Realtime,
        WorkloadKind::Videos,
        WorkloadKind::Batches,
    ])
}

fn image_cost() -> impl Strategy<Value = Value> {
    prop_oneof![
        6 => valid_price().prop_map(Value::String),
        2 => (0.0..100.0_f64).prop_map(|f| json!(f)),
        2 => decimal_text().prop_map(Value::String),
        1 => hostile(),
    ]
}

fn image_entry() -> impl Strategy<Value = Value> {
    (
        prop::sample::select(vec![
            "output_image",
            "output_image",
            "output_image",
            "input_image",
        ]),
        prop::sample::select(vec!["image", "image", "image", "token"]),
        image_cost(),
        prop::option::of(prop::sample::select(vec![
            "1024x1024",
            "hd",
            "low",
            "",
            "Has Space",
            "UPPER",
        ])),
    )
        .prop_map(|(billable, unit, cost, variant)| {
            let mut e = json!({"billable": billable, "unit": unit, "cost_usd": cost});
            if let Some(v) = variant {
                e["variant"] = json!(v);
            }
            e
        })
}

/// An image-endpoints body: well formed (the strict shape the code accepts).
fn image_body() -> impl Strategy<Value = Value> {
    (
        prop::collection::vec(
            (
                prop::collection::vec(image_entry(), 0..4),
                prop::option::of(prop_oneof![Just(json!(0)), Just(json!("0.1")), hostile()]),
            ),
            0..4,
        ),
        any::<bool>(),
    )
        .prop_map(|(endpoints, wrapped)| {
            let list: Vec<Value> = endpoints
                .into_iter()
                .map(|(pricing, discount)| {
                    let mut e = json!({"pricing": pricing});
                    if let Some(d) = discount {
                        e["discount"] = d;
                    }
                    e
                })
                .collect();
            let body = json!({"endpoints": list});
            if wrapped { json!({"data": body}) } else { body }
        })
}

// ---- checks ----------------------------------------------------------------

fn canonical_amount(s: &str) -> bool {
    (s == "0" || (!s.starts_with('0') && s.bytes().all(|b| b.is_ascii_digit())))
        && !s.is_empty()
        && s.parse::<i64>().is_ok()
}

fn meter_of(name: &str) -> Option<Meter> {
    Meter::ALL.into_iter().find(|m| m.as_str() == name)
}

fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn lines(out: &Value) -> &Vec<Value> {
    out["lines"].as_array().expect("lines is an array")
}

fn is_zero_rate(v: &Value) -> bool {
    v.as_str()
        .and_then(reference_parse)
        .is_some_and(|(digits, _)| digits.iter().all(|d| *d == 0))
}

/// Shape and honesty invariants of any `map` output. `catalog` is the model
/// the lines were mapped from, for tracing zero amounts; `None` when the
/// mapped model is not known to the caller.
fn check_output(
    out: &Value,
    catalog: Option<&Value>,
    endpoint_may_force_review: bool,
) -> Result<(), TestCaseError> {
    let all = lines(out);
    let mut seen = std::collections::BTreeSet::new();
    for l in all {
        let meter = meter_of(str_of(&l["meter"]));
        prop_assert!(meter.is_some(), "unknown meter in {l}");
        let meter = meter.unwrap();
        prop_assert!(
            l["needs_review"].is_boolean() && l["source"].is_string(),
            "{l}"
        );
        prop_assert!(
            seen.insert((
                l["meter"].to_string(),
                l["variant"].to_string(),
                l["min_prompt_tokens"].to_string()
            )),
            "two lines for one meter/variant/threshold: {l}"
        );
        if l["not_applicable"] == true {
            prop_assert!(l.get("microusd_per_batch").is_none(), "{l}");
            continue;
        }
        match &l["microusd_per_batch"] {
            Value::Null => prop_assert!(l["needs_review"] == true, "unpriced but unflagged: {l}"),
            Value::String(a) => {
                prop_assert!(canonical_amount(a), "not a micro-USD integer: {l}");
                let batch = l["batch"].as_u64().unwrap_or(0);
                prop_assert!(meter.batches().contains(&batch), "batch {batch} for {l}");
                if let Some(min) = l.get("min_prompt_tokens") {
                    prop_assert!(
                        min.as_u64()
                            .is_some_and(|n| (1..=i32::MAX as u64).contains(&n)),
                        "{l}"
                    );
                }
                if let (true, Some(model)) = (a == "0", catalog) {
                    let source = str_of(&l["source"]);
                    let traced = if source == "absent pricing.request" {
                        l["meter"] == "requests" && model["pricing"]["request"].is_null()
                    } else if source == "images endpoints pricing" {
                        // Per-image prices have their own oracle (see the image test).
                        true
                    } else if let Some(key) = source.strip_prefix("pricing.overrides.") {
                        model["pricing"]["overrides"]
                            .as_array()
                            .is_some_and(|o| o.iter().any(|e| is_zero_rate(&e[key])))
                    } else if let Some(key) = source.strip_prefix("pricing.") {
                        is_zero_rate(&model["pricing"][key])
                    } else {
                        false
                    };
                    prop_assert!(traced, "a free price with no zero rate behind it: {l}");
                }
            }
            other => prop_assert!(false, "amount is {other}"),
        }
    }
    // The draft.
    let draft = out["draft"]["price_lines"].as_array().expect("draft lines");
    for p in draft {
        let n = [
            p["not_applicable"] == true,
            p["unknown"] == true,
            p["microusd_per_batch"].is_string(),
        ];
        prop_assert!(
            n.iter().filter(|b| **b).count() == 1,
            "ambiguous draft line {p}"
        );
        if let Some(a) = p["microusd_per_batch"].as_str() {
            prop_assert!(canonical_amount(a), "{p}");
            for key in ["needs_review", "source", "note"] {
                prop_assert!(p.get(key).is_none(), "{key} leaked into {p}");
            }
        }
    }
    for l in all {
        if l["not_applicable"] == true || !l["microusd_per_batch"].is_null() {
            continue;
        }
        let same = |p: &&Value| {
            p["meter"] == l["meter"]
                && p["variant"] == l["variant"]
                && p["min_prompt_tokens"] == l["min_prompt_tokens"]
        };
        prop_assert!(
            !draft
                .iter()
                .filter(same)
                .any(|p| p["microusd_per_batch"].is_string()),
            "an unpriced line was priced in the draft: {l}"
        );
        prop_assert!(
            draft.iter().any(|p| p["meter"] == l["meter"]),
            "an unpriced meter with no draft line: {l}"
        );
    }
    // The draft is valid, or says it is not.
    let parsed = serde_json::from_value::<PriceLines>(Value::Array(draft.clone()));
    let warned = out["warnings"]
        .as_array()
        .expect("warnings")
        .iter()
        .any(|w| str_of(w).contains("failed validation"));
    prop_assert_eq!(parsed.is_ok(), out["display_lines"].is_array());
    prop_assert_eq!(parsed.is_err(), warned);
    if parsed.is_ok() {
        for m in Meter::ALL {
            prop_assert!(
                draft.iter().any(|p| p["meter"] == m.as_str()),
                "meter {} is not stated in a valid draft",
                m.as_str()
            );
        }
    }
    let review = all.iter().any(|l| l["needs_review"] == true) || parsed.is_err();
    if endpoint_may_force_review {
        // `map_with_endpoints` also flags a draft priced below another endpoint.
        prop_assert!(!review || out["needs_review"] == true);
    } else {
        prop_assert_eq!(out["needs_review"] == true, review);
    }
    for w in out["warnings"].as_array().unwrap() {
        let w = str_of(w);
        prop_assert!(
            !w.is_empty() && w.len() <= 600,
            "warning length {}",
            w.len()
        );
        prop_assert!(
            !w.chars().any(char::is_control),
            "control character in {w:?}"
        );
    }
    // Ceilings are conservative and never above the model's own limits.
    let input = out["ceilings"]["input_token_limit"].as_u64().unwrap();
    let output = out["ceilings"]["output_token_limit"].as_u64().unwrap();
    prop_assert!(input <= DEFAULT_INPUT_CEILING && output <= DEFAULT_OUTPUT_CEILING);
    prop_assert_eq!(
        &out["draft"]["input_token_limit"],
        &out["ceilings"]["input_token_limit"]
    );
    prop_assert_eq!(
        &out["draft"]["output_token_limit"],
        &out["ceilings"]["output_token_limit"]
    );
    if let Some(model) = catalog {
        let valid = |v: &Value| v.as_u64().filter(|n| (1..=i32::MAX as u64).contains(n));
        let context = valid(&model["context_length"]);
        let completion = valid(&model["top_provider"]["max_completion_tokens"]);
        prop_assert_eq!(out["ceilings"]["context_length"].as_u64(), context);
        prop_assert_eq!(
            out["ceilings"]["max_completion_tokens"].as_u64(),
            completion
        );
        if let (Some(c), true) = (context, input > 0) {
            prop_assert!(input <= c);
        }
        if let (true, Some(c)) = (output > 0, completion.or(context)) {
            prop_assert!(output <= c);
        }
    }
    Ok(())
}

/// The warnings the mapper owes for catalog prices it cannot represent.
fn check_warnings(out: &Value, model: &Value, workload: WorkloadKind) -> Result<(), TestCaseError> {
    let warnings: Vec<&str> = out["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(str_of)
        .collect();
    let count = |needle: &str| warnings.iter().filter(|w| w.contains(needle)).count();
    for (key, what) in [
        ("web_search", "Web search"),
        ("image", "Image input"),
        ("audio", "Audio input token"),
        ("audio_output", "Audio output token"),
        ("input_audio_cache", "Cached audio input"),
    ] {
        let expected =
            usize::from(!model["pricing"][key].is_null() && workload != WorkloadKind::Images);
        prop_assert_eq!(
            count(&format!("{what} pricing has no meter for this workload")),
            expected,
            "{}",
            key
        );
    }
    let discount = &model["pricing"]["discount"];
    let discounted = match discount {
        Value::Null => Some(false),
        Value::Number(n) => Some(n.as_f64().is_none_or(|f| f != 0.0)),
        Value::String(s) => match reference(s, 1) {
            Some(r) if !r.wide => Some(!(r.zero && r.exact)),
            None => Some(true),
            Some(_) => None,
        },
        _ => Some(true),
    };
    if let Some(expected) = discounted {
        prop_assert_eq!(
            count("A catalog discount was not applied"),
            usize::from(expected)
        );
    }
    match workload {
        WorkloadKind::Realtime => prop_assert_eq!(count("realtime sessions"), 1),
        WorkloadKind::Videos | WorkloadKind::Batches => {
            prop_assert_eq!(count("video and batch jobs"), 1)
        }
        _ => {}
    }
    Ok(())
}

/// Generation token rates: each key's line is the reference amount, rounded
/// up and flagged when inexact, or an explicit unknown. Never a free price.
fn check_generation_prices(out: &Value, model: &Value) -> Result<(), TestCaseError> {
    let table = [
        ("prompt", "input_tokens"),
        ("completion", "output_tokens"),
        ("input_cache_read", "cache_read_tokens"),
        ("input_cache_write", "cache_write_tokens"),
        ("input_cache_write", "cache_write_5m_tokens"),
        ("input_cache_write_1h", "cache_write_1h_tokens"),
    ];
    for (key, meter) in table {
        let line = lines(out)
            .iter()
            .find(|l| l["meter"] == meter && l["min_prompt_tokens"].is_null())
            .unwrap_or_else(|| panic!("no base line for {meter}"));
        let rate = &model["pricing"][key];
        let always_priced = matches!(meter, "input_tokens" | "output_tokens");
        if rate.is_null() && !always_priced {
            prop_assert!(
                line["not_applicable"] == true && line["needs_review"] == true,
                "{line}"
            );
            continue;
        }
        prop_assert!(line["not_applicable"] != true, "{line}");
        let amount = line["microusd_per_batch"].as_str();
        let expected = rate.as_str().and_then(|s| reference(s, 1_000_000));
        match (amount, expected) {
            (Some(a), Some(r)) => {
                prop_assert_eq!(
                    Some(a.parse::<i64>().unwrap()),
                    r.amount,
                    "{} for {}",
                    line,
                    rate
                );
                prop_assert_eq!(line["batch"].as_u64(), Some(1_000_000));
                if !r.exact {
                    prop_assert!(
                        line["needs_review"] == true,
                        "rounded up, unflagged: {line}"
                    );
                    prop_assert!(str_of(&line["note"]).contains("rounded up"), "{line}");
                } else if meter != "output_tokens" {
                    prop_assert!(line["needs_review"] == false, "{line}");
                }
            }
            (Some(_), None) => prop_assert!(false, "priced from an unusable rate {rate}: {line}"),
            (None, Some(r)) => {
                prop_assert!(
                    r.amount.is_none() || r.wide,
                    "refused a good rate {rate}: {line}"
                )
            }
            (None, None) => {}
        }
        if amount.is_none() {
            prop_assert!(line["needs_review"] == true, "{line}");
            // The draft states the meter `unknown`, or carries only a prompt-size
            // tier for it (see `a_tier_without_a_base_price_is_stated_unknown`);
            // `check_output` already requires some draft line and the review flag.
            let draft = out["draft"]["price_lines"].as_array().unwrap();
            prop_assert!(
                draft
                    .iter()
                    .any(|p| p["meter"] == meter && p["unknown"] == true)
                    || draft
                        .iter()
                        .any(|p| p["meter"] == meter && !p["min_prompt_tokens"].is_null()),
                "no unknown draft line for {meter}"
            );
        }
    }
    // Prompt-size tiers come from a string rate on a valid threshold.
    for l in lines(out)
        .iter()
        .filter(|l| !l["min_prompt_tokens"].is_null())
    {
        let (Some(source), Some(a)) = (
            str_of(&l["source"]).strip_prefix("pricing.overrides."),
            l["microusd_per_batch"].as_str(),
        ) else {
            continue;
        };
        let backed = model["pricing"]["overrides"]
            .as_array()
            .is_some_and(|list| {
                list.iter().any(|o| {
                    o["min_prompt_tokens"] == l["min_prompt_tokens"]
                        && o[source]
                            .as_str()
                            .and_then(|s| reference(s, 1_000_000))
                            .is_some_and(|r| r.amount == a.parse().ok())
                })
            });
        prop_assert!(backed, "a tier line with no rate behind it: {l}");
    }
    Ok(())
}

// ---- properties ------------------------------------------------------------

proptest! {
    #![proptest_config(config(4000))]

    /// `microusd` equals the independent decimal reference: the same amount,
    /// rounded up, with the same exactness; a syntax the grammar refuses is
    /// `None`; `None` for a well-formed price only when the amount does not fit
    /// an `i64` or the `u128` arithmetic could overflow. Never a negative.
    #[test]
    fn microusd_matches_the_decimal_reference(raw in decimal_text(), units in units()) {
        let got = microusd(&raw, units);
        match (reference(&raw, units), got) {
            (None, got) => prop_assert_eq!(got, None, "accepted {:?}", raw),
            (Some(r), Some((amount, exact))) => {
                prop_assert!(amount >= 0);
                prop_assert_eq!(r.amount, Some(amount), "{:?} x {}", raw, units);
                prop_assert_eq!(r.exact, exact, "{:?} x {}", raw, units);
            }
            (Some(r), None) => prop_assert!(
                r.amount.is_none() || r.wide,
                "refused {:?} x {} although {:?} fits",
                raw,
                units,
                r.amount
            ),
        }
    }

    /// Exact decimal strings (no exponent) of the size OpenRouter prints give
    /// exactly `digits x units / 10^(fraction digits - 6)` with no rounding
    /// surprise: the amount times 10^(frac-6) is never below the true price.
    #[test]
    fn rounding_is_always_up_and_never_more_than_one_micro_usd(
        whole in 0_u64..1_000,
        frac in "[0-9]{0,15}",
        units in prop::sample::select(vec![1_u64, 1_000, 1_000_000]),
    ) {
        let text = format!("{whole}.{frac}");
        let (amount, exact) = microusd(&text, units).expect("a plain decimal is accepted");
        // True price x units x 10^6 as a ratio n / d with d = 10^frac.len().
        let n = (u128::from(whole) * 10_u128.pow(frac.len() as u32)
            + frac.parse::<u128>().unwrap_or(0))
            * u128::from(units)
            * 1_000_000;
        let d = 10_u128.pow(frac.len() as u32);
        prop_assert!(u128::from(amount as u64) * d >= n, "rounded down: {text} x {units}");
        prop_assert!(u128::from(amount as u64) * d < n + d, "more than one micro-USD up");
        prop_assert_eq!(exact, n.is_multiple_of(d));
    }
}

proptest! {
    #![proptest_config(config(1500))]

    /// `map` never panics for any catalog entry (including non-objects),
    /// workload and image body; it is deterministic (same output, same warnings
    /// in the same order); every amount is a non-negative integer micro-USD
    /// string in an allowed batch; an unpriced meter is flagged and stated
    /// `unknown`; a free price always traces to an explicit zero rate; the draft
    /// either validates or says it did not; ceilings stay conservative.
    #[test]
    fn map_output_is_well_formed_for_any_catalog_entry(
        model in any_model(),
        workload in workload(),
        image in prop_oneof![Just(None), image_body().prop_map(|v| Some(v.to_string().into_bytes()))],
    ) {
        let out = map(&model, workload, image.as_deref());
        prop_assert_eq!(&out, &map(&model, workload, image.as_deref()), "not deterministic");
        prop_assert_eq!(out["workload"].as_str(), Some(workload.as_str()));
        check_output(&out, Some(&model), false)?;
        check_warnings(&out, &model, workload)?;
    }

    /// Generation rates are exactly the reference amounts, rounded up and
    /// flagged when inexact; a missing, unparseable, negative, exponent-overflow
    /// or non-string rate is never mapped to zero.
    #[test]
    fn map_generation_rates_match_the_reference(model in any_model()) {
        let out = map(&model, WorkloadKind::Generation, None);
        check_generation_prices(&out, &model)?;
    }

    /// A rate that is an unusable price string is never silently free, at any
    /// workload: whatever line carries it is unpriced and flagged, and the
    /// draft states the meter `unknown` instead of `0`.
    #[test]
    fn an_unusable_rate_is_unknown_not_free(
        rate in prop_oneof![
            decimal_text()
                .prop_filter("unusable", |s| reference(s, 1).is_none_or(|r| r.amount.is_none()))
                .prop_map(Value::String),
            hostile().prop_filter("not a string", |v| !v.is_string() && !v.is_null()),
        ],
        workload in workload(),
    ) {
        let model = json!({"id": "v/m", "pricing": {"prompt": rate, "completion": rate, "request": rate}});
        let out = map(&model, workload, None);
        for l in lines(&out) {
            if matches!(str_of(&l["source"]), "pricing.prompt" | "pricing.completion" | "pricing.request") {
                prop_assert!(l["microusd_per_batch"].is_null(), "priced from {rate}: {l}");
                prop_assert!(l["needs_review"] == true);
            }
        }
        check_output(&out, Some(&model), false)?;
    }

    /// Image endpoint prices: the OutputImages line per size is the highest
    /// per-image price among the output-image entries that parse; entries in
    /// other units are skipped with a warning; none usable is unpriced.
    #[test]
    fn image_prices_are_the_highest_parsed_output_image_price(
        body in image_body(),
        model in catalog_model(),
    ) {
        let out = map(&model, WorkloadKind::Images, Some(body.to_string().as_bytes()));
        check_output(&out, Some(&model), false)?;
        let endpoints = body["endpoints"].as_array().or(body["data"]["endpoints"].as_array()).unwrap();
        let mut best: std::collections::BTreeMap<Option<String>, i64> = Default::default();
        let mut skipped = 0;
        let mut ambiguous = false;
        for e in endpoints {
            for p in e["pricing"].as_array().unwrap() {
                if p["billable"] != "output_image" || p["unit"] != "image" {
                    skipped += 1;
                    continue;
                }
                let text = p["cost_usd"].to_string();
                let Some(r) = reference(text.trim_matches('"'), 1) else { continue };
                ambiguous |= r.wide;
                let Some(a) = r.amount else { continue };
                let variant = p["variant"].as_str().filter(|v| crate::billing::MeterVariant::new(v).is_some()).map(str::to_owned);
                let slot = best.entry(variant).or_insert(a);
                *slot = (*slot).max(a);
            }
        }
        let image_lines: Vec<&Value> = lines(&out).iter().filter(|l| l["meter"] == "output_images").collect();
        prop_assert!(!image_lines.is_empty());
        if !ambiguous {
            if best.is_empty() {
                prop_assert_eq!(image_lines.len(), 1);
                prop_assert!(image_lines[0]["microusd_per_batch"].is_null());
            } else {
                prop_assert_eq!(image_lines.len(), best.len());
                for l in image_lines {
                    let variant = l["variant"].as_str().map(str::to_owned);
                    let want = best.get(&variant).copied();
                    prop_assert_eq!(l["microusd_per_batch"].as_str().and_then(|a| a.parse::<i64>().ok()), want, "{}", l);
                }
            }
        }
        let unit_warnings = out["warnings"].as_array().unwrap().iter()
            .filter(|w| str_of(w).contains("cannot be represented and was ignored")).count();
        prop_assert_eq!(unit_warnings, skipped);
    }

    /// An image-endpoints body that is not a whole JSON document (truncated
    /// at any point, or arbitrary bytes) never prices anything.
    #[test]
    fn a_broken_image_body_prices_nothing(
        body in image_body(),
        cut in any::<usize>(),
        junk in prop::collection::vec(any::<u8>(), 0..64),
        use_junk in any::<bool>(),
    ) {
        let mut bytes = body.to_string().into_bytes();
        if use_junk {
            bytes = junk;
            // Random bytes are not an endpoints document unless they happen to be one.
            prop_assume!(serde_json::from_slice::<Value>(&bytes).is_err());
        } else {
            bytes.truncate(cut % bytes.len());
        }
        let model = json!({"id": "v/m", "pricing": {"prompt": "0.000001"}});
        let out = map(&model, WorkloadKind::Images, Some(&bytes));
        let line = lines(&out).iter().find(|l| l["meter"] == "output_images").unwrap();
        prop_assert!(line["microusd_per_batch"].is_null(), "{line}");
        prop_assert!(line["needs_review"] == true);
        check_output(&out, Some(&model), false)?;
    }
}

fn endpoint_list() -> impl Strategy<Value = Value> {
    let endpoint = (
        pricing_object(),
        prop::option::of(hostile()),
        prop::option::of(limit()),
        any::<bool>(),
    )
        .prop_map(|(pricing, name, context, down)| {
            let mut e =
                json!({"pricing": pricing, "provider_name": name.unwrap_or(json!("Provider"))});
            if let Some(Some(c)) = context {
                e["context_length"] = c.clone();
                e["max_completion_tokens"] = c;
            }
            if down {
                e["status"] = json!(-5);
            }
            e
        });
    (
        prop::collection::vec(endpoint, 0..4),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(list, wrapped, junk)| {
            if junk {
                json!({"endpoints": "not a list"})
            } else if wrapped {
                json!({"data": {"endpoints": list}})
            } else {
                json!({"endpoints": list})
            }
        })
}

proptest! {
    #![proptest_config(config(600))]

    /// With endpoint prices and recent provider-reported costs the mapper
    /// still never panics (the evidence comparison is checked arithmetic over
    /// every non-negative cost the database allows), is deterministic, reports
    /// a selection from the documented set, a bounded printable provider label,
    /// and a match count that never exceeds the evidence supplied.
    #[test]
    fn endpoint_selection_is_total_deterministic_and_bounded(
        // A catalog entry is an object (it was found by its `id`). A `top_provider`
        // that is neither an object nor null is a known panic, kept out of here
        // and pinned by `a_malformed_top_provider_is_not_a_panic`.
        model in catalog_model().prop_map(|mut m| {
            if !m.is_object() {
                m = json!({});
            }
            if !(m["top_provider"].is_object() || m["top_provider"].is_null()) {
                m["top_provider"] = json!({});
            }
            m
        }),
        endpoints in endpoint_list(),
        evidence in prop::collection::vec((any::<u64>(), any::<u64>(), 0..=i64::MAX), 0..6),
        workload in workload(),
    ) {
        let evidence: Vec<Evidence> = evidence
            .into_iter()
            .map(|(input, output, cost)| Evidence { input, output, cost })
            .collect();
        let out = map_with_endpoints(&model, workload, None, Some(&endpoints), &evidence);
        let again = map_with_endpoints(&model, workload, None, Some(&endpoints), &evidence);
        prop_assert_eq!(&out, &again);
        check_output(&out, None, true)?;
        let endpoint = &out["endpoint"];
        if workload != WorkloadKind::Images || endpoint.is_object() {
            let selection = str_of(&endpoint["selection"]);
            prop_assert!(["catalog", "cheapest", "evidence"].contains(&selection), "{endpoint}");
            if selection != "catalog" {
                prop_assert!(endpoint["matched_attempts"].as_u64().unwrap() <= evidence.len() as u64);
                prop_assert_eq!(selection == "evidence", endpoint["matched_attempts"].as_u64().unwrap() > 0);
                let label = str_of(&endpoint["provider"]);
                prop_assert!(label.chars().count() <= 64 && !label.chars().any(char::is_control));
            }
        }
    }
}

// ---- named cases -----------------------------------------------------------

/// The refusals the grammar documents, each a refusal (`None`), not a zero.
#[test]
fn malformed_negative_and_nonfinite_prices_are_refused() {
    for bad in [
        "NaN",
        "nan",
        "inf",
        "-inf",
        "Infinity",
        "-1",
        "-0",
        "-0.5",
        "+1",
        "1,5",
        "1_0",
        "0x1p3",
        "١٢٣",
        "１２３",
        ".",
        "e5",
        "1e",
        "1e+",
        "1e5.5",
        "1.2.3",
        "--1",
        "",
        "   ",
        "1e41",
        "1e-41",
        "0e40",
        " -1",
        "1 2",
        "1e999",
        "\u{0}",
        "0.0.0",
    ] {
        assert_eq!(microusd(bad, 1_000_000), None, "{bad:?}");
    }
    assert_eq!(microusd(&"9".repeat(65), 1), None);
    assert_eq!(microusd(&format!("0.{}1", "0".repeat(70)), 1), None);
    assert_eq!(
        microusd("9223372036854775808", 1),
        None,
        "i64::MAX + 1 micro-USD"
    );
    // Accepted, exponent form included.
    assert_eq!(microusd("0.000002", 1_000_000), Some((2_000_000, true)));
    assert_eq!(microusd("2e-6", 1_000_000), Some((2_000_000, true)));
    assert_eq!(microusd(" 2E-6 ", 1_000_000), Some((2_000_000, true)));
    assert_eq!(
        microusd("0.0000001", 1),
        Some((1, false)),
        "rounded up, never to zero"
    );
    assert_eq!(microusd("0", 1_000_000), Some((0, true)));
}

/// A catalog entry with no `pricing` at all, or a non-object, prices nothing.
#[test]
fn a_catalog_entry_without_prices_maps_to_unknown_not_free() {
    for model in [
        json!({}),
        json!(null),
        json!([]),
        json!({"pricing": "free"}),
        json!({"pricing": []}),
    ] {
        let out = map(&model, WorkloadKind::Generation, None);
        for meter in ["input_tokens", "output_tokens"] {
            let l = lines(&out).iter().find(|l| l["meter"] == meter).unwrap();
            assert!(l["microusd_per_batch"].is_null(), "{l}");
            assert_eq!(l["needs_review"], true);
        }
        assert!(
            out["draft"]["price_lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["meter"] == "input_tokens" && p["unknown"] == true)
        );
    }
}

/// FINDING: a prompt-size tier priced for a meter whose base rate is unusable
/// leaves the meter with only the tier line. `draft_price_lines` adds the
/// explicit `unknown` line only when the meter has no line at all, so the draft
/// states nothing for prompts at or below the threshold (it validates and
/// displays as "Free (prompt > 1 tokens)"). Downstream this is fail-closed
/// (`bounds.rs` treats a group with no base line as unknown, `tier()` is
/// `Unpriced` for small prompts) and the mapped output is flagged
/// `needs_review`, so nothing is charged as free; but the draft is less honest
/// than its comment says ("an explicit unknown line for each meter whose price
/// the catalog does not give").
#[test]
#[ignore = "#635: a tier-only meter gets no explicit unknown base line in the draft"]
fn a_tier_without_a_base_price_is_stated_unknown() {
    let model = json!({"id": "v/m", "pricing": {
        "input_cache_write": "1e400",
        "overrides": [{"input_cache_write": "0", "min_prompt_tokens": 1}]}});
    let out = map(&model, WorkloadKind::Generation, None);
    let draft = out["draft"]["price_lines"].as_array().unwrap();
    assert!(
        draft
            .iter()
            .any(|p| p["meter"] == "cache_write_tokens" && p["unknown"] == true),
        "{draft:?}"
    );
}

/// FINDING: `map_with_endpoints` writes the chosen endpoint's
/// `max_completion_tokens` with `priced["top_provider"]["max_completion_tokens"]
/// = ...`, and `IndexMut` on a string, number, boolean or array panics ("cannot
/// access key ... in JSON string"). A catalog entry whose `top_provider` is any
/// of those, with a chosen endpoint that lists `max_completion_tokens`, aborts
/// the request task. The catalog is untrusted JSON from a third party (a fixed
/// HTTPS origin), and a panic on its content is a refusal of the wrong kind.
/// The generated endpoint test keeps `top_provider` to an object or null so it
/// stays green; this test is the shrunk regression.
#[test]
#[ignore = "#634: map_with_endpoints panics when top_provider is not an object or null"]
fn a_malformed_top_provider_is_not_a_panic() {
    for tp in [json!("x"), json!(5), json!([]), json!(true)] {
        let model =
            json!({"id": "v/m", "pricing": {"prompt": "1", "completion": "2"}, "top_provider": tp});
        let endpoints = json!({"endpoints": [{"pricing": {"prompt": "1", "completion": "2"}, "max_completion_tokens": 100}]});
        let out = map_with_endpoints(
            &model,
            WorkloadKind::Generation,
            None,
            Some(&endpoints),
            &[],
        );
        assert!(out["draft"].is_object());
    }
}

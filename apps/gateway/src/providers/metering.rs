//! Wire-presence-aware metering. Unknown counters never become zero.
use crate::{
    billing::{BillingUsage, MeterUsage},
    inference::{error::InferenceError, types::Usage},
};
use serde_json::Value;
type Result<T> = std::result::Result<T, InferenceError>;

pub(crate) fn count(value: &Value) -> Result<Option<u64>> {
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_u64()
        .filter(|n| *n <= i64::MAX as u64)
        .map(Some)
        .ok_or(InferenceError::InvalidUpstream)
}
fn object(value: &Value) -> Result<()> {
    if value.is_null() || value.is_object() {
        Ok(())
    } else {
        Err(InferenceError::InvalidUpstream)
    }
}
fn checked(
    mut billing: BillingUsage,
    inclusive: bool,
    input: Option<u64>,
    output: Option<u64>,
) -> Result<Usage> {
    if billing.cache_write_input_tokens == Some(0) {
        // An observed zero total proves every disjoint allocation is zero.
        if billing.write_parts().iter().flatten().any(|n| *n != 0) {
            return Err(InferenceError::InvalidUpstream);
        }
        billing.cache_write_default_input_tokens = Some(0);
        billing.cache_write_5m_input_tokens = Some(0);
        billing.cache_write_1h_input_tokens = Some(0);
    }
    if inclusive {
        billing.total_input_tokens = input;
        if let (Some(total), Some(read), Some(write)) = (
            input,
            billing.cache_read_input_tokens,
            billing.cache_write_input_tokens,
        ) {
            billing.uncached_input_tokens = Some(
                total
                    .checked_sub(read)
                    .and_then(|n| n.checked_sub(write))
                    .ok_or(InferenceError::InvalidUpstream)?,
            );
        }
    } else {
        billing.uncached_input_tokens = input;
        if let (Some(plain), Some(read), Some(write)) = (
            input,
            billing.cache_read_input_tokens,
            billing.cache_write_input_tokens,
        ) {
            billing.total_input_tokens = Some(
                plain
                    .checked_add(read)
                    .and_then(|n| n.checked_add(write))
                    .ok_or(InferenceError::InvalidUpstream)?,
            );
        }
    }
    billing
        .validate()
        .map_err(|_| InferenceError::InvalidUpstream)?;
    Ok(Usage {
        input_tokens: input,
        output_tokens: output,
        billing: Some(billing),
        ..Default::default()
    })
}

/// OpenAI cloud and profile-specific compatible counters are inclusive.
pub(crate) fn inclusive(
    value: &Value,
    input_key: &str,
    output_key: &str,
    details_key: &str,
    write_key: &str,
) -> Result<Usage> {
    if value.is_null() {
        return Ok(Usage::default());
    }
    object(value)?;
    object(&value[details_key])?;
    let details = &value[details_key];
    let write = count(&details[write_key])?;
    let output = count(&value[output_key])?;
    let mut usage = checked(
        BillingUsage {
            cache_read_input_tokens: count(&details["cached_tokens"])?,
            cache_write_input_tokens: write,
            cache_write_default_input_tokens: write,
            // This profile's writes have no Anthropic TTL pricing categories.
            cache_write_5m_input_tokens: Some(0),
            cache_write_1h_input_tokens: Some(0),
            ..BillingUsage::default()
        },
        true,
        count(&value[input_key])?,
        output,
    )?;
    usage.reasoning_tokens = reasoning_tokens(value, output);
    Ok(usage)
}
/// Telemetry only (Logs): reasoning tokens reported inside the output details
/// (`completion_tokens_details` for Chat Completions, `output_tokens_details`
/// for Responses). Never used for charging; an absent or implausible value
/// (not an integer, or above the output count) is unknown, not an error.
fn reasoning_tokens(value: &Value, output: Option<u64>) -> Option<u64> {
    ["completion_tokens_details", "output_tokens_details"]
        .into_iter()
        .find_map(|key| value[key]["reasoning_tokens"].as_u64())
        .filter(|n| *n <= i64::MAX as u64 && output.is_none_or(|o| *n <= o))
}
pub(crate) fn anthropic(value: &Value) -> Result<Usage> {
    if value.is_null() {
        return Ok(Usage::default());
    }
    object(value)?;
    object(&value["cache_creation"])?;
    let allocation = &value["cache_creation"];
    checked(
        BillingUsage {
            cache_read_input_tokens: count(&value["cache_read_input_tokens"])?,
            cache_write_input_tokens: count(&value["cache_creation_input_tokens"])?,
            cache_write_default_input_tokens: if allocation.is_object() {
                Some(0)
            } else {
                None
            },
            cache_write_5m_input_tokens: count(&allocation["ephemeral_5m_input_tokens"])?,
            cache_write_1h_input_tokens: count(&allocation["ephemeral_1h_input_tokens"])?,
            ..BillingUsage::default()
        },
        false,
        count(&value["input_tokens"])?,
        count(&value["output_tokens"])?,
    )
}
/// Bedrock SDK values may be used only after transport validates raw presence.
pub(crate) fn bedrock(value: &Value) -> Result<Usage> {
    object(value)?;
    let mut billing = BillingUsage {
        cache_read_input_tokens: count(&value["cacheReadInputTokens"])?,
        cache_write_input_tokens: count(&value["cacheWriteInputTokens"])?,
        ..BillingUsage::default()
    };
    if !value["cacheDetails"].is_null() {
        let details = value["cacheDetails"]
            .as_array()
            .ok_or(InferenceError::InvalidUpstream)?;
        if details.len() > 2 {
            return Err(InferenceError::InvalidUpstream);
        }
        // A supplied list is a complete TTL allocation; a missing list is not.
        billing.cache_write_default_input_tokens = Some(0);
        billing.cache_write_5m_input_tokens = Some(0);
        billing.cache_write_1h_input_tokens = Some(0);
        let mut seen = std::collections::BTreeSet::new();
        for item in details {
            let ttl = item["ttl"]
                .as_str()
                .ok_or(InferenceError::InvalidUpstream)?;
            if !seen.insert(ttl) {
                return Err(InferenceError::InvalidUpstream);
            }
            let n = count(&item["inputTokens"])?.ok_or(InferenceError::InvalidUpstream)?;
            match ttl {
                "5m" => billing.cache_write_5m_input_tokens = Some(n),
                "1h" => billing.cache_write_1h_input_tokens = Some(n),
                _ => return Err(InferenceError::InvalidUpstream),
            }
        }
    }
    checked(
        billing,
        false,
        count(&value["inputTokens"])?,
        count(&value["outputTokens"])?,
    )
}
pub(crate) fn embedding(value: &Value) -> Result<Usage> {
    object(value)?;
    let input = count(&value["prompt_tokens"])?;
    let total = count(&value["total_tokens"])?;
    if let (Some(input), Some(total)) = (input, total)
        && input != total
    {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(uncached_embedding(input))
}
pub(crate) fn ollama_embedding(value: &Value) -> Result<Usage> {
    object(value)?;
    // EmbedResponse's prompt_eval_count is the aggregate of runner Embedding
    // tokenCount over the batch, not generation's cache-exclusive input count.
    // Native EmbedResponse defines no prompt-cache charging category. Its absent
    // (omitempty) count stays unknown even when a server might have omitted zero.
    Ok(uncached_embedding(count(&value["prompt_eval_count"])?))
}
/// Input-only, uncached token charging with semantic output zero (rerank, and
/// the input side of System One).
pub(crate) fn input_only(input: Option<u64>) -> Usage {
    uncached_embedding(input)
}
/// Non-token meters for a single upstream request of a text workload: meters
/// the workload cannot produce are semantic zeros, `requests` is the one
/// request sent, and `search_units` is the provider's observation (unknown
/// when absent, never zero).
pub(crate) fn text_workload_meters(search_units: Option<u64>) -> MeterUsage {
    MeterUsage {
        output_images: Some(0),
        input_characters: Some(0),
        input_audio_seconds_ms: Some(0),
        output_audio_seconds_ms: Some(0),
        search_units,
        requests: Some(1),
    }
}
/// Exact provider-reported USD amount (the JSON number's source text, never an
/// f64 round trip) as micro-USD, rounded up. Evidence only, never the charge.
pub(crate) fn usd_text_to_microusd_ceil(text: &str) -> Result<i64> {
    let bad = InferenceError::InvalidUpstream;
    if text.is_empty() || text.len() > 64 || text.starts_with('-') {
        return Err(bad);
    }
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(i) => (&text[..i], &text[i + 1..]),
        None => (text, "0"),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits_ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits_ok(int)
        || (int.len() > 1 && int.starts_with('0'))
        || (mantissa.contains('.') && !digits_ok(frac))
    {
        return Err(bad);
    }
    let exp_digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
    if !digits_ok(exp_digits) || exp_digits.len() > 4 {
        return Err(bad);
    }
    let exp: i64 = exponent.parse().map_err(|_| bad)?;
    let all = format!("{int}{frac}");
    let digits = all.trim_start_matches('0');
    if digits.is_empty() {
        return Ok(0);
    }
    // value × 10^6 = digits × 10^shift
    let shift = exp + 6 - frac.len() as i64;
    let (whole, remainder) = if shift >= 0 {
        if digits.len() as i64 + shift > 19 {
            return Err(bad);
        }
        (format!("{digits}{}", "0".repeat(shift as usize)), false)
    } else {
        let cut = digits.len() as i64 + shift;
        if cut <= 0 {
            (String::from("0"), true)
        } else {
            let (w, r) = digits.split_at(cut as usize);
            (w.to_owned(), r.bytes().any(|b| b != b'0'))
        }
    };
    if whole.len() > 19 {
        return Err(bad);
    }
    let whole: i64 = whole.parse().map_err(|_| bad)?;
    whole.checked_add(i64::from(remainder)).ok_or(bad)
}
fn uncached_embedding(input: Option<u64>) -> Usage {
    // These certified embedding APIs define input-only, uncached token charging.
    Usage {
        input_tokens: input,
        output_tokens: Some(0),
        billing: Some(BillingUsage {
            total_input_tokens: input,
            uncached_input_tokens: input,
            cache_read_input_tokens: Some(0),
            cache_write_input_tokens: Some(0),
            cache_write_default_input_tokens: Some(0),
            cache_write_5m_input_tokens: Some(0),
            cache_write_1h_input_tokens: Some(0),
        }),
        ..Default::default()
    }
}
/// Merge cumulative observations, retaining earlier evidence where later fields are absent.
pub(crate) fn merge(old: Usage, new: Usage) -> Result<Usage> {
    fn counter(old: Option<u64>, new: Option<u64>) -> Result<Option<u64>> {
        if let (Some(a), Some(b)) = (old, new)
            && b < a
        {
            return Err(InferenceError::InvalidUpstream);
        }
        Ok(new.or(old))
    }
    let billing = match (old.billing, new.billing) {
        (Some(a), Some(b)) => {
            let v = a
                .counts()
                .into_iter()
                .zip(b.counts())
                .map(|(a, b)| counter(a, b))
                .collect::<Result<Vec<_>>>()?;
            let mut b = BillingUsage {
                total_input_tokens: v[0],
                uncached_input_tokens: v[1],
                cache_read_input_tokens: v[2],
                cache_write_input_tokens: v[3],
                cache_write_default_input_tokens: v[4],
                cache_write_5m_input_tokens: v[5],
                cache_write_1h_input_tokens: v[6],
            };
            if let (Some(plain), Some(read), Some(write)) = (
                b.uncached_input_tokens,
                b.cache_read_input_tokens,
                b.cache_write_input_tokens,
            ) {
                b.total_input_tokens = Some(
                    plain
                        .checked_add(read)
                        .and_then(|n| n.checked_add(write))
                        .ok_or(InferenceError::InvalidUpstream)?,
                );
            }
            b.validate().map_err(|_| InferenceError::InvalidUpstream)?;
            Some(b)
        }
        (a, b) => b.or(a),
    };
    Ok(Usage {
        input_tokens: counter(old.input_tokens, new.input_tokens)?,
        output_tokens: counter(old.output_tokens, new.output_tokens)?,
        billing,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn inclusive_disjoint_writes_and_missing() {
        let u = inclusive(&json!({"input_tokens":100,"output_tokens":2,"input_tokens_details":{"cached_tokens":20,"cache_write_tokens":30}}),"input_tokens","output_tokens","input_tokens_details","cache_write_tokens").unwrap();
        assert_eq!(u.billing.unwrap().uncached_input_tokens, Some(50));
        let u = inclusive(
            &json!({"input_tokens":100,"input_tokens_details":{"cached_tokens":0}}),
            "input_tokens",
            "output_tokens",
            "input_tokens_details",
            "cache_write_tokens",
        )
        .unwrap();
        assert_eq!(u.billing.unwrap().cache_write_input_tokens, None);
        assert_eq!(u.billing.unwrap().uncached_input_tokens, None);
        assert!(inclusive(&json!({"input_tokens":1,"input_tokens_details":{"cached_tokens":2,"cache_write_tokens":0}}),"input_tokens","output_tokens","input_tokens_details","cache_write_tokens").is_err());
    }
    #[test]
    fn reasoning_tokens_are_telemetry_only_and_never_guessed() {
        let chat = |v| {
            inclusive(
                &v,
                "prompt_tokens",
                "completion_tokens",
                "prompt_tokens_details",
                "cache_write_tokens",
            )
            .unwrap()
        };
        let u = chat(
            json!({"prompt_tokens":5,"completion_tokens":9,"completion_tokens_details":{"reasoning_tokens":4}}),
        );
        assert_eq!(u.reasoning_tokens, Some(4));
        // Absent, implausible (above output) or malformed values stay unknown without failing.
        assert_eq!(
            chat(json!({"prompt_tokens":5,"completion_tokens":9})).reasoning_tokens,
            None
        );
        assert_eq!(chat(json!({"prompt_tokens":5,"completion_tokens":3,"completion_tokens_details":{"reasoning_tokens":4}})).reasoning_tokens, None);
        assert_eq!(chat(json!({"prompt_tokens":5,"completion_tokens":9,"completion_tokens_details":{"reasoning_tokens":"4"}})).reasoning_tokens, None);
        let u = inclusive(&json!({"input_tokens":5,"output_tokens":9,"output_tokens_details":{"reasoning_tokens":2}}),"input_tokens","output_tokens","input_tokens_details","cache_write_tokens").unwrap();
        assert_eq!(u.reasoning_tokens, Some(2));
    }
    #[test]
    fn exclusive_ttl_is_not_double_counted_or_guessed() {
        let u=anthropic(&json!({"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":30,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20}})).unwrap();
        assert_eq!(u.billing.unwrap().total_input_tokens, Some(60));
        let u=anthropic(&json!({"input_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":30})).unwrap();
        assert_eq!(u.billing.unwrap().cache_write_5m_input_tokens, None);
        assert!(anthropic(&json!({"cache_creation_input_tokens":30,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":30}})).is_err());
        assert!(
            bedrock(
                &json!({"cacheWriteInputTokens":3,"cacheDetails":[{"ttl":"5m","inputTokens":4}]})
            )
            .is_err()
        );
    }
    #[test]
    fn provider_cost_text_is_exact_and_rounded_up() {
        for (text, micro) in [
            ("0", 0),
            ("0.0", 0),
            ("1.01e-05", 11),
            ("0.00013507", 136),
            ("3.045e-06", 4),
            ("1.1508e-05", 12),
            ("5.796e-06", 6),
            ("0.04", 40_000),
            ("1", 1_000_000),
            ("1e-6", 1),
            ("1E-7", 1),
            ("1.000000", 1_000_000),
            ("0.0000010", 1),
            ("2.5e+2", 250_000_000),
            ("1e-300", 1),
            ("9223372036854.775807", i64::MAX),
        ] {
            assert_eq!(usd_text_to_microusd_ceil(text), Ok(micro), "{text}");
        }
        for bad in [
            "",
            "-1",
            "-0",
            "01",
            "1.",
            ".5",
            "1e",
            "1e+",
            "abc",
            "\"1\"",
            "1e99999",
            "9223372036855",
            "null",
            "true",
            "NaN",
        ] {
            assert!(usd_text_to_microusd_ceil(bad).is_err(), "{bad}");
        }
        let m = text_workload_meters(None);
        assert_eq!(
            (m.requests, m.search_units, m.output_images),
            (Some(1), None, Some(0))
        );
    }
    #[test]
    fn malformed_and_overflow_rejected() {
        for value in [json!(-1), json!(1.5), json!("2"), json!(u64::MAX)] {
            assert!(count(&value).is_err());
        }
        assert_eq!(count(&Value::Null).unwrap(), None);
    }
}

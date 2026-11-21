//! Endpoint comparison requires complete rates; availability claims require
//! a readable status. Recent recorded cost evidence wins before ranking.
use super::*;

/// Exact list cost of `e` at an endpoint's token prices (each part rounded up).
fn endpoint_cost(pricing: &Value, e: &Evidence) -> Option<i64> {
    let part = |key: &str, units: u64| microusd(&price_str(pricing, key)?, units).map(|x| x.0);
    part("prompt", e.input)?
        .checked_add(part("completion", e.output)?)?
        .checked_add(part("request", 1)?)
}
/// Comparable list price: one million input plus one million output tokens
/// plus one request fee, in micro-USD. `None` for variable/unparseable prices.
fn reference_cost(pricing: &Value) -> Option<i64> {
    let part = |key: &str, units: u64| microusd(&price_str(pricing, key)?, units).map(|x| x.0);
    part("prompt", 1_000_000)?
        .checked_add(part("completion", 1_000_000)?)?
        .checked_add(part("request", 1)?)
}
/// Which endpoint to price from: `(index, selection, matched attempts)`.
/// Evidence wins when any attempt's provider cost matches an endpoint's list
/// price (within per-part rounding); among equally matching endpoints the most
/// expensive is chosen. Otherwise the cheapest currently available endpoint.
fn choose_endpoint(
    endpoints: &[Value],
    evidence: &[Evidence],
) -> Option<(usize, &'static str, usize)> {
    let priced: Vec<(usize, i64)> = endpoints
        .iter()
        .enumerate()
        .filter_map(|(i, e)| reference_cost(&e["pricing"]).map(|c| (i, c)))
        .collect();
    let matches = |i: usize| {
        evidence
            .iter()
            .filter(|e| {
                endpoint_cost(&endpoints[i]["pricing"], e)
                    .is_some_and(|c| (i128::from(c) - i128::from(e.cost)).abs() <= 2)
            })
            .count()
    };
    if let Some((i, n)) = priced
        .iter()
        .map(|(i, c)| (*i, matches(*i), *c))
        .filter(|(_, n, _)| *n > 0)
        .max_by_key(|(_, n, c)| (*n, *c))
        .map(|(i, n, _)| (i, n))
    {
        return Some((i, "evidence", n));
    }
    // Only a missing status or an integer zero reports up. Unknown and
    // down statuses cannot support a cheapest-current-endpoint claim.
    let up = |i: &usize| {
        endpoints[*i]
            .get("status")
            .is_none_or(|s| s.as_i64() == Some(0))
    };
    let pool: Vec<_> = if priced.iter().any(|(i, _)| up(i)) {
        priced.into_iter().filter(|(i, _)| up(i)).collect()
    } else {
        return None;
    };
    pool.into_iter()
        .min_by_key(|(_, c)| *c)
        .map(|(i, _)| (i, "cheapest", 0))
}
pub(super) fn provider_label(e: &Value) -> String {
    e["provider_name"]
        .as_str()
        .or(e["tag"].as_str())
        .unwrap_or("unnamed provider")
        .chars()
        .filter(|c| !c.is_control() && !is_format(*c))
        .take(64)
        .collect()
}
// Unicode General_Category=Cf: invisible formatting and bidi controls must
// not change the apparent provider identity in an operator warning.
fn is_format(c: char) -> bool {
    matches!(c as u32, 0x00ad | 0x0600..=0x0605 | 0x061c | 0x06dd | 0x070f |
        0x0890..=0x0891 | 0x08e2 | 0x180e | 0x200b..=0x200f | 0x202a..=0x202e |
        0x2060..=0x2064 | 0x2066..=0x206f | 0xfeff | 0xfff9..=0xfffb | 0x110bd |
        0x110cd | 0x13430..=0x1343f | 0x1bca0..=0x1bca3 | 0x1d173..=0x1d17a |
        0xe0001 | 0xe0020..=0xe007f)
}
/// `map` priced from one concrete endpoint when the endpoint list is known.
pub(crate) fn map_with_endpoints(
    model: &Value,
    workload: WorkloadKind,
    images: Option<&[u8]>,
    endpoints: Option<&Value>,
    evidence: &[Evidence],
) -> Value {
    if !model.is_object()
        || !limits::mapping_value(model)
        || endpoints.is_some_and(|e| !limits::mapping_value(e))
        || evidence.len() > limits::MAX_ITEMS
    {
        return limits::refused(model, workload);
    }
    let list = endpoints
        .and_then(|v| {
            v["data"]["endpoints"]
                .as_array()
                .or(v["endpoints"].as_array())
        })
        .filter(|l| !l.is_empty());
    let Some((list, (index, selection, matched))) =
        list.and_then(|l| choose_endpoint(l, evidence).map(|c| (l, c)))
    else {
        let mut out = map(model, workload, images);
        if workload != WorkloadKind::Images && !all_rates_unknown(&out) {
            out["endpoint"] = json!({"selection":"catalog"});
            push_warning(
                &mut out,
                "Comparable current endpoint prices were unavailable; the catalog's top-provider price was used. OpenRouter may route to an endpoint with a different price.",
            );
            out["needs_review"] = json!(true);
        }
        return out;
    };
    let endpoint = &list[index];
    let (priced, tiers_dropped, unusable_limit) = endpoint_model(model, endpoint);
    let mut out = map(&priced, workload, images);
    if all_rates_unknown(&out) {
        return out;
    }
    if unusable_limit {
        push_warning(
            &mut out,
            "An endpoint token ceiling was unusable; the catalog ceiling was retained. Review the request limits.",
        );
        out["needs_review"] = json!(true);
    }
    describe_endpoint(&mut out, list, endpoint, selection, matched);
    if tiers_dropped {
        push_warning(
            &mut out,
            "Prompt-size price tiers are listed only for the catalog's top provider and were not applied to this endpoint; review large-prompt rates.",
        );
        out["needs_review"] = json!(true);
    }
    out
}
// A whole-import refusal has no price source to report to the operator.
fn all_rates_unknown(out: &Value) -> bool {
    out["draft"]["price_lines"]
        .as_array()
        .is_some_and(|lines| lines.iter().all(|line| line["unknown"] == true))
}

fn endpoint_model(model: &Value, endpoint: &Value) -> (Value, bool, bool) {
    let mut priced = model.clone();
    let mut pricing = endpoint["pricing"].clone();
    // Prompt-size tiers are listed for the catalog's top provider only.
    let same_rates = ["prompt", "completion"]
        .iter()
        .all(|k| price_str(&pricing, k) == price_str(&model["pricing"], k));
    let tiers_dropped = !model["pricing"]["overrides"].is_null() && !same_rates;
    // Only an object can carry the tiers (an endpoint is chosen only when its
    // `pricing.prompt` reads as a string, so this is also guaranteed upstream).
    if same_rates && pricing.is_object() && pricing["overrides"].is_null() {
        pricing["overrides"] = model["pricing"]["overrides"].clone();
    }
    priced["pricing"] = pricing;
    let valid_limit = |v: &Value| v.as_u64().filter(|n| (1..=i32::MAX as u64).contains(n));
    let unusable_limit = ["context_length", "max_completion_tokens"]
        .iter()
        .any(|key| endpoint.get(*key).is_some_and(|v| valid_limit(v).is_none()));
    if let Some(n) = valid_limit(&endpoint["context_length"]) {
        priced["context_length"] = json!(n);
    }
    if let Some(n) = valid_limit(&endpoint["max_completion_tokens"]) {
        // `top_provider` comes from the untrusted catalog; `map` reads a
        // non-object one as absent, so it is replaced rather than indexed into
        // (`IndexMut` panics on a string, number, boolean or array).
        if !priced["top_provider"].is_object() {
            priced["top_provider"] = json!({});
        }
        priced["top_provider"]["max_completion_tokens"] = json!(n);
    }
    (priced, tiers_dropped, unusable_limit)
}
fn describe_endpoint(
    out: &mut Value,
    list: &[Value],
    endpoint: &Value,
    selection: &str,
    matched: usize,
) {
    let complete = |e: &&Value| reference_cost(&e["pricing"]).is_some();
    let comparable_up = list
        .iter()
        .filter(complete)
        .filter(|e| e.get("status").is_none_or(|s| s.as_i64() == Some(0)))
        .count();
    let incomplete = list.len() - list.iter().filter(complete).count();
    if incomplete > 0 {
        push_warning(
            out,
            "Endpoints with missing or unusable comparison rates were excluded; review routing prices before publishing.",
        );
        out["needs_review"] = json!(true);
    }
    if selection == "evidence"
        && endpoint
            .get("status")
            .is_some_and(|s| s.as_i64() != Some(0))
    {
        push_warning(
            out,
            "The endpoint matching recent costs is not currently reported up; review its availability.",
        );
        out["needs_review"] = json!(true);
    }
    let label = provider_label(endpoint);
    let rate = |e: &Value| {
        price_str(&e["pricing"], "prompt")
            .and_then(|p| microusd(&p, 1_000_000))
            .map(|x| format!("{}/M input tokens", crate::billing::v3::format_usd(x.0)))
    };
    let chosen_rate = rate(endpoint).unwrap_or_else(|| "an unlisted input rate".into());
    let highest = list
        .iter()
        .filter_map(|e| reference_cost(&e["pricing"]).map(|c| (c, e)))
        .max_by_key(|(c, _)| *c);
    let pricier =
        highest.filter(|(c, _)| reference_cost(&endpoint["pricing"]).is_some_and(|own| *c > own));
    out["endpoint"] = json!({"provider":label,"selection":selection,"matched_attempts":matched,"endpoint_count":list.len(),"input_rate":rate(endpoint)});
    push_warning(
        out,
        &match selection {
            "evidence" => format!(
                "Priced from the {label} endpoint ({chosen_rate}), which matches the provider-reported cost of {matched} recent request{}.",
                if matched == 1 { "" } else { "s" }
            ),
            _ => format!(
                "Priced from the {label} endpoint ({chosen_rate}), the cheapest of {} comparable current endpoint{}.",
                comparable_up,
                if comparable_up == 1 { "" } else { "s" }
            ),
        },
    );
    if let Some((_, top)) = pricier {
        push_warning(
            out,
            &format!(
                "OpenRouter may route to another endpoint at a higher price ({} on {}); budget holds use the imported price.",
                rate(top).unwrap_or_else(|| "unlisted".into()),
                provider_label(top)
            ),
        );
        if selection != "evidence" {
            out["needs_review"] = json!(true);
        }
    }
}
fn push_warning(out: &mut Value, w: &str) {
    if let Some(list) = out["warnings"].as_array_mut() {
        if list.iter().any(|v| v.as_str() == Some(w)) {
            return;
        }
        if list.len() < limits::MAX_WARNINGS - 1 {
            list.push(json!(w));
        } else if list.len() == limits::MAX_WARNINGS - 1 {
            list.push(json!(
                "Additional catalog warnings were omitted; review all rates before publishing."
            ));
            out["needs_review"] = json!(true);
        }
    }
}

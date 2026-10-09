//! Rerank and System One response decoding shared by the adapters that speak
//! the Jina/Cohere-style rerank wire and the TypeSafe System One wire
//! (OpenRouter and the approved local profiles). Request encoding, transport
//! and provider-specific extras stay in each adapter.
use serde_json::Value;

use super::metering;
use crate::inference::{error::InferenceError, types::*};

type Result<T> = std::result::Result<T, InferenceError>;

/// Rerank `usage`: input-only tokens and observed search units.
///
/// `total_tokens` (Jina, OpenRouter, vLLM) and `prompt_tokens` (vLLM) both
/// count the input; when both are reported they must agree. Absent counters
/// stay unknown, never zero. `usage` itself may be absent.
pub(crate) fn rerank_usage(value: &Value, cost: Option<i64>) -> Result<Usage> {
    if !(value.is_null() || value.is_object()) {
        return Err(InferenceError::InvalidUpstream);
    }
    let total = metering::count(&value["total_tokens"])?;
    let prompt = metering::count(&value["prompt_tokens"])?;
    if let (Some(total), Some(prompt)) = (total, prompt)
        && total != prompt
    {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut usage = metering::input_only(total.or(prompt));
    usage.meters = Some(metering::text_workload_meters(metering::count(
        &value["search_units"],
    )?));
    usage.provider_cost_microusd = cost;
    Ok(usage)
}

/// Jina/Cohere-style `results`: `index`, `relevance_score` and an optional
/// echoed `document` (discarded); any other field fails.
pub(crate) fn rerank_results(value: &Value, request: &RerankRequest) -> Result<Vec<RerankResult>> {
    let results = value["results"]
        .as_array()
        .ok_or(InferenceError::InvalidUpstream)?;
    let results = results
        .iter()
        .map(|r| {
            let object = r.as_object().ok_or(InferenceError::InvalidUpstream)?;
            if object
                .keys()
                .any(|k| !matches!(k.as_str(), "index" | "relevance_score" | "document"))
            {
                return Err(InferenceError::InvalidUpstream);
            }
            Ok(RerankResult {
                index: r["index"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or(InferenceError::InvalidUpstream)?,
                relevance_score: r["relevance_score"]
                    .as_f64()
                    .filter(|n| n.is_finite())
                    .ok_or(InferenceError::InvalidUpstream)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let response = RerankResponse {
        results,
        usage: Usage::default(),
    };
    if !response.valid_for(request) {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(response.results)
}

/// System One `usage`: both TypeSafe counters are required. Input is
/// uncached input-only charging; output tokens are recorded as reported.
pub(crate) fn systemone_usage(value: &Value, cost: Option<i64>) -> Result<Usage> {
    if !value.is_object() {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut usage = metering::input_only(metering::count(&value["input_tokens"])?);
    usage.output_tokens = metering::count(&value["output_tokens"])?;
    if usage.input_tokens.is_none() || usage.output_tokens.is_none() {
        return Err(InferenceError::InvalidUpstream);
    }
    usage.meters = Some(metering::text_workload_meters(Some(0)));
    usage.provider_cost_microusd = cost;
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> RerankRequest {
        RerankRequest {
            model: "m".into(),
            query: "q".into(),
            documents: vec!["a".into(), "b".into()],
            top_n: None,
        }
    }

    #[test]
    fn rerank_usage_counters_agree_or_stay_unknown() {
        let u = rerank_usage(&json!({"prompt_tokens":7,"total_tokens":7}), None).unwrap();
        assert_eq!((u.input_tokens, u.output_tokens), (Some(7), Some(0)));
        let u = rerank_usage(&json!({"prompt_tokens":7}), None).unwrap();
        assert_eq!(u.billing.unwrap().uncached_input_tokens, Some(7));
        let u = rerank_usage(&Value::Null, None).unwrap();
        assert_eq!(u.input_tokens, None);
        assert_eq!(u.billing.unwrap().total_input_tokens, None);
        assert_eq!(u.meters.unwrap().search_units, None);
        for bad in [
            json!({"prompt_tokens":7,"total_tokens":8}),
            json!({"total_tokens":-1}),
            json!({"search_units":"1"}),
            json!([]),
        ] {
            assert!(rerank_usage(&bad, None).is_err(), "{bad}");
        }
    }

    #[test]
    fn rerank_results_reject_unknown_fields_and_invalid_sets() {
        let ok = json!({"results":[{"index":1,"relevance_score":0.5,"document":{"text":"b"}}]});
        assert_eq!(rerank_results(&ok, &request()).unwrap()[0].index, 1);
        for bad in [
            json!({"results":[{"index":1,"relevance_score":0.5,"extra":1}]}),
            json!({"results":[{"index":2,"relevance_score":0.5}]}),
            json!({"results":[]}),
            json!({"results":[{"index":0,"score":0.5}]}),
            json!([{"index":0,"relevance_score":0.5}]),
        ] {
            assert!(rerank_results(&bad, &request()).is_err(), "{bad}");
        }
    }
}

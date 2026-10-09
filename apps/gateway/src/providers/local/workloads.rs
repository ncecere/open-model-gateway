//! Rerank and System One wire mapping for approved local profiles.
//!
//! - **Jina/Cohere rerank** (`openai_compatible` at `{base}/rerank`; `vllm` at
//!   `{base without /v1}/rerank`, its canonical route): `{model, query,
//!   documents, top_n?}` → `{results:[{index, relevance_score, document?}],
//!   usage:{total_tokens?, prompt_tokens?, search_units?}}`. vLLM source:
//!   `vllm/entrypoints/pooling/scoring/{protocol,api_router,serving}.py`.
//! - **SGLang rerank** (`{base}/rerank`): `{model, query, documents,
//!   return_documents:false}` → a bare array `[{index, score, document?,
//!   meta_info?}]`. `top_n` is never sent: SGLang would drop the other
//!   documents' `meta_info.prompt_tokens`, so the gateway ranks and truncates
//!   itself and input usage is known only when every pair reports it. Source:
//!   `python/sglang/srt/entrypoints/openai/{protocol,serving_rerank}.py`.
//! - **System One** (`openai_compatible`, `ollama` at `{base}/systemone`): the
//!   TypeSafe wire. Ollama (v0.35.0+) may add `prompt_eval_cached_count`, a
//!   subset of `input_tokens` restored from its cache (`llm/score.go`).
use serde_json::{Value, json};

use super::super::{metering, text_workloads};
use crate::inference::{error::InferenceError, evidence, types::*};

type Result<T> = std::result::Result<T, InferenceError>;

#[derive(Clone, Copy)]
pub(super) enum RerankWire {
    Jina,
    Sglang,
}

pub(super) fn rerank_body(model: &str, request: &RerankRequest, wire: RerankWire) -> Value {
    let mut body = json!({
        "model": model,
        "query": request.query,
        "documents": request.documents,
    });
    match wire {
        RerankWire::Jina => {
            if request.top_n.is_some() {
                body["top_n"] = json!(request.result_limit());
            }
        }
        RerankWire::Sglang => body["return_documents"] = json!(false),
    }
    body
}

pub(super) fn decode_rerank(
    value: &Value,
    request: &RerankRequest,
    wire: RerankWire,
) -> Result<RerankResponse> {
    match wire {
        RerankWire::Jina => {
            if !value.is_object() {
                return Err(InferenceError::InvalidUpstream);
            }
            let usage = text_workloads::rerank_usage(&value["usage"], None)?;
            evidence::preserve(
                text_workloads::rerank_results(value, request)
                    .map(|results| RerankResponse { results, usage }),
                || Some(Ok(usage)),
            )
        }
        RerankWire::Sglang => decode_sglang(value, request),
    }
}

/// Summed `meta_info.prompt_tokens`, or unknown when any pair omits it.
fn sglang_usage(items: &[Value]) -> Result<Usage> {
    let mut total = Some(0u64);
    for item in items {
        let meta = &item["meta_info"];
        if !(meta.is_null() || meta.is_object()) {
            return Err(InferenceError::InvalidUpstream);
        }
        total = match (total, metering::count(&meta["prompt_tokens"])?) {
            (Some(sum), Some(n)) => Some(
                sum.checked_add(n)
                    .filter(|n| *n <= i64::MAX as u64)
                    .ok_or(InferenceError::InvalidUpstream)?,
            ),
            _ => None,
        };
    }
    let mut usage = metering::input_only(total);
    usage.meters = Some(metering::text_workload_meters(None));
    Ok(usage)
}

fn decode_sglang(value: &Value, request: &RerankRequest) -> Result<RerankResponse> {
    let items = value.as_array().ok_or(InferenceError::InvalidUpstream)?;
    let usage = sglang_usage(items)?;
    let results = (|| {
        // Every document is scored and returned when `top_n` is not sent.
        if items.len() != request.documents.len() {
            return Err(InferenceError::InvalidUpstream);
        }
        let mut seen = vec![false; items.len()];
        let mut results = items
            .iter()
            .map(|item| {
                let object = item.as_object().ok_or(InferenceError::InvalidUpstream)?;
                if object
                    .keys()
                    .any(|k| !matches!(k.as_str(), "index" | "score" | "document" | "meta_info"))
                {
                    return Err(InferenceError::InvalidUpstream);
                }
                let index = item["index"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|n| *n < seen.len() && !std::mem::replace(&mut seen[*n], true))
                    .ok_or(InferenceError::InvalidUpstream)?;
                Ok(RerankResult {
                    index,
                    relevance_score: item["score"]
                        .as_f64()
                        .filter(|n| n.is_finite())
                        .ok_or(InferenceError::InvalidUpstream)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        results.sort_by(|a, b| {
            b.relevance_score
                .total_cmp(&a.relevance_score)
                .then(a.index.cmp(&b.index))
        });
        results.truncate(request.result_limit());
        let response = RerankResponse { results, usage };
        if response.valid_for(request) {
            Ok(response)
        } else {
            Err(InferenceError::InvalidUpstream)
        }
    })();
    evidence::preserve(results, || Some(Ok(usage)))
}

pub(super) fn decode_systemone(
    value: &Value,
    request: &SystemoneRequest,
) -> Result<SystemoneResponse> {
    if !value.is_object() {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut usage = text_workloads::systemone_usage(&value["usage"], None)?;
    if let Some(cached) = metering::count(&value["prompt_eval_cached_count"])? {
        // A subset of the inclusive input; local prefix caches report no write category.
        let (input, output, meters) = (usage.input_tokens, usage.output_tokens, usage.meters);
        if input.is_none_or(|input| cached > input) {
            return Err(InferenceError::InvalidUpstream);
        }
        usage = metering::inclusive(
            &json!({"input_tokens": input, "output_tokens": output, "details": {"cached_tokens": cached}}),
            "input_tokens",
            "output_tokens",
            "details",
            "cache_write_tokens",
        )?;
        usage.meters = meters;
    }
    evidence::preserve(
        parse_answers(&value["answers"], request)
            .map(|answers| SystemoneResponse { answers, usage }),
        || Some(Ok(usage)),
    )
}

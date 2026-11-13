//! `map`: one catalog model's pricing (untrusted JSON) to a price suggestion,
//! in named steps.
use super::*;

/// Pure mapping from one catalog model (+ image endpoint body) to a v3 draft.
pub(crate) fn map(model: &Value, workload: WorkloadKind, images: Option<&[u8]>) -> Value {
    let pricing = &model["pricing"];
    let mut d = Draft {
        lines: vec![],
        warnings: vec![],
    };
    let used = |k: &str| !pricing[k].is_null();
    match workload {
        WorkloadKind::Generation => generation_lines(&mut d, pricing),
        WorkloadKind::Embeddings | WorkloadKind::Systemone | WorkloadKind::Rerank => {
            token_lines(&mut d, workload, pricing);
        }
        WorkloadKind::Images => image_workload_lines(&mut d, pricing, images, used),
        WorkloadKind::AudioTranscriptions => transcription_lines(&mut d, pricing),
        WorkloadKind::AudioSpeech => speech_lines(&mut d, pricing),
        // OpenRouter serves no realtime sessions; there is nothing to import.
        WorkloadKind::Realtime => d.warnings.push(
            "OpenRouter does not serve realtime sessions; set the text and audio token rates by hand."
                .into(),
        ),
        // Async jobs are served on OpenAI connections only (docs/async-jobs.md).
        WorkloadKind::Videos | WorkloadKind::Batches => d.warnings.push(
            "OpenRouter video and batch jobs are not supported by the gateway; set the rates by hand."
                .into(),
        ),
    }
    unmetered_warnings(&mut d, pricing, workload, used);
    request_line(&mut d, pricing);
    for meter in Meter::ALL {
        d.na(meter, false, "");
    }
    let price_lines = draft_price_lines(&d);
    let parsed = serde_json::from_value::<PriceLines>(json!(price_lines)).ok();
    if parsed.is_none() {
        d.warnings
            .push("The draft lines failed validation; edit them before publishing.".into());
    }
    let max_units = max_units(&mut d);
    let Ceilings {
        input_limit,
        output_limit,
        context,
        max_completion,
    } = ceilings(&mut d, model);
    let needs_review = d.lines.iter().any(|l| l["needs_review"] == true) || parsed.is_none();
    json!({
        "source":"openrouter_public_catalog",
        "catalog_model_id":model["id"],
        "workload":workload.as_str(),
        "needs_review":needs_review,
        "lines":d.lines,
        "warnings":d.warnings,
        "display_lines":parsed.as_ref().map(|p| p.0.iter().map(display_line).collect::<Vec<_>>()),
        "ceilings":{"input_token_limit":input_limit,"output_token_limit":output_limit,"context_length":context,"max_completion_tokens":max_completion},
        "draft":{"pricing_version":3,"input_token_limit":input_limit,"output_token_limit":output_limit,"price_lines":price_lines,"max_units":max_units},
    })
}
/// Generation: token lines, the reasoning-rate check and prompt-size tiers.
fn generation_lines(d: &mut Draft, pricing: &Value) {
    for (key, meter, sku) in TOKEN_KEYS {
        match price_str(pricing, key) {
            Some(p) => d.priced(
                meter,
                sku,
                Some(&p),
                &format!("pricing.{key}"),
                false,
                None,
                None,
                "",
            ),
            None if meter.is_token()
                && !matches!(meter, Meter::InputTokens | Meter::OutputTokens) =>
            {
                d.na(
                    meter,
                    true,
                    "not listed in the catalog; confirm the model cannot produce it",
                )
            }
            None => d.priced(
                meter,
                sku,
                None,
                &format!("pricing.{key}"),
                true,
                None,
                None,
                "",
            ),
        }
    }
    if price_str(pricing, "internal_reasoning")
        .is_some_and(|r| Some(r) != price_str(pricing, "completion"))
    {
        d.warnings.push("Reasoning tokens are priced separately and have no gateway meter; review the output token rate.".into());
        for l in d.lines.iter_mut().filter(|l| l["meter"] == "output_tokens") {
            l["needs_review"] = json!(true);
        }
    }
    overrides(d, pricing);
}
/// Embeddings, Systemone and rerank: input/output token lines (rerank adds a
/// manual search-unit line) and prompt-size tiers.
fn token_lines(d: &mut Draft, workload: WorkloadKind, pricing: &Value) {
    let review = workload == WorkloadKind::Rerank;
    for (key, meter, sku) in &TOKEN_KEYS[..2] {
        d.priced(
            *meter,
            sku,
            price_str(pricing, key).as_deref(),
            &format!("pricing.{key}"),
            review,
            None,
            None,
            "",
        );
    }
    if workload == WorkloadKind::Rerank {
        d.priced(
            Meter::SearchUnits,
            "Search units",
            None,
            "manual",
            true,
            None,
            None,
            "OpenRouter's catalog does not publish rerank prices",
        );
        d.warnings.push(
            "Rerank prices are not in the catalog; set the token and search rates by hand.".into(),
        );
    }
    overrides(d, pricing);
}
/// Images: prompt/completion token lines and the per-image endpoint prices.
fn image_workload_lines(
    d: &mut Draft,
    pricing: &Value,
    images: Option<&[u8]>,
    used: impl Fn(&str) -> bool,
) {
    d.priced(
        Meter::InputTokens,
        "Input",
        price_str(pricing, "prompt").as_deref(),
        "pricing.prompt",
        true,
        None,
        None,
        "image-model prompt token rate",
    );
    d.priced(
        Meter::OutputTokens,
        "Output",
        price_str(pricing, "completion").as_deref(),
        "pricing.completion",
        true,
        None,
        None,
        "",
    );
    if used("image_output") || used("image_token") {
        d.warnings.push("The catalog's per-token image rates are synthetic for per-image models and were ignored.".into());
    }
    image_lines(d, images);
}
/// Audio transcriptions: audio input seconds and output tokens.
fn transcription_lines(d: &mut Draft, pricing: &Value) {
    d.priced(Meter::InputAudioSecondsMs, "Audio input", price_str(pricing, "prompt").as_deref(), "pricing.prompt", true, None, None, "transcription prompt units are model-specific (second, hour or token); interpreted as USD per second");
    d.priced(
        Meter::OutputTokens,
        "Output",
        price_str(pricing, "completion").as_deref(),
        "pricing.completion",
        true,
        None,
        None,
        "",
    );
    d.na(
        Meter::InputTokens,
        true,
        "token-priced transcription models need an input token rate instead",
    );
}
/// Audio speech: input characters and audio output seconds.
fn speech_lines(d: &mut Draft, pricing: &Value) {
    d.priced(
        Meter::InputCharacters,
        "Characters",
        price_str(pricing, "prompt").as_deref(),
        "pricing.prompt",
        false,
        None,
        None,
        "speech prompt is priced per input character",
    );
    let completion = price_str(pricing, "completion");
    let free = completion
        .as_deref()
        .and_then(|c| microusd(c, 1))
        .is_some_and(|x| x.0 == 0);
    if free {
        d.priced(
            Meter::OutputAudioSecondsMs,
            "Audio output",
            Some("0"),
            "pricing.completion",
            false,
            None,
            None,
            "",
        );
    } else {
        d.priced(Meter::OutputAudioSecondsMs, "Audio output", completion.as_deref(), "pricing.completion", true, None, None, "per-second speech models put the output rate in completion; interpreted as USD per second");
    }
    d.na(Meter::InputTokens, false, "");
    d.na(Meter::OutputTokens, false, "");
}
/// Warnings for catalog prices with no meter here, and for a catalog discount.
fn unmetered_warnings(
    d: &mut Draft,
    pricing: &Value,
    workload: WorkloadKind,
    used: impl Fn(&str) -> bool,
) {
    for (key, what) in [
        ("web_search", "Web search"),
        ("image", "Image input"),
        ("audio", "Audio input token"),
        ("audio_output", "Audio output token"),
        ("input_audio_cache", "Cached audio input"),
    ] {
        if used(key) && workload != WorkloadKind::Images {
            d.warnings.push(format!(
                "{what} pricing has no meter for this workload and was ignored."
            ));
        }
    }
    if discounted(&pricing["discount"]) {
        d.warnings
            .push("A catalog discount was not applied; list prices are conservative.".into());
    }
}
/// The per-request line: the catalog's fee, or zero when none is listed.
fn request_line(d: &mut Draft, pricing: &Value) {
    match price_str(pricing, "request") {
        Some(p) => d.priced(
            Meter::Requests,
            "Request",
            Some(&p),
            "pricing.request",
            false,
            None,
            None,
            "",
        ),
        None => d.priced(
            Meter::Requests,
            "Request",
            Some("0"),
            "absent pricing.request",
            false,
            None,
            None,
            "no per-request fee listed",
        ),
    }
}
/// The draft's price lines.
fn draft_price_lines(d: &Draft) -> Vec<Value> {
    // Draft: fully priced and not-applicable lines without review
    // annotations, plus an explicit `unknown` line for each meter whose price
    // the catalog does not give (publication requires every meter stated).
    let mut price_lines: Vec<Value> = d
        .lines
        .iter()
        .filter(|l| l["not_applicable"] == true || l["microusd_per_batch"].is_string())
        .map(|l| {
            let mut l = l.clone();
            if let Some(o) = l.as_object_mut() {
                o.remove("needs_review");
                o.remove("source");
                o.remove("note");
            }
            l
        })
        .collect();
    for line in &d.lines {
        let meter = &line["meter"];
        if line["microusd_per_batch"].is_null()
            && line["not_applicable"] != true
            && !price_lines.iter().any(|l| &l["meter"] == meter)
        {
            price_lines.push(json!({"meter":meter,"unknown":true}));
        }
    }
    price_lines
}
/// The draft's `max_units`, and a warning for each meter that needs one set.
fn max_units(d: &mut Draft) -> serde_json::Map<String, Value> {
    let mut max_units = serde_json::Map::new();
    if d.lines
        .iter()
        .any(|l| l["meter"] == "requests" && l["microusd_per_batch"] != "0")
    {
        max_units.insert("requests".into(), json!("1"));
    }
    for (meter, what) in [
        (Meter::OutputImages, "images per request"),
        (Meter::InputCharacters, "input characters per request"),
        (Meter::InputAudioSecondsMs, "input audio per request"),
        (Meter::OutputAudioSecondsMs, "output audio per request"),
        (Meter::SearchUnits, "searches per request"),
    ] {
        if d.lines.iter().any(|l| {
            l["meter"] == meter.as_str()
                && l["not_applicable"] != true
                && l["microusd_per_batch"] != "0"
        }) {
            d.warnings.push(format!(
                "Set the maximum {what} so budgets can hold a finite amount."
            ));
        }
    }
    max_units
}
/// The imported token ceilings and the catalog limits they came from.
struct Ceilings {
    input_limit: u64,
    output_limit: u64,
    context: Option<u64>,
    max_completion: Option<u64>,
}
/// Token ceilings from the catalog limits, with their warnings.
fn ceilings(d: &mut Draft, model: &Value) -> Ceilings {
    // Conservative ceilings (never the full context window): the hold and the
    // per-attempt token reservation must fit default workspace limits.
    let na = |m: Meter| {
        d.lines
            .iter()
            .any(|l| l["meter"] == m.as_str() && l["not_applicable"] == true)
    };
    let input_na = Meter::ALL
        .into_iter()
        .filter(|m| m.is_token() && *m != Meter::OutputTokens)
        .all(na);
    let context = model["context_length"]
        .as_u64()
        .filter(|n| (1..=i32::MAX as u64).contains(n));
    let max_completion = model["top_provider"]["max_completion_tokens"]
        .as_u64()
        .filter(|n| (1..=i32::MAX as u64).contains(n));
    let input_limit = if input_na {
        0
    } else {
        context.map_or(DEFAULT_INPUT_CEILING, |c| c.min(DEFAULT_INPUT_CEILING))
    };
    let output_limit = if na(Meter::OutputTokens) {
        0
    } else {
        max_completion
            .or(context)
            .map_or(DEFAULT_OUTPUT_CEILING, |c| c.min(DEFAULT_OUTPUT_CEILING))
    };
    if !input_na && context.is_none() {
        d.warnings.push(format!(
            "The catalog lists no context length; the input token ceiling defaults to {}.",
            DEFAULT_INPUT_CEILING
        ));
    }
    if context.is_some_and(|c| c > input_limit && !input_na)
        || max_completion.is_some_and(|c| c > output_limit && !na(Meter::OutputTokens))
    {
        d.warnings.push("Token ceilings were set conservatively below the model's maximum so requests fit default tokens-per-minute limits; raise them deliberately if you need longer prompts or replies.".into());
    }
    Ceilings {
        input_limit,
        output_limit,
        context,
        max_completion,
    }
}

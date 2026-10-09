# Cache-aware pricing

Cache accounting preserves provider observations and separates billable categories. It does not enable prompt caching, expose request cache controls or guarantee every provider returns complete cache metadata. Missing usage/rates are unknown, not free.

## Raw versus normalized input

Execution `input_tokens,output_tokens` retain raw provider observations. `billing_usage` carries nullable decimal strings:

- `total_input_tokens`: inclusive input.
- `uncached_input_tokens` and `cache_read_input_tokens`.
- `cache_write_input_tokens`: aggregate writes.
- `cache_write_default_input_tokens`, `cache_write_5m_input_tokens`, `cache_write_1h_input_tokens`: disjoint write allocation.

With complete evidence:

```text
inclusive input = uncached + cache read + aggregate write
aggregate write = default write + 5-minute write + 1-hour write
```

**The aggregate write overlaps its allocations and is never charged again.** Unknown allocations cannot be filled by arbitrary subtraction/TTL guesses. Explicit observed zero writes prove zero allocations; a missing aggregate does not. Contradictory, negative, noninteger or overflowing counters fail validation. Stream observations are cumulative snapshots; merges preserve earlier observations rather than adding them twice or erasing known counts.

## Provider semantics

OpenAI/compatible profiles treat input as inclusive, subtracting cache read/write only when both are observed. Compatible Chat recognizes profile-specific details (`created_cache_tokens` for vLLM; other current profiles use their declared cache-write detail); absent metadata remains unknown. Do not assume every compatible server supplies it.

Anthropic raw input is uncached; inclusive total additionally needs observed read/write counters. Cache creation aggregate overlaps 5-minute/1-hour allocations. If TTL allocation is missing, it remains unknown rather than assuming all writes use a default TTL.

Bedrock raw usage is validated for wire presence before SDK defaults can invent counters. Read/write plus supplied `cacheDetails` normalize TTL allocations. Missing list/counters remain unknown. Request cache controls are not part of the current frontend subsets.

Supported string-embedding profiles define input-only uncached accounting with non-applicable cache zeros and semantic output zero. Missing prompt input remains unknown. Ollama native `prompt_eval_count` support is documented in [local profile notes](../apps/gateway/src/providers/local/README.md); the follow-up is not a new live-server or fresh integration validation claim here.

## Publish exact rates

Pricing-v2 requires all cache categories explicitly:

```json
{
  "read":{"status":"priced","microusd_per_million":"100000"},
  "write":{"status":"priced","microusd_per_million":"1250000"},
  "write_5m":{"status":"unknown"},
  "write_1h":{"status":"not_applicable"}
}
```

- `priced` with `"0"` is an explicit configured free rate.
- `unknown` cannot prove a finite bound or value positive usage.
- `not_applicable` asserts that the category cannot apply to this configured deployment. Positive observations contradict that assertion and cannot be valued as zero. An unobserved NA category counts as possible usage only when observed counters force a positive residual into it. For example, an aggregate write of 10 with missing TTL allocations, where every allocation that could absorb it is NA, is unbounded. Entirely unknown usage does not, by itself, make NA categories possible. Unknown rates stay conservative: any remaining capacity in an `unknown` category is possible usage.

V2 publishes uncached-input and output rates alongside these four cache rates and trusted hard token ceilings. The input ceiling includes reads/writes, not just uncached input. Versions are append-only and pinned at admission. V1 uses the normalized inclusive input total when billing metadata exists; an unknown inclusive total cannot fall back to cache-exclusive raw input. Raw-only legacy observations remain supported. V1 has no reconstructed cache components, and historical rows are not rewritten into v2 splits.

## Charge, floor and bound are different

Each settled disjoint component is `ceil(tokens × micro-USD-per-million / 1,000,000)`. Checked i128 arithmetic avoids floating point; persisted amounts remain signed-64 integers and API strings.

For example: 10 uncached, 5 read, and 15 aggregate writes split 3 default/4 five-minute/8 one-hour, plus 2 output. At rates 1,000,000 / 100,000 / 1,250,000 / 1,250,000 / 2,000,000 / 3,000,000, separately rounded components are `10,1,4,5,16,6` micro-USD: total `42`. The 15 write aggregate adds **no** second charge.

Complete normalized usage/output and applicable rates settle the six components. Partial known usage yields only a **floor**. Admission requires a conservative finite bound for budget enforcement; unknown rate categories leave that bound unknown even when a floor is positive. The bound conservatively covers independent category ceilings and per-component rounding; it need not equal a typical invoice estimate.

## Pricing v3: price lines and meters

`pricing_version:3` replaces scalar token rates (stored null) with immutable `price_lines` and per-request `max_units`. Each priced line is exact integer micro-USD per batch:

```json
{"meter":"output_images","microusd_per_batch":"20500","batch":1,"unit_label":"/image","sku_label":"Image output","variant":"768"}
```

| Meter | Batch / unit label | Counted from |
| --- | --- | --- |
| `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_write_tokens`, `cache_write_5m_tokens`, `cache_write_1h_tokens` | 1,000,000 · `/M tokens` | uncached input, output, cache read, default/5m/1h write allocations |
| `output_images` | 1 · `/image` (optional `variant`) | `meter_usage.output_images` + `output_image_variant` |
| `input_characters` | 1,000,000 · `/M characters` | `meter_usage.input_characters` |
| `input_audio_seconds_ms`, `output_audio_seconds_ms` | 1,000 `/second`, 60,000 `/minute`, 3,600,000 `/hour` | milliseconds |
| `search_units` | 1 · `/search` | `meter_usage.search_units` |
| `requests` | 1 · `/request` | `meter_usage.requests` |

- `unit_label` must equal the canonical label for the meter/batch; `sku_label` is 1–80 non-control characters. `variant` is allowed only on `output_images` (`[A-Za-z0-9._:-]{1,32}`).
- `(meter, variant, min_prompt_tokens)` is unique. `{"meter":...,"not_applicable":true}` is the meter's only line.
- `min_prompt_tokens` (1–2,147,483,647) is a prompt-size tier: it applies when inclusive input tokens are **strictly greater**; the highest exceeded threshold wins per meter. Unknown inclusive input with tiers leaves the charge unresolved.
- A variant-specific line applies to that variant; variant-less lines are the default for other or unreported variants. An image variant with no line is unpriced.
- **Total-only input.** When a provider reports only the inclusive input total (no cache split, for example SGLang or vLLM without `prompt_tokens_details`) and every cache meter with an unknown count is `not_applicable`, those categories are absent, so uncached input is the total minus the known cache parts. An aggregate cache write that its known allocations do not explain derives nothing (it stays unknown); a priced cache meter with an unknown count also leaves input unknown.
- **A missing meter line is unknown, never free.** `"0"` is explicitly free; free meters need no ceiling and settle as zero even when unobserved. `not_applicable` treats an unobserved meter as absent; positive usage contradicts it.
- `input_token_limit` may be `0` only when every input-family token meter (`input_tokens` and the four cache meters) is `not_applicable`, for example text to speech or per-second transcription (migration `0003`); otherwise it is at least 1. `output_token_limit` may be `0`.
- A **failed** attempt settles at a known `0` only when the provider rejected it before processing (`upstream_rejected`: 4xx validation, moderation or a filtered/missing model), it reported no usage at all (no token or meter counts, no nonzero provider cost), and every meter of the pinned v3 price is explicitly free or not applicable. Its token counts are recorded as semantic zeros. Every other failure, partial usage, priced or missing meter, and v1/v2 price stays unknown.

Each used meter charges `ceil(count × microusd_per_batch / batch)`; the total is the sum. Any possibly-used meter that is unpriced or unobserved leaves the actual unknown with the known charges kept as a floor. Settled v3 `cost_components` add six meter keys (`output_images_microusd`, `input_characters_microusd`, `input_audio_microusd`, `output_audio_microusd`, `search_units_microusd`, `requests_microusd`) to the six token keys.

The admission hold charges every applicable input-family token meter on the full `input_token_limit` (output on the requested maximum) at its highest applicable tier rate, plus each unit meter's `max_units` at its highest (variant) rate. Tiers above the input ceiling cannot apply. Any non-NA meter without a line, without base (non-tier) coverage, or—unless free—without `max_units` makes the price unbounded: budgeted admission is denied with `budget_exceeded` at the budget's scope. Usage above a ceiling, positive NA usage or an unpriced variant settles when exact but marks `unbounded_cost`. V1/v2 behavior is unchanged.

`GET .../prices` adds `price_lines`, `max_units`, `display_lines` and `display_summary`, computed from integers (for example `$0.10/M input tokens`, `$0.0205/image (768)`, `$15/M characters`, `$0.20/minute`). A zero rate always reads `Requests: Free` (`Output images: Free (1K)`), never `$0/request`. Whole dollars omit decimals; otherwise at least two decimals without trailing zeros.

`provider_cost_microusd` (for example OpenRouter `usage.cost`, rounded up to micro-USD) is stored as evidence only and never used as the charge.

Unresolved attempts retain original/grown holds. A known floor cannot shrink a hold or clear `unbounded_cost`. Failed/cancelled usage is not automatically a settled bill. Evidence refinement must preserve every existing raw/normalized counter and use the pinned price. See [governance](governance.md), [governance API](governance-api.md) and [cost reporting](cost-reporting.md).

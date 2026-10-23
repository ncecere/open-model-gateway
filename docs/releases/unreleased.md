# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.7.1](v0.7.1.md). They become the next release's notes.

## No-argument tools and empty successful results (#220)

Chat function definitions may omit `parameters`, meaning no arguments. Only omission becomes the closed empty-object schema (`type:object`, `properties:{}`, `additionalProperties:false`); explicit object schemas are unchanged and null/nonobject parameters remain invalid. Omitted `strict` stays absent, with existing per-adapter strict capability rules unchanged.

Native Messages accepts omitted `tool_result.content` as empty text and explicit `is_error:false` as the default success. Call IDs and content bounds remain intact. Error-marked results (`is_error:true`) are explicitly unsupported, not silently downgraded; null content, malformed flags, missing IDs and unknown fields remain invalid. Messages still requires `max_tokens`. No database/runtime-grant changes, hosted/beta/thinking expansion or general nullable-field compatibility is included.

## Nullable OpenAI defaults (#221)

Chat Completions and Responses accept omitted/null `stream` as non-streaming. Responses accepts omitted/null/false `store` as stateless operation under the gateway privacy subset; explicit `store:true` still fails before admission and every native upstream Responses request still sends `store:false`. Existing nullable supported scalar/options keep omission-equivalence and pricing defaults. This is not blanket null acceptance: OpenAI SDK 7.28 `tools` collections are optional, not nullable; malformed nested values, nonnullable fields and unknown/unsupported options remain rejected. Public Chat assistant `refusal:null` means no refusal and is accepted without forwarding; non-null refusal content, other-role refusal fields, empty messages and unknown fields such as `parsed:null` remain rejected. This does not add typed refusal support. Native Anthropic 0.131 `stream`/`tools` and required `max_tokens` are unchanged. No schema or environment changes.

## Deterministic refusals no longer take a budget hold (#228)

A Chat or Messages request that an Anthropic or Bedrock route would always reject (for example a `strict` function tool, an out-of-range temperature, or a tool result with no matching call) is now refused before durable admission: no execution, reservation, ledger, counter or routing-health row, and no upstream call. Before this, such a request was admitted first and left an unknown-cost hold with nothing dispatched, so a shared-workspace key holder could tie up the workspace's budget by repeating it.

Validation reuses each adapter's own request encoder, runs once per adapter per request, and checks every authorized route: another route that can serve the request still does. When every route refuses, the error kind (400 invalid request or 501 unsupported) comes only from routes the routing policy would use, so residency, disabled routes and cooldown apply first. Existing unknown holds are untouched; credential, endpoint and network failures after admission still retain their holds as before. No schema or grant changes.

## Batch planning no longer sizes the parent envelope for deterministic adapter rejections (#238)

Creating a gateway-run batch (`POST /v1/batches`) now runs every chat line through the same deterministic, pre-admission check `Engine::execute_protocol` runs for an interactive request (#228), in addition to the existing coarse capability check, before the parent envelope is sized or reserved.

- A line with an explicit maximum is not pinned to a route; the runner may dispatch it to any authorized, protocol-capable candidate deployment of the model. It is excluded from sizing only when *every* such candidate would refuse it - the same once-per-adapter/provider enumeration the engine uses, read-only (no admission, no routing-health writes), not just the one deployment planning happened to size the line against. When every capable candidate refuses, the reported kind comes only from the routes the read-only routing policy would actually use, exactly as the engine computes it for an interactive request.
- A line with an omitted maximum is pinned at plan time to the one deployment its price and output cap were computed against, and is judged on that route alone. When that pinned route deterministically refuses it, the runner's dispatch now classifies the refusal exactly as an interactive request pinned to that one route would (400 invalid request or 501 unsupported) - never a bare `model_not_found`, even when a different, unpinned route for the same model would have accepted it.
- The check runs against the line's own resolved output bound (the same clamped body `encode_native_line` already sees), not the client's raw, possibly-omitted request.
- A deterministic exclusion anywhere in the batch also forces gateway-run execution: a batch is never submitted to a provider's native batch API while one of its lines is excluded from the envelope, which would otherwise submit (and bill) a line nothing was reserved for.

A batch whose lines are all such rejections is refused per line at creation (no batch, no reservation, nothing dispatched); a mixed batch reserves only for its valid lines. An excluded line is still copied into the batch and still dispatches; it is refused per line with no execution or reservation of its own. Unknown-hold retention and envelope release on close are unchanged. No schema or grant changes.

## Fixes

- **Chat Completions API guide (#225).** The model-page guide now correctly lists `max_tokens` as an accepted alias of `max_completion_tokens`; when both are supplied, values must be equal (otherwise 400). Optional maxima and existing unsupported-capability disclosures are unchanged; no runtime behavior changes.

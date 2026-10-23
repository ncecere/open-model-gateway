# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.7.2](v0.7.2.md). They become the next release's notes.

## Batch planning no longer sizes the parent envelope for deterministic adapter rejections (#238)

Creating a gateway-run batch (`POST /v1/batches`) now runs every chat line through the same deterministic, pre-admission check `Engine::execute_protocol` runs for an interactive request (#228), in addition to the existing coarse capability check, before the parent envelope is sized or reserved.

- A line with an explicit maximum is not pinned to a route; the runner may dispatch it to any authorized, protocol-capable candidate deployment of the model. It is excluded from sizing only when *every* such candidate would refuse it - the same once-per-adapter/provider enumeration the engine uses, read-only (no admission, no routing-health writes), not just the one deployment planning happened to size the line against. When every capable candidate refuses, the reported kind comes only from the routes the read-only routing policy would actually use, exactly as the engine computes it for an interactive request.
- A line with an omitted maximum is pinned at plan time to the one deployment its price and output cap were computed against, and is judged on that route alone. When that pinned route deterministically refuses it, the runner's dispatch now classifies the refusal exactly as an interactive request pinned to that one route would (400 invalid request or 501 unsupported) - never a bare `model_not_found`, even when a different, unpinned route for the same model would have accepted it.
- The check runs against the line's own resolved output bound (the same clamped body `encode_native_line` already sees), not the client's raw, possibly-omitted request.
- A deterministic exclusion anywhere in the batch also forces gateway-run execution: a batch is never submitted to a provider's native batch API while one of its lines is excluded from the envelope, which would otherwise submit (and bill) a line nothing was reserved for.

A batch whose lines are all such rejections is refused per line at creation (no batch, no reservation, nothing dispatched); a mixed batch reserves only for its valid lines. An excluded line is still copied into the batch and still dispatches; it is refused per line with no execution or reservation of its own. Unknown-hold retention and envelope release on close are unchanged. No schema or grant changes.

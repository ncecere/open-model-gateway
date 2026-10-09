# Open Model Gateway (unreleased, planned v0.3.2)

These changes close three gaps found during live DGX Spark acceptance on 2026-10-09. A route the adapter can't serve is now refused when it is configured, not on every request. A price that can't bound a request is refused when it is published, and a budgeted request on an older such price gets an error that names the actual problem. Chat Completions accepts the legacy `max_tokens` that many OpenAI-compatible clients still send. There are no migrations.

## Highlights

- **Routes must be servable.** Previously a route could be created on a connection whose profile can't carry the model's protocols, for example a System One model on a `vllm` connection or a rerank model on `ollama`. The model then read Ready while every request failed with `unsupported_capability`. Now:
  - The profile → protocol capability table lives in one place, `providers::capabilities`. Every adapter's `supports_protocol` reads it, so the inference path's `unsupported_capability`, route validation, readiness counts and the Add model form can't disagree. A test compares the dashboard's `protocolProfiles` with it.
  - The gateway returns `400` with `error.reason:"route_unsupported_capability"` when you create such a route (`POST /platform/deployments` or `POST /platform/model-setup`), enable one (`PATCH /platform/deployments/{id}`), or change a model's `supported_protocols` (`PATCH /platform/models/{id}`) to protocols that an enabled route can't serve. The message names the profile, the workload and protocols, and what the profile does serve. For example: `The vllm connection profile can't serve System One (systemone); it serves chat_completions, embeddings, rerank.` Disabling a route is always allowed. A text model only needs each route to serve one of its protocols, so a Chat + Responses model can still have a `vllm` route for Chat next to an `openai` route for Responses.
  - Existing bad routes are not migrated. Model readiness adds `serving_routes`, `unsupported_routes` and `unsupported_profiles`. An enabled model whose enabled routes all can't serve reads **Not serving: vllm connection can't serve System One decisions** in Admin › Models, on the model page (a "Routes can serve this model" checklist step) and in Compare. A model with some unservable routes needs attention. Workspace catalogs (`routes`/`priced_routes`), the access explanation (`no_enabled_route`) and the overview's ready-model count only count routes that can serve.
- **v3 prices must state every meter.** Previously a v3 price published through the API could leave meters out. A missing meter is unknown, so the price couldn't bound a request, and budgeted keys were refused with `budget_exceeded`, which read like an exhausted budget. Now:
  - **At publication** (`POST /platform/deployments/{id}/prices`, the `price` of model setup, and a `batch_price_lines` list), every meter the route's workload can use must be stated, as a priced line, `{"meter":...,"not_applicable":true}`, or the new explicit `{"meter":...,"unknown":true}`. A missing meter returns 400 `error.reason:"price_meters_incomplete"`, with `error.missing_meters` listing the meters and a message naming the workload. The meters are those that workload's admission bound treats as possibly used; the table is in [cache pricing](../cache-pricing.md#meter-completeness). An explicit unknown values exactly like a missing line, and it is stored as that missing line, so stored price lines keep their validated shape and need no migration. The dashboard's "Unknown" choice now publishes it. The OpenRouter price suggestion's `draft` adds an `unknown` line for each meter the catalog doesn't price.
  - **At admission**, when any budget applies, a v3 price that can't bound the attempt (a possibly-used meter without a line, stated unknown, or with no `max_units`) is refused before dispatch with HTTP **503 `price_unbounded`** and `x-should-retry: false`. It no longer reports `budget_exceeded` at each budget's scope. The 503 matches the `provider_configuration_error` that an unpriced route gets under a budget. The problem is the price's configuration, so the error carries no scope or amounts. Realtime window growth on such a price is refused the same way. Without budgets, admission is unchanged: the attempt runs with an unbounded hold. v1/v2 prices keep `provider_configuration_error`.
  - **Immutability.** Older price rows are never rewritten. Budgeted keys on them now get `price_unbounded`. Publish a new, complete version to fix them.
- **Chat Completions accepts `max_tokens`.** Previously, Chat requests with the legacy `max_tokens` got 400 "Invalid or unsupported chat request fields". Many OpenAI-compatible clients send it. It is now an alias of `max_completion_tokens`:
  - Both fields together are accepted only when the values are equal. Different values, or 0, return 400.
  - The one value bounds the reservation and becomes the upstream maximum: `max_completion_tokens` for OpenAI and OpenRouter, `max_tokens` for local profiles (as before), Messages `max_tokens` for Anthropic and Converse `maxTokens` for Bedrock.
  - Batch lines follow the same rule. Before, they accepted `max_tokens` only without `max_completion_tokens`.
  - Responses is unchanged: it takes `max_output_tokens` and rejects both Chat names.
  - The official OpenAI SDK contract test now sends `max_tokens` (non-stream and stream), both fields with equal values, and both with different values (refused).

## Breaking and behaviour changes

- **Route creation, enabling and model protocol changes can now return 400 `route_unsupported_capability`.** Scripts that created such routes (they never served requests) must use a connection whose profile serves the model; the [protocol matrix](../protocol-matrix.md) lists what each profile serves.
- **v3 price POSTs that omit a meter the route can use return 400 `price_meters_incomplete`.** Add the missing meters as priced, `not_applicable` or `unknown` lines. The `governance-api.md` v3 example now states every meter. v1/v2 bodies are unaffected.
- **Budgeted requests on unbounded v3 prices return 503 `price_unbounded`** (previously 429 `budget_exceeded`). The OpenAI and Anthropic SDKs don't retry it (`x-should-retry: false`). Alerts and dashboards that counted these as budget denials will no longer see them as `budget_exceeded`.
- **Chat accepts `max_tokens`.** Requests that sent `max_tokens` and expected a 400 now succeed.

## Upgrading

No migrations and no new runtime grants: the readiness and validation queries read tables the runtime role can already select. API additions:

- model readiness `serving_routes`, `unsupported_routes` and `unsupported_profiles`;
- compare rows `serving_routes`;
- the price line form `{meter, unknown:true}` (publication only; never returned);
- the management error reasons `route_unsupported_capability` and `price_meters_incomplete` (with `missing_meters`) in `error.reason`, with `error.code` the HTTP status as for every management error;
- the inference error code `price_unbounded`.

After upgrading, open Admin › Models and filter by **Not serving** to find routes that can't serve. For any model whose budgeted requests report `price_unbounded`, publish a complete price.

## Build and CI

CI and the release pipeline no longer pull from Docker Hub, whose anonymous rate limit and an outage failed builds on GitHub's shared runners. The `Dockerfile`'s node and rust build stages now come from `mirror.gcr.io`, Google's Docker Hub mirror (the same image digests), and it uses BuildKit's built-in Dockerfile frontend (no `# syntax=` line). The runtime image is unchanged. The staging Compose file still defaults to Docker Hub's `postgres` and `caddy`; `STAGING_POSTGRES_IMAGE` and `STAGING_CADDY_IMAGE` replace them. Details: [verification](../verification.md#no-docker-hub-pulls-in-ci).

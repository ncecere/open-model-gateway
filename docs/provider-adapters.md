# Engine and adapter contract

```text
Client protocol (protocols/{chat_completions,responses,messages}.rs)
       ↓ typed ChatRequest
Engine (inference/)
  workspace authorization → deterministic eligible deployment → provider registry
  concurrency permit → durable started record → deadline-bound execution
       ↓ ProviderAdapter::execute
Provider (providers/{openai,openai_responses,anthropic,bedrock}.rs)
  connection validation → secret resolution → HTTP → typed result/events
       ↑ ChatResponse or EventStream
Client protocol formats JSON / SSE; engine finalizes execution accounting
```

## Adding a provider

1. Add a module under `apps/gateway/src/providers/` implementing `ProviderAdapter`.
2. Choose a stable lowercase registry ID (letters, digits, underscores, up to 64 characters). Provider IDs are extensible strings in PostgreSQL, not an enumerated provider list.
3. Implement `capabilities()` and `execute(&Deployment, ChatRequest)`. Override `supports_protocol(ApiProtocol)` and `execute_protocol` for additional supported client protocols; defaults allow Chat Completions only. The engine receives no provider HTTP types, URLs, or credential values.
4. Validate the provider's connection fields and capability-specific settings. Resolve credentials through injected dependencies; a cloud adapter may use workload identity rather than bearer secrets.
5. Translate supported inputs and reject anything that cannot be represented faithfully. Keep provider-native extension support explicit when introducing it; do not add an unchecked JSON passthrough.
6. Return a complete `ChatResponse` or a lazy `EventStream`. Dropping either an execution future or a stream must drop upstream work. Do not spawn detached network producers.
7. Register the adapter in `main.rs`, the composition root. No engine routing changes or provider-specific database migrations are needed.
8. Add local upstream fixtures and run `providers::contract::assert_text_chat_contract` along with provider-specific failure, tool, and stream-framing tests.

Modules can become crates later without changing the engine contract. Runtime-loaded native plugins are intentionally out of scope.

## Current scope

The service registers `openai`, `anthropic`, and `bedrock`. OpenAI uses `https://api.openai.com/v1`; custom endpoints and regions are rejected. Redirects and environment proxies are disabled. Localhost transport is injectable only inside adapter unit tests, not through production configuration.

The client-facing Chat Completions subset supports:

- One choice (`n` omitted or `1`).
- Text-string messages with system, developer, user, assistant, and tool roles.
- Function definitions, function tool calls/results, and tool-choice selection.
- `temperature` and `max_completion_tokens`.
- Streaming and non-streaming responses; `stream_options.include_usage` controls client-visible usage.

Unsupported fields return a sanitized 400. This includes legacy `max_tokens`, multimodal content arrays, structured output settings, reasoning options, log probabilities, and arbitrary provider extensions. Nonempty upstream refusal/audio/reasoning content that cannot fit the current contract is also rejected rather than dropped. Supporting a capability at adapter level does not guarantee that every upstream model supports it; provider rejections remain possible. Model-specific capability metadata is future work.

Responses and Messages expose native stateless text/tool subsets with an explicit combination matrix. Anthropic and Bedrock also accept the Chat subset where representable. Unsupported combinations fail. This is not complete vendor API compatibility; see [protocol matrix](protocol-matrix.md).

## Stream contract

- `Delta` contains text and/or indexed tool-call fragments.
- Exactly one `Finish` ends the choice. No deltas may follow it.
- Optional `Usage` contains provider-reported counts, not estimates. It may follow `Finish`; counts are snapshots, not additive.
- Exactly one `Done` terminates successful execution.
- EOF without `Done`, duplicate terminal events, malformed upstream frames, or an explicit error is failure.
- The Chat frontend emits `[DONE]` only for success. A mid-stream failure emits a sanitized JSON error SSE payload and closes without a success marker; the HTTP status is already 200 at that point.
- Dropping the stream releases the concurrency permit and drops the upstream connection. This requests cancellation, but cannot guarantee the provider stops generation or charges nothing.

Provider adapters never retry internally. The engine defaults to one attempt and deterministic first-available routing. Explicit model policies may enable weighted selection/cooldowns and up to three separately admitted attempts; only allowlisted pre-stream errors can fail over, with matching residency constraints. Ambiguous transport failover is separately opt-in. No retry/fallback occurs after a stream is returned. See [routing](routing.md).

## Credentials and limits

`GATEWAY_SECRET_ENV_ALLOWLIST` is a comma-separated operator-controlled list of environment variable names. The default is empty. An `env:NAME` credential reference is resolved only if NAME is allowlisted. Values are not stored in PostgreSQL and are never returned by the API or included in error messages. A reference is not a per-tenant vault policy: only platform operators may configure provider connections through the session-authenticated management API.

- `GATEWAY_MAX_CONCURRENT_REQUESTS`: per-process, non-queuing inference limit; default 128.
- `GATEWAY_REQUEST_TIMEOUT_SECONDS`: inference work deadline; default 120, supported range 1–3600.
- Provider HTTP connection timeout: 10 seconds.
- Inbound request limit: 2 MiB.
- OpenAI response limit: 4 MiB for complete JSON, 1 MiB per SSE frame, and at most 128 tool-call indices.

Deadline checks cover repository lookups, provider execution, and awaited stream reads. A watchdog drops upstream transport at the hard deadline even when the returned stream is unpolled. Configure ingress write/idle timeouts to release slow-client HTTP resources. PostgreSQL-backed org/workspace/key quotas and spend reservations apply before every attempt; see [governance](governance.md).

## Accounting is not billing

`inference_executions` contains a durable `started` row before each outbound attempt, followed by `succeeded`, `failed`, or `cancelled`. It records organization, workspace, key, deployment, public model, provider, elapsed time, and known token counts. It does not store prompts, output text, tool arguments, credentials, prices, or estimated charges.

Missing usage stays NULL, including cancelled streams where final usage never arrived. Finalization has a separate three-second storage deadline. Disconnect finalization is best effort; process crashes or failed storage writes can leave `started` rows until lease reconciliation. A bounded reconciliation worker now closes expired leases without refunding unknown cost. Immutable configured-rate price versions and an append-only reservation/settlement ledger support budget admission. They are estimates, not actual vendor charges or customer billing. Unknown unpriced usage blocks newly enabled budgets instead of becoming free; explicit evidence-backed resolution requires a pinned price.

## Enable the local example deliberately

After applying migrations and running `bootstrap-dev`, inject a real OpenAI credential into the **gateway process** as `OPENAI_API_KEY` using your preferred secret mechanism. Set `GATEWAY_SECRET_ENV_ALLOWLIST=OPENAI_API_KEY`. Never put the credential in the web environment or source code.

The bootstrap connection and deployment remain disabled by default. To enable only the local example, run this against the disposable local development database:

```sh
docker compose exec -T postgres psql -U gateway -d gateway -v ON_ERROR_STOP=1 <<'SQL'
BEGIN;
UPDATE provider_connections p SET enabled = true
FROM organizations o
WHERE p.organization_id = o.id AND o.slug = 'local-dev' AND p.name = 'Example OpenAI';
UPDATE deployments d SET enabled = true
FROM provider_connections p, organizations o
WHERE d.organization_id = o.id AND p.organization_id = o.id
  AND d.provider_connection_id = p.id AND o.slug = 'local-dev' AND p.name = 'Example OpenAI';
COMMIT;
SQL
```

The seeded upstream model is `gpt-4.1`; select a model supported by your provider account before making calls. The authenticated dashboard is the normal provider/model/key administration path. The SQL above is only an optional disposable-local-fixture shortcut.

```sh
curl -N http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer $GATEWAY_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"company/smart","messages":[{"role":"user","content":"Hello"}],"max_completion_tokens":32,"stream":true,"stream_options":{"include_usage":true}}'
```

This request incurs upstream usage if enabled with real credentials. Automated tests use mocks; they require no provider secrets or paid calls.

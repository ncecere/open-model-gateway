# Guardrails design (issue #36)

**Status:** design only, not implemented. **Date:** 2026-10-13.

*User decision* marks the user's decisions of 2026-10-13. **Recommendation** marks proposals that still need review. Today the gateway never inspects prompt or response text.

## 1. Goals and non-goals

**Goals**

- Protect the following, built one at a time and each verified before the next (*user decision*):
  - leaked secrets;
  - personal data (mask or block);
  - content moderation;
  - output DLP;
  - prompt-injection signals.
- Off by default. Opt-in per workspace, with optional platform defaults per workspace type, composed the way limits compose today ([governance](governance.md)).
- State plainly what is inspected, by which detector, and where text goes when the detector is a model.
- Actions are monitor, redact and block, configured per policy (*user decision*).
- Store no matched text. On a match, record only policy, detector, category and count (*user decision*).
- Keep every accounting invariant in AGENTS.md: one reservation per upstream attempt, unknown cost keeps its hold, no failover after a stream is returned, no implicit retry.

**Non-goals**

- Storing or replaying prompts or responses, including as "evidence" of a block.
- Inspecting images, audio bytes or files at rest. These are *uninspectable* (section 6).
- A guarantee. Pattern detectors miss things and model detectors are probabilistic. Guardrails reduce risk; they are not a compliance certificate.

## 2. Threats and use cases

| Protection | Threat | Typical action | Detectors |
| --- | --- | --- | --- |
| **Secrets** | Credentials pasted into a prompt or tool result reach a third-party provider: cloud keys, tokens, PEM keys, JWTs, connection strings, the gateway's own `omg_…` keys (`apps/gateway/src/auth.rs`). | block | pattern |
| **Personal data** | Email addresses, phones, cards (Luhn), IBANs (mod-97), national IDs or IP addresses go to a provider with an unknown data policy. | redact or block | pattern; names and addresses need System One or the judge |
| **Moderation** | Abusive, self-harm, sexual or violent content, in either direction. | monitor or block | System One, LLM judge |
| **Output DLP** | The model echoes secrets, personal data or configured confidential terms. | monitor, stop or block | pattern, System One |
| **Prompt-injection signals** | Instructions hidden in retrieved documents or tool results. | monitor, optionally block | pattern heuristics, System One, judge |
| **Custom DLP** | Code names, customer identifiers, internal hostnames. | any | pattern (admin-defined) |

Output DLP is not a separate detector. It is the output side of the secrets, personal-data and custom rules.

## 3. Where checks run

### Input

The frontends (`protocols/chat_completions.rs`, `responses.rs`, `messages.rs`) normalize requests into the canonical `ChatRequest` (`inference/types.rs`). `Engine::execute_protocol` (`apps/gateway/src/inference/mod.rs`) then: (1) acquires a capacity permit, (2) looks up `deployments`, (3) filters `candidates`, (4) builds the `route_plan`, and (5) for each attempt calls `repository.admit` (durable reservation), then the adapter, then for streams the `tracked` wrapper, which settles on `Done`.

**Recommendation:** inspect input inside the engine, after step 3 and before step 4, once per root request. Reasons:

- **One place.** Chat, Responses, Messages and gateway-run batch lines all pass through it; `jobs/runner.rs` calls the same `execute_protocol`.
- **Cheap failures first.** `model_not_found` and unsupported capabilities fail before any detector is paid for.
- **No reservation for a blocked input.** Inspection happens before the first `admit`, so a blocked input reserves nothing and is charged nothing upstream. Admission sees the request that is actually sent, including any redaction.
- **Same input on failover.** Attempts 2–3 reuse the inspected request, since each attempt already gets `request.clone()`. Input is not inspected again.

`Engine::execute_workload` (`inference/workload.rs`) gets the same step at the same point, after `candidates` and before `route_plan`. A new `Workload::inspectable_text()` method defaults to "uninspectable". Embeddings, rerank, System One and speech input implement it.

### Detector calls: admission, billing, capacity

Model-based detectors (*user decision*) call a model the gateway serves:

- System One through `execute_workload` with a `SystemoneRequest`;
- the LLM judge through `execute_protocol` with a `ChatRequest`.

Each detector call is therefore its own root request, with its own route plan, reservation, settlement and execution row. It sends text to that model's provider.

**Recommendation: detector calls are billed to the requesting key's workspace and key lineage (the same `Principal`).**

- **Cost follows the cause.** Cost reports, cost-center snapshots and budgets stay accurate.
- **No global scope.** A platform-owned "guardrail workspace" would be one scope that every admission touches. That is the global row AGENTS.md forbids ([governance](governance.md#no-installation-wide-limits)).
- **Limits see the real work.** Budgets, tokens per minute and requests at once all include detector traffic.

**Bookkeeping.**

- Detector calls get a fresh `root_request_id`, because `inference_executions` enforces `UNIQUE(root_request_id, attempt_number)` with attempt numbers 1–3.
- A new nullable `guardrail_parent_request_id` links each detector call to the client request.
- An engine-internal flag stops detector calls from running guardrails themselves.

**Recommendation: model authorization uses a detector variant of the lookup that skips only the key's model restriction.**

Both the candidate lookup (`live_deployments`, `inference/repository.rs` ~427) and live admission (`governance.rs` ~637) check `key_model_restrictions`/`key_model_selections`, so a detector call with the client's `Principal` would be refused for any restricted key. The engine (never the client) passes `purpose: guardrail`, and both queries then omit only that predicate. Key revalidation, `workspace_model_allowed` (catalog or direct assignment), the enabled checks and every limit stay. The candidate cache key (`cache.rs`, `(workspace, key, model)`) gains the purpose; `forget_key` still evicts by key.

Why not apply the restriction: it states which models the *holder* may choose, while an administrator chose the detector model. Restriction lineage is also immutable, even across rotation ([management API](management-api.md#membership-invitations-and-credentials)), so applying it would permanently break fail-closed model rules for existing restricted keys.

If the model is unavailable to the workspace, the detector fails (section 4).

**Recommendation: capacity is a parameter.** Both entry points take the shared engine semaphore with `try_acquire_owned`:
- `execute_protocol` at `mod.rs` line 124;
- `execute_workload` at `workload.rs` line 367.

The parent request already holds a permit from it, so under saturation detector calls would fail as `Busy`. Both entry points should take the semaphore as a parameter. Public calls pass the engine's semaphore; detector calls pass a separate bounded guardrail semaphore (`GATEWAY_GUARDRAIL_MAX_CONCURRENT`).

**Known cost.** A detector can be paid for and the client's own admission still be denied, for example when the budget runs out in between. Avoiding this would mean reserving first, which contradicts "a blocked input reserves nothing". Running pattern rules first, and model rules only when still needed, limits it.

### Output

Only the attempt whose output is delivered is inspected. Failover happens only on `Busy`/`UpstreamUnavailable` before anything is returned (`routing::may_failover`), so that attempt is always the last one.

The new guardrail errors are added to neither `may_failover` nor `affects_health` (`routing.rs`), so:
- an output block never fails over or retries;
- it never marks a route unhealthy.

## 4. Detector interface

One internal trait, implemented by built-in and later external detectors (*user decision*):

```rust
#[async_trait]
trait Detector: Send + Sync {
    fn kind(&self) -> DetectorKind;      // Pattern | Systemone | LlmJudge | External(name)
    fn egress(&self) -> Egress;          // Local | Model { public_model } | Service { name }
    fn categories(&self) -> &[Category]; // secret, personal_data, moderation, prompt_injection, custom
    async fn inspect(&self, ctx: &DetectContext, input: &Inspection<'_>)
        -> Result<Findings, DetectorError>;
}
```

`Inspection` is a list of borrowed text segments, each with message index, role, part (`content`, `tool_arguments`, `tool_result`, `system`, `tool_description`) and a *new* flag (after the last assistant turn). `DetectContext` carries the principal, parent request ID, deadline and rule settings. `Findings` lists `{category, segment, span, score?}`: spans exist only in memory for redaction, and scores are compared with per-rule thresholds.

**Built-in detector kinds**

- **Pattern.** Rules plus checksums (Luhn, mod-97, key formats).
  - Runs on Rust's `regex` crate, which is linear time, so there is no catastrophic backtracking.
  - `regex` would be a new direct dependency. Today `regex-syntax` is in `Cargo.lock` only as a transitive dependency.
  - Free, and nothing leaves the gateway.
- **System One.** A `SystemoneRequest` (`inference/workload_types.rs`) with the selected text as `state` and one `noul` question per category. `valid_for` validates the answer probabilities; output is reserved to the price's `output_token_limit` (`OutputReservation::PriceCeiling`).
- **LLM judge.** A chat model with a rubric.
  - `ChatRequest` has no structured-output field ([protocol matrix](protocol-matrix.md#chat-completions)). The verdict therefore comes from one function tool, `report`, forced with `tool_choice: Function("report")`; both already exist in the canonical request.
  - The judged text is wrapped in delimiters as data. The verdict is validated strictly.
  - `max_output_tokens` is small, so the hold stays small.

**Timeouts.** `timeout_ms` per rule: default 5 s, maximum 30 s, and never beyond the request deadline (`EngineLimits::request_timeout`, 120 s by default).

**Failure mode.** Each rule sets `on_error` to `fail_closed` or `fail_open`.

**Recommendation:** default to `fail_closed` for block and redact rules, and `fail_open` for monitor rules. A monitor rule that fails records `detector_failed` and the request continues.

Under `fail_closed`, the client sees the following. **Recommendation:** only transient failures are retryable. SDKs retry 5xx responses, and each retry pays for the detectors again.

| Detector failure | Client sees |
| --- | --- |
| Timeout, `Busy`, `UpstreamUnavailable`, guardrail pool full | 503 `guardrail_unavailable`, retryable |
| `RouteCoolingDown` | 503 `guardrail_unavailable` with `Retry-After` |
| Admission denied (budget, unresolved usage, token reservation) | that denial unchanged, with its existing `x-should-retry: false` |
| Model unavailable or unsupported, configuration, `price_unbounded`, invalid verdict | 503 `guardrail_detector_failed` with `x-should-retry: false` |
| Input above the detector's size cap | 400 `guardrail_input_too_large` with `x-should-retry: false` |

Using `x-should-retry: false` on a 503 has a precedent: the existing `price_unbounded` (`protocols/mod.rs`). The comment there notes that the official OpenAI and Anthropic SDKs honor the header.

**External services later** (OpenAI moderation, Bedrock Guardrails, Presidio) implement the same trait with `Egress::Service`. They need an approved HTTPS endpoint and a credential reference under the provider-connection rules (allowlisted destinations, no redirects, proxies or implicit retries), plus a price when billed. Nothing else changes.

## 5. Policy model

### Storage and layers

Layers mirror the limit layers (*user decision*):
- a workspace-type default;
- a platform per-workspace override that replaces it;
- a workspace local layer, tighten-only, set by shared-workspace owners and admins, and by the owner of a personal workspace.

Storage is two tables: `guardrail_rule_sets` (layer, scope, revision, and `local_only` on platform layers only) and `guardrail_rules`.

*User decision:* personal-workspace owners may **tighten** their own guardrails through the local layer. Unlike limits (`personal_limits_platform_controlled`), this cannot loosen anything the platform layers set, so it needs no platform control.

*User decision:* no key layer at first. A tighten-only key layer (for example agent keys on monitor while app keys redact) can be added later if needed.

```json
{"id":"<uuid>","name":"Secrets","protection":"secrets",
 "detector":{"kind":"pattern","categories":["secret"]},
 "input":{"action":"block","redact_scope":"user_text"},
 "output":{"mode":"monitor"},
 "threshold":null,"on_error":"fail_closed","timeout_ms":null,
 "workloads":["generation","embeddings"],"uninspectable":"deny"}
```

### Composition (tighten-only)

The effective policy is the platform layer's rules plus the local layer's rules.

**Every rule runs independently**, with its own detector, threshold and failure mode.

**Per request, the strictest outcome wins**, in this order: block > redact > monitor.
- When several rules redact, their spans are combined.
- The response is handled with the strictest output mode among the rules that apply: off < monitor < stop-mid-stream < block-before-sending.

**Tightening.** A local layer may add rules and may tighten a platform rule (stronger action or output mode, `fail_closed`, lower threshold). It may never weaken or remove one.

**Egress.** `local_only` is a platform-layer field only.

When it is set, no rule whose detector sends text out (System One, judge, external) is effective. This is enforced in two places:
- **On write:** 403 `guardrail_egress_not_allowed`.
- **When the effective policy is composed:** a local rule written before the platform turned `local_only` on becomes inactive, and GET shows it with `inactive_reason:"egress_not_allowed"`. It is never silently dropped.

### Caching

Limit policies are not cached ([governance](governance.md#caches-and-change-notifications)), but guardrail rules are read on every request.

**Recommendation:** cache the effective rule set per replica under a new `guardrails` notification topic, added by a migration. It inherits the existing invalidation behavior:
- milliseconds through LISTEN, at most about 1 s through polling;
- caches are bypassed when versions can't be confirmed for 3 s.

The check takes no admission locks.

### Management API sketch

All routes use the management envelope `{error:{code:"<status>",message,reason?}}`.

| Route | Who |
| --- | --- |
| `GET/PUT /platform/workspace-types/{kind}/guardrails` | Admin writes; Auditor reads |
| `GET/PUT/DELETE /platform/workspaces/{ws}/guardrails` | Override. DELETE restores the type default. |
| `GET/PUT /workspaces/{ws}/guardrails` | Shared owner/admin. GET also returns `effective`, `provenance` and `egress`. |
| `GET /platform/guardrails/detectors` | Built-in kinds, categories and egress |

Rejection reasons:

| HTTP | `reason` |
| --- | --- |
| 403 | `guardrail_loosen_not_allowed` (adds `rule`) |
| 403 | `guardrail_egress_not_allowed` |
| 400 | `guardrail_detector_unavailable` |
| 400 | `guardrail_pattern_unbounded` (section 7) |

Changes are audited as `guardrail_policy.updated`, with layer, rule IDs, actions and modes. For custom patterns only their count is recorded, never their content.

### Dashboard

- **Workspace › Guardrails:** effective rules with provenance, each with one sentence on what is inspected and where it goes ("New user messages are sent to `company/jev` (OpenRouter) for moderation."), plus "Streaming responses are held until checked" when block-before-sending is on.
- **Admin › Guardrails:** type defaults and overrides.
- **Logs:** a `guardrail=monitored|redacted|blocked` badge and filter; request detail lists findings and linked detector requests with cost.
- **Usage & costs:** a `purpose: client|guardrail` breakdown.

## 6. Client-visible behavior

Errors keep the shapes in `protocols/mod.rs`:
- OpenAI: `{error:{message,type,code,param}}`;
- Anthropic: `{type:"error",error:{type,message}}`.

**Recommendation:** both shapes gain one `guardrail` object:

```json
{"error":{"message":"Blocked by guardrail policy \"Secrets\" (category: secret).",
 "type":"invalid_request_error","code":"guardrail_input_blocked","param":null,
 "guardrail":{"policy_id":"<uuid>","rule_id":"<uuid>","stage":"input","category":"secret","detector":"pattern"}}}
```

Block errors name the policy and the category family. They never contain matched text, offsets or scores. The new `InferenceError` variants carry only IDs and fixed enums, so the type stays `Copy`.

| Case | HTTP | OpenAI `code` / Anthropic `type` | Retry |
| --- | --- | --- | --- |
| Input blocked | 400 | `guardrail_input_blocked` / `invalid_request_error` | `x-should-retry: false` |
| Output blocked (non-stream) | 400 | `guardrail_output_blocked` / `invalid_request_error` | `x-should-retry: false` |
| Enforcing rule on an uninspectable workload | 400 | `guardrail_not_supported` / `invalid_request_error` | `x-should-retry: false` |
| Detector failures | as in section 4 | Anthropic: `api_error` (5xx) or `invalid_request_error` (400) | as in section 4 |

**Decided: output blocks are errors.** A 200 response with `finish_reason: content_filter` would be a response the provider never sent, and AGENTS.md forbids fabricated responses.

**Redaction token.** `[REDACTED:<CATEGORY>_<n>]`, for example `[REDACTED:EMAIL_1]`.
- Numbers are assigned by **first appearance in the conversation**, scanning messages in order.
- The same value always gets the same number. The value-to-number map exists only in memory for the request.
- Redaction never changes JSON structure. A match inside tool-call arguments is monitored or blocked, never redacted.

**Recommendation: redaction applies to pattern detectors only.**
- Redacting history (section 8) needs the same output on every turn.
- Model detectors are nondeterministic and, by default, judge only new content.
- So model detectors support monitor and block only.

**Per workload**

- **Chat, Responses, Messages:** all of the above. Streaming errors use each frontend's existing failure path:
  - Chat: a `data:` error body and no `[DONE]`;
  - Responses: an `error` event;
  - Messages: an `error` event.
- **Embeddings, rerank, System One (client calls), speech input:** input checks only, because their outputs are vectors, scores, answers or audio.
- **Images, transcription input, realtime:** uninspectable.
  - An enforcing rule refuses them with `guardrail_not_supported`: images and transcription before admission, realtime with a 400 before the WebSocket upgrade.
  - `uninspectable: allow` lets them through and records a `skipped` finding.
- **Batches:** gateway-run lines go through `execute_protocol`, and blocked lines get the interactive body (`batch_line_error`). Native batches run pattern rules at create time beside the existing line validation; a block rejects the batch (400 `guardrail_input_blocked`, line numbers only). Native batches needing model detectors or output blocking are refused (`guardrail_not_supported`), never silently switched to gateway-run prices. Output checks run per line at collection.

## 7. Output modes and streaming

The output mode is set per policy (*user decision*).

**Engine-side buffer cap.** Today no engine-level output bound exists:
- `Accumulator`, with its 4 MiB `OUTPUT_LIMIT`, lives in `protocols/responses.rs` and is used only by Responses and Messages;
- Chat streams without buffering;
- non-stream bodies are bounded by the adapters' 4 MiB cap.

**Recommendation:** the guardrail wrapper keeps its own text buffer with a cap, `GATEWAY_GUARDRAIL_MAX_OUTPUT_BYTES` (default 4 MiB). This buffer serves both block-before-sending and the post-`Done` model checks. When the cap is exceeded:
- under `fail_open`, checks stop and the rest is forwarded unchecked;
- under `fail_closed`, the stream is stopped as described under stop-mid-stream below, with code `guardrail_output_too_large`.

**The scanner runs inside the `tracked` loop** in `mod.rs`. If the stream were stopped by dropping `tracked`, `ExecutionGuard::drop` would record the attempt as `Cancelled` (`mod.rs` around line 464), which would be wrong.

The modes:

- **Monitor.** The stream is not changed.
  - Pattern rules scan deltas as they are forwarded. A carry-over window catches matches that are split across chunks.
  - Model rules run after `Done` on the capped buffer, in the guardrail pool. They are recorded as `skipped` when the pool is full or the client cancelled.
  - The text is dropped as soon as the verdict arrives.
- **Block-before-sending.** Events are buffered until `Done`. At `Done`, `tracked` has already settled the attempt as `succeeded`: the provider did the work. The buffer is then inspected, and either replayed or replaced with `GuardrailOutputBlocked`.
  - Frontends send only content-free opening events beforehand: Chat's role chunk, `response.created`/`response.in_progress`, or `message_start`.
  - Their existing 15 s SSE keep-alives keep idle proxies from closing the connection.
  - Clients see no tokens until the end, which is why the dashboard shows the mode.
- **Stop-mid-stream.** Text is forwarded with a holdback (default 256 bytes, configurable up to 4 KiB).
  - Every stop-mode pattern must have a maximum match length no larger than the holdback. It is computed at save time with `regex-syntax` (`Hir::properties().maximum_len()`).
  - A pattern with an unbounded or longer maximum is rejected (`guardrail_pattern_unbounded`), or rewritten to trigger on a short prefix. For example, the built-in PEM rule triggers on its `-----BEGIN … PRIVATE KEY-----` line, because the full key is 1.7 KB or more.
  - On a match the loop calls `guard.finish(Failed, Some(GuardrailOutputBlocked))`, yields the error, then drops upstream, which cancels it. The finished guard's `drop` is a no-op. Usage is usually unreported, so the hold stays unknown. Sent tokens can't be taken back.
  - Responses and Messages already emit tool calls whole ([protocol matrix](protocol-matrix.md#streaming-and-evidence-boundaries)); Chat streams tool-call deltas, so the holdback covers them.
  - **Recommendation:** pattern rules only in this mode.

## 8. Coding agents

Agents and harnesses such as Claude Code resend the whole conversation on every turn. They rely on the history staying byte-identical.

**Today**, `ChatRequest` carries no cache markers, thinking blocks or signatures. `/v1/messages` rejects `cache_control`, thinking and `anthropic-beta` ([protocol matrix](protocol-matrix.md#messages)). The design still has to hold once those are supported.

Ways redaction can break agents:
- **Prompt caching.** Provider caches match exact prefixes. If an earlier turn changes, cache reads become cache writes, and the prefix hash that the proposed cache-aware routing would key on changes too.
- **Thinking signatures.** Changing a signed thinking block makes the provider reject the request.
- **Tool history.** If the model sees `[REDACTED:…]` in a file it read, it can write the placeholder back through its edit tool and corrupt the user's file.

**The user's decision** recommends monitor or block for agent traffic. It also offers a redact mode that touches only new user input. Applied on its own, that mode leaks: on the next turn the agent resends the original text as history, unredacted. First-send-only redaction protects only the first send.

*User decision:* the default `redact_scope` is `user_text`.
- Every turn, the same deterministic pattern redaction runs over every user-authored text segment, earlier turns included, numbered by first appearance as in section 6.
- Earlier turns therefore produce byte-identical redacted text, and the upstream prefix and its cache stay stable. A policy change shifts the prefix once.
- Assistant turns, tool calls, tool results and thinking blocks are never touched, and any cache markers are preserved.

Even so, a value the agent reads into a tool result is still sent, so **for secrets, block is the recommended action**, not redact. `redact_scope: all`, which includes tool results, is an explicit opt-in with a dashboard warning.

- *User decision:* model detectors judge only *new* segments by default, to keep cost down. Re-judging the whole history every turn would make cost grow quadratically.
- Pattern rules, which are free, scan everything.
- Monitor mode is the "inspect without modifying" mode: nothing sent upstream changes.

## 9. Privacy and recorded metadata

- **Never stored:** matched text, spans, offsets, context, redaction maps, detector inputs, judge rationales or raw scores. A detector request is an ordinary metadata-only execution row.
- **`guardrail_events`:** a new table, partitioned by month like the rest of history. One row per decision:
  - `root_request_id`, workspace, key, stage;
  - policy, rule and revision;
  - detector kind and category;
  - `count`;
  - the action: `monitored|redacted|blocked|stopped|detector_failed|skipped`.

  A blocked input has no execution row, so this table is how Logs shows blocked requests.
- **Visibility** is the same as Logs: members see their own requests, shared admins the whole workspace, personal owners only their own, platform staff Team/Project detail only. *User decision:* platform staff also see guardrail counts for personal workspaces as totals (per action and category, no request detail), the same way they see personal usage totals.
- **Audit:** `audit_events`, which is append-only, records policy changes only, not individual decisions.
- **Metrics:** `gateway_guardrail_decisions_total{stage,detector,family,action}` and `gateway_guardrail_detector_seconds{detector}`.
  - Label values come from fixed sets, with custom rules counted as `custom`.
  - No workspace, key or request labels, per the rule already stated in `metrics.rs`.
- **Logs:** request ID and decision only.

## 10. Cost, latency and capacity

- **Pattern rules** are linear in the bytes scanned. Scanned input is bounded by:
  - the inbound body caps: 2 MiB for generation, `GATEWAY_MAX_BODY_BYTES_*` for other workloads;
  - for output, the guardrail buffer cap (section 7) and the scanner's carry/holdback window.

  An optional per-rule `max_scan_bytes` can lower this further; exceeding it follows `on_error`. Custom patterns are limited to 50 per layer and 512 bytes each.
- **Model rules** send at most `max_detector_input_bytes` per call (**Recommendation:** 64 KiB), one call per rule and stage.
  - They add a model round trip before dispatch, so they run in parallel after the pattern rules.
- **Accounting.** Each detector call is a normal attempt (reservation, settlement, held unknown cost, cost-center snapshot) and counts against budgets, tokens per minute, requests at once and, *user decision*, requests per minute (documented: a request with detector calls can count up to three times).

## 11. Testing

All model tests use mocks: no paid calls.

- **Corpora.**
  - Positives: synthetic secrets assembled at test time, or vendor-documented example keys, so push protection never fires; Luhn test card numbers.
  - Negatives: source code, UUIDs, hashes and base64 data, to measure false positives.
- **Streaming:** every positive split at every byte boundary; no byte of a stop-mode match forwarded; unbounded patterns rejected for stop mode; monitor leaves streams byte-identical.
- **Redaction across turns:** a multi-turn transcript gives a byte-identical upstream prefix every turn; assistant and tool segments never change.
- **Model detectors** (mock System One and chat servers): verdicts, invalid verdicts, timeouts, budget denials, the retry classes of section 4, restricted keys.
- **Accounting** (real PostgreSQL): no reservation for a blocked input; detector attempts have their own linked reservations; a stop is `failed`, not `cancelled`, and keeps its hold; blocks never fail over or touch route health.
- **Privacy.** A canary value goes through every action, then every table, log and metric is scanned for it.
- **Contracts and UI.** Error bodies for the three protocols and for batch lines; browser tests for the dashboard.

## 12. Build plan

Each phase is one issue with tests, review and a PR.

**1. Pattern secrets (input).** Rule storage, layers and composition, management API, cache topic, `guardrail_events`, metrics, Logs, dashboard pages, the `Detector` trait, and secrets with monitor and block for generation and embeddings.
- *Accept when:* documented bodies on all three protocols; nothing reserved; canary test passes; local layers cannot loosen; egress is enforced when composing.

**2. Pattern personal data.** Checksummed categories, redaction (`user_text`, `latest_user_turn`, `all`) and uninspectable handling.
- *Accept when:* the upstream prefix is byte-stable across turns; assistant and tool segments are untouched; the false-positive rate is reported.

**3. Output monitor.** Scanner inside `tracked`, the engine buffer cap, post-`Done` checks.
- *Accept when:* streams are byte-identical with monitor on; findings are recorded.

**4. System One detector.** The guardrail-purpose lookup and admission variant, the semaphore parameter, failure modes and the egress display.
- *Accept when:* linkage and cost are visible; restricted keys work; the retry classes match section 4.

**5. LLM-judge detector.** The `report` verdict and strict validation.
- *Accept when:* invalid verdicts and timeouts follow `on_error`.

**6. Output blocking modes.**
- *Accept when:*
  - for stop-mode rules, whose maximum match length is at most the holdback, no byte of a match is forwarded;
  - for prefix-triggered rules, nothing at or after the trigger is forwarded;
  - outcomes match section 7;
  - batch lines carry the interactive error bodies.

**7. External-detector interface.** Approved endpoints, credential references and prices.
- *Accept when:* an external mock works through the unchanged policy model.

## 13. Decisions on the open questions (2026-10-13)

1. Detector attempts **count toward requests-per-minute** limits; this is documented.
2. **No per-key layer** at first; add a tighten-only one later if needed.
3. **Personal-workspace owners may tighten** their own guardrails (local layer, tighten-only).
4. Redaction covers **all user-authored text on every turn** (`user_text`), never assistant, tool or thinking blocks.
5. Platform staff see **guardrail counts for personal workspaces as totals**, never request detail.
6. Model detectors judge **only new content** by default, to keep cost down.

No open questions remain; implementation starts with phase 1 (section 12) after phase 2 of the program.

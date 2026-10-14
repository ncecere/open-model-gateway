# Guardrails design (issue #36)

**Status:** design only, not implemented. **Date:** 2026-10-14. Decisions marked *user decision* were approved on 2026-10-14. Decisions marked **Recommendation** are proposals in this document and still need review. Today the gateway never inspects prompt or response text. Everything below describes planned behavior.

## 1. Goals and non-goals

**Goals**

- Protect the following, one at a time, each verified before the next (*user decision*):
  - leaked secrets;
  - personal data (mask or block);
  - content moderation;
  - output DLP;
  - prompt-injection signals.
- Off by default. Opt-in per workspace, with optional platform defaults per workspace type, composed the way limits compose today ([governance](governance.md)).
- Be explicit about what is inspected, which detector inspects it, and where text goes when a detector is a model.
- Actions are monitor, redact and block, configured per policy (*user decision*).
- Store no matched text. On a match, record only policy, detector, category and count (*user decision*).
- Keep every existing accounting invariant (AGENTS.md):
  - every upstream attempt has its own reservation;
  - unknown cost keeps its hold;
  - there is no failover after a stream is returned;
  - there is no implicit retry.

**Non-goals**

- Storing or replaying prompts or responses, including for "evidence" of a block.
- Inspecting images, audio bytes or files at rest. These are listed as *uninspectable* (section 6).
- A guarantee. Pattern detectors miss things and model detectors are probabilistic. Guardrails reduce risk; they are not a compliance certificate.

## 2. Threats and use cases

| Protection | Threat | Typical action | Detectors |
| --- | --- | --- | --- |
| **Secrets** | A user or agent pastes credentials into a prompt or tool result: cloud keys, tokens, PEM private keys, JWTs, connection strings, or the gateway's own `omg_…` keys (`apps/gateway/src/auth.rs`). These then reach a third-party provider. | block (apps), monitor (agents) | pattern |
| **Personal data** | Email addresses, phone numbers, card numbers (Luhn), IBANs (mod-97), national IDs or IP addresses are sent to a provider whose data policy is unknown. | redact or block | pattern; names and addresses need System One or the LLM judge |
| **Moderation** | Abusive, self-harm, sexual or violent content, in either direction. | monitor or block | System One, LLM judge (external later) |
| **Output DLP** | The model echoes secrets or personal data from its context, or emits configured confidential terms. | monitor, stop or block | pattern, System One |
| **Prompt-injection signals** | Instructions hidden in retrieved documents or tool results try to steer the model. | monitor (signal), optionally block | pattern heuristics (low precision), System One, LLM judge |
| **Custom DLP** | Project code names, customer identifiers, internal hostnames. | any | pattern (admin-defined) |

Output DLP is not a separate detector. It is the output side of the secrets, personal-data and custom rules.

## 3. Where checks run

### Input

The generation path is `Engine::execute_protocol` in `apps/gateway/src/inference/mod.rs`:

1. Acquire a capacity permit.
2. Look up `deployments`.
3. Filter `candidates` by protocol and adapter.
4. Build the `route_plan`.
5. For each attempt: `repository.admit` (durable reservation), the adapter's `execute_protocol`, then for streams the `tracked` wrapper that settles on `Done`.

Each frontend (`protocols/chat_completions.rs`, `responses.rs`, `messages.rs`) first normalizes the wire body into the canonical `ChatRequest` (`inference/types.rs`).

**Recommendation:** run input inspection inside the engine, after step 3 and before step 4. It runs on the canonical `ChatRequest`, once per root request. Reasons:

- **One place.** Chat, Responses, Messages and gateway-run batch lines all go through it. `jobs/runner.rs` calls the same `execute_protocol`.
- **No wasted detector spend.** Unknown models (`model_not_found`) and unsupported capabilities fail before any detector cost is incurred.
- **Nothing reserved for a blocked input.** Inspection happens before the first `admit`, so a block or redaction decision is made before any reservation for the client's request exists. A blocked input reserves nothing and is charged nothing for the upstream request. Admission also sees the request that is actually sent, including any redaction.
- **Same input for every attempt.** Failover attempts 2–3 reuse the inspected (possibly redacted) request. Each attempt already receives `request.clone()`. Input is never inspected again per attempt.

Non-generation workloads use `Engine::execute_workload` (`inference/workload.rs`). **Recommendation:** add a `Workload::inspectable_text()` method whose default is "uninspectable". Embeddings, rerank, System One and speech input implement it. The check runs at the same point, after candidates and before `route_plan`.

### Detector calls: admission and billing

The System One and LLM-judge detectors (*user decision*) call a model through the gateway's own provider layer:

- System One uses `execute_workload` with a `SystemoneRequest`.
- The LLM judge uses `execute_protocol` with a `ChatRequest`.

Each detector call is therefore its own root request, with its own route plan, reservation, settlement and execution row. It is billed as its own attempt and sends text to that model's provider.

**Recommendation: detector calls are admitted and billed under the requesting key's workspace and key lineage, the same `Principal`.**

- **Cost follows the cause.** Cost reports, cost-center snapshots and budgets stay accurate. A workspace cannot push guardrail spend onto someone else.
- **No global scope.** The alternative, a platform-owned "guardrail workspace", would be one scope touched by every admission. That is the global row AGENTS.md forbids (no installation-wide limits; see [governance](governance.md#no-installation-wide-limits)).
- **Limits keep their meaning.** Budgets, tokens per minute and requests at once all see the real upstream work.

Bookkeeping:

- A detector attempt gets a fresh `root_request_id`. `inference_executions` keeps `UNIQUE(root_request_id, attempt_number)` with attempt numbers 1–3, so detector calls cannot share the client's root.
- A new nullable column, `guardrail_parent_request_id`, links the detector request to the client request.
- Detector calls never run guardrails themselves. A flag on the call prevents recursion.

**Recommendation: model authorization.** A detector model must be available to the workspace (catalog or direct assignment), checked by the live query in `inference/repository.rs`. The key's own model restriction is *not* applied, because the policy chose the model, not the key holder. If the model is unavailable, that is a detector error and is handled by the rule's failure mode (section 4).

**Recommendation: separate capacity.** `execute_workload` calls `try_acquire_owned` on the same engine semaphore that the parent request already holds a permit from. Under saturation, detectors would fail as `Busy`. Detector calls should use a separate bounded semaphore (`GATEWAY_GUARDRAIL_MAX_CONCURRENT`).

**Known cost:** detector spend can happen even though the client's own admission is later denied, for example because the budget runs out between the two. This cannot be avoided without reserving the main attempt first, which would contradict "a blocked input reserves nothing". The order "pattern detectors first, model detectors only if still needed" limits it.

### Output

Output is inspected only for the attempt whose output is delivered. Failover happens only on `Busy`/`UpstreamUnavailable` before anything is returned (`routing::may_failover`), so this is always the last attempt.

An output block is never an error eligible for failover and never triggers a retry. Output modes are covered in section 7.

## 4. Detector interface

One internal trait. Built-in and external detectors (*user decision*) implement it:

```rust
#[async_trait]
trait Detector: Send + Sync {
    fn kind(&self) -> DetectorKind;      // Pattern | Systemone | LlmJudge | External(name)
    fn egress(&self) -> Egress;          // Local | Model { public_model } | Service { name }
    fn categories(&self) -> &[Category]; // fixed families: secret, personal_data, moderation,
                                         // prompt_injection, custom
    async fn inspect(&self, ctx: &DetectContext, input: &Inspection<'_>)
        -> Result<Findings, DetectorError>;
}
```

**Inputs.** `Inspection` is a list of borrowed text segments. Each segment carries:

- a position: message index, role, and part kind (`content`, `tool_arguments`, `tool_result`, `system`, `tool_description`);
- a flag for whether it is *new*, meaning after the last assistant turn.

`DetectContext` carries the principal (for billing), the parent request ID, the deadline and the rule's settings (thresholds, categories).

**Outputs.** `Findings` lists `{category, segment, span, score?}`.

- Spans exist only in memory, so that redaction can be applied. They are never persisted or logged.
- Scores are compared with per-rule thresholds.

**Built-in detector kinds**

- **Pattern.** Compiled rules plus checksums (Luhn, mod-97, key-format checks).
  - Uses Rust's linear-time `regex` crate, so a pattern cannot backtrack catastrophically. This is a new dependency; the crate is not in `Cargo.toml` today.
  - Free, and nothing leaves the gateway.
- **System One.** Builds a `SystemoneRequest` (`inference/workload_types.rs`): `state` is the selected text, plus one `noul` question per category with instructions.
  - The answers carry probabilities, and `valid_for` already validates them.
  - Admission reserves output up to the price's `output_token_limit` (`OutputReservation::PriceCeiling`).
- **LLM judge.** A chat model with a rubric.
  - `ChatRequest` has no structured-output field, and the frontends reject it ([protocol matrix](protocol-matrix.md#chat-completions)). Structured output therefore comes from one function tool, `report`, whose JSON schema is the verdict, called with `tool_choice: Function("report")`. Both are already supported in the canonical request.
  - The text being judged is placed inside delimiters as data. The verdict is validated strictly, and any invalid verdict is a `DetectorError`.
  - `max_output_tokens` is small, so the hold stays bounded.

**Timeouts.** Each rule has `timeout_ms`, default 5 s and maximum 30 s, always capped by the request deadline (`EngineLimits::request_timeout`, 120 s by default). Pattern detectors are synchronous and bounded by size (section 10).

**Failure mode.** Each rule sets `on_error` to `fail_closed` or `fail_open`. These errors trigger it:

- timeout;
- detector unavailable;
- detector model unavailable;
- detector admission denied;
- invalid verdict;
- input too large for the detector.

**Recommendation for defaults:**

- `fail_closed` for **block** and **redact** rules. Sending unchecked text would defeat the rule.
- `fail_open` for **monitor** rules. These record `detector_failed` and the request continues.

Under `fail_closed`:

- If the detector's admission was denied (for example `budget_exceeded`), the client receives *that* denial, because it is the same scope's budget.
- Other errors return `503 guardrail_unavailable`. The request is retryable, with no `x-should-retry: false`.

**External services later.** An external detector implements the same trait, with `Egress::Service`. Examples: OpenAI moderation, Bedrock Guardrails, Presidio.

- It needs an approved endpoint and a credential reference, under the same rules as provider connections: HTTPS, allowlisted destinations, no redirects, no ambient proxies, no implicit retries.
- If it is billed, it needs a price.
- The policy model, findings, actions and frontends do not change.

## 5. Policy model

### Storage and layers

Guardrail policy has the same layer structure as limits (*user decision*):

- **Workspace-type default** (personal/team/project), set by the platform.
- **Platform per-workspace override**, which replaces the type default, as policy overrides do today.
- **Workspace local layer**, set by shared owners or admins, tighten-only.

Tables: `guardrail_rule_sets` (layer, scope, `local_only`, revision) and `guardrail_rules` (one row per rule).

**Recommendation: personal workspaces.** As with limits (`personal_limits_platform_controlled`), personal workspaces take their guardrails from the platform layers only.

**Recommendation: keys.** No per-key layer in the first build (see open questions).

A rule:

```json
{"id":"<uuid>","name":"Secrets","protection":"secrets",
 "detector":{"kind":"pattern","categories":["secret"]},
 "input":{"action":"block","redact_scope":"latest_user_turn"},
 "output":{"mode":"monitor"},
 "threshold":null,"on_error":"fail_closed","timeout_ms":null,
 "workloads":["generation","embeddings"],"uninspectable":"deny"}
```

### Composition (tighten-only)

The effective policy is the platform layer's rules plus the local layer's rules. Where both cover the same protection and stage, the stricter setting wins. Strictness orderings:

- input action: off < monitor < redact < block;
- output mode: off < monitor < stop-mid-stream < block-before-sending;
- failure mode: `fail_open` < `fail_closed`;
- detector threshold: a lower threshold is stricter;
- egress: `local_only: true` is stricter.

A local layer may add rules or strengthen rules. It may not remove or weaken a platform rule.

**Egress rule.** Adding a model detector *loosens* privacy, because text leaves the gateway. A local layer under a platform `local_only` therefore cannot add System One, judge or external detectors.

### Caching

Today, limit policies are deliberately not cached ([governance](governance.md#caches-and-change-notifications)). Guardrails would be read on every request before admission.

**Recommendation:** cache the effective rule set per replica under a new `guardrails` notification topic, added by migration. It inherits the existing behavior:

- invalidation through LISTEN within milliseconds, at most about 1 s through the poll;
- bypass when versions cannot be confirmed for 3 s.

The guardrail check is not part of the admission lock order and takes no authority locks.

### Management API sketch

All routes use the management error envelope `{error:{code:"<status>",message,reason?}}`.

| Route | Who |
| --- | --- |
| `GET/PUT /platform/workspace-types/{kind}/guardrails` | Admin writes; Auditor reads |
| `GET/PUT/DELETE /platform/workspaces/{ws}/guardrails` | Override. DELETE restores the type default. |
| `GET/PUT /workspaces/{ws}/guardrails` | Shared owner/admin tightening. GET also returns `effective`, `provenance` and `egress`. |
| `GET /platform/guardrails/detectors` | Built-in kinds, categories, egress |

Rejection reasons:

- `guardrail_loosen_not_allowed` (403, adds `rule`);
- `guardrail_egress_not_allowed` (403);
- `guardrail_detector_unavailable` (400, the model is not available or not a System One/chat model);
- `personal_guardrails_platform_controlled` (403).

Changes are audited as `guardrail_policy.updated`. The audit metadata records the layer, rule IDs, protections, actions and output modes. For custom patterns it records the **number** of patterns, never their content.

### Dashboard

- **Workspace › Guardrails:** the effective rules with provenance and a plain sentence for each rule: *what* is inspected and *where it goes*. For example: "New user messages are sent to `company/jev` (OpenRouter) for moderation."
  - When block-before-sending is on, a warning reads "Streaming responses are held until checked".
- **Admin › Guardrails:** type defaults and overrides.
- **Logs:** a badge and filter (`guardrail=monitored|redacted|blocked`); request detail shows the findings and linked detector requests with their cost.
- **Usage & costs:** a `purpose: client|guardrail` breakdown.

## 6. Client-visible behavior

**Recommendation: error shapes.** Errors follow `protocols/mod.rs`: OpenAI envelope `{error:{message,type,code,param}}`; Anthropic `{type:"error",error:{type,message}}`. Both gain one extra object, `guardrail`:

```json
{"error":{"message":"Blocked by guardrail policy \"Secrets\" (category: secret).",
 "type":"invalid_request_error","code":"guardrail_input_blocked","param":null,
 "guardrail":{"policy_id":"<uuid>","rule_id":"<uuid>","stage":"input","category":"secret",
              "detector":"pattern"}}}
```

Messages name the policy and the category family. They never include matched text, offsets or detector scores. `InferenceError` stays `Copy`: the new variants carry only IDs and fixed enums.

| Case | HTTP | OpenAI `code` / Anthropic `type` | Headers |
| --- | --- | --- | --- |
| Input blocked | 400 | `guardrail_input_blocked` / `invalid_request_error` | `x-should-retry: false` |
| Output blocked (non-stream) | 400 | `guardrail_output_blocked` / `invalid_request_error` | `x-should-retry: false` |
| Enforcing policy, uninspectable workload, `uninspectable: deny` | 400 | `guardrail_not_supported` / `invalid_request_error` | `x-should-retry: false` |
| Detector failed, `fail_closed` | 503 | `guardrail_unavailable` / `api_error` | none |
| Detector admission denied, `fail_closed` | that denial (e.g. 429 `budget_exceeded`) | unchanged | unchanged |

**Recommendation:** an output block is an explicit error, not a fabricated 200 response with `finish_reason: content_filter`. The gateway did not receive that response from the provider, and AGENTS.md forbids fabricated responses.

**Redaction token.** `[REDACTED:<CATEGORY>_<n>]`, for example `[REDACTED:EMAIL_1]`. Within one request, the same value gets the same number (an in-memory map that is discarded when the request ends), so the model can still tell that two mentions refer to the same thing. Redaction never touches JSON structure. A match inside tool-call arguments is treated as monitor or block, never redact.

**Per workload**

- **Chat, Responses, Messages:** everything above. Streaming errors use each frontend's existing failure path:
  - Chat sends a `data:` error body and no `[DONE]`;
  - Responses sends an `error` event;
  - Messages sends an `error` event.
- **Embeddings, rerank, System One (client calls), speech input:** input checks only, because the output is vectors, scores, answers or audio. Redacted embeddings are embeddings of the redacted text.
- **Images, audio transcription input, realtime:** uninspectable. An enforcing rule covering these workloads refuses them with `guardrail_not_supported`:
  - images and transcription before admission;
  - realtime with 400 before the WebSocket upgrade.

  `uninspectable: allow` lets them through and records a `skipped` finding.
- **Batches:**
  - Gateway-run lines pass through `execute_protocol`. A blocked line produces the same body as the interactive request in the error file (`batch_line_error`).
  - Native batches send the whole file upstream. Their pattern rules run at create time beside the existing line validation, and a block rejects the batch with 400 `guardrail_input_blocked` plus line numbers.
  - Rules that need model detectors or output blocking refuse native batches (`guardrail_not_supported`) rather than silently switching to gateway-run with different prices.
  - Output monitor and block run per line when results are collected.

## 7. Output modes and streaming

Output mode is set per policy (*user decision*).

- **Off.**
- **Monitor.** The stream is unchanged.
  - Pattern detectors scan the text deltas in parallel with forwarding, keeping a bounded carry-over window so that matches split across chunks are found.
  - Model detectors run after `Done` on the accumulated text, which is bounded by the existing 4 MiB `Accumulator` limit. They run in the bounded guardrail pool. When the pool is full they are skipped and recorded as `skipped`. They are skipped if the client cancelled.
  - The text is dropped as soon as the verdict returns.
- **Block-before-sending.** The engine wraps `tracked`, buffers events until `Done`, inspects, then either replays the events or yields `GuardrailOutputBlocked`.
  - The upstream attempt has already settled as `succeeded` with its usage: the provider did the work and charges for it.
  - Frontends have already sent only content-free opening events (Chat's role chunk; `response.created`/`in_progress`; `message_start`). Their 15 s SSE keep-alives keep idle proxies from closing the connection.
  - The client sees no tokens until the end. That is why the dashboard must show the mode.
- **Stop-mid-stream.** The gateway forwards text with a small holdback, so a match shorter than the holdback (default 256 bytes, at least the longest pattern) is caught before any of it is sent.
  - On a match, it drops the upstream stream (which cancels upstream work) and emits the protocol's error event.
  - Sent tokens cannot be retracted.
  - The attempt is recorded as `failed` with `error_code: guardrail_output_blocked`. Usage is usually not reported, so the hold is retained as unknown cost.
  - Responses and Messages already buffer tool calls until the end ([protocol matrix](protocol-matrix.md#streaming-and-evidence-boundaries)), so tool calls are checked whole. Chat streams tool-call deltas, so the holdback applies to them too.
  - **Recommendation:** pattern detectors only in this mode. Judging every window with a model would multiply cost.

## 8. Coding agents

Coding agents and harnesses (Claude Code and all others) resend the whole conversation on every turn. They rely on byte-identical history.

**Today.** `ChatRequest` carries no cache markers, thinking blocks or signatures. `/v1/messages` rejects `cache_control`, thinking and `anthropic-beta` ([protocol matrix](protocol-matrix.md#messages)). The design must still hold once those are supported.

**What redaction breaks**

- **Prompt caching.** Provider caches match exact prefixes. Changing earlier turns changes the prefix, which turns cache reads into cache writes (more cost). It also changes the prefix hash that the proposed cache-aware routing would key on.
- **Thinking signatures.** Signed thinking blocks become invalid if anything in them changes, and the provider rejects the request.
- **Tool history.** The model sees `[REDACTED:…]` in a file it read. It can then write the placeholder back into source code through its edit tool, silently corrupting the user's files.
- **Divergence.** The client's history holds the original text while the model saw something else.

**Policy (*user decision*).** Recommend monitor or block for agent traffic. For redaction, the default `redact_scope` is `latest_user_turn`: it touches only new user-authored text after the last assistant turn, never earlier assistant turns, tool calls, tool results or `cache_control`-marked blocks.

- **Recommendation:** `redact_scope: all` is an explicit option, with a dashboard warning about caching and agents.
- **Recommendation:** model detectors inspect only *new* segments by default. Re-judging the whole history on every turn would make detector cost grow quadratically with conversation length. Pattern detectors, which are free, scan everything.
- Monitor mode is the "inspect without modifying" mode. Nothing that is forwarded upstream changes.

## 9. Privacy and recorded metadata

- **Never stored:** matched text, spans, offsets, surrounding context, redaction maps, detector inputs, judge rationales or raw scores.
  - The detector request itself is an ordinary execution row: metadata only, like every request today.
- **On each decision:** a new `guardrail_events` table, partitioned by month like the rest of history, records:
  - `root_request_id`, workspace, key and stage;
  - policy, rule and revision;
  - detector kind;
  - category;
  - `count`;
  - action taken (`monitored|redacted|blocked|stopped|detector_failed|skipped`).

  A blocked input has no upstream execution row, so this table is what lets Logs show the blocked request.
- **Visibility** matches Logs:
  - members see their own requests;
  - shared admins see the whole workspace;
  - personal workspaces are visible to the owner only;
  - platform staff see Team/Project details only.
- **Audit** (`audit_events`, append-only) records policy changes, not individual decisions. Per-request decisions would bloat the immutable log.
- **Metrics:** `gateway_guardrail_decisions_total{stage,detector,family,action}` and `gateway_guardrail_detector_seconds{detector}`.
  - Labels come from fixed sets only. Custom rules fold into `custom`.
  - No workspace, key or request labels, as already required by `metrics.rs`.
- **Logs:** request ID and decision only, never text.

## 10. Cost, latency and capacity

- **Pattern detectors** are linear in the bytes scanned, and the scanned size is already bounded:
  - input: by the inbound body caps (2 MiB for generation; per-workload `GATEWAY_MAX_BODY_BYTES_*`);
  - output: by the 4 MiB output accumulator;
  - streaming: scanner state is limited to the carry window.

  An optional `max_scan_bytes` per rule can be lower. Exceeding it is a detector error and follows `on_error`. Custom patterns are bounded to 50 per layer and 512 bytes each.
- **Model detectors** are limited per call by `max_detector_input_bytes` (**Recommendation:** 64 KiB), with one call per rule and stage. They add a full model round trip before dispatch: run them in parallel across rules, after the pattern rules.
- **Accounting.** Each detector call is a normal attempt: reservation, settlement, unknown cost keeps its hold, and cost-center snapshot. It counts against budgets, tokens per minute and requests at once of the same scopes (open question 1 covers requests per minute).

## 11. Testing

- **Pattern corpora.**
  - Positives: synthetic secrets assembled at test time from fragments, or vendor-documented example keys, so that GitHub push protection is never triggered; Luhn test card numbers.
  - Negatives: source code, UUIDs, hashes and base64 data, to measure false positives.
- **Streaming.** Property tests split each positive at every byte boundary, for both monitor and stop-mid-stream modes. A test shows that no matched byte is forwarded under holdback.
- **Model detectors.** Mock System One and chat servers (the existing provider mock tests) cover verdicts, invalid verdicts, timeouts, budget denials and both failure modes. No paid calls.
- **Accounting** (real PostgreSQL):
  - a blocked input creates no reservation for the client's request;
  - detector attempts have their own reservations, linked through `guardrail_parent_request_id`;
  - stop-mid-stream retains the hold;
  - a block never triggers failover.
- **Privacy.** A canary string is pushed through every action. Then the test scans every table, log line and metric for it.
- **Contracts.** Error bodies for all three protocols and for batch lines. Browser tests for the dashboard pages.

## 12. Build plan

Every phase ships through the usual loop: one issue, tests, review, PR.

1. **Pattern secrets (input).** Rule storage, layers, management API, cache topic, `guardrail_events`, metrics, Logs badge, Workspace/Admin pages, the `Detector` trait, and the secrets pattern set with monitor and block for generation and embeddings.
   *Accept when:*
   - blocks return the documented bodies on Chat, Responses and Messages;
   - nothing is reserved;
   - the canary privacy test passes;
   - local layers cannot loosen.
2. **Pattern personal data.** Checksummed PII categories, redaction (`latest_user_turn`, `all`), and uninspectable handling.
   *Accept when:*
   - redaction is deterministic within a request;
   - tool arguments and results are never rewritten under the default scope;
   - the false-positive rate on the negative corpus is reported.
3. **Output monitor.** Streaming scanner with carry-over; post-`Done` checks.
   *Accept when:* streams are byte-identical with monitor on and findings are recorded.
4. **System One detector.** Detector calls are separately admitted and billed, using the guardrail pool; failure modes; egress display.
   *Accept when:*
   - parent/child linkage and cost are visible in Logs and costs;
   - `fail_closed` and `fail_open` are tested on mocks.
5. **LLM-judge detector.** Rubric, `report` tool verdict, strict validation.
   *Accept when:* invalid verdicts and judge timeouts follow `on_error`.
6. **Output blocking modes.** Block-before-sending and stop-mid-stream.
   *Accept when:*
   - no blocked byte is forwarded under holdback;
   - settlement states match section 7;
   - batch lines carry the interactive bodies.
7. **External-detector interface.** Approved endpoints, credential references and pricing for external services; first adapter chosen then.
   *Accept when:* an external mock detector works through the unchanged policy model.

## 13. Open questions

1. Should detector attempts count toward **requests-per-minute** limits? They are real upstream requests, but each would inflate a client's count up to 3×. Recommended: yes, and document it.
2. Should there be a tighten-only **per-key** layer (for example, agent keys on monitor while app keys redact)? Recommended: after phase 2, if needed.
3. Is HTTP 400 `guardrail_output_blocked` acceptable for output blocks, rather than a 200 with `content_filter`?
4. Should platform staff see **guardrail counts** for personal workspaces as totals, the way they see usage totals?
5. Is it acceptable that by default, model detectors judge only *new* content and not the full history?

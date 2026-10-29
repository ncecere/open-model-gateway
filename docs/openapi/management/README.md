# Management OpenAPI contract maintenance

[management.openapi.json](../management.openapi.json) is an **incomplete management contract** for #81, not yet a finished client-generation contract. The first 22 operations have response-validation coverage; other areas remain explicitly incomplete. The test-only generator reads compiled `route_policy::ROUTES`, expands declared methods and includes exactly `/api/v1/*`. Inference, SCIM, health and metrics are excluded; anonymous authentication/branding operations and authorized installation-policy tombstones are included. Implicit HEAD follows GET policy without synthetic operations. `info.version` is the gateway package version.

The contract has 251 operations, 40 shared error reasons and **458 incomplete request/response schema slots** (502 → 458: 44 completed slots). Completed areas are auth (4 operations/8 slots), branding (1/2), invitations (1/2), me (15/30), and access-request-targets (1/2). `management_openapi_reports_incomplete_schemas` pins progress; later slices must reduce the baseline. #81 stays open until exhaustive schema and real-handler coverage is complete with no placeholders.

## Run the gates

From the repository root:

```sh
cargo test -p open-model-gateway --lib management_openapi -- --nocapture
```

Normal tests regenerate in memory and compare exact deterministic bytes with the committed artifact. To write it explicitly, after reviewing fragment changes:

```sh
OMG_REGENERATE_OPENAPI=1 cargo test -p open-model-gateway --lib management_openapi_artifact_matches_regeneration
```

Writing is forbidden whenever `CI` is set; there is no regeneration default, network access or production endpoint. Regeneration validates the document before writing. Review and commit the JSON diff; never regenerate in CI to conceal drift.

## Reviewed fragments

Every `.json` file in this directory has the following shape (this example is not a complete inventory):

```json
{
  "operations": {
    "GET /api/v1/me": {
      "access": "Browser",
      "token_policy": "Denied",
      "scope": null,
      "operation": {}
    }
  },
  "schemas": {}
}
```

The operation fragments collectively register every operation (`identity-operations.json` contains the first completed areas; `identity-schemas.json` contains their hand-written schemas). When adding/changing a management route, update its exact method/template and access/token/scope snapshot **in the same PR**. Route-only changes fail even with artifact regeneration. These snapshots are review acknowledgements, not a second authorization source: extensions and security requirements always come from compiled `Access`/`TokenPolicy`. Missing/unmatched entries, duplicate operation/schema fragments (including duplicate top-level map keys), unknown fragment fields and overrides of generated authority/IDs are errors.

`operation` supplies hand-written OpenAPI fields such as summaries, descriptions, responses and request bodies. Generated path parameters are immutable; fragments may append inline non-path parameters. Schema fragments go in `schemas`; they may replace a generated placeholder by its exact component name. The shared `ErrorEnvelope` is generated and cannot be overridden.

Body slots are referenced by operation extensions `x-omg-request-schema` and `x-omg-response-schema`. Each initially points to a distinct component marked `x-omg-schema-status: incomplete`. They deliberately do **not** fabricate request-body presence, JSON media types, successful statuses or universally enveloped errors; the default response is descriptive only. Later slices add real `requestBody`/`responses`, body absence, media types, query parameters, DTO constraints and privacy-specific variants. Replacing a slot must remove its incomplete marker only after the corresponding behavior is tested. Placeholders are never acceptance validators for actual requests/responses.

Access extensions express minimum boundaries, not complete authority. Live ownership, membership, platform roles, hidden 404s and personal/shared-workspace visibility remain enforced by handlers. The shared error schema documents the handler envelope and generated reason vocabulary, not every framework extraction response or every reason-specific detail. Existing source-route and authorization tests are unchanged.

## Offline schema safety

The dev-only validator uses explicit Draft 2020-12 and the linear-time `regex` backend with a compilation-size limit. Invalid/unsupported regexes (including look-around/backreferences) fail schema compilation; RED controls exercise both `pattern` and `patternProperties`, including under `not`. Every component, including unused fragments, and inline Schema Object is checked before the artifact can be written. Identical ordinary Schema Objects share compilation only within that document-validation call (nested `$id` rebasing is forbidden; dynamic/anchor schemas are not deduplicated). Artifact references must be local `#/...` JSON Pointers; nested `$id` rebasing is forbidden.

The [vendored metaschemas](../vendor/README.md) are SHA-256 checked before loading. Only their exact identifiers resolve to checked-in bytes; unknown URI retrieval is refused, with HTTP/file/default features disabled. The tests check OpenAPI structure, schema syntax, references, path parameters, authority, reason coverage, fragment matching and artifact drift. Format assertions are enabled; UUID assertions use the actual serde UUID parser (including compact, braced and URN spellings). Webhook rotation requests use `omg-chrono-date-time`, checked against chrono's real serde parser, including relaxed RFC 3339 spellings; response timestamps retain canonical `date-time` schemas.

## Completed-area response and request gates

The test-only `management_openapi/responses.rs` harness captures actual status, headers and bytes before JSON fallback. `validate(document, method, template, status, headers, bytes)` resolves an exact operation and status, checks required/declared headers and cookies, selects the media type, and validates JSON with the offline Draft 2020-12 validator. Text and binary representations are checked separately. Empty bytes are not JSON null; implicit HEAD uses GET's contract without inventing an operation. Object-valued error envelopes must carry the actual status string in `error.code`; identity's string-valued errors and framework text rejections retain their real contracts.

Operations in `AREA_GROUPS` reject any reachable incomplete slot, including request slots and referenced schemas. `PositiveFixtures` records executed real-handler observations, not claimed fixture names: each completed area's observed operation set must equal its artifact inventory. `AREA_GROUPS` contains named `(constant_name, AREAS_CONSTANT)` entries. A Rust source inventory requires every registered constant to appear directly in `finish(AREAS_CONSTANT)` inside a `#[sqlx::test]` runner; helper-only calls, comments and per-area loops do not satisfy the binding. Groups remain disjoint and every area must match artifact operations. Actual workspace templates use `{ws}`, e.g. `workspaces/{ws}/keys`. Denials, implicit HEAD and 304 do not earn representation coverage. Body-bearing operations require a validated nonempty successful representation. Empty 204 successes and auth 303 redirects need explicit `x-omg-positive-kind` annotations; a cookie-clearing callback redirect still does not count as sign-in. Middleware-only stub matrices retain their independent status assertions and are deliberately separate from real-handler response validation/positive coverage. Intentional 410 tombstones need an explicit positive annotation. The all-feature `management_openapi_completed_area_fixtures` PostgreSQL test executes all completed areas, including a running loopback gateway, populated reads, redaction variants, all three logo media types and 304, OIDC redirects/cookies and empty logout.

```sh
# DATABASE_URL must select an explicitly authorized PostgreSQL test fixture;
# sqlx creates isolated test databases. No migrations of an existing installation.
cargo test -p open-model-gateway --lib --all-features management_openapi
```

The three JSON request DTOs (invitation acceptance, access-request submission, notification read) have schema-versus-real-serde vectors for unknown/nested-unknown, missing, wrong-type, null/omission, UUID forms and serde's accepted positional struct arrays, plus real extractor refusal probes after successful controls. A bounded field-name inventory from the actual derived deserializer also detects newly added optional fields; unsupported custom/flattened shapes fail rather than guess. Both object and positional forms are exercised through real handlers; no production deserialization behavior is changed. These DTOs have no nested object DTO; nested-object probes cover their scalar/array-item boundaries. Closed fixed projections reject unexpected fields. Stored audit metadata and incident details remain JSON values/maps, as in the actual handlers, not invented closed DTOs; admission-ceiling identity redaction has distinct constrained variants. Full recursively reachable DTO binding and validation of the remaining areas are still later #81 work.

### Shared serving-layer refusals and schema quality

`management_openapi/middleware.rs` adds the serving-layer contracts to every applicable operation, including future slices. Management authentication can refuse duplicate/mixed credentials or invalid/expired/revoked sessions/tokens (401), browser Origin/CSRF (403), bearer per-IP/replica/per-token throttles before route policy (429), and authentication storage failure (500). Authorization adds token-policy/scope/IP/role refusals (403), workspace/key hiding (404 where applicable), storage/audit failures (503), and token-readiness refusals (501 only for an unready Scope route). Logout verifies its own session/CSRF and does not use management bearer throttles. The serving body cap also documents 413 text responses. Handler variants are retained rather than replaced with fabricated universal envelopes.

Identity refusals carry the real string-valued JSON error. `x-omg-body-header-requirements` binds those alternatives to required `Cache-Control: no-store` / `Referrer-Policy: no-referrer`, plus a positive integer `Retry-After` for authentication throttles. This conditional extension is necessary because OpenAPI cannot condition Header Object requiredness on a body alternative; a handler's export-rate-limit envelope, for example, does not send Retry-After. DB-free raw-wire tests exhaust the real authentication limiter with a closed pool, so they cannot connect to a database.

Completed components (including unused ones), inline schemas, parameters, headers and every composition/conditional branch reject `true`, `{}` and unbounded/open object schemas. Objects must be closed and explicitly declare `required`; the all-optional ReadInput object deliberately declares `required: []` to preserve serde behavior. Every array, including nullable arrays, must constrain `items`: closed objects, typed scalar/enum/pattern schemas, or recursively constrained arrays. Fixed positional tuples may use `items:false` and constrained `prefixItems`. Missing/`true`/`{}` items and permissive `prefixItems`, `unevaluatedItems`, `additionalItems` or `contains` are forbidden. Conjunctive refinements inherit the closed base; disjunctions cannot introduce an unconstrained escape. Wildcard `patternProperties` and schema-valued `additionalProperties`/`unevaluatedProperties` require an exact reviewed `ARBITRARY_JSON` pointer and matching justification, even when map values are typed. Every completed operation has missing-required, wrong-type and sensitive-extra response mutation controls; recursive controls inject an unexpected property into every populated nested closed object and a leak-bearing unconstrained element into every array, and require each mutation to fail (wire-header/body controls for non-JSON operations). Nullable object/array examples are populated rather than hidden behind a leading null.

Only these exact stored JSON locations are allowlisted in `quality.rs`; each carries a matching justification `$comment`, and the exception cannot be moved or referenced as a standalone response schema:

| Location under `#/components/schemas/` | Why it is arbitrary |
| --- | --- |
| `IdentityNotification/properties/details` | Non-ceiling `alert_events.details` is stored producer-specific JSON. Its existing monetary `patternProperties` is reviewed here: it only tightens micro-USD values and does not authorize identities in ceiling details, which are closed by the kind conditional. |
| `SettingsAlertEvent/properties/details` | Non-ceiling history returns stored `alert_events.details` verbatim through `EVENT_JSON`; the admission-ceiling event variants refine it with the closed `SettingsCeilingDetails*` schemas (personal, Team/Project and redacted). |
| `get_api_v1_me_exportResponse/properties/audit/properties/items/items/properties/metadata` | The subject export returns stored `audit_events.metadata` verbatim; keys depend on the audited action. |

Admission-ceiling `details` always uses the closed `IdentityCeilingDetails` producer schema from `alerts::ceiling_conditions`: scope, personal flag, workspace kind, window/count/rate, rate/wait triggers and nullable thresholds/p95; only `workspace_name` may be omitted by redaction. Other notification kinds retain arbitrary stored details. Admin admission-ceiling notifications explicitly suppress personal identities: `personal=true` forces `workspace: null` and null/absent `workspace_name`; `details.workspace_id`, `owner_email`, nested `workspace` and all other unknown fields are forbidden. Shared references can only identify Team/Project workspaces. Auditor variants always remove workspace identities. Runtime validators and completed-operation checks cache only the immutable committed artifact by pointer identity; mutated-document controls always compile and check afresh.

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

The [vendored metaschemas](../vendor/README.md) are SHA-256 checked before loading. Only their exact identifiers resolve to checked-in bytes; unknown URI retrieval is refused, with HTTP/file/default features disabled. The tests check OpenAPI structure, schema syntax, references, path parameters, authority, reason coverage, fragment matching and artifact drift. Format assertions are enabled; UUID assertions use the actual serde UUID parser (including compact, braced and URN spellings).

## Completed-area response and request gates

The test-only `management_openapi/responses.rs` harness captures actual status, headers and bytes before JSON fallback. `validate(document, method, template, status, headers, bytes)` resolves an exact operation and status, checks required/declared headers and cookies, selects the media type, and validates JSON with the offline Draft 2020-12 validator. Text and binary representations are checked separately. Empty bytes are not JSON null; implicit HEAD uses GET's contract without inventing an operation. Object-valued error envelopes must carry the actual status string in `error.code`; identity's string-valued errors and framework text rejections retain their real contracts.

Operations in `AREA_GROUPS` reject any reachable incomplete slot, including request slots and referenced schemas. `PositiveFixtures` records executed real-handler observations, not claimed fixture names: each completed area's observed operation set must equal its artifact inventory. `AREA_GROUPS` contains named `(constant_name, AREAS_CONSTANT)` entries. A Rust source inventory requires every registered constant to appear directly in `finish(AREAS_CONSTANT)` inside a `#[sqlx::test]` runner; helper-only calls, comments and per-area loops do not satisfy the binding. Groups remain disjoint and every area must match artifact operations. Actual workspace templates use `{ws}`, e.g. `workspaces/{ws}/keys`. Denials, implicit HEAD and 304 do not earn representation coverage. Body-bearing operations require a validated nonempty successful representation. Empty 204 successes and auth 303 redirects need explicit `x-omg-positive-kind` annotations; a cookie-clearing callback redirect still does not count as sign-in. Middleware-only stub matrices retain their independent status assertions and are deliberately separate from real-handler response validation/positive coverage. Intentional 410 tombstones need an explicit positive annotation. The all-feature `management_openapi_completed_area_fixtures` PostgreSQL test executes all completed areas, including a running loopback gateway, populated reads, redaction variants, all three logo media types and 304, OIDC redirects/cookies and empty logout.

```sh
# DATABASE_URL must select an explicitly authorized PostgreSQL test fixture;
# sqlx creates isolated test databases. No migrations of an existing installation.
cargo test -p open-model-gateway --lib --all-features management_openapi
```

The three JSON request DTOs (invitation acceptance, access-request submission, notification read) have schema-versus-real-serde vectors (`requests::vectors`: unknown/nested-unknown, missing, wrong-type, null/omission, UUID forms and serde's accepted positional struct arrays), plus real extractor refusal probes after successful controls. A bounded field-name inventory from the actual derived deserializer also detects newly added optional fields; unsupported custom/flattened shapes fail rather than guess. Contract and serde must agree **exactly**, with one reviewed exception: see `SEMANTIC_REFINEMENTS` below. Both object and positional forms are exercised through real handlers; no production deserialization behavior is changed. Stored audit metadata and incident details remain JSON values/maps, as in the actual handlers, not invented closed DTOs; admission-ceiling identity redaction has distinct constrained variants. Full recursively reachable DTO binding and validation of the remaining areas are still later #81 work.

## Coverage and HTTP-vector gates

`PositiveFixtures::finish(GROUP)` checks more than conformance; for every completed operation of the group it requires **coverage** of what real handlers were seen to accept and return, and **executed HTTP boundary vectors**. Everything below is test-only; messages name methods, templates, statuses, vector kinds, parameter names and JSON pointers, never a sent or captured value.

**Area groups.** Real workspace templates use `{ws}`, not `{workspace}`; #81 slices register one `SLICE<N>_AREAS` constant (for example `workspaces/{ws}/keys`) in `AREA_GROUPS` and pass it directly to `finish` inside a `#[sqlx::test]`.

**Coverage** (`coverage.rs`), from POSITIVE observations only (2xx, or a reviewed 204/303; denials, HEAD and 304 earn nothing):

- *Query parameters*: each documented query parameter is carried by at least one positive request.
- *Arrays*: every response array location (indices wildcarded, e.g. `/data/*/outcomes`; typed map values at `<map>/*`) is non-empty at least once.
- *Branches*: every `oneOf`/`anyOf` branch and both sides of every nullable (`type:[X,"null"]`, `null` in an enum) is the matching branch of an observed value at that data location. `anyOf` credits every branch an observed value matches; `if/then/else` arms are not separate branches (their contents are walked); `not` and `contains` have nothing to observe. Typed `patternProperties` / schema-valued `additionalProperties` values are walked at `<map>/*`; a name is credited to `additionalProperties` only if no `properties` or `patternProperties` entry selects it.
- A `$ref` followed twice at the same data location is a reported cycle, not a stack overflow.
- *Stale exemptions fail.* An `x-omg-may-be-empty` array that WAS non-empty, an `x-omg-unobserved-branch` whose branch (or nullable side) WAS observed, and an `x-omg-unobserved-parameter` that a positive request carried are all refused at `finish`: delete the exemption.

**Exemptions** (each needs a precise, non-empty reason; `quality.rs` refuses empty ones and misplaced ones):

| Annotation | Where | Exact semantics |
| --- | --- | --- |
| `x-omg-unobserved-parameter: "<reason>"` | a query parameter | No positive request can ever carry it (the handler always refuses a request that does). It is **stale**, and refused, once any positive request accepted the parameter. Its HTTP vectors are still required. |
| `x-omg-may-be-empty: "<reason>"` | an array schema | That one array location may stay empty in every observation. Stale (refused) once the array was non-empty in a positive response. |
| `x-omg-unobserved-branch: "<reason>"` | a `oneOf`/`anyOf` branch, or a nullable node | On a branch: that branch need not be observed. On a nullable node the reason must read `null: <reason>` or `non-null: <reason>` and exempts only that side; a nullable union branch is ambiguous (give the null its own branch). To exempt a shared nullable at one location, use an `anyOf` whose branch carries the annotation. Stale (refused) once that branch or side was observed. |
| `x-omg-unprobed-location: "<reason>"` | a location of a JSON request schema: a property, a map value, a positional or tail item, or a `oneOf`/`anyOf` arm | That declared request location need not be carried by any valid body or seed of the operation (and neither need its contents). Refused outside a request body schema. |
| `x-omg-unsendable-boundary: "<reason>"` | a query parameter object, or a property (or positional item) declared directly on a request body's root schema or one of its union arms | The decisive length or item bound of this location cannot be sent over HTTP (see *Nothing is dropped silently*); the reviewed reason says what covers it instead. Refused on any other location. |
| `x-omg-pattern-boundaries: "<reason>"` | a string schema with a `pattern` | The pattern has no length bounds the harness can derive and the fixture supplies no candidates; the reason says where its boundaries are exercised instead (see *Patterns without derivable bounds*). |

**Placement rules for every annotation above.** A reviewed exemption excuses one gap at one location owned by ONE operation. `quality.rs` refuses an exemption (`x-omg-may-be-empty`, `x-omg-unobserved-branch`, `x-omg-unobserved-parameter`, `x-omg-unprobed-location`, `x-omg-unsendable-boundary`, `x-omg-pattern-boundaries`) anywhere, at any depth, under a component schema that more than one place references; the count is transitive (a component referenced once by a component referenced twice applies twice), and the generated `x-omg-request-schema`/`x-omg-response-schema` aliases do not count as places. Annotate the inline location, or move the schema into a component referenced once. Each needs a non-empty reason.

`x-omg-max-bytes: <n>` is a separate reviewed extension on a string schema: the handler limits UTF-8 **bytes** while JSON Schema `maxLength` counts characters (so `maxLength` stays as a necessary bound, never larger than `n`). The vector oracle refuses values over the byte limit (on every array element, positional item, and schema-valued map value as well as properties) and sends multibyte values around both limits; the callback `code` (8192 bytes) uses it. It must state a limit that always applies, so `quality.rs` refuses it on a `oneOf`/`anyOf` branch itself and anywhere under `if`/`then`/`else`/`not`; a property, item or map value inside a branch may carry its own.

### Fail closed: what the vector harness never skips

- **Nothing is dropped silently.** A candidate the harness cannot send (the request-target limit of 60000 percent-encoded query bytes, measured on the REAL encoded request rather than estimated; the 100000-character string cap, 20000 for multibyte; the 1000-item array cap) is recorded. If it is a DECISIVE boundary (`minLength`, `minLength-1`, `maxLength`, `maxLength+1`, multibyte `maxLength`/`maxBytes` (+1), `minItems`, `minItems-1`, `maxItems`, `maxItems+1`) and NO vector of that kind could be sent for the location, the run fails with operation, location and kind (never a value) unless the query parameter or direct body property carries `x-omg-unsendable-boundary`. Other candidates (pattern lengths, one multibyte variant of several) are only counted; a run prints the counts, nothing else.
- **Declared locations are walked.** The walk mutates what the valid body (or a seed) carries, so `body_vectors_*` also records which DECLARED request locations (schema paths such as `/properties/reason`, `/oneOf/1/prefixItems/2`, `/properties/m/additionalProperties`) the valid body and the seeded bases carried: every optional property, schema-valued map value (`patternProperties`/`additionalProperties`), positional index (and the `items` tail after `prefixItems`) and `oneOf`/`anyOf` arm the body matched. `finish` fails on each declared location that no vector run of the operation carried, unless it has `x-omg-unprobed-location`. Run `body_vectors_*` once per wire form: the positional arm of a `oneOf` object/positional body is a location, so a run of the object form alone fails; a cross-field arm needs a valid body that matches it (for example `{"all":true,"ids":null}`). Seeded walk bases may be cross-field INVALID (`{"ids":[x],"all":true}` is a refusal vector, not a valid body), so a valid base that matches the arm is always walked too: seeds add reach, they never replace valid bodies.
- **Sibling consts and enums.** The `const` values and `enum` members of every `oneOf`/`anyOf` branch under a location (matching or not) are sent as "enum member" vectors, so `oneOf: [{const: "month"}, {const: "day"}]` with a handler refusing "day" fails.
- **Patterns without derivable bounds.** A string location whose `pattern` is not one character class under a quantifier (`simple_bounds`), with no `maxLength`/`minLength`/`x-omg-max-bytes`, cannot have its length boundaries derived (for example `^(0|[1-9][0-9]{0,14})$`). The fixture must supply boundary `candidates` (`body_vectors_extended(…, seeds, candidates, send)`: `(pointer, value)`; `query_vectors_extended(…, seeds, candidates, …)`: `(parameter, value)`), whose verdicts the SCHEMA computes and which must include a value it accepts and one it refuses; or the schema carries `x-omg-pattern-boundaries`. An unused candidate fails.

**Vector APIs** (`http_vectors.rs`). The SCHEMA is the oracle; the real handler, over HTTP, must reach the same verdict. A handler accepts when it got past input validation (2xx/3xx, or a domain refusal 404/409) and refuses with 400 or 422; **any other status (401/403/413/415/429/5xx) fails the run** and is never counted as a refusal. 404/409 count as "got past input validation" by design; fixtures use `Probe::decided` where that is ambiguous.

- `query_vectors(fixtures, method, template, seeds, fresh, send)` (and `query_vectors_extended` with candidates) and `query_vectors_fixed(…, base, send)` for every operation with query parameters. `fresh` yields a context and a VALID request per call (single-use state); `seeds` give valid values for optional parameters the base omits. Vectors: bounds ±1, `maxLength`+1, pattern/format violations, enum members and non-members, wrong types, empty, omission, flipped booleans/consts, special characters, multibyte values.
- `body_vectors_http(fixtures, method, template, valid, send)` for every JSON request body (one run per wire form), `body_vectors_seeded(…, seeds, send)` when declared optional siblings that cross-field rules relate (`ids` with `all: true`) must be sent: a seed is `(JSON pointer, valid value)` added to the valid body, then walked; an unused seed fails; and `body_vectors_extended(…, seeds, candidates, send)` for patterns without derivable bounds. Vectors additionally: wrong types, null where not nullable, unknown fields top-level and nested, omissions, `minItems`/`maxItems` ±1, flipped booleans and consts, control-character (`\n`, `\0`, U+007F, U+0085) and multibyte pattern candidates, length boundaries m-1, m, n, n+1 derived from simple anchored patterns (`^X{n}$`, `^X{m,n}$`, `^X+$`, X one class or literal) from the valid seed's characters, and **every** `prefixItems` index (run the positional wire form with its own valid body).
- `Probe::decided(captured, accepted)` supplies the verdict when the status cannot tell input refusal from domain refusal. Derive it from real server state, never a constant: the callback pre-inserts a copy of the login attempt under each state it sends, so consumption proves acceptance and a surviving row proves refusal. It replaces only the accept/refuse reading of a status in the expected set.
- Single-use and rate-limited requests (login state, invitation tokens, exports, access requests) get fresh state per request in `fresh`, or one run per valid body.

**`SEMANTIC_REFINEMENTS`** (`requests.rs`) lists operations whose handler refuses values serde accepts (length, alphabet, exactly-one-of), with the reviewed JSON **pointers** that carry the rule. A vector may find the contract stricter than serde only if EVERY pointer it touches is listed (a vector that also changes an unreviewed field, the positional extra item or the root must agree exactly, as must every vector that changes nothing), and a looser contract never passes. Each listed pointer must exist in the operation's request schema: as a property of the object form and as a positional item at that field's index in the DTO's field order. `finish` still demands HTTP vectors for these operations, which compare the schema with the real handler exactly.

**`DECIMAL_RATES`** (`quality.rs`) lists the reviewed fractional-decimal rate fields (`conversion_residual_microusd`, `*_per_million`) exempt from the integer micro-USD string rule that every other `*_microusd` property must follow (`^(0|[1-9][0-9]*)$`, or `^(0|-?[1-9][0-9]*)$` where the value is signed, for example drifted totals).

### Shared serving-layer refusals and schema quality

`management_openapi/middleware.rs` adds the serving-layer contracts to every applicable operation, including future slices. Management authentication can refuse duplicate/mixed credentials or invalid/expired/revoked sessions/tokens (401), browser Origin/CSRF (403), bearer per-IP/replica/per-token throttles before route policy (429), and authentication storage failure (500). Authorization adds token-policy/scope/IP/role refusals (403), workspace/key hiding (404 where applicable), storage/audit failures (503), and token-readiness refusals (501 only for an unready Scope route). Logout verifies its own session/CSRF and does not use management bearer throttles. The serving body cap also documents 413 text responses. Handler variants are retained rather than replaced with fabricated universal envelopes.

Identity refusals carry the real string-valued JSON error. `x-omg-body-header-requirements` binds those alternatives to required `Cache-Control: no-store` / `Referrer-Policy: no-referrer`, plus a positive integer `Retry-After` for authentication throttles. This conditional extension is necessary because OpenAPI cannot condition Header Object requiredness on a body alternative; a handler's export-rate-limit envelope, for example, does not send Retry-After. DB-free raw-wire tests exhaust the real authentication limiter with a closed pool, so they cannot connect to a database.

Completed components (including unused ones), inline schemas, parameters, headers and every composition/conditional branch reject `true`, `{}` and unbounded/open object schemas. Objects must be closed and explicitly declare `required`; the all-optional ReadInput object deliberately declares `required: []` to preserve serde behavior. Every array, including nullable arrays, must constrain `items`: closed objects, typed scalar/enum/pattern schemas, or recursively constrained arrays. Fixed positional tuples may use `items:false` and constrained `prefixItems`. Missing/`true`/`{}` items and permissive `prefixItems`, `unevaluatedItems`, `additionalItems` or `contains` are forbidden. Conjunctive refinements inherit the closed base; disjunctions cannot introduce an unconstrained escape. Wildcard `patternProperties` and schema-valued `additionalProperties`/`unevaluatedProperties` require an exact reviewed `ARBITRARY_JSON` pointer and matching justification, even when map values are typed. Every completed operation has missing-required, wrong-type and sensitive-extra response mutation controls; recursive controls inject an unexpected property into every populated nested closed object and a leak-bearing unconstrained element into every array, and require each mutation to fail (wire-header/body controls for non-JSON operations). Nullable object/array examples are populated rather than hidden behind a leading null.

Only these exact stored JSON locations are allowlisted in `quality.rs`; both carry matching justification `$comment`s, and the exception cannot be moved or referenced as a standalone response schema:

| Location under `#/components/schemas/` | Why it is arbitrary |
| --- | --- |
| `IdentityNotification/properties/details` | Non-ceiling `alert_events.details` is stored producer-specific JSON. Its monetary typing is reviewed here: each known producer field is typed individually (`pending_microusd` is signed, `sum(held - held_unknown)` has no sign guard; the others are unsigned), and any other `*_microusd` name must still be an integer string (signed form). None of it authorizes identities in ceiling details, which are closed by the kind conditional. |
| `get_api_v1_me_exportResponse/properties/audit/properties/items/items/properties/metadata` | The subject export returns stored `audit_events.metadata` verbatim; keys depend on the audited action. |

Admission-ceiling `details` always uses the closed `IdentityCeilingDetails` producer schema from `alerts::ceiling_conditions`: scope, personal flag, workspace kind, window/count/rate, rate/wait triggers and nullable thresholds/p95; only `workspace_name` may be omitted by redaction. Other notification kinds retain arbitrary stored details. Admin admission-ceiling notifications explicitly suppress personal identities: `personal=true` forces `workspace: null` and null/absent `workspace_name`; `details.workspace_id`, `owner_email`, nested `workspace` and all other unknown fields are forbidden. Shared references can only identify Team/Project workspaces. Auditor variants always remove workspace identities. Runtime validators and completed-operation checks cache only the immutable committed artifact by pointer identity; mutated-document controls always compile and check afresh.

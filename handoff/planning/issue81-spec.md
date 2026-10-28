<!-- omg-work-item:management-openapi -->

## Objective
Publish a machine-readable management contract grounded in actual handlers, authority and error/money types.

Parent tracking issue: #38.

**Status:** Planned work item; specification and implementation plan require approval. Filing is not implementation authority or acceptance of held work.

## Scope and acceptance
- [ ] Document supported management operations with exact status-string error.code, machine error.reason and integer micro-USD strings.
- [ ] CI detects handler/DTO/schema drift and unauthorized undocumented operations; inference-key denial and personal privacy are represented correctly.
- [ ] Generate/validate the artifact from reviewed source without secret examples or invented routes; site publication stays separately authorized.

## Verification
- [ ] Add baseline/red regressions and positive controls; retain existing assertions, no skips or weakened gates.
- [ ] `cargo test --workspace --all-features`
- [ ] `npm run typecheck:web`
- [ ] `npm run test:web`
- [ ] Exercise this issue's actual API/runtime/browser path and negative authority cases; mock/fake-wire coverage is not live acceptance.

Database gates require a fresh, explicitly selected disposable database and separately authorized serialized role/ACL changes. Frontend gates require a fresh lockfile install, not a held patched cache. No real provider calls without explicit cost approval. Listed commands are future gates, not completed results.

## Dependencies
Blocked by: None currently fixed; confirm conditional/recommended dependencies during specification.

## Likely source areas
- `apps/gateway/src/management/`
- `docs/management-api.md`
- `tests/ (new contract checks)`
- `.github/workflows/ci.yml`

## Decisions before implementation
Choose generation/check approach without adding dependencies before approval; website/docs-site publication is deferred.

## Delivery slices and checkpoint
1. Approve the scoped spec, authority/compatibility contract and testable success criteria.
2. Add regressions, then implement one complete path in small source/test slices (normally no more than about five files per implementation task).
3. Run applicable complete gates and review runtime evidence before selecting the next slice.
4. Parent-owned secret scan, scoped commit, exact-SHA CI and verified release before another issue starts.

## Release and exclusions
Patch if contract-only; minor if new API behavior is introduced. One implementation issue per branch and release; parent epics are tracking containers, not release bundles.

No automatic old-installation reset/migration, global inference limits, changed personal privacy, weakened tests, paid/live deployment, secrets-manager integration, website publication or unrelated dependency upgrades. Changes to APIs/schema/ACLs/dependencies/CI require explicit approval.


=====COMMENT=====
## Implementation specification: #81 drift-checked management OpenAPI 3.1


## Decision and scope

Choose **(c), a hybrid**: generate route, authorization and shared-error metadata from reviewed Rust source; hand-write schemas and validate them against DTOs and real handler responses.

This is smaller than converting SQL-built `Json<Value>` responses into derived types. Pure hand-maintenance risks duplicating authorization incorrectly. `Cargo.lock` contains schemars 0.9.0/1.2.2 and syn, but no utoipa or JSON Schema validator. Do not introduce utoipa/procedural macros: they would add build-time execution and supply-chain review without resolving dynamic response shapes.

Cover every declared `/api/v1/*` operation, including anonymous authentication/branding routes and authorized `410` installation-policy tombstones. Exclude inference (#82), SCIM, health and metrics. No API, database or authorization behavior changes are authorized.

## Artifact and generation

Commit OpenAPI **3.1.0 JSON** at `docs/openapi/management.openapi.json`, with reviewed schema/operation fragments under `docs/openapi/management/`.

Use deterministic, test-only generation from compiled `ROUTES`, expanding space-separated methods and retaining exact parameter names. Combine generated metadata with fragments; reject duplicate operations, unmatched fragments, unresolved references and missing path parameters. Normal tests compare regeneration in memory against the committed artifact; writing requires an explicit developer regeneration mode. No production dependency or runtime specification endpoint.

Document implicit HEAD behavior without inventing separately registered operations.

## Authority and privacy

Generate `x-omg-access` and token-policy extensions directly from `Access` and `TokenPolicy`.

Security alternatives:

- Browser: `omg_session` cookie; mutations additionally require `X-CSRF-Token` and exact configured `Origin`, represented as conjunctive header security requirements.
- Admin token: HTTP bearer, only on individually `Scope(…)`-declared operations. Record the exact scope in an extension, not OAuth scopes on a bearer scheme.
- Anonymous operations: explicit empty security requirements.

`Denied` never permits tokens; `BrowserAdminOnly` requires a browser-session Platform Admin, including GET token inventory. Write scopes do not imply read scopes. Describe live owner/role checks, credential-family exclusivity, duplicate-header refusal and fail-closed undeclared routes. Never define inference keys as management credentials.

Access extensions describe minimum boundaries, not complete authority. Add tested operation-specific visibility rules: personal details owner-only; platform personal totals permitted; shared members’ own activity versus administrators’ workspace-wide activity; object ownership and hidden `404`s. Auditor platform authority is read-only, while independent User/membership entitlements remain usable. Define distinct redacted response variants, including webhook references and admission-ceiling identities, rather than optional sensitive fields everywhere.

## Schema requirements

Request objects mirror serde names, defaults, requiredness, nullability and `deny_unknown_fields` through `additionalProperties:false`, recursively. Explicitly distinguish omission from null for custom deserializers: policy `nullable`/`supplied`/`present`, PATCH fields and reconciliation’s required-nullable `billing_usage`. Include nested types outside management, manually decoded webhook bodies, empty-body operations, query parameters, multipart uploads and binary responses.

Money uses exact integer micro-USD strings, never numbers or floats; preserve field-specific signedness/ranges and nullable unknown values. Counts and limits retain their actual wire types.

Generate shared error vocabulary from `REASONS`, `POLICY_REASONS` and inventoried detailed-error call sites. Validate serialization of `{error:{code,message,reason?}}`: `code` equals the HTTP status string. Document reason-specific details such as `period`, `limit`, `missing_meters` and retry information. Framework extraction errors retain their actual status/content type; do not falsely promise the envelope universally.

## RED-first drift gates

1. Add failing controls for omitted/extra operations, wrong method/template, changed access/scope, undocumented reasons and corrupted schemas. Preserve the independent source-route inventory and all existing authorization assertions.
2. Reuse syn/source inventories to bind handlers and recursively reachable DTOs to schemas. Compare properties, types, requiredness, nullability and closure; fail on unsupported serde constructs rather than silently approximating. Custom deserializers require schema-versus-real-deserialization boundary vectors.
3. Extend `configuration_matrix_tests.rs`: retain successful controls before unknown-field probes; validate both requests and responses. Add nested unknown-field, missing-field, wrong-type and null/omission cases, including manually decoded bodies.
4. Validate actual status, headers, media type and response body in existing integration helpers before JSON fallback discards extraction failures. Require successful/nonempty fixtures and redacted variants for every operation, or its intentional tombstone response. No permissive placeholder schemas at completion.
5. Extend served-router authz/token matrices to validate denial contracts while retaining inference-key, foreign-personal, Auditor, scope, CSRF and Origin checks. Schema validation supplements—not replaces—privacy assertions.

## Tooling, delivery and effort

Propose dev-only `jsonschema = "=0.33.0"` with default features disabled, explicit Draft 2020-12 and local-only reference resolution. Approval requires maintenance/advisory and transitive build-script/proc-macro review. Vendor checksum-pinned OpenAPI metaschemas/dialects; no schema fetching, package installation or upstream calls during validation.

PR1 delivers route/auth/error skeleton and drift tests; subsequent small PRs complete schemas by area: identity/access, catalogs/setup, governance/pricing/reporting, then remaining settings/alerts/logs/files/batches. Intermediate work remains explicitly incomplete; acceptance requires exhaustive coverage.

Use gateway release `info.version`, stable operation IDs and reviewed contract diffs; contract-only release is patch. Run existing all-feature Rust and web gates, plus real loopback/browser refusal paths on fresh disposable PostgreSQL. Publication remains separately authorized.

Estimate **15–25 engineer-days plus reviews**. Main risks: dynamic SQL shapes, custom deserialization, redaction variants and incomplete positive fixtures.

## Coordinator decisions (approved for implementation)

1. **Approach (c) hybrid** as specified; no utoipa/proc-macro generators, no runtime spec endpoint.
2. **Dev-only `jsonschema`** pinned exactly, default features off, local references only — approved subject to the PR1 supply-chain review (version, maintenance/advisories, transitive build scripts/proc macros recorded in the PR). Vendored metaschemas checksum-pinned; no network at test time.
3. **Forcing function:** once PR1 merges, every PR that adds or changes a management route must update the fragments in the same PR (drift gates fail otherwise). Unmerged route-adding PRs (#102 scheduled reports, #48 cost reconciliation) rebase onto it.
4. Slices: PR1 route/auth/error skeleton + drift gates (placeholder schemas explicitly marked incomplete and counted); then schemas by area. #81 closes only at exhaustive coverage with no placeholders. Publication on the docs site remains separately authorized.


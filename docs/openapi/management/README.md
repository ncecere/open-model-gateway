# Management OpenAPI contract maintenance

[management.openapi.json](../management.openapi.json) is the **incomplete PR1 skeleton** for #81, not a finished client-generation or body-validation contract. The test-only generator reads compiled `route_policy::ROUTES`, expands declared methods and includes exactly `/api/v1/*`. Inference, SCIM, health and metrics are excluded; anonymous authentication/branding operations and authorized installation-policy tombstones are included. Implicit HEAD follows GET policy without synthetic operations. `info.version` is the gateway package version.

The skeleton now has 249 operations, 40 shared error reasons and **498 incomplete request/response schema slots**. `management_openapi_reports_incomplete_schemas` reports and pins progress; later schema slices must deliberately reduce the baseline. #81 stays open until exhaustive schema and real-handler coverage is complete with no placeholders.

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

The initial `operations.json` registers every operation. When adding/changing a management route, update its exact method/template and access/token/scope snapshot here **in the same PR**. Route-only changes fail even with artifact regeneration. These snapshots are review acknowledgements, not a second authorization source: extensions and security requirements always come from compiled `Access`/`TokenPolicy`. Missing/unmatched entries, duplicate operation/schema fragments (including duplicate top-level map keys), unknown fragment fields and overrides of generated authority/IDs are errors.

`operation` supplies hand-written OpenAPI fields such as summaries, descriptions, responses and request bodies. Generated path parameters are immutable; fragments may append inline non-path parameters. Schema fragments go in `schemas`; they may replace a generated placeholder by its exact component name. The shared `ErrorEnvelope` is generated and cannot be overridden.

Body slots are referenced by operation extensions `x-omg-request-schema` and `x-omg-response-schema`. Each initially points to a distinct component marked `x-omg-schema-status: incomplete`. They deliberately do **not** fabricate request-body presence, JSON media types, successful statuses or universally enveloped errors; the default response is descriptive only. Later slices add real `requestBody`/`responses`, body absence, media types, query parameters, DTO constraints and privacy-specific variants. Replacing a slot must remove its incomplete marker only after the corresponding behavior is tested. Placeholders are never acceptance validators for actual requests/responses.

Access extensions express minimum boundaries, not complete authority. Live ownership, membership, platform roles, hidden 404s and personal/shared-workspace visibility remain enforced by handlers. The shared error schema documents the handler envelope and generated reason vocabulary, not every framework extraction response or every reason-specific detail. Existing source-route and authorization tests are unchanged.

## Offline schema safety

The dev-only validator uses explicit Draft 2020-12 and the linear-time `regex` backend with a compilation-size limit. Invalid/unsupported regexes (including look-around/backreferences) fail schema compilation; RED controls exercise both `pattern` and `patternProperties`, including under `not`. Every component, including unused fragments, and inline Schema Object is compiled before the artifact can be written. Artifact references must be local `#/...` JSON Pointers; nested `$id` rebasing is forbidden.

The [vendored metaschemas](../vendor/README.md) are SHA-256 checked before loading. Only their exact identifiers resolve to checked-in bytes; unknown URI retrieval is refused, with HTTP/file/default features disabled. The tests check OpenAPI structure, schema syntax, references, path parameters, authority, reason coverage, fragment matching and artifact drift. They do not yet bind all DTOs or validate every real handler response; those remain later #81 slices.

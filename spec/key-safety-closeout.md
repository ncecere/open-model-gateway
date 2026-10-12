# #62: original-acceptance closeout

## Objective and approved boundary
Close the original key-safety issue after verifying the four completed containment fixes together. The user approved this as the first of the next ten issues, followed sequentially by #20, #19, #21, #64, #22, #23, #70, #71 and #73. This is verification-first, not a fifth feature slice.

Existing contracts remain `key-safety-copy.md`, `key-safety-create-submit.md`, `key-safety-rotate-submit.md` and `key-safety-authority-events.md`. They and #62's original scope are the acceptance source. Do not infer missing implementations from previously excluded/unverified scenarios.

## Structure and style
- Existing owners: `apps/web/src/pages/keys.tsx`, `apps/web/src/components/ui.tsx`; existing session/router ownership remains in `pages/home.tsx`.
- Existing unit regressions stay unchanged. Add actual-runtime coverage under `tests/browser/`, reusing the real signed-OIDC, Rust, PostgreSQL and mock-upstream stack.
- Retain the actual mounted React callback rather than dispatching an event only on a detached DOM node. Use complete events and typed boundary checks; no casts suppressing a bad event contract.
- Keep credentials inside private browser memory. Assertions return booleans/counts, never token values. No OS clipboard writes, traces, HARs or disclosure screenshots in the added key-safety tests.

## Acceptance and testing
1. Actual create/rotate callbacks retained across cancel, unmount/accepted SPA navigation and the existing explicit authority boundaries cannot start new POSTs. Revoked disclosure callbacks cannot copy.
2. Live create/rotate/Copy, corrected validation/retry, fresh-owner recovery and admitted operations keep their documented behavior. Cancellation is not rollback; unknown outcomes never replay automatically.
3. Tokens remain outside query/mutation caches, persistent storage, URLs and logs. Actual negative backend requests preserve human/service/Personal distinctions and live membership authorization.
4. Reuse the completed baseline RED, mutation, full-web/typecheck/build and reviewed synthetic-browser evidence when its complete relevant inputs are unchanged. Additional runtime tests do not justify rerunning unaffected production gates solely for reassurance.
5. Run `npm run test:browser` with all existing projects/assertions plus the added acceptance coverage, against a fresh explicitly selected PostgreSQL 17 fixture; then verify exact-SHA CI. A production failure requires a reproduced regression before any minimal fix.

Commands (from the verified fresh-lockfile candidate, never the held root dependency cache):
```sh
npm run typecheck:web
npm run test:web
npm run build:web
CARGO_TARGET_DIR=/Users/nicholascecere/projects/rust/open-model-gateway/target cargo build --locked --offline -p open-model-gateway
# Parent supplies a private OMG_BROWSER_ADMIN_DATABASE_URL for the owned PG17 fixture.
OMG_BROWSER_PORT_BASE=18490 OMG_BROWSER_SKIP_BUILD=1 OMG_BROWSER_GATEWAY_BIN=/Users/nicholascecere/projects/rust/open-model-gateway/target/debug/open-model-gateway OMG_BROWSER_WEB_DIR=apps/web/dist npm run test:browser
```

## Boundaries and delivery
Always preserve existing tests, backend authorization, generic dialogs, StrictMode, mounted/retired owner checks, controller ownership, abort/continuation semantics and no-replay policy. Use the minimum changes needed for a reproduced original-acceptance failure.

The user authorized fresh isolated database/Kubernetes fixtures, fixture-only ACL changes and task-branch pushes for CI/dev images. Do not touch existing databases, the root dependency cache, another owner's environment or namespaces outside `dev-omg` on the shared homelab. Do not adopt archived Root/history/pagination architecture, add providers, change schemas/permissions, upgrade dependencies, fabricate successful results or expand the ten-issue list.

Main merges, releases and production deployment remain unauthorized. Distinguish code/runtime acceptance from publication; the original issue's release-delivery checkpoint cannot be silently waived or claimed complete. #73 also needs a numeric paid cap and selected route before any paid call; no such call belongs to #62.

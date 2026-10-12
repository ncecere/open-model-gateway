# Spec: key-safety Copy containment — first slice of #62

## Objective and approval

The user approved a minimal current-main Copy-only slice targeting a compatible v0.4.2 patch. Prevent retained one-time-secret Copy handlers from starting a clipboard write after the disclosure is explicitly closed or its component unmounts. Preserve valid current Copy and failure feedback. This is not completion of all #62 and is not release/deployment authorization.

## Scope and boundaries

- Cover creation in `apps/web/src/pages/keys.tsx` and replacement-key disclosure through `ActionDialog` in `apps/web/src/components/ui.tsx`.
- Revoke Copy synchronously on explicit close/discard and during unmount cleanup, before a retained callback can perform another write.
- Bind post-await feedback to that same disclosure lifetime. An already-admitted clipboard write cannot be canceled or retroactively erased.
- No new Root handoff, history patch, query/schema changes, permission changes, provider calls, database mutations or vendored primitive edits.
- Stale create/rotate submission admission, authority-refresh/pagehide policies and other #62 flows remain separate pending slices.
- Retained-function tests are containment challenges, not proof of an ordinary detached-DOM click, XSS, memory erasure or backend authorization.

## Commands and testing

Use a fresh lockfile installation in an owned source archive; do not use or undo the held patched root cache.

```sh
npm ci --ignore-scripts --no-audit --no-fund
npm run test:web -- src/components/key-copy-lifetime.test.tsx
npm run typecheck:web
npm run test:web
npm run build:web
```

Add a new complete test file; preserve existing tests/helpers/assertions unchanged. RED must demonstrate stale clipboard writes against the actual main-based components before production changes. Cover close/unmount, synchronous close-before-commit, fresh success/failure, delayed completion/rejection and repeated disclosure/StrictMode positive controls. Use synthetic credentials only. Browser verification must state whether it uses a synthetic frontend fixture or the real gateway suite; the latter needs separately authorized disposable database access.

## Source structure and style

Keep the guard local to each existing owner. Tokenless monotonic disclosure invalidation is preferable to a new global owner or a mutable latest-secret callback. Follow existing TypeScript/React conventions, for example:

```ts
const generation = useRef(0);
const capturedGeneration = generation.current;
// A retained handler compares its captured generation before external work.
```

React lifecycle details and close-before-commit behavior must be proven by tests, not inferred from this illustrative snippet.

## Success and delivery

1. Retained closed/unmounted callbacks cause zero additional clipboard writes.
2. Fresh callbacks still write the exact displayed synthetic secret and show existing feedback; stale async completion cannot publish feedback into a later view.
3. Full frontend tests, typecheck and build pass on fresh dependencies; applicable browser evidence and independent review are recorded honestly.
4. Make one scoped commit on the key-safety issue branch after review/secret checks. Keep the existing uncommitted `tasks/plan.md` outside that commit. Do not close #62, bump release metadata, push or publish a release in this single-task build.

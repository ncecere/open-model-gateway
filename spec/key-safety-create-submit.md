# Spec: creation-submit retirement — second slice of #62

## Approval and scope

The user approved a single next slice after Copy-only commit `3e3f5d5`: prevent retained Create callbacks from starting new POSTs after explicit cancel or owner unmount, preserving fresh submissions, corrected retries and already-admitted completion. Compatible v0.4.2 patch target only; no release/deployment authorization.

Modify only `CreateKeyDialog` in `apps/web/src/pages/keys.tsx`, add a complete task-owned regression file, and record this spec. Stay on the existing #62 branch. Preserve all existing tests, including the accepted Copy tests. Do not import archived Root/history code or use/change the patched root dependency cache.

## Contract

- An actual render-bound form submit handler retained from a valid draft starts zero new key-creation POSTs after cancel or unmount.
- Cancel revokes synchronously before React removes the connected form; cleanup revokes during owner unmount. Reopening a fresh owner must not reactivate an old handler.
- Fresh human-key creation still sends its exact expected same-origin request. Invalid drafts remain blocked; explicit correction/retry remains live. StrictMode must not disable valid fresh submission.
- A POST admitted before unmount is not canceled, automatically replayed or retroactively denied by this change. Its existing completion/cache invalidation behavior remains unchanged. When still mounted, it can disclose its result normally; when unmounted, it must not disclose into the replacement owner.
- Browser containment is not backend authorization. Existing service/human/personal permission checks and Rust live authorization remain unchanged. No credentials enter query/mutation caches, storage, URLs or logs.

## Exclusions

Rotation submission, authority-refresh/pagehide policy, navigation architecture, stale draft/prop changes within a still-live owner, duplicate/re-entrant submissions within a live owner and all other #62 flows remain separate. No changes to APIs, schema, ACLs, dependencies, vendored primitives, provider calls or databases. No automatic retries or new cancellation semantics.

## Implementation and verification

Use an owner-local mounted/closed latch to reject retired submission before validation state updates or transport. Layout setup/cleanup tracks whether this owner is mounted; explicit close latches permanently and is never reset, including during StrictMode effect replay. Keep the existing Copy-generation cleanup intact. Each replacement owner gets a separate ref; never borrow its authority or latest draft. Do not gate already-admitted continuation on the new check.

A candidate-only experiment with the simpler render-captured Copy-generation comparison prevented stale POSTs but suppressed untouched invalid-form feedback under StrictMode: its initial effect cleanup changes that generation without a subsequent render. The complete regression file caught this (13 passed / 1 failed). It was rejected and candidate source restored; the mounted/closed split preserves replay of the same mounted owner without undoing explicit close. Hidden/Offscreen/Activity lifecycle policy is not part of this slice.

RED must run the complete new test file against committed HEAD, including actual retained-handler live controls. Cover explicit cancel, unmount, cancel-before-commit, reopening, fresh ordinary/retained/StrictMode creation, invalid/corrected input, explicit corrected retry and deferred admitted completion across unmount/reopening. Synthetic same-origin fetch only, never actual minting.

Use a fresh HEAD archive and lockfile install with dependency scripts disabled. Run focused RED/GREEN, `npm run typecheck:web`, the full `npm run test:web`, and `npm run build:web`. Check original tests and tested source byte-for-byte, whitespace and scoped secret patterns. Verify applicable behavior in an owned synthetic Chromium fixture; this is not full real-gateway/database browser acceptance.

Independent review must challenge the bounded contract and evidence; cross-model choice is separate per interactive review cycle. Keep #62 open, record only this slice complete, commit precisely the task-owned files and stop. Do not stage `tasks/plan.md`, push, merge, bump versions, publish, deploy or start another task.

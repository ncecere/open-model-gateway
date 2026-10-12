# Spec: rotation-submit retirement — third slice of #62

## Approval and scope

The user approved one bounded rotation slice after `e053508`: reject retired submits in shared `ActionDialog`, preserve fresh actions/retries and existing admitted-request cancellation/completion behavior, verify and commit. Resumed from `88bed9ec` after the separately authorized main-workflow merge/push and Copy/Create-only cluster verification; those operations did not deploy rotation. Compatible v0.4.2 target only; no new push/release/deployment authority.

Change only the existing owner in `apps/web/src/components/ui.tsx`, add a complete task-owned regression file and this spec. The local admission guard necessarily protects other users of this shared owner too; include non-secret action controls. Preserve all 77 original frontend test files and existing Copy/creation containment. No archived Root/history implementation or patched root dependency cache.

## Contract

- Actual render-bound valid rotation form submits retained after explicit Cancel/discard or actual owner unmount start zero new rotation POSTs. Explicit discard revokes synchronously while the form can still be connected; owner teardown must reject before passive cleanup. Fresh owners must not reactivate old callbacks.
- Fresh ordinary/retained/StrictMode rotation sends the exact existing same-origin request and discloses its replacement normally. Untouched invalid fields still show validation; explicit correction/retry remains live. Opening dirty-discard confirmation is not retirement: Keep editing preserves the draft and live admission; confirmed Discard revokes it.
- The existing shared `inFlight` exclusion, AbortController ownership, abort-on-discard/unmount, post-await mounted/signal checks and failure/completion behavior remain. Abort is not proof of rollback of a server-accepted rotation. No automatic replay after failure, cancellation or unknown outcome; do not transplant creation's different cancellation semantics.
- An admitted live request may complete successfully or fail and allow explicit retry. Late resolve/reject after owner removal cannot publish a replacement credential/error into a new owner. After the old request settles, invoking its retained callback still starts no new request.
- Shared non-secret actions retain fresh validation, dirty-dialog confirmation, intended completion/after behavior and cancellation. Their retired submit callbacks are rejected too; do not add provider/key-specific switches.
- Secrets remain outside query/mutation caches, storage, URLs and logs. Browser lifetime checks are not backend authorization; Rust authorization, service/human/personal rules and APIs remain unchanged.

## Exclusions and implementation

Authority-refresh/pagehide/navigation architecture, Activity/Offscreen, replacing action/draft props in a still-live owner and other #62 policies remain separate. No API/schema/ACL/dependency/vendor primitive changes, actual credential operations, database access, provider calls, deployment or broader docs/site work.

Use owner-local liveness and a permanently latched explicit retirement bit. Reject before validation state updates or `action.run`; never borrow a replacement owner's authority. Keep existing Copy-generation invalidation and existing cancellation/continuation logic. Do not use a render-captured Copy generation as the sole submit guard: initial StrictMode effect replay must preserve untouched-form validation.

## Verification and delivery

Complete new-file RED against committed HEAD using synthetic fetch and actual bound form handlers, including retained live positive controls, cancel/discard, unmount/teardown-before-passive-cleanup, synchronous discard-before-commit, reopening, StrictMode, invalid/corrected input, explicit retry and pending resolve/reject. Use same-event-batch controls and prove retirement actually removes the dialog. Include shared non-secret controls and existing abort/in-flight exclusion assertions; preserve original tests/assertions.

Fresh owned HEAD archive and lockfile install with dependency scripts disabled. Focused GREEN, `npm run typecheck:web`, full `npm run test:web`, `npm run build:web`; original-test byte comparison, exact source pins, whitespace/scoped credential patterns and an owned synthetic Chromium fixture. These do not constitute real gateway/database/browser or backend authorization acceptance.

Independent bounded review and per-artifact cross-model choice before the scoped commit. Update only this slice on GitHub, keep #62 open, leave `tasks/plan.md` unchanged/uncommitted, commit exactly task-owned files and stop. No version bump, merge, push, release, deployment or next task.

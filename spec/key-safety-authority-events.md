# #62 — explicit authority-invalidation events

Approved as one bounded task after Copy, Create-submit and Rotate-submit retirement.
Baseline: `059d08d1b3ed9d29545ba53895840e86e6768d18`; compatible v0.4.2 work, no version bump.

## Contract

- The existing `omg:unauthorized` and `omg:access-refresh` events invalidate the current CreateKeyDialog and ActionDialog owners synchronously. After dispatch returns, even before React commits removal/checking UI, retained valid Create/Rotate submissions issue no new POST and retained Copy bindings issue no clipboard write.
- Retirement is permanent for that owner. Successful access revalidation may mount a fresh authorized owner; old callbacks cannot borrow its authority. StrictMode replay must not retire untouched fresh owners.
- Existing Home transitions clear credential disclosures on commit. No secret enters query/mutation caches, browser storage or URLs. This is lifetime containment, not proof of physical string erasure or clipboard clearing.
- Preserve normal validation, corrected explicit retry, fresh authorized human/service-key flows, and existing backend/Personal-owner checks. Generic ActionDialog controls remain usable when authorized.
- Existing admitted-request and cancellation contracts remain unchanged: Home already aborts transport on these events; ActionDialog aborts on discard/passive unmount; Create's previously admitted continuation is not newly gated. Unknown/server-accepted outcomes are never automatically replayed. Abort is not rollback.
- Do not reclassify errors or emit new authority events. Ordinary background session refetch, logout, navigation, workspace switching and pagehide policies remain outside this slice. Refreshed session permission loss is exercised through the existing explicit refresh boundary, not a new authorization system.

## Acceptance

1. Actual Home/Router/Query integration tests capture the real React form and Copy handlers, prove authorized positives, dispatch each event and challenge the old handler in the same synchronous stack before React commits.
2. Both creation and rotation disclosures disappear at the existing Home transition; old bindings remain denied after checking, authorized recovery, and negative-authority recovery. Check delayed clipboard feedback and admitted-request cancellation/no replay.
3. Untouched StrictMode and generic non-secret ActionDialog positive controls; all 78 pre-existing frontend test files remain unchanged.
4. Genuine RED against baseline, focused GREEN, full frontend suite, typecheck and build in a fresh owned HEAD archive with lockfile install/scripts disabled; root dependency cache untouched.
5. Synthetic real-browser verification, fresh-context adversarial review and per-artifact cross-model choice. These do not certify real Rust/DB authorization or complete #62.
6. Exact-path scoped commit and GitHub completion comment; original #62 body/state and `tasks/plan.md` unchanged. Stop. No push/deploy/merge/release.

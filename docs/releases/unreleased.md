# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.8.0](v0.8.0.md). They become the next release's notes.

## Added

- **Aged unknown-hold review API (#101, slice A).** Read-only workspace and platform queues for reservations unknown for at least 24 hours in database time. Workspace activity remains owner/admin/own-human-key scoped; the platform queue exposes Team/Project rows (including retained holds after workspace disable) and one identity-free personal aggregate. Exact nullable retained floors are never spend or upper bounds. Keyset pagination uses strictly validated, stateless position cursors that work across replicas; every page reapplies the caller's full SQL visibility predicate, so cursor edits or cross-scope reuse cannot broaden access. Workspace routes refuse admin tokens; platform reads require `platform:read`. No migration, new privilege, alert, UI or accounting action in this slice. See [governance API](../governance-api.md#aged-unknown-hold-review-queue).

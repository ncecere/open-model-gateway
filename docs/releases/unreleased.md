# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.14.1](v0.14.1.md). They become the next release's notes.

## New features

- **Keys in access reviews** (#90, completes R18 / #42). A review can cover the keys of Teams and Projects (`workspace_keys`): one item per live key lineage, human and service-account keys, never personal workspaces' keys. A rotation keeps the lineage, so rotating does not escape a review. The workspace's owners and admins review them (never their own key; Platform Admins are not key reviewers). **Remove revokes every key of the lineage at once** through the revoke path; revoked keys never return. Keep is the only other decision; key items are never assigned.
  - **Dashboard:** key items are shown in the review pages in a following change, before the release; until then a review covering keys can be started only through the API.
  - **Privacy:** platform readers (Platform Admins, Auditors) see key items as key safety's platform scope does: key id, workspace, holder kind, expiry and last use, never the key's name or holder, in the item list, the assignment response and the CSV export; whether a key has a reviewer, and the item order, never depend on its holder. Reviewers see the key name and holder as the workspace key list shows them.

## Behaviour changes

- Access review items: `subject_kind` gains `workspace_key`; `user_id`, `role` and `source` may now be null (key items); every item gains `api_key_id`, `key_name`, `holder_kind`, `service_account_id`, `service_account_name`, `expires_at` and `last_used_at` (null for memberships and platform roles). Campaign `subjects` gains `workspace_keys` (up to three); `counts.by_kind` gains `workspace_key`. The export appends four columns (`api_key_id`, `holder_kind`, `expires_at`, `last_used_at`). Strict generated clients must be regenerated.
- A workspace filter is accepted with `workspace_keys` as well as `workspace_memberships`.

## Migrations and runtime grants

- **0080_access_review_keys:** campaign subjects CHECK (re-added; scans the campaigns table); item columns `api_key_id`, `key_lineage_id`, `service_account_id`; `user_id`, `role` and `source` nullable for key items only (a shape CHECK, scanning the items table); scoped foreign keys to `api_keys` and `service_accounts` (briefly `SHARE ROW EXCLUSIVE` on both); `UNIQUE (campaign_id, key_lineage_id)`; the campaign, item and decision guards, the removal check and the three reviewer functions replaced.
- Runtime grants: `INSERT (api_key_id)` added on `access_review_items`.

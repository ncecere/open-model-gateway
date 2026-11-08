# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.13.0](v0.13.0.md). They become the next release's notes.

## New features

- **Access review campaigns** (#90, part 1). A Platform Admin starts a one-shot campaign (`POST /api/v1/platform/access-reviews`) that snapshots the live shared Team/Project memberships and/or Admin and Auditor platform role grants it covers, optionally narrowed to listed workspaces, due 1 to 90 days later (default 14), and closes it when done. Platform Admins and Auditors read campaigns and their items, each showing the person, role, source (manual, group or bootstrap) and whether the grant is still in effect. Browser session only. At most 20 open campaigns and 5,000 items per campaign. Requires migration **0076** and the updated runtime grants.
- **Access review decisions** (#90, part 2). Reviewers see the items they may decide in `GET /api/v1/me/access-reviews`: the owners and admins of each Team or Project review its memberships, Platform Admins review platform roles, nobody reviews their own access, and a Platform Admin can assign an item to one specific eligible reviewer. They decide **keep**, or **flag_idp** for group-provided access to be fixed at the identity provider; decisions are append-only and audited, and the latest counts. Removing access arrives in a later part. Requires migration **0077** and the updated runtime grants.

## Behaviour changes

- Management API: eight new operations (**271**); new `409` reasons `access_review_limit`, `access_review_too_large`, `access_review_empty`, `access_review_changed`, `access_review_closed`, `access_review_item_inactive`, `access_review_flag_requires_group`, `access_review_reviewer_ineligible` and `access_review_own_access`; new audit actions `access_review.created`, `access_review.closed` (resource `access_review`), `access_review.decided` and `access_review.reviewer_assigned` (resource `access_review_item`).

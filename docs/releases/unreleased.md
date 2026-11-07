# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.13.0](v0.13.0.md). They become the next release's notes.

## New features

- **Access review campaigns** (#90, part 1). A Platform Admin starts a one-shot campaign (`POST /api/v1/platform/access-reviews`) that snapshots the live shared Team/Project memberships and/or Admin and Auditor platform role grants it covers, optionally narrowed to listed workspaces, due 1 to 90 days later (default 14), and closes it when done. Platform Admins and Auditors read campaigns and their items, each showing the person, role, source (manual, group or bootstrap) and whether the grant is still in effect. Browser session only. At most 20 open campaigns and 5,000 items per campaign. Reviewer decisions follow. Requires migration **0076** and the updated runtime grants.

## Behaviour changes

- Management API: five new operations (**268**); new `409` reasons `access_review_limit`, `access_review_too_large`, `access_review_empty`, `access_review_changed` and `access_review_closed`; new audit actions `access_review.created` and `access_review.closed` (resource `access_review`).

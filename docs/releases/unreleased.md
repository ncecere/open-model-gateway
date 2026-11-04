# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.11.0](v0.11.0.md). They become the next release's notes.

## New features

- **Request a model for your workspace** (#83, slice B). A member of a Team or Project can ask for a model from the workspace's catalog to be selected for the workspace (`POST /me/access-requests` with `model_id`). An owner or admin of that workspace approves or denies it in the workspace's request queue; approving rechecks the requester's membership and the model's availability and selects the model exactly as an admin selection would. Platform authority alone cannot decide these requests, and the requester never can. API only in this slice; the dashboard follows. Requires migration **0072** and the updated runtime grants.

## Behaviour changes

- Access-request objects gain `model_id` and `model_name_at_request`; `target_kind` can be `model_selection`, `requested_role` is `null` for it, and `membership_grant_id` is `null` on an approved model request. The platform decision routes refuse model requests with `403`.

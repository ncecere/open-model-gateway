# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.11.0](v0.11.0.md). They become the next release's notes.

## New features

- **Request a model for your workspace** (#83, slice B). A member of a Team or Project can ask for a model from the workspace's catalog to be selected for the workspace (`POST /me/access-requests` with `model_id`). An owner or admin of that workspace approves or denies it in the workspace's request queue; approving rechecks the requester's membership and the model's availability and selects the model exactly as an admin selection would. Platform authority alone cannot decide these requests, and the requester never can. In the dashboard, members see **Request** on models available to add on the workspace's **Models** page, and owners/admins decide in Workspace Settings › **Access requests**; the Admin page lists model requests without actions. At most 5 model requests can be pending per requester and workspace, and 100 per workspace. Requires migration **0072** and the updated runtime grants.

- **Access requests in your notifications** (#83). Owners and admins of a Team or Project see pending requests to join it or for a model in **Notifications** (Platform Admins: pending membership requests), and requesters see when theirs is approved, denied or expires. They follow your current access, open the queue that can decide them, and count towards the unread badge. In-app only: never emailed or sent to webhooks. Requires migration **0073** and the updated runtime grants.

## Behaviour changes

- Access-request objects gain `model_id` and `model_name_at_request`; `target_kind` can be `model_selection`, `requested_role` is `null` for it, and `membership_grant_id` is `null` on an approved model request. The platform decision routes refuse model requests with `403`.
- `GET /me/notifications` items can now be access-request events (`source: "access_request"`), and the summary's `unread` includes them. Clients that assumed every item is an alert incident need the new contract.

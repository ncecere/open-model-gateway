# Key safety audit

The key safety audit lists active API keys that need attention. It is read-only: findings are computed from live configuration on every request, nothing is stored, and no key is changed. Fixes reuse the existing key actions.

## Where it appears

- **Admin › Records › Key safety** (Platform Admin and Auditor): High / Medium / Low tiles count Team and Project keys by their worst finding. A fourth tile counts personal keys that need attention, as numbers only. Below is one toolbar row (search by workspace or finding, a Severity filter) and one compact row per key: "API key in *workspace*", the holder type (member or service account), up to three finding badges, last use, expiry and one fix.
- **Workspace › API keys**: each key with findings gets a small *High / Medium / Low risk* badge, with its findings as the tooltip. The **Safety: Needs attention** filter shows only those keys.
- **Key page › Overview**: a *Safety* card lists the key's findings, each with its fix. The card is hidden when the key has no findings.

## Findings

Only active keys are checked. Revoked, expired and disabled keys, and keys of disabled workspaces, are skipped.

| Code | Badge | Severity | Rule |
| --- | --- | --- | --- |
| `no_expiry` | No expiry | High | The key has no expiry date. |
| `expiry_beyond_max` | Expiry too long | Medium | Human key that expires later than today plus the installation's maximum key lifetime (Admin › Settings › General). Service-account keys are exempt because the maximum applies to human keys only. |
| `no_limits` | No limits | High | No budget **and** no rate cap at any applicable layer. |
| `no_budget` | No budget | Medium | A rate cap applies, but no budget does at any layer. |
| `owner_lost_access` | Holder left | High | Team/Project human key whose holder no longer has an effective membership, for example a suspended account or a revoked grant. Personal keys and service-account keys are exempt. Defensive: every gateway path that ends a membership (manual removal, SSO/SCIM group loss, SCIM deactivation, suspension, role loss, disabling the workspace, cleanup) revokes the holder's keys at the same time, so this should not appear in normal operation; it flags keys left active by changes made outside those paths, such as a database restore or direct SQL. Such a key is already refused at request time (access is checked live). |
| `unused` | Unused | Low | The last use was at least *unused days* ago (default 30). |
| `never_used` | Never used | Low | The key was never used and was created at least `min(unused days, 7)` days ago, so new keys aren't flagged right away. |
| `broad_model_access` | All models | Low | The key has no model restriction, and its workspace has at least 5 usable models. |
| `not_rotated` | Not rotated | Medium | The current secret is at least *rotation days* old (default 180). Rotation issues a new secret in the same lineage, which resets this. |

Limits are **effective**, which means every layer that applies at admission counts: the installation, the workspace-type default (or the platform replacement override for that workspace, which replaces the default), the workspace's local policy and the key lineage's policy. A budget at any of these layers, including an amount of zero, counts as a budget. Any of requests per minute, tokens per minute or concurrent requests counts as a cap.

A key's severity is its worst finding.

## Fixes

| Fix | Findings | Action |
| --- | --- | --- |
| Set expiry | No expiry, Expiry too long | The rotate dialog with a new expiry. A key's expiry can't be edited in place. |
| Rotate | Not rotated | The rotate dialog. |
| Add budget | No limits, No budget | The key page's *Limits* tab. |
| Restrict models | All models | The key page's *Access* tab. A key's models can't change, so create a restricted key and then revoke this one. |
| Disable | Unused, Never used | The disable confirmation. |
| Revoke | Holder left | The revoke confirmation. |

Fixes are offered only to people who may perform them, using the same rules as the key page. On the Admin page, a fix opens the key's own page only when you administer that workspace. Otherwise it opens the Team or Project page, because platform roles don't grant key authority.

## API

Both endpoints accept `unused_days` (1–365, default 30) and `rotation_days` (7–1095, default 180). Unknown or invalid parameters return `400`.

- `GET /api/v1/workspaces/{ws}/key-safety[?key_id=]` returns the same keys as the API keys list. Shared administrators and the personal owner see every active key. Members see only their own human keys. Platform staff without membership get `403`. Each row is `{key:{id,name,workspace{id,name,kind},holder:"you"|"member"|"service_account",issued_to_user_id,service_account_id,created_at,expires_at,last_used_at,model_restricted},severity,findings:[{code,severity,days?|models?}]}`.
- `GET /api/v1/platform/key-safety[?workspace_id=]` (Admin and Auditor) returns **Team and Project keys only**, and rows omit the key name, holder identity and service account: `key:{id,workspace,holder:"member"|"service_account",created_at,expires_at,last_used_at,model_restricted}`. Personal keys appear only as `personal:{keys,flagged,high,medium,low}`, never as rows or identifiers. `personal` is `null` when filtered to one workspace. `workspace_id` must name a live Team or Project, otherwise `404`. At most 500 rows are returned, most severe first, with `truncated` set when there are more. The counts always cover every key.

Both responses include `summary:{keys,flagged,high,medium,low}`, which counts keys by their worst finding, and `thresholds:{unused_days,rotation_days,max_lifetime_days,broad_models,never_used_grace_days}`. Rows are flagged keys only.

There is no migration and no new SQL privilege. The audit reads tables that runtime can already `SELECT`.

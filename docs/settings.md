# Installation settings

Admin › Settings holds installation-wide settings. Platform Admins change them; Auditors read them; Users don't see them. Values live in one database row (`installation_settings`, migration 0010) plus `installation.name`. Every change is written to the audit log (`settings.general_updated`, `settings.privacy_updated`, `settings.email_updated`, `settings.email_test`, `settings.storage_updated`, `settings.storage_test`, `settings.logo_uploaded`, `settings.logo_removed`) with typed metadata only, never addresses, URLs or credentials. The API is in [management API](management-api.md#installation-settings).

| Page | Holds |
| --- | --- |
| General | Display name, support URL, logo, maximum lifetime of new human keys. |
| Defaults & limits | Personal/Team/Project defaults (formerly Admin › Limits; `/admin/limits` still opens it). There are no installation-wide limits; watch total spend with an installation spend alert. See [governance](governance.md#no-installation-wide-limits). |
| Data & privacy | OpenRouter data collection, request log retention, prompt/response storage, and the encrypted file store (Storage). |
| Email | SMTP relay for invitations, status, test send. |
| Sign-in | Read-only OIDC configuration, signing-key (JWKS) status and SCIM provisioning status; SSO group mappings stay on Admin › SSO groups. |
| Alerts | Installation rules and history, including **Installation spend** (`spend_threshold`): a non-blocking notification when total spend reaches a share of an amount. **Admission ceiling** (`admission_ceiling`) names one workspace or key near the request rate a single scope can sustain (Platform Admins only). See [alerts](alerts.md). |

## General

- **Display name**: 1–120 characters, no control characters. Shown as the installation name (`GET /me` → `installation.name`).
- **Support URL**: optional absolute `https://` URL without credentials or fragment, at most 2048 characters. Returned to every signed-in user in `GET /me` (`installation.support_url`); the gateway never fetches it.
- **Logo**: see [Logo](#logo).
- **Logo URL (deprecated)**: the old external `logo_url` (same URL rules) is still accepted and returned by the API for compatibility, but nothing shows it: drawing it would make every browser call a third-party host. Uploading a logo clears it. If one is set without an uploaded logo, the Logo row says "Logo URL is no longer shown; upload a logo."
- **Maximum key lifetime**: 1–365 days (default 365). Applies when a person creates or rotates their own key (`expires_in_days` above it is rejected with `reason:"key_lifetime_exceeds_maximum"`). Existing keys keep their expiry; service-account keys keep the general 1–365 day bound. `GET /me` exposes it as `installation.key_max_lifetime_days`.
- **Time zone**: UTC, fixed. Budget windows, reports and dates use UTC.

### Logo

Without a logo, the app shows the Open Model Gateway mark ("Portal") in the sidebar and on the sign-in page. An uploaded logo replaces it in both places: in the sidebar's 20 px box and at 40 px on the sign-in page, scaled to fit (any aspect ratio). Its alt text is the installation's display name; in the sidebar, where the name is the link text beside it, it is decorative.

- **Upload** (Admin): PNG, JPEG or WebP, at most 512 KiB, 16 to 4096 px per side; square and 64 px or larger is recommended. SVG is never accepted. The server checks that the file's magic bytes match its declared type, that the image structure parses to its end (no trailing data, no animation) and that it contains no markup, then reads the dimensions from the image header.
- **Storage**: the image is kept in the encrypted [file store](file-storage.md) (purpose `branding`, installation scope, never expires), so the store must be configured. Without it the row says so in one line and links to Data & privacy › Storage (`409 reason:"file_storage_not_configured"`). `installation_settings.branding_logo_file_id` (migration 0023) references the current file, with its dimensions; a trigger accepts only a live, committed installation branding file.
- **Replace / Remove**: replacing the logo or removing it deletes the previous object after the change is saved (a failed delete is retried by the sweeper). Removing works with the store off.
- **Serving**: `GET /api/v1/branding/logo` serves it same-origin and without a session (the sign-in page needs it before login), with `nosniff`, `Content-Security-Policy: default-src 'none'`, an ETag and a 5-minute public cache. `GET /me` and the public `GET /api/v1/auth/config` return `logo` (`{url, updated_at}` or null; null when the store is off). See the [management API](management-api.md#public-installation-logo).
- **Audit**: `settings.logo_uploaded` (image kind and byte count) and `settings.logo_removed`.

## Data & privacy

| Setting | Stored value | Environment override (locks the setting) |
| --- | --- | --- |
| OpenRouter data collection | `deny` (default) or `allow`, sent as `provider.data_collection` on every OpenRouter request | `GATEWAY_OPENROUTER_DATA_COLLECTION` |
| Request log retention | Empty (keep) or 30–3650 days | `GATEWAY_EXECUTION_DETAIL_RETENTION_DAYS` |
| Prompt and response storage | Never stored. A fact, not a setting. | — |

When the variable is set, the page shows the effective value as locked and the API rejects changes with `409 reason:"setting_locked_by_environment"` (resending the stored value is allowed). Without it, the stored value applies: the replica that saves it switches at once and the others within one maintenance tick (5 seconds). Until a replica has read the setting it sends `deny`.

Retention compacts settled request metadata older than the window (error code and latency are cleared and `details_redacted_at` is set), hourly in batches of up to 1000. It never deletes or changes usage, prices, reservations, the monetary ledger or the audit log; pending and unknown-cost records are kept. Without a value nothing is compacted.

## Storage

The Storage card on Data & privacy covers the encrypted [file store](file-storage.md). The backend, bucket or endpoint and encryption keys come from the server environment and are shown read-only: the backend kind, the bucket and endpoint **host** (marked HTTP when plaintext), the active encryption key ID with the number of decrypt-only keys, and the last health check (shown only when it was run against the current configuration).

| Group | Purposes | Allow toggle | Retention (stored) |
| --- | --- | --- | --- |
| Batch files | `batch_input`, `batch_output` | `file_batch_enabled` (default off) | `file_batch_retention_days`, 1–365 (default 7) |
| Video outputs | `video_output` | `file_video_enabled` (default off) | `file_video_retention_days`, 1–365 (default 7) |
| User files | `user_file` | `file_user_files_enabled` (default off) | `file_user_files_retention_days`, 1–365 (default 30) |
| Exports | `export` | follows the backend being on | `file_export_retention_days`, 1–365 (default 1) |
| Branding | `branding` | follows the backend being on | never expires |

The toggles cover groups that hold customer content. Turning one on requires a configured backend (otherwise `409 reason:"file_storage_not_configured"`) and a passing health check, which the server runs before saving (otherwise `409 reason:"file_storage_unhealthy"`). Turning a group off stops new files; existing files remain until their retention ends. Retention applies to existing files: a file expires at `created_at` plus the current retention, or at its own explicit expiry if that comes first.

**Test storage** writes, reads and deletes a small random object and records the result (`file_store_last_check_*`, with a fingerprint of the configuration). At most 5 per admin per minute and 60 per hour (`429 reason:"storage_test_rate_limited"`). The audit log records `settings.storage_updated` and `settings.storage_test` with counts and the backend kind only.

## Email

SMTP delivery for workspace invitations and [alerts](alerts.md). Set host, port, TLS mode, optional username and password reference, and the From address and name.

- **TLS**: `starttls` (required, never opportunistic; usually port 587) or `implicit` (usually 465), through rustls with the platform's web PKI roots. `none` is accepted only for a relay on the same machine (`localhost`, `127.0.0.1`, `::1`); otherwise `400 reason:"plaintext_requires_loopback"`.
- **Password**: never stored or returned. Enter a reference `env:NAME` whose variable name is on `GATEWAY_SECRET_ENV_ALLOWLIST`, as for provider credentials; it is resolved at send time. A reference that isn't allowlisted is rejected (`reason:"credential_reference_not_allowed"`). Username and reference go together.
- **Status**: `not_configured`, `credential_unavailable` (the referenced variable is unset or no longer allowlisted) or `ready`, plus the last test result. Saving the relay clears the last test.
- **Send test email**: sends a fixed message to the signed-in admin's verified sign-in address. At most 2 per admin per minute and 20 per installation per hour (`429 reason:"email_test_rate_limited"`), counted from the audit log, so the limit holds across replicas. The result is a category: `credential`, `address`, `connection`, `tls`, `authentication`, `rejected` or `timeout`. Server replies are not shown.
- Delivery: one connection per message, no pool, no retries, 10-second command and 30-second message timeouts. Message bodies, recipients and server replies are never logged.

### Invitations

When a relay is configured, creating an invitation also emails the invite code to the invited address. The code is in the message body only, never a URL: the message points to `${GATEWAY_PUBLIC_URL}/invitations/accept` (or tells the person to open the gateway when the variable is unset), where they sign in and enter the code. The API still returns the code once, so the copy-and-send flow keeps working, and reports `email_delivery: "sent" | "failed" | "not_configured"`. Sending happens after the invitation is committed; a failed send leaves the invitation valid.

## Sign-in

Read-only: whether OIDC is configured, the issuer, client ID, client type (confidential or public), groups claim, public URL and the callback URL to register, plus the number of enabled SSO group mappings. Values come from the server environment ([identity](identity.md)) and change only with a restart. The client secret is never shown.

Live status is also shown:

- **Signing keys:** cached key count, last refresh, and whether keys are current, a cached copy (refresh failing) or unavailable ([identity](identity.md#signing-keys-jwks)).
- **Provisioning (SCIM):** on or off, the base URL to copy, active and total users, groups and memberships, and the last SCIM write ([SCIM](scim.md)). The token is never shown.

`GET /api/v1/platform/settings/sign-in` adds `jwks` (`keys`, `refreshed_at`, `fresh_until`, `last_failure_at`, `state`: `fresh` | `stale` | `unavailable`) and `scim` (`enabled`, plus `base_url`, `users`, `active_users`, `groups`, `memberships` and `last_sync_at` when enabled).

## Operations

The row is created by migration 0010; the runtime role may update its reviewed columns but cannot insert, delete or re-key it (`deploy/staging/runtime-grants.sql`, checked by `verify-privileges.sql`).

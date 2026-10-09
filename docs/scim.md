# SCIM provisioning

The gateway accepts SCIM 2.0 pushes from an identity provider such as Okta or Microsoft Entra ID. SCIM keeps the user directory and group membership current between sign-ins: a deactivated person loses access right away, and group changes reach group mappings without waiting for that person to sign in again.

SCIM does not replace OIDC. People still sign in through the configured issuer, and provisioning a user grants nothing by itself. Access still comes from manual grants or from [SSO group mappings](identity.md#authentication-is-not-entitlement).

## Turn it on

| Variable | Meaning |
| --- | --- |
| `GATEWAY_SCIM_TOKEN_ENV` | Name of the environment variable that holds the bearer token, for example `OKTA_SCIM_TOKEN`. Use uppercase letters, digits and `_`. |
| *(the named variable)* | The token: 32–1024 printable characters, no spaces. Generate it randomly, for example with `openssl rand -hex 32`. |

```sh
GATEWAY_SCIM_TOKEN_ENV=OKTA_SCIM_TOKEN
OKTA_SCIM_TOKEN=<random token from your secret store>
```

- The server reads the token once at startup and keeps only its SHA-256 hash. It never stores the token in the database, logs it or shows it in the UI. To rotate, change the secret and restart.
- SCIM requires OIDC. Startup fails if `GATEWAY_SCIM_TOKEN_ENV` is set without OIDC, names a variable that is unset, or the token is too short.
- When `GATEWAY_SCIM_TOKEN_ENV` is unset, every `/scim` path returns 404.

In the identity provider, use:

- **Base URL:** `${GATEWAY_PUBLIC_URL}/scim/v2`. Admin › Settings › Sign-in shows it with a copy button.
- **Authentication:** HTTP header, `Authorization: Bearer <token>`.
- **Unique identifier for users:** `userName` (Okta and Entra both match on it by default).

Admin › Settings › Sign-in also shows whether SCIM is on, user and group counts, and the time of the last SCIM write. It never shows the token.

## What is supported

The subset of RFC 7643 and RFC 7644 that Okta and Entra use.

| Endpoint | Methods |
| --- | --- |
| `/scim/v2/Users` | `GET` (list, `filter`, `startIndex`, `count`), `POST` |
| `/scim/v2/Users/{id}` | `GET`, `PUT`, `PATCH`, `DELETE` (deactivates) |
| `/scim/v2/Groups` | `GET` (list, `filter`, `excludedAttributes=members`), `POST` |
| `/scim/v2/Groups/{id}` | `GET`, `PUT`, `PATCH` (returns 204), `DELETE` |
| `/scim/v2/ServiceProviderConfig`, `/ResourceTypes`, `/Schemas` | `GET` |

- **Filters:** one `attribute eq "value"` expression. Users: `userName` (case-insensitive), `externalId`, `id`, `emails.value`. Groups: `displayName` (case-insensitive), `externalId`, `id`. Other operators or compound filters return `400 invalidFilter`.
- **Paging:** `count` is capped at 200. There is no sorting, ETag, bulk or password support.
- **PATCH:** `add`, `replace` and `remove` (any letter case), with or without `path`. Entra's string booleans (`"False"`) are accepted. User paths include `active`, `userName`, `externalId`, `displayName`, `name.givenName`, `name.familyName`, `emails` and `emails[type eq "work"].value`. Group paths include `displayName`, `externalId`, `members` and `members[value eq "<id>"]`.
- **Stored user attributes:** `userName`, `externalId`, `name.givenName`, `name.familyName`, `displayName`, the primary (or first) email and `active`. Other attributes, such as `title` or the enterprise extension, are accepted but not stored. Invalid values for stored attributes return `400 invalidValue`.
- **Responses** use `application/scim+json` and `Cache-Control: no-store`. A user resource contains directory attributes only: never platform roles, workspaces, keys or activity.
- Every write is audited (`scim.user.*`, `scim.group.*`, with no names or emails in the metadata).

## Users

A SCIM user is a gateway user, and its `id` is the user's UUID. `GET` and filters also return users who were created another way (manual provisioning or sign-in), so the provider can match existing people instead of creating duplicates. Until the provider writes to such a user, `userName` is the user's email.

- **Create** (`POST`): adds a user with the primary email (or `userName`, if that is an email). There are no grants. As with manual provisioning, the first OIDC sign-in with that verified email links the account once. A `userName` or email that is already in use returns `409 uniqueness`.
- **Email change:** updates the directory email and ends the user's browser sessions, because a directory change is not fresh sign-in proof.
- **Deactivate** (`active: false`, or `DELETE`): suspends the user. Their sessions and their personal and issued API keys are revoked. Grants are kept, and the normal 30-day cleanup applies.
- **Reactivate** (`active: true`): lifts only SCIM's own suspension, within the 30-day grace period. Access again depends on the user's grants. **Revoked keys and sessions stay revoked**; the user signs in again and issues new keys. SCIM never lifts an administrative suspension.
- After cleanup, the user's SCIM attributes and memberships are cleared and the user returns 404. A new `POST` creates a new user; old grants, workspaces and keys never come back.
- SCIM's `active` is the provider's view. A user an administrator suspended can show `active: true` and still have no access.

Shared service accounts are not SCIM users and are never affected.

## Groups and access

A pushed group's `displayName` and `externalId` are both matched against the `group_value` of SSO group mappings for the configured OIDC issuer. Mappings for other issuers are ignored.

- Grants created this way have **group provenance**, the same as grants from a sign-in claim. Manual and bootstrap grants are never touched.
- **SCIM owns a pushed group.** From the moment a group is pushed, its membership comes from SCIM, not from the ID-token claim. A token that lists the group does not grant it to someone SCIM says is not a member. Groups that SCIM has not pushed still come from the token at sign-in.
- Membership changes apply immediately to the affected users. Renaming a group, changing its `externalId` or deleting it re-checks the members, plus anyone holding a grant through the old or new values.
- Losing the last platform grant this way is entitlement loss: sessions and keys are revoked, and the account enters the 30-day grace period. Regaining a mapped grant within that period lifts it, unless SCIM or an administrator suspended the user.
- Removing a workspace group grant revokes that user's keys in that Team or Project.
- Unknown member ids return `400 invalidValue`; they are never dropped silently.
- A new or changed mapping applies to existing SCIM members at their next sign-in or the next SCIM change to that group.

There are no last-admin or last-owner safeguards for SCIM changes: like other external entitlement loss, the identity provider's decision wins. Keep a manual Admin grant for at least one break-glass account.

## Operations and limits

- Each write runs in one transaction under the installation lock, the same lock that sign-in and management use. A change to a very large group's name or `externalId` re-checks every member in that transaction.
- Runtime database access is limited to the reviewed columns in `deploy/staging/runtime-grants.sql` (migration `0014_scim.sql`). SCIM user links cannot be deleted or re-keyed. Groups and memberships are directory state that can be removed, and grant history stays in the grant tables and audit log. `verify-privileges.sql` checks this and runs rollback-only probes.
- Rate-limit `/scim` at the edge, and keep `Authorization` headers out of edge logs.
- Tests in `apps/gateway/src/scim/tests.rs` use local requests only. Real Okta and Entra acceptance has not been run yet.

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

Before the identity provider sends its first push, set up a Platform Admin who can sign in and the group mapping: see [Before the first push](#before-the-first-push-bootstrap-admin-and-group-mapping).

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
- **Deactivate** (`active: false`, or `DELETE`): suspends the user. Their sessions and their personal and issued API keys are revoked. Grants are kept, and the normal 30-day cleanup applies. Deactivating the last active Platform Admin is refused (see [below](#the-last-platform-admin-is-protected)).
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

### The last Platform Admin is protected

SCIM never removes platform access from the last active Platform Admin. An active Platform Admin has a live Admin grant (manual, bootstrap or group provenance) on an account that is neither suspended nor cleaned up. If a SCIM write would leave the installation with none, it is refused whole:

- **What is covered:** `PATCH`/`PUT` with `active: false`, `DELETE` of a user, and any Group change (`POST`, `PUT`, `PATCH`, `DELETE`, rename, `externalId` change, member removal) that would drop the last effective Admin grant.
- **Response:** `409` with `scimType: "mutability"` and the detail "Can't deactivate the last active Platform Admin. Grant Admin to someone else first."
- **No partial change:** the whole request rolls back. Other attributes in the same request are not stored, and no session or key is revoked.
- **Record:** an audit event `scim.last_admin_protected` (no names or emails) and the built-in installation alert "SCIM tried to remove the last Platform Admin". Platform Admins and Auditors see it in Notifications and Admin › Settings › Alerts › History, and Platform Admins get it by email when a relay is set up. At most one is open at a time. It clears once a second active Platform Admin exists.
- **Concurrency:** the check runs under the installation lock that manual grant and suspend changes also take, so two concurrent requests cannot both pass it.

To retire the last Admin, grant Admin to someone else first, then repeat the change in the identity provider. The same rule applies to manual changes: Admin › Users and the management API refuse to revoke the last Admin grant, suspend the last Admin, or change or delete the group mapping that holds it. There is no last-owner safeguard for SCIM: losing a Team or Project owner through SCIM follows the identity provider.

## Before the first push: bootstrap admin and group mapping

Provisioning grants nothing. The people SCIM creates get access only from an SSO group mapping or a manual grant, and only a signed-in Platform Admin can create either. Set that administrator up before the identity provider pushes anything. Do the steps in this order.

1. **Provision a bootstrap Platform Admin who can sign in.** Use an identity-provider account that a person controls and that has a verified email: an operator's own account, or a disposable one made for this purpose. Never use a placeholder address that has no account behind it. The identity provider must allow that account to open the gateway's OIDC application. Run the command with trusted database access (see [identity](identity.md#linking-suspension-and-cleanup)); `python3 scripts/staging.py provision-user --email …` runs the same command in the [staging rehearsal](staging.md#oidc-information-needed-next).

   ```sh
   cargo run -p open-model-gateway -- provision-user --email operator@example.org --platform-admin
   ```

   *Check:* the command prints `Provisioned user <id>. OIDC must provide the same verified email on first sign-in.`
2. **Sign in once as that admin, while the SCIM provider is still off.** The first sign-in links the identity to the provisioned account by verified email. The link is one-use and is now consumed. *Check:* the dashboard opens and shows the Admin pages. If the sign-in is denied, stop and fix it; do not activate SCIM.
3. **Create the SSO group mapping.** In Admin › SSO groups, add a mapping for the configured issuer whose group value is the `displayName` or `externalId` of the group the provider will push (see [Groups and access](#groups-and-access)). Map only the groups that should have access, to the lowest role they need. The issuer and the group value must match exactly, including case and any trailing slash; copy the issuer from Admin › Settings › Sign-in. *Check:* the mapping is listed and enabled, and its issuer (turn on the Issuer column) is the one shown in Admin › Settings › Sign-in. Mappings for another issuer are ignored.
4. **Only then activate the SCIM provider and run the first sync.** If the provider has a dry-run mode, read its planned requests first. *Check:* Admin › Settings › Sign-in shows the user and group counts and a recent last SCIM write. In Admin › Users, members of the mapped group have a role that came from the group, everyone else pushed has no role, and the bootstrap admin still has its Admin role.

### A pushed user with no grant is denied

A pushed person who is in no mapped group and has no manual grant cannot sign in. This is correct behaviour, not a fault: neither provisioning nor authentication grants access ([identity](identity.md#authentication-is-not-entitlement)).

- **After the push**, the account is active, has no grant, and may link once at its first sign-in. Admin › Users lists it without a role.
- **When that person signs in**, the gateway links the identity, finds no platform role and denies the sign-in. No session is created. The browser returns to the sign-in page (`/?auth_error=access_denied`), which says "No access yet. Ask a platform admin to add you."
- **The denied sign-in suspends the account** with the reason `entitlement_loss` and schedules the normal cleanup 30 days later. Admin › Users then shows the account as **No access**, and the person's page shows the cleanup date.
- **To give access**, add the person to a mapped group in the identity provider, create a mapping for a group they are already in, or grant them a role in Admin › Users. The suspension lifts at their next sign-in, or at the next SCIM group change that affects them, if that happens within the 30 days.
- **`provision-user` does not lift it.** It refuses any suspended account with "Account is suspended; reactivate through audited platform management first".

This is why the order above matters. If SCIM runs first and the only Platform Admin cannot sign in, nobody can create the mapping. Each pushed person who tries to sign in is denied and suspended, and `provision-user` then refuses those accounts, so the installation has no working administrator. If you are already there, run `provision-user --platform-admin` for an account that has never signed in: a new address, or a pushed person who has not tried to sign in yet. It refuses any account a denied sign-in has suspended, so if your own account is suspended, use another identity. Sign in with it and create the mapping. Then, in Admin › Users, revoke the Admin role of the account that could not sign in, so that nobody can claim it later. People who were already denied get access at their next sign-in within the 30 days. Do not grant access automatically or relax the suspension to get past this.

## Authentik

Authentik 2025.10's SCIM provider needs three non-default settings:

- **`userName` must be the email.** The default user mapping sends Authentik's username, not the email. The gateway matches existing accounts by `userName` (the email until SCIM first writes it) or by email, so with the default mapping a `POST` returns `409 uniqueness` on the email and the user never links. Use a custom SCIM user mapping that returns `"userName": request.user.email`.
- **Limit pushed groups.** `filter_group` on the provider filters users only; Authentik still pushes every directory group. Use a custom group mapping that pushes only the groups the gateway maps:

  ```python
  if group.name != "<access group>":
      raise SkipObject
  return {"displayName": group.name}
  ```

  `SkipObject` is provided in the expression context. Importing `SkipObjectException` from `authentik.lib.sync.outgoing.exceptions` fails on 2025.10.
- **Omit `emails` for reserved test domains.** Authentik validates email addresses before sending, so a test account on `.invalid`, `.test` or `.example` aborts the whole user sync at that account ("Stopping sync due to error"); the rest of the page never syncs. Omit `emails` for those domains in the mapping. The account still matches by `userName`.

Authentik's provider also has a dry-run mode, a safe way to see the planned requests before turning it on.

### Observed with Authentik during the live acceptance run (#60)

These are observations from one run against Authentik 2025.10.3 on 2026-10-28, not guarantees about Authentik. They matter to anyone who scripts the provider's setup, its ownership checks or its teardown through Authentik's API. Check each against the version in use.

- **Mapping expressions came back with trailing whitespace trimmed.** A script that compared the stored expression with the submitted text had to trim both first.
- **Dramatiq actor names were module-qualified.** A lookup by the bare actor name did not find the task.
- **A SCIM provider's `used_by` edges had two types.** User mappings and backchannel links appeared as `authentik_core.provider`. Group mappings and the filter group appeared as `authentik_providers_scim.scimprovider`. An ownership check had to expect both.
- **Sign-ins left OAuth2 access tokens that added edges to the OAuth2 provider.** They had to be revoked, after confirming each token's provider and user, before a strict ownership check passed.
- **The run's own cleanup of disposable users deactivated them in Authentik.** It had been run by mistake partway through. Any teardown step that deactivates or removes test users belongs at the end of acceptance.

## Operations and limits

- Each write runs in one transaction under the installation lock, the same lock that sign-in and management use. The last-admin check runs at the end of that transaction (see [above](#the-last-platform-admin-is-protected)). A change to a very large group's name or `externalId` re-checks every member in that transaction.
- Runtime database access is limited to the reviewed columns in `deploy/staging/runtime-grants.sql` (migration `0014_scim.sql`; the last-admin alert kind is in `0018_job_limits.sql`). SCIM user links cannot be deleted or re-keyed. Groups and memberships are directory state that can be removed, and grant history stays in the grant tables and audit log. `verify-privileges.sql` checks this and runs rollback-only probes.
- Rate-limit `/scim` at the edge, and keep `Authorization` headers out of edge logs.
- Tests in `apps/gateway/src/scim/tests.rs` use local requests only. Authentik has been run twice. Authentik 2025.10 was run against v0.7.1 on 2026-10-23, covering matching of existing users, one pushed group, and deactivate/reactivate. Live acceptance passed on 2026-10-28 against Authentik 2025.10.3 on a signed v0.9.1 image (#60), on an isolated staging installation that has since been removed. It covered provisioning, a denied sign-in with no grant, first linking with a mapped group grant, deactivation, and a group removal ending a live session. No later release has been run against a real identity provider. Okta and Entra have still not been run.

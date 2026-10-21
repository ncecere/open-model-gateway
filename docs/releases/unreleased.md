# Open Model Gateway: unreleased

Notes for changes on `main` since v0.6.1. They become the next release's notes.

## Security

- **Every route has a declared, tested authorization** (#195). Prompted by LiteLLM CVE-2026-35029, where a route-prefix allowlist let any logged-in user reach a configuration API. Every method and route template the gateway serves is declared in `apps/gateway/src/route_policy.rs` with its required access, matched exactly (never by prefix). Management routes are checked against that declaration before the request body is read, and an undeclared management route is refused even for a Platform Admin; handlers keep their own checks. Tests compare the declarations with the routes found in the source, check every route against 15 kinds of caller, and pin the seven unauthenticated routes. Management requests now make one extra read before the handler (about 1 ms per workspace request and 2–3 ms per platform request in a local measurement; see [operations](../operations.md#read-snapshots-and-the-reporting-replica)).

  Refusals you may notice:
  - A workspace member who isn't an admin, rotating another person's API key, gets `404` in every state (it was `403` for an active key). Workspace admins still get `403`, and now also for a disabled key (it was `key_disabled`).
  - Auditors who aren't members of a workspace get `403` instead of `404` on its key-policy routes.
  - Platform-admin-only workspace actions refuse non-admins with `403` before looking up the workspace.
  - Malformed workspace or key ids in a management path return `404`.
- **No request input can choose an upstream destination or credential (#196).** An audit against the LiteLLM client-side-credentials class of bug (CVE-2026-59823) found none. The [threat model](../threat-model.md#provider-credentials-and-egress) now lists every outbound call and who controls its destination, and tests send steering bodies, headers and model strings through the inference protocols against an approved and an attacker mock upstream. It found and fixed three smaller gaps:
  - **Local-upstream approvals.** An `https://` approval could name a 6to4 or Teredo IPv6 address that embeds a metadata, link-local or loopback IPv4 address. Such an address is now judged by the IPv4 address it carries: an embedded metadata or link-local address is refused, and an embedded loopback address only with `GATEWAY_ENV=development`. A private or public embedded address is still accepted over HTTPS, as the plain address is, and everything else (NAT64 included) is decided as before. Refusing more special-use ranges needs a startup-compatible migration and is tracked in #203.
  - **Model names.** A `model` containing a NUL or other control character, or longer than a database index entry, returned `503 accounting_unavailable` (or reached a query it could never match). It is now `404 model_not_found` without a query. Every other name is still looked up exactly, so a model name added by SQL with a space, `@` or an accented letter keeps working.
  - **Provider job and file ids** of `.` or `..` were accepted as a URL path segment. They are refused.

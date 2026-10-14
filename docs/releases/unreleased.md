# Open Model Gateway: unreleased

Notes for changes on `main` since v0.4.0. They become the next release's notes.

## Inference

- `POST /v1/responses` with `stream: true` now forwards text as `response.output_text.delta` events as the model generates it, instead of after the model finishes. Tool calls are still emitted whole, after the text; usage, settlement and the 4 MiB output bound are unchanged (#22).
- `POST /v1/messages` with `stream: true` now sends `message_start` immediately and streams text as `text_delta` events; tool calls arrive whole after the text. `message_start` usage is now `{"input_tokens":0}` (a placeholder); the observed input, cache and output counts arrive in `message_delta`, which the Anthropic SDKs apply (#23).

## Scale

- Dashboard lists stay fast with tens of thousands of users (#65). The platform users list builds rows only for the requested page and per-user lookups are indexed (migration `0037_user_lookup_indexes`): page 1 took 1.1 s and deep pages minutes to hours with 50,000 users. The platform users, user detail and workspaces lists, and workspace members, keys and service accounts, read from the same lock-free snapshots as reports, so one slow list no longer queues every other admin request behind it; like those reads, a statement running over 10 s is cancelled and returns 503 "The request took too long".
- Home loads its models with one request (`GET /api/v1/me/models`) instead of one catalog request per workspace, and its usage tiles with `GET /api/v1/me/summary?totals_only=true` (#66). For a user in 2,000 workspaces, Home took about 2 minutes and 2,000 requests, then the browser tab crashed.
- Audit logs and API key lists no longer load every user, member and workspace to show names (#67): audit rows carry `actor_name`, `workspace_name` and `target_name` and key rows `issued_to_name`, resolved by the gateway under the same visibility rules. With 50,000 users the platform audit log couldn't load its user list at all.
- Choosing people never loads the whole directory (#67): Add member searches on the server for Platform Admins too (`GET /api/v1/platform/workspaces/{ws}/member-candidates`), the new Team/Project form searches for its initial owner, and member tables page and search on the server (`q` on the member lists). People who are already members are no longer listed as disabled options; they simply aren't candidates.
- Logs and Usage search their Workspace and API key filters on the server and label a selected filter by its own record, instead of loading every workspace or key (#69). The SSO group mappings and cost center assignment pick Teams and Projects by search, mapping rows carry their workspace's name, and the cost report's workspace rows carry their `kind`. Key lists take `q`.
- Choosing or filtering a model no longer loads every model (#68). The Admin Logs and Usage model filters, the catalog's Add models and a team's or project's Assign models pickers, the direct-model actions in Usage › By workspace, Add route and Routing search `GET /api/v1/platform/models?q=` on the server, and a filtered model is named from its own record; a connection's page lists its models a page at a time. The platform model list and a workspace's catalog build rows only for the requested page: with 2,004 models, loading every model took 11 requests and 23.4 s (worst page 3.9 s against 0.17 s for page 1), and a workspace catalog of 874 models 5 requests and 2.8 s (worst page 0.84 s). The Admin and workspace Models pages still load their whole list for client-side facet counts, now without slow deep pages.

## Dashboard

- Add model now marks required pricing ceilings, shows a linked validation summary and focuses the first invalid field. Text and System One output-ceiling errors consistently require a positive value (#18).

## Deployment

- New `open-model-gateway grants apply` applies the reviewed runtime grants (`deploy/staging/runtime-grants.sql`, now embedded in the binary); `grants verify` runs the rollback-only privilege probes. The Helm migration Job and the staging `grant-runtime` step use it, so the chart no longer needs a PostgreSQL client image (`migrations.grants.image` is removed) or a copied SQL file (#20).

- Helm: a first install with `migrations.run=true` is now one step, including the chart-managed CloudNativePG Cluster. The migration Job runs as a plain resource on install (a `pre-upgrade` hook on upgrades) and waits for the database; gateway pods wait for the schema (`wait-for-database --schema`) instead of crash-looping. New `open-model-gateway wait-for-database [--schema]` (#19).

- Helm loadtest profile: the `loadgen` Job now passes the flags `loadgen run` accepts (`--target`, `--rate`, `--duration`, `--warmup`, `--keys`, `--mock-url`). `loadtest.loadgen.requests` and `concurrency` are replaced by `rate`, `duration`, `warmup`, `keys` and `extraArgs`. The load-test image is built on request by the `loadtest-image` workflow (#25).

## Operations

- Encrypted, off-host backups (#104). `scripts/backup.py backup --encrypt-to age1…` streams `pg_dump` through [age](https://age-encryption.org), so the plaintext dump never reaches disk. `verify --identity` checks the decrypted plaintext; `restore --identity` decrypts in two passes without writing plaintext. Manifests record the file-store key IDs that stored files use, and restore refuses unless `--file-keys-env` holds all of them (or `--skip-file-key-check`). New `upload`, `list`, `download` and `prune` copy generations to S3-compatible storage (SigV4, standard library only; the archive is uploaded before its manifest; prune is a dry run without `--yes`). These are logical dumps, not point-in-time recovery; see [operations](../operations.md#encrypted-backups) for key custody.

## Verification

- Clean-host restore drill on the homelab cluster (#105): RPO was the 48 s since the last backup, and recovery to a serving gateway took 92 s. See docs/operations.md. The drill found that backups carried extensions from the cluster's `template1` (pgvector), which blocked restore as the migrator. `backup` now dumps only `plpgsql`. The docs now cover administrator-created targets (`REVOKE ALL … FROM PUBLIC`, `GRANT CONNECT`), the rename cutover, and revoking restored sessions.

- Browser regressions cover invalid and corrected Add-model submission at desktop and phone widths. The Usage & costs resize check waits for the mobile shell to render before asserting zero horizontal overflow.

# Handoff: 54-issue closeout, paused 2026-10-28 (Wave 4 in progress)

Resume target: `bitop-coder-03:/home/nick/.t3/projects/open-model-gateway`.
**This branch is a handoff bundle only. Never merge it.** Everything else is on normal `issue/*` branches.

## Where the program stands
- Waves 0–3 are done. #391 (CNPG durability) is closed: v0.9.3 is in production, quorum durability is live, and the failover drill passed. #48 is closed (PRs #395/#397/#398).
- **Wave 4 (API contracts) is in progress**: #81 management OpenAPI (6 slices), then #82 inference OpenAPI (3 PRs), then #38 umbrella.
- Plan: `planning/close-all-54-plan.md` (waves 5–10 follow; operator decisions at the bottom).

## In-flight work: branches (all pushed)
| Work | Branch | Head | State / next step |
|---|---|---|---|
| #81 slice 1 harness + identity (PR **#399**) | `issue/81-slice1-harness` | `a7199b39` | Full local gates passed. Blocked by one CodeQL `rust/cleartext-logging` alert, which is FIXED on the coverage branch (`e2e0e8ee`). Fast-forward #399 to the coverage branch once that is finished. |
| #81 coverage gates | `issue/81-harness-coverage` | `86c8dbd8` | A (`fe45ced4` finish() enforces query/array/branch/null coverage) and B (`07ed1d33` HTTP boundary vectors) are DONE. **Remaining:** C, make slice 1's fixture `management_openapi_completed_area_fixtures` pass the gates (it needs `query_vectors` for 11 operations, `body_vectors_http`, and some coverage gaps); D, full checks; E, append the rules to `planning/81-slice-brief.md` ("Slice author guide"). |
| #81 slice 2 identity/access | `issue/81-slice2` | `9d6e7b2e` | Fix round partly done. Check `planning/81-findings/s2.md` against the commits; then fixtures and gates. |
| #81 slice 3 catalogs/setup | `issue/81-slice3` | `521e0b35` | Has a WIP commit of unwired fixture helpers. Remaining: empty `serving_routes_gained`, unobserved null branches (policy, model-setup), missing body vectors. |
| #81 slice 4 money/reporting | `issue/81-slice4` | `35152910` | Schema fixes done; the FIXTURES are not started (see `81-findings/s4.md`). |
| #81 slice 5 settings/alerts | `issue/81-slice5` | `08076788` | Settings done. Next: alerts. Split rule/event/webhook schemas into platform/workspace/created variants; ordinary-only workspace events schema. |
| #81 slice 6 logs/files/credentials | `issue/81-slice6` | `70226f2c` | Review fixes done. Next: coverage gate on `management_openapi_slice6_fixtures`; seed batch_lines/batch_route_waits; redacted request details. |
| #400 personal-visibility rule (PR **#401**) | `issue/400-personal-visibility-rule` | `1e72b449` | Reviewed and fixed. Needs full exact-head local gates, then merge, then close #400 with a summary. Auto-merge NOT armed. |
| #16 tungstenite 0.30 | `issue/16-tungstenite-030` | `c9cad055` | Option B (axum pinned `=0.8.9`, browser-tungstenite 0.29 alias; upstream 0.30). Remaining: DB-backed realtime contracts `cargo test -p open-model-gateway --all-features --lib protocols::realtime::tests::contract:: -- --test-threads=1`, then two reviews, gates, PR. |
| #60 SCIM staging profile (preservation) | `issue/60-isolated-scim-staging` | `3664e2d4` | #60 is closed. These 3 commits existed only locally; they were pushed to preserve them. Decide later whether to PR them. |

Every #81 slice branch already contains the slice-1 harness cherry-picks plus `e2e0e8ee 8f03791c fe45ced4`. After the coverage branch finishes, each slice must also cherry-pick B and the rest (`07ed1d33 4d30d6c4`; plus `86c8dbd8` is slice-1-specific: skip it) and wire HTTP vectors into its fixtures.

## Merge order for #81
1. #399 ← coverage branch (fast-forward), full gates, CodeQL/CI, merge.
2. Each slice 2–6: rebase onto main (patch-id dedupes the harness cherry-picks), regenerate the artifact (`OMG_REGENERATE_OPENAPI=1 cargo test -p open-model-gateway --lib management_openapi_artifact_matches_regeneration`), re-check the incomplete baseline, full gates, PR (body says "part of #81"; no closing keyword), merge, one at a time.
3. #81 closes when the incomplete baseline is 0 and every area is in `AREA_GROUPS` with passing gates. Update `docs/openapi/management/README.md` progress numbers once at the end.

## Rules that must carry over (see also AGENTS.md)
- **Subagents use Sonnet 5.5.** Review bar: two reviews (standard + adversarial) for production/credential/authz/money/published-artifact changes; one for test-only/tooling.
- TDD; exact-head local gates; protected checks; merge only with `--match-head-commit <reviewed head>`. Never blind-retry, skip tests, weaken assertions or waive CVEs.
- The #81 slices change NO production behaviour; they document current behaviour (#400 Option A for personal-workspace identity).
- Contract-harness messages must never include captured values, concrete paths, headers or bodies (CodeQL).
- **sqlx test isolation:** sqlx drops every `_sqlx_test_*` database created before its process started, **per master database**. Each concurrent test run needs its own master DB (or its own server). Never let two suites share one master DB.
- Name test-only modules `*_tests.rs` (the egress source scans treat other files as production).
- Commit after every step (network resets killed several agents mid-task).
- Secrets only in memory, stdin or 0600 files; never in argv, logs or chat.

## Not portable (recreate on the new machine)
- **Local Postgres for tests.** Here, each slice used its own `postgres:17` container (mirror pin `mirror.gcr.io/library/postgres@sha256:858bb5e98eed02eb8cafbcdc007f9a65d1b613972028944a8ec2133ccaf80d27`) with `-c fsync=off -c synchronous_commit=off -c full_page_writes=off -c max_connections=200`, a 1 GiB memory cap, a Docker volume (not tmpfs), and a master DB `omg81`. The URL lives in a 0600 JSON handoff (`{"url": ...}`).
- **Full-gate harness** `scripts/gates/steering370-full.py` + `steering370-pg.py`: written for macOS. They hardcode `/Users/nicholascecere/...` paths, the Docker socket `unix:///Users/nicholascecere/.docker/run/docker.sock`, Python 3.9 and the fixture port 54395. Adapt the paths before use. The harness runs fmt, clippy, Rust default + all-features, CI inventory, web, browser (Playwright), supply chain, Helm, staging, container and backup gates against an owned, pinned PG17 fixture, and refuses to finish if unknown containers appear (do not delete foreign containers).
- **Toolchain:** Rust 1.99.1, Node 22.23.0, Helm v4.3.0, Python 3.9-compatible scripts.
- **Ops scripts** in `scripts/ops/` (production backup/restore drill, failover drill, privilege verifier, OIDC login, release verifier) assume the macOS homelab checkout and kubeconfig at `/Users/nicholascecere/homelab/bitop-talos/kubeconfig`, context `admin@bitop-talos`, and OpenBao at `https://secrets.bitop.dev` (`bao login` needed). Encrypted backups are in `~/.local/state/open-model-gateway/backups/` on the Mac; they are not in git.
- Do not touch on the homelab: the dirty canonical checkout `homelab/bitop-talos`, kind clusters `pdp-p0`/`pdp-op`, or the b261 Released PVs on cp-2.

## Open follow-up issues
#386 (PG image adoption, operator decision), #389, #390, #393, #396 (flaky metrics test, port TOCTOU), #132. #28 is deferred, #63 closed, and paid smoke tests are approved under a $5 cap for Waves 6/8.

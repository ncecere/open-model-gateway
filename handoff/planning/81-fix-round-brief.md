# #81 slice fix round: shared brief

You own ONE #81 slice worktree. Its schemas are written and reviewed, and reviews require changes. This round:

1. **Adopt the new harness commits** (on branch issue/81-harness-coverage):
   `git cherry-pick e2e0e8ee 8f03791c fe45ced4`
   - e2e0e8ee: contract failure messages never include captured values/paths/bodies (CodeQL cleartext-logging fix). Keep that property in anything you add.
   - 8f03791c: coverage engine + quality rules: `*_microusd` money pattern rule (response patterns reject leading zeros and -0; decimal RATE fields only via the reviewed exception list), typed ARBITRARY_JSON, and the exemption annotations `x-omg-may-be-empty: "<reason>"` / `x-omg-unobserved-branch: "<reason>"`.
   - fe45ced4: `finish()` enforces, for every completed operation in your group:
     - every documented query parameter used in a positive request;
     - every response array location non-empty at least once;
     - every union branch observed, including null vs non-null.
   Resolve conflicts keeping both sides' intent; regenerate the artifact instead of hand-merging (`OMG_REGENERATE_OPENAPI=1 cargo test -p open-model-gateway --lib management_openapi_artifact_matches_regeneration`).
2. **Fix every item** in your slice's findings file `.local/issue-planning/81-findings/s<N>.md` (absolute path below). Verify each claim against the handler source first. If a claim is wrong, say so with file:line evidence in your report rather than applying it.
3. **Make the coverage gates pass** with REAL fixture data: populated arrays, every branch, null and non-null, every documented query parameter. Exemptions only where genuinely unreachable in tests, each with a precise reason. A gate failure that reveals a wrong schema (e.g. nullable where the handler never nulls) means fixing the schema, not exempting it.
4. **Later (not now):** HTTP boundary-vector helpers (`query_vectors`/`body_vectors_http`) are still being built on the harness branch. You will be told to cherry-pick them separately. Do not implement them yourself.

Personal-workspace privacy decision (#400 = Option A): Platform Admins and Auditors MAY see a personal workspace's identity with its usage/cost/storage totals and platform-managed configuration (policy/limits, catalog overrides); NEVER its keys, request/execution details, prompts, sessions, file contents or owner choices. Model identity-bearing personal rows explicitly; do not change behaviour.

## Rules (unchanged)
- No production behaviour changes. Test-only code, fragments and the regenerated artifact only.
- Your own PostgreSQL only: DATABASE_URL from `/tmp/omg81-s<N>-pg.json` (`url` key; never print it). Never start, stop or remove containers; never use ports 54339/54349/54369; no Docker commands.
- Build: `CARGO_TARGET_DIR=<your worktree>/target-wt CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_BUILD_JOBS=2`; prepend `/Users/nicholascecere/projects/rust/open-model-gateway/.local/toolchains/1.99.1/bin` to PATH; `--test-threads=4` for DB runs.
- **Commit after EACH logical step** (network interruptions have killed agents mid-task). New commits only: never amend, rebase or push.
- The slice-author guide is in `/Users/nicholascecere/projects/rust/open-model-gateway/.local/issue-planning/81-slice-brief.md`.

## Required checks before reporting
- `cargo fmt --all -- --check`;
- `cargo clippy -p open-model-gateway --all-targets --all-features -- -D warnings`;
- `cargo test -p open-model-gateway --lib egress`;
- the FULL all-features library suite with your DB: `cargo test -p open-model-gateway --all-features --lib -- --test-threads=4` (if SDK test deps are missing, run an offline `npm ci` from the lockfile; never change manifests).

## Report (≤250 words)
Commits; each finding → fixed / disputed (with evidence); exemptions added with reasons; schema corrections the gates exposed; full-suite counts.

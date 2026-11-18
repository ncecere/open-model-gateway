#!/usr/bin/env bash
# The test run inside the Spark test pod (deploy/spark/test-job.yaml, started by
# scripts/spark-test.py). Environment (all defined in test-job.yaml): OMG_SUITE
# (lib|integration|web|checks|all), OMG_SHA (the exact commit), OMG_REPO_URL (the public
# repository), OMG_JOBS (build jobs), OMG_TEST_THREADS (libtest threads), OMG_WEB_WORKERS
# (Vitest workers), OMG_TEST_ARGS (extra libtest or Vitest arguments, lib and web only),
# OMG_DB_HOST, OMG_TARGET_LIMIT_GIB, OMG_CACHE_LIMIT_GIB, OMG_FRESH (1: start with an empty
# target directory), OMG_NONCE (the random value every marker line carries). The PostgreSQL
# superuser password is a file ($OMG_SECRETS/password), read only inside with_db and
# with_loopback_db, never an environment variable.
#
# The commands are the ones .github/workflows/ci.yml runs (deploy/spark/test-coverage.json lists
# them and what is not run; tests/supply-chain/spark-test-coverage.test.mjs compares both with
# ci.yml). The lines `::omg-test::<nonce>:: ...` are the machine-readable progress the tool turns
# into the summary; test output cannot forge them without the nonce, which is not exported to
# the steps. Exit status: 0 only if every step of every selected suite passed.
#
# The paths below default to the pod's; tests/spark-test-script.test.py overrides them.
set -uo pipefail

OMG_CACHE=${OMG_CACHE:-/cache}
OMG_SECRETS=${OMG_SECRETS:-/secrets}
OMG_SCRIPTS=${OMG_SCRIPTS:-/scripts}
OMG_TMP=${OMG_TMP:-/tmp}
OMG_NODE_ROOT=${OMG_NODE_ROOT:-/opt/node}
OMG_IMAGE_RUSTUP=${OMG_IMAGE_RUSTUP:-/usr/local/rustup}
OMG_DB_PORT=${OMG_DB_PORT:-5432}
OMG_LIB_SHARDS=${OMG_LIB_SHARDS:-4}
OMG_FRESH=${OMG_FRESH:-0}
OMG_NPM_CI_FLAGS=${OMG_NPM_CI_FLAGS:-}
SRC=$OMG_CACHE/src
# Not exported: the steps (cargo, npm, the tests) do not inherit it.
NONCE=${OMG_NONCE:-}
export -n OMG_NONCE 2>/dev/null || true

mark() { printf '::omg-test::%s:: %s\n' "$NONCE" "$*"; }

# Everything printed passes this filter: the password and any PostgreSQL URL with a password.
redact() {
  local pw=''
  [ -r "$OMG_SECRETS/password" ] && pw=$(<"$OMG_SECRETS/password")
  pw=$(printf '%s' "${pw:-__none__}" | sed 's/[][\.*^$/&|]/\\&/g')
  sed -u -E -e "s/${pw}/[redacted]/g" -e 's#(postgres(ql)?://[^:/@[:space:]]+:)[^@[:space:]]+@#\1[redacted]@#g'
}

step() { # step <suite> <name> <command...>: run in the source tree, record the exit status and time
  local suite=$1 name=$2 start=$SECONDS code
  shift 2
  mark "step-start suite=$suite name=$name"
  echo "$suite/$name" >"$OMG_TMP/omg-step"
  ( cd "$SRC" && "$@" )
  code=$?
  mark "step-end suite=$suite name=$name exit=$code seconds=$((SECONDS - start))"
  return "$code"
}

with_db() { # run a command with DATABASE_URL pointing at the run's PostgreSQL; the password exists only here
  local pw
  pw=$(<"$OMG_SECRETS/password") || return 1
  DATABASE_URL="postgres://postgres:${pw}@${OMG_DB_HOST}:${OMG_DB_PORT}/gateway" "$@"
}

with_loopback_db() { # bootstrap fixtures and workspace tests require a literal loopback database address
  local code pw relay
  pw=$(<"$OMG_SECRETS/password") || return 1
  PG_FORWARD_TARGET="$OMG_DB_HOST" node "$OMG_SCRIPTS/pg-forward.mjs" &
  relay=$!
  sleep 1
  if ! kill -0 "$relay" 2>/dev/null; then
    wait "$relay" 2>/dev/null
    echo "PostgreSQL loopback relay exited before running tests" >&2
    return 1
  fi
  DATABASE_URL="postgres://postgres:${pw}@127.0.0.1:5432/gateway" "$@"
  code=$?
  kill "$relay" 2>/dev/null
  wait "$relay" 2>/dev/null
  return "$code"
}

setup_environment() {
  mkdir -p "$OMG_CACHE" "$OMG_TMP" "$SRC"
  export HOME=$OMG_CACHE/home CARGO_HOME=$OMG_CACHE/cargo RUSTUP_HOME=$OMG_CACHE/rustup CARGO_TARGET_DIR=$OMG_CACHE/target
  export NPM_CONFIG_CACHE=$OMG_CACHE/npm
  export PATH="$OMG_NODE_ROOT/bin:/usr/local/cargo/bin:$PATH"
  # No debug info (smaller cache, less memory). Not incremental, as CI (the one compile OOM-killed at the 10 GiB limit was an incremental one; a changed gateway crate rebuilds in about 2.5 minutes without it).
  export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
  export RUST_TEST_THREADS="$OMG_TEST_THREADS" CARGO_BUILD_JOBS="$OMG_JOBS" CARGO_TERM_COLOR=never CARGO_TERM_PROGRESS_WHEN=never CARGO_NET_RETRY=5
  export CI=true GIT_TERMINAL_PROMPT=0 GIT_CONFIG_NOSYSTEM=1
  mark "environment arch=$(uname -m) cpus=$(awk '{print $1}' /sys/fs/cgroup/cpu.max 2>/dev/null)/$(awk '{print $2}' /sys/fs/cgroup/cpu.max 2>/dev/null) jobs=$OMG_JOBS test_threads=$OMG_TEST_THREADS web_workers=$OMG_WEB_WORKERS fresh=$OMG_FRESH"
  node --version && npm --version
}

# The cache is trusted only as far as the refs that ran in the slot. What cargo, rustup, git and npm
# do not verify against a lockfile or a hash is reset at the start of every run: the toolchain, the
# cargo configuration, binaries and unpacked sources, the registry index, the home directory
# (npm and git configuration, credentials) and the source checkout's .git (hooks, configuration,
# locks). Kept: the crates (registry/cache) and git databases cargo checks against Cargo.lock, and
# the npm cache npm checks against package-lock.json integrity values. target/ cannot be verified
# and is kept for speed unless OMG_FRESH=1.
reset_cache() {
  local c=$OMG_CACHE
  rm -rf "$c/home" "$c/.cargo" "$c/config.toml" "$c/config"
  mkdir -p "$c/home" "$c/cargo" "$c/npm" "$c/target"
  rm -f "$c"/cargo/config.toml "$c"/cargo/config "$c"/cargo/credentials "$c"/cargo/credentials.toml "$c"/cargo/.crates.toml "$c"/cargo/.crates2.json
  rm -rf "$c"/cargo/bin "$c"/cargo/registry/src "$c"/cargo/registry/index "$c"/cargo/git/checkouts "$c"/cargo/.package-cache "$c"/cargo/.package-cache-mutate
  rm -rf "$c/rustup"
  cp -a "$OMG_IMAGE_RUSTUP" "$c/rustup" || return 1
  if [ "$OMG_FRESH" = 1 ]; then
    find "$c/target" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
  fi
}

prepare_source() {
  rm -rf "$SRC/.git"
  mkdir -p "$SRC" && cd "$SRC" || return 1
  git init -q . || return 1
  # An exact commit of the public repository, no credentials. A commit that was never pushed cannot be fetched.
  if ! git fetch -q --no-tags --depth 1 "$OMG_REPO_URL" "$OMG_SHA"; then
    echo "cannot fetch $OMG_SHA from $OMG_REPO_URL: push the branch first"
    return 1
  fi
  # Make the index the commit's, then rewrite only the files whose content differs: rewriting a file that
  # is the same would change its mtime and make Cargo recompile the gateway crate (about 2 minutes).
  git reset -q --mixed FETCH_HEAD || return 1
  git update-index -q --refresh >/dev/null 2>&1 || true
  git diff-files --name-only -z | xargs -0 -r git checkout-index -f -- || return 1
  git clean -q -ffdx || return 1
  if [ "$(git rev-parse HEAD)" != "$OMG_SHA" ]; then
    echo "HEAD is $(git rev-parse HEAD), not $OMG_SHA"
    return 1
  fi
  if [ -n "$(git status --porcelain)" ] || ! git diff --quiet HEAD; then
    echo "the checkout differs from $OMG_SHA"
    return 1
  fi
  mark "source commit=$OMG_SHA"
}

prepare_toolchain() {
  local channel
  channel=$(sed -n 's/^channel *= *"\([^"]*\)".*/\1/p' rust-toolchain.toml)
  [ -n "$channel" ] || { echo "rust-toolchain.toml names no channel"; return 1; }
  rustup toolchain install "$channel" --profile minimal --component rustfmt --component clippy --no-self-update || return 1
  rustc --version && cargo --version
}

mib() {
  if [ "${2:-required}" = optional ] && [ ! -e "$1" ]; then
    # A shell existence test also fails on access errors. Only ENOENT means zero.
    python3 -c 'import os, sys
try:
    os.lstat(sys.argv[1])
except FileNotFoundError:
    print(0)
    sys.exit(0)
sys.exit("cache entry is not accessible: " + sys.argv[1])' "$1"
    return
  fi
  du -sk -- "$1" | awk 'NR == 1 && $1 ~ /^[0-9]+$/ {size=$1; valid=1} END {if (!valid || NR != 1) exit 1; printf "%d", size / 1024}'
}

prune_cache() { # bounded growth: the local-path volume does not enforce its size
  local c=$OMG_CACHE target_limit=$((OMG_TARGET_LIMIT_GIB * 1024)) total_limit=$((OMG_CACHE_LIMIT_GIB * 1024))
  local npm crates git_db target total src
  npm=$(mib "$c/npm" optional) || return 1
  crates=$(mib "$c/cargo/registry/cache" optional) || return 1
  git_db=$(mib "$c/cargo/git/db" optional) || return 1
  if [ "$npm" -gt 3072 ]; then
    rm -rf "$c/npm" && mkdir -p "$c/npm" || return 1
    echo "npm cache over 3 GiB: removed"
  fi
  if [ "$crates" -gt 4096 ]; then
    rm -rf "$c/cargo/registry/cache" || return 1
    echo "crate cache over 4 GiB: removed"
  fi
  if [ "$git_db" -gt 2048 ]; then
    rm -rf "$c/cargo/git/db" || return 1
    echo "git databases over 2 GiB: removed"
  fi
  target=$(mib "$c/target") || return 1
  if [ "$target" -gt "$target_limit" ]; then
    echo "target directory over $OMG_TARGET_LIMIT_GIB GiB: removing incremental data"
    find "$c/target" -maxdepth 2 -name incremental -type d -prune -exec rm -rf {} + || return 1
    target=$(mib "$c/target") || return 1
  fi
  total=$(mib "$c") || return 1
  if [ "$target" -gt "$target_limit" ] || [ "$total" -gt "$total_limit" ]; then
    echo "cache over its limit: removing the target directory"
    find "$c/target" -mindepth 1 -maxdepth 1 -exec rm -rf {} + || return 1
    target=$(mib "$c/target") || return 1
    total=$(mib "$c") || return 1
  fi
  crates=$(mib "$c/cargo/registry/cache" optional) || return 1
  npm=$(mib "$c/npm" optional) || return 1
  src=$(mib "$SRC") || return 1
  mark "cache total_mib=$total target_mib=$target crates_mib=$crates npm_mib=$npm src_mib=$src"
}

NPM_READY=
prepare_npm() {
  [ "$NPM_READY" = ok ] && return 0
  step "$1" npm-ci npm ci $OMG_NPM_CI_FLAGS || return 1
  NPM_READY=ok
}

wait_db() {
  local i
  for i in $(seq 1 "${OMG_DB_WAIT_TRIES:-120}"); do
    if (exec 3<>"/dev/tcp/$OMG_DB_HOST/$OMG_DB_PORT") 2>/dev/null; then
      mark "database host=$OMG_DB_HOST ready=1"
      return 0
    fi
    sleep "${OMG_DB_WAIT_SLEEP:-5}"
  done
  echo "the PostgreSQL service $OMG_DB_HOST did not accept connections within 10 minutes"
  return 1
}

db_marker() { # the server the tests ran against, next to CI's postgres:17-alpine (max_connections 100)
  local line
  line=$(cd "$SRC" && with_db node -e '
    const { Client } = require("pg");
    (async () => {
      const c = new Client({ connectionString: process.env.DATABASE_URL });
      await c.connect();
      const r = await c.query("select current_setting($$server_version$$) v, (select datcollate from pg_database where datname = $$gateway$$) c, current_setting($$max_connections$$) m");
      console.log(`version=${r.rows[0].v.split(" ")[0]} lc_collate=${r.rows[0].c} max_connections=${r.rows[0].m}`);
      await c.end();
    })().catch(() => process.exit(1));
  ' 2>/dev/null) || line="unavailable=1"
  mark "database $(printf '%s' "$line" | tr -cd 'A-Za-z0-9._= -')"
}

extra_args() {
  EXTRA=()
  [ -n "${OMG_TEST_ARGS:-}" ] && read -r -a EXTRA <<<"$OMG_TEST_ARGS"
  local a
  for a in ${EXTRA[@]+"${EXTRA[@]}"}; do
    case "$a" in
      --list | --help | -h | --version | -V | --format | --format=* | --show-output | --report-time | --ensure-time | --shuffle*) echo "refused argument $a: it does not run tests"; return 1 ;;
    esac
  done
}

suite_checks() {
  local bad=0
  step checks fmt cargo fmt --all -- --check || bad=1
  step checks clippy-workspace cargo clippy --workspace --all-targets --all-features -- -D warnings || bad=1
  step checks clippy-shipped cargo clippy -p open-model-gateway --bins --lib --locked -- -D warnings || bad=1
  return "$bad"
}

# The gateway library is one test binary of about 2,450 tests. In one process its memory grows to more
# than 10 GiB (measured on the Spark: OOM-killed at the 10 GiB limit after 1,789 tests), so, as CI does
# (.github/scripts/rust-shards.mjs: sorted exact names, element i to shard i % count), it runs as
# OMG_LIB_SHARDS processes one after the other. A test filter (OMG_TEST_ARGS) runs unsharded.
LIB=(cargo test -p open-model-gateway --all-features --lib)

shard_names() { # shard_names <index> <names file>: the names of one shard, numbered as CI numbers them
  awk -v k="$1" -v n="$OMG_LIB_SHARDS" '(NR - 1) % n == k' "$2"
}

lib_shard() { # lib_shard <index> <names file>: the tests of one shard, and a check that every one was accounted for
  local index=$1 names=$2 expected out code
  expected=$(shard_names "$index" "$names" | wc -l)
  out=$(mktemp "$OMG_TMP/lib-shard.XXXXXX")
  mark "lib-shard index=$index tests=$expected"
  # shellcheck disable=SC2046
  with_loopback_db "${LIB[@]}" -- --exact $(shard_names "$index" "$names") 2>&1 | tee "$out"
  code=${PIPESTATUS[0]}
  if ! awk -v want="$expected" '/^test result: / { for (i = 1; i <= NF; i++) { if ($i == "passed;" || $i == "failed;" || $i == "ignored;") n += $(i - 1) } found = 1 } END { exit !(found && n == want) }' "$out"; then
    echo "shard $index: the passed, failed and ignored counts are not the $expected tests selected"
    code=1
  fi
  rm -f "$out"
  return "$code"
}

suite_lib() {
  local bad=0 names index
  prepare_npm lib || return 1
  step lib build "${LIB[@]}" --no-run || return 1
  wait_db || return 1
  db_marker
  extra_args || return 1
  if [ -n "${OMG_TEST_ARGS:-}" ]; then
    step lib filtered with_loopback_db "${LIB[@]}" -- "${EXTRA[@]}"
    return
  fi
  names=$(mktemp "$OMG_TMP/lib-names.XXXXXX")
  if ! ( cd "$SRC" && "${LIB[@]}" -- --list --format terse 2>/dev/null ) | sed -n 's/: test$//p' | LC_ALL=C sort >"$names"; then
    rm -f "$names"
    echo "cannot list the gateway library tests"
    return 1
  fi
  mark "lib-inventory tests=$(wc -l <"$names")"
  [ -s "$names" ] || { echo "no lib tests listed"; return 1; }
  for ((index = 0; index < OMG_LIB_SHARDS; index++)); do
    step lib "shard-$index" lib_shard "$index" "$names" || bad=1
  done
  rm -f "$names"
  return "$bad"
}

exact_probe() { # a dedicated exact test must run once; libtest succeeds even if the name matches nothing
  local out code
  out=$(mktemp "$OMG_TMP/exact-probe.XXXXXX") || return 1
  "$@" 2>&1 | tee "$out"
  code=$?
  if ! awk '/^test result: / { results++; if ($0 ~ /^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; [0-9]+ filtered out(; finished in [0-9.]+s)?$/) ok++ } END { exit !(results == 1 && ok == 1) }' "$out"; then
    echo "expected exactly one passing probe"
    code=1
  fi
  rm -f "$out"
  return "$code"
}

workspace_targets() {
  DATETIME_PG_URL="$DATABASE_URL" node "$OMG_SCRIPTS/test-integration.mjs" workspace
}

suite_integration() {
  local bad=0
  prepare_npm integration || return 1
  wait_db || return 1
  db_marker
  # Every gateway test target, the gateway binary's own tests, then the targets CI gives to gateway-special
  # except filestore_s3 (it starts MinIO and RustFS containers: no Docker in a pod) and the backup drill.
  # The tools' step includes the datetime workspace crate's PostgreSQL tests and all
  # non-gateway doctests; the gateway's doctests have their own step.
  step integration targets with_loopback_db node "$OMG_SCRIPTS/test-integration.mjs" gateway || return 1
  step integration runtime-privileges with_loopback_db node "$OMG_SCRIPTS/test-integration.mjs" runtime || bad=1
  step integration runtime-privileges-ignored with_loopback_db node "$OMG_SCRIPTS/test-integration.mjs" runtime-ignored || bad=1
  step integration key-issuance-contention exact_probe with_loopback_db env RUST_TEST_NOCAPTURE=1 cargo test -p open-model-gateway --all-features --lib management::tests::authz_matrix_tests::key_issuance_contention_tests::key_issuance_contention_measurement -- --ignored --exact || bad=1
  step integration anthropic-terminal exact_probe with_loopback_db cargo test -p open-model-gateway --all-features --lib providers::anthropic::terminal_tests::terminal_fidelity_official_sdks_and_priced_http_evidence -- --ignored --exact || bad=1
  step integration tools with_loopback_db workspace_targets || bad=1
  step integration doctests with_loopback_db cargo test -p open-model-gateway --all-features --doc || bad=1
  return "$bad"
}

suite_web() {
  local bad=0
  prepare_npm web || return 1
  extra_args || return 1
  step web typecheck npm run typecheck:web || bad=1
  step web vitest npm run test --workspace @omg/web -- --maxWorkers="$OMG_WEB_WORKERS" ${EXTRA[@]+"${EXTRA[@]}"} || bad=1
  return "$bad"
}

main() {
  local started=$SECONDS overall=0 suites suite
  mark "run suite=$OMG_SUITE commit=$OMG_SHA"
  case "$OMG_SUITE" in
    all) suites="web checks lib integration" ;;
    lib | integration | web | checks) suites=$OMG_SUITE ;;
    *) echo "unknown suite $OMG_SUITE"; mark "setup-failed"; return 1 ;;
  esac
  setup_environment || { mark "setup-failed"; return 1; }
  step setup reset reset_cache || { mark "setup-failed"; return 1; }
  step setup source prepare_source || { mark "setup-failed"; return 1; }
  step setup toolchain prepare_toolchain || { mark "setup-failed"; return 1; }
  prune_cache || { echo "cache measurement or pruning failed"; mark "setup-failed"; return 1; }
  for suite in $suites; do
    mark "suite-start suite=$suite"
    "suite_$suite" || overall=1
    mark "suite-end suite=$suite"
  done
  prune_cache || { echo "cache measurement or pruning failed"; overall=1; }
  mark "done exit=$overall seconds=$((SECONDS - started))"
  return "$overall"
}

terminate() { # PID 1 ignores SIGTERM unless trapped: stop the children at once so `down` and the deadline do not wait
  trap '' TERM INT
  mark "terminated"
  kill -TERM -- 0 2>/dev/null
  sleep 1
  exit 143
}

if [ -z "${OMG_SOURCE_ONLY:-}" ]; then
  trap terminate TERM INT
  status_file=$(mktemp "$OMG_TMP/omg-status.XXXXXX")
  ( main 2>&1; echo "$?" >"$status_file" ) | redact &
  pipeline=$!
  wait "$pipeline"
  code=$(cat "$status_file" 2>/dev/null)
  exit "${code:-1}"
fi

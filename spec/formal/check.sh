#!/usr/bin/env bash
# Reproduces the TLC runs of spec/formal (issue #594, an experiment: not wired into CI).
#
#   spec/formal/check.sh [all|safety|wide|live|lease|diag|assumptions|mutations|witnesses]
#
# Needs docker and curl. The checker runs with NO network, a 3 GB memory limit and a
# read-only copy of the specs. Pinned inputs (change them only together with the
# README): the JRE image by digest and tla2tools.jar v1.7.4 by SHA-256 (cross-checked
# with the SHA-1 published on the release page). The jar is downloaded once into
# $TLA_WORK (default ~/.cache/omg-formal), outside the repository, and never committed.
set -euo pipefail

IMAGE='eclipse-temurin@sha256:18bb4a91a3dea3f03ee31d3755af34dfb40f904fc77ab56cdc2f092e32fff9c2'
JAR_URL='https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar'
JAR_SHA256='936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88'
JAR_SHA1='bee4a54f3ee3d4afc347c3240ec2d9e93b075104'

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work="${TLA_WORK:-${XDG_CACHE_HOME:-$HOME/.cache}/omg-formal}"
mkdir -p "$work/tools" "$work/state"
# TLC looks for the .cfg next to the .tla, so generated configs go in a staging copy.
rm -rf "$work/stage"; mkdir "$work/stage"
cp "$here"/*.tla "$here"/*.cfg "$work/stage/"

fail() { echo "check.sh: $*" >&2; exit 1; }
command -v docker >/dev/null || fail "docker is not installed"

fetch_jar() {
  local jar="$work/tools/tla2tools.jar"
  if [[ ! -f "$jar" ]]; then
    command -v curl >/dev/null || fail "curl is not installed"
    curl --proto '=https' --tlsv1.2 -fsSL -o "$jar.part" "$JAR_URL"
    mv "$jar.part" "$jar"
  fi
  [[ "$(sha256sum "$jar" | cut -d' ' -f1)" == "$JAR_SHA256" ]] || fail "tla2tools.jar SHA-256 mismatch; delete $jar"
  [[ "$(sha1sum "$jar" | cut -d' ' -f1)" == "$JAR_SHA1" ]] || fail "tla2tools.jar SHA-1 mismatch; delete $jar"
}

pull_image() {
  docker image inspect "$IMAGE" >/dev/null 2>&1 || docker pull "$IMAGE" >/dev/null
}

# run_tlc LABEL EXPECT MODULE CFG [tlc args...]
#   EXPECT: "pass" or "violate:<Name>" (the invariant or property TLC must name).
run_tlc() {
  local label="$1" expect="$2" module="$3" cfg="$4"; shift 4
  local log meta
  log="$work/state/last.log"
  meta="$work/state/meta-$$"
  rm -rf "$meta"
  local started=$SECONDS
  docker run --rm --network none -m 3g --user "$(id -u):$(id -g)" \
    -v "$work/stage:/spec:ro" -v "$work/tools:/tools:ro" -v "$work/state:/state" \
    "$IMAGE" java -XX:MaxRAMPercentage=70 -XX:+UseParallelGC -cp /tools/tla2tools.jar tlc2.TLC \
    -metadir "/state/meta-$$" -workers 3 "$@" -config "$cfg" "/spec/$module.tla" >"$log" 2>&1 || true
  rm -rf "$meta"
  local took=$((SECONDS - started))
  local states depth result steps
  states="$(grep -E 'states generated, .* distinct states found' "$log" | tail -1 | sed -E 's/^([0-9,]+) states generated, ([0-9,]+) distinct.*/\1 generated \2 distinct/' || true)"
  depth="$(grep -E 'depth of the complete state graph search is' "$log" | tail -1 | sed -E 's/.* is ([0-9]+).*/\1/' || true)"
  steps="$(grep -cE '^State [0-9]+:' "$log" || true)"
  if grep -q 'Model checking completed. No error has been found' "$log"; then
    result="pass"
  elif grep -qE 'Error: (Invariant|Action property) [A-Za-z_]+ is violated' "$log"; then
    result="violate:$(grep -E 'Error: (Invariant|Action property) [A-Za-z_]+ is violated' "$log" | head -1 | sed -E 's/.*(Invariant|property) ([A-Za-z_]+) is violated.*/\2/')"
  elif grep -q 'Temporal properties were violated' "$log"; then
    result="violate:Resolves"
  else
    echo "--- TLC log ($label) ---" >&2; tail -25 "$log" >&2
    fail "$label: TLC did not finish cleanly"
  fi
  local verdict="ok"
  [[ "$result" == "$expect" ]] || verdict="UNEXPECTED (wanted $expect)"
  printf '%-34s %-26s %-34s depth=%-3s trace=%-3s %4ss  %s\n' "$label" "$result" "${states:-?}" "${depth:-?}" "$steps" "$took" "$verdict"
  [[ "$verdict" == "ok" ]] || failed=1
  if [[ "$result" == violate:* && "$steps" -gt 0 ]]; then
    printf '    trace: %s\n' "$(grep -E '^State [0-9]+: <' "$log" | sed -E 's/^State [0-9]+: <(Initial predicate|[A-Za-z_]+).*/\1/' | sed 's/Initial predicate/Init/' | paste -sd'>' | sed 's/>/ > /g')"
  fi
}

# with_cfg BASE.cfg 'sed script' NAME -> config path inside the container
with_cfg() {
  local base="$1" script="$2" name="$3"
  sed -E "$script" "$here/$base" >"$work/stage/$name.cfg"
  echo "/spec/$name.cfg"
}

failed=0
section() { printf '\n== %s\n' "$1"; }

do_safety() {
  section "MoneyPath: the code as read (2 attempts, 2 replicas, 1 crash)"
  run_tlc "MoneyPath" pass MoneyPath /spec/MoneyPath.cfg
}
do_wide() {
  section "MoneyPath wide: 3 attempts, 2 crashes"
  run_tlc "MoneyPathWide" pass MoneyPath /spec/MoneyPathWide.cfg
}
do_live() {
  section "MoneyPath liveness (weak fairness on the clock, reconciler, restart)"
  run_tlc "MoneyPathLive" pass MoneyPath /spec/MoneyPathLive.cfg
  run_tlc "Live mutation: reconciler_off" violate:Resolves MoneyPath \
    "$(with_cfg MoneyPathLive.cfg 's/Mut = "none"/Mut = "reconciler_off"/' mut-live)"
}
do_lease() {
  section "LeaseFence: two replicas, one lease, a fencing token"
  run_tlc "LeaseFence InTx (as built)" pass LeaseFence /spec/LeaseFence.cfg
  run_tlc "LeaseFence InTx + TrustMargin" pass LeaseFence \
    "$(with_cfg LeaseFence.cfg 's/TrustMargin = FALSE/TrustMargin = TRUE/' lf1)"
  run_tlc "Mutation: no FOR SHARE" violate:NoStaleCommit LeaseFence \
    "$(with_cfg LeaseFence.cfg 's/Mode = "InTx"/Mode = "InTxNoShare"/;s/TrustMargin = FALSE/TrustMargin = TRUE/' lf2)"
  run_tlc "Mutation: check outside the tx" violate:NoStaleCommit LeaseFence \
    "$(with_cfg LeaseFence.cfg 's/Mode = "InTx"/Mode = "Local"/;s/TrustMargin = FALSE/TrustMargin = TRUE/' lf3)"
  run_tlc "Batch runner lease, as coded" violate:NoStaleCommit LeaseFence \
    "$(with_cfg LeaseFence.cfg 's/Mode = "InTx"/Mode = "RunnerFlag"/' lf4)"
}
do_diag() {
  section "Diagnostic (expected to fail): complete usage received but the reservation ends unknown"
  run_tlc "MoneyPathDiag" violate:KnownUsageSettled MoneyPath /spec/MoneyPathDiag.cfg
}
mutation() { # NAME EXPECTED_PROPERTY
  run_tlc "Mutation: $1" "violate:$2" MoneyPath "$(with_cfg MoneyPath.cfg "s/Mut = \"none\"/Mut = \"$1\"/" "mut-$1")"
}
do_mutations() {
  section "MoneyPath mutations: each deliberate bug must produce a counterexample"
  mutation reuse_reservation OwnReservation
  mutation release_on_cancel UnknownRetainsHold
  mutation reconcile_releases_hold UnknownRetainsHold
  mutation reconcile_no_state_guard SettledTerminal
  mutation settle_no_state_check SettledTerminal
  mutation unknown_as_zero SettledMatchesTruth
  mutation failover_after_stream NoFailoverAfterStream
  # The same two bugs seen only by the action property (state invariants off).
  local m
  for m in release_on_cancel reconcile_releases_hold; do
    run_tlc "Mutation (action property): $m" violate:HoldNeverReleased MoneyPath \
      "$(with_cfg MoneyPath.cfg "s/Mut = \"none\"/Mut = \"$m\"/;s/^INVARIANTS.*/INVARIANTS TypeOK/" "prop-$m")"
  done
}
do_assumptions() {
  section "Assumption tests: relaxing a timing/evidence assumption"
  run_tlc "early_expiry (A1 off)" pass MoneyPath \
    "$(with_cfg MoneyPath.cfg 's/Mut = "none"/Mut = "early_expiry"/' as-early)"
  run_tlc "evidence_early (A3 off)" violate:SettledMatchesTruth MoneyPath \
    "$(with_cfg MoneyPath.cfg 's/Mut = "none"/Mut = "evidence_early"/' as-evid)"
}
do_witnesses() {
  section "Reachability witnesses: each scenario must be reachable (violation expected)"
  local w
  for w in W_Failover W_StreamSettled W_BilledUnknown W_Evidence W_CancelUnknown W_ReconcilerWon W_TwoHolds W_RejectedFree W_CrashPending; do
    run_tlc "Witness $w" "violate:$w" MoneyPath \
      "$(with_cfg MoneyPath.cfg "s/^INVARIANTS.*/INVARIANTS $w/;/^PROPERTIES/d" "wit-$w")"
  done
}

target="${1:-all}"
fetch_jar
pull_image
printf '%-34s %-26s %-34s %-9s %-9s %5s\n' "run" "result" "states" "depth" "trace" "time"
case "$target" in
  all) do_safety; do_wide; do_live; do_lease; do_diag; do_mutations; do_assumptions; do_witnesses ;;
  safety) do_safety ;;
  wide) do_wide ;;
  live) do_live ;;
  lease) do_lease ;;
  diag) do_diag ;;
  mutations) do_mutations ;;
  assumptions) do_assumptions ;;
  witnesses) do_witnesses ;;
  *) fail "unknown target '$target'" ;;
esac
if [[ "$failed" != 0 ]]; then fail "a run did not give the expected result"; fi
echo; echo "all runs gave the expected result"

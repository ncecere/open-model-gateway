#!/usr/bin/env bash
# Prepare a controlled ORAS rebuild, not a trusted distribution. Never execute
# the result here: a future consumer must first verify an independently
# reproduced, static binary checksum. Transport/source review is still required.
set -euo pipefail
umask 077

fail() {
  echo "build-oras: $*" >&2
  exit 1
}

[ "$#" -eq 1 ] && [ -n "$1" ] || fail "usage: build-oras.sh OUTPUT_FILE"
[ ! -e "$1" ] && [ ! -L "$1" ] || fail "output already exists: $1"
output_dir=$(cd -- "$(dirname -- "$1")" && pwd -P)
output="$output_dir/$(basename -- "$1")"
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)

work=$(mktemp -d "${TMPDIR:-/tmp}/build-oras.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
trap 'exit 1' HUP INT TERM
# Absolute paths are required for the SDK and every private Go cache. Preserve
# the logical TMPDIR prefix (for example, /var rather than /private/var on macOS).
work_absolute=$(cd -- "$work" && pwd -L)
work=$work_absolute
mkdir -p "$work/home" "$work/cache" "$work/mod" "$work/gopath" "$work/sdk" "$work/tmp"

# Retain PATH for native tools (and dependency mocks), but no caller credentials,
# Go configuration, proxy settings, Git config, helpers, or URL rewrites.
clean_env() {
  env -i PATH="$PATH" HOME="$work/home" TMPDIR="$work/tmp" LANG=C \
    GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null \
    GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 "$@"
}

clean_env bash "$script_dir/fetch-verified.sh" \
  'https://go.dev/dl/go1.26.10.linux-amd64.tar.gz' \
  'ccfe7ae822d2c2f41f4fe7c956199967a4dac22d0ee0d2b2bfdb440aab3b20c5' \
  "$work/sdk.tar.gz"
# The compiler and its supporting SDK files come only from this verified archive.
clean_env tar -xzf "$work/sdk.tar.gz" -C "$work/sdk"

commit=9b1fc815f6b7fba743f08b9b5c5846639cc11d3c
clean_env git -c core.hooksPath=/dev/null clone --no-checkout \
  'https://github.com/oras-project/oras.git' "$work/source"
clean_env git -C "$work/source" -c core.hooksPath=/dev/null checkout --detach "$commit"
actual=$(clean_env git -C "$work/source" rev-parse 'HEAD^{commit}')
[ "$actual" = "$commit" ] || fail "checked-out source does not match the pinned commit"
clean_env git -C "$work/source" diff --exit-code HEAD -- go.mod go.sum

ldflags="-w -s -buildid= -X oras.land/oras/internal/version.Version=1.3.6 -X oras.land/oras/internal/version.GitCommit=$commit -X oras.land/oras/internal/version.BuildMetadata= -X oras.land/oras/internal/version.GitTreeState=clean"
(
  cd -- "$work/source"
  clean_env env CGO_ENABLED=0 GOOS=linux GOARCH=amd64 GOAMD64=v1 \
    GOTOOLCHAIN=local GOENV=off GOWORK=off GOFLAGS= GOEXPERIMENT= \
    GOPROXY=https://proxy.golang.org GOSUMDB=sum.golang.org \
    GOCACHE="$work/cache" GOMODCACHE="$work/mod" GOPATH="$work/gopath" \
    "$work/sdk/go/bin/go" build -mod=readonly -trimpath -buildvcs=false \
    "-ldflags=$ldflags" -o "$work/oras" ./cmd/oras
)
# Compare both index and worktree to the immutable commit, byte-for-byte; do not
# tidy, generate, replace dependencies, or accept rewritten manifests.
clean_env git -C "$work/source" diff --exit-code HEAD -- go.mod go.sum
[ -f "$work/oras" ] && [ ! -L "$work/oras" ] || fail "compiler produced no regular binary"

# Exclusive creation also refuses a target appearing during the build (including
# dangling symlinks and directories). Unlike mv -n, a collision is an error.
# Compilation is already complete; remove our own output if the handoff fails.
(
  set -o noclobber
  exec 3> "$output"
  trap 'status=$?; if [ "$status" -ne 0 ]; then rm -f -- "$output"; fi' EXIT
  trap 'exit 1' HUP INT TERM
  cat "$work/oras" >&3
  exec 3>&-
  chmod 700 "$output"
)

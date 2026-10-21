#!/usr/bin/env bash
# The one way a workflow downloads a file at run time (#194):
#
#   bash .github/scripts/fetch-verified.sh <https-url> <sha256> <dest>
#
# The URL and the lowercase hex SHA-256 are literals written in the workflow, so a
# change to either shows up in review (tests/supply-chain.test.mjs rejects any other
# curl, wget, gh release download or inline HTTP client in a run step). The file is
# downloaded to a temporary file next to <dest>, its SHA-256 is checked, and only then
# is it moved into place. Anything else (a bad argument, a failed download, a
# mismatch, an existing <dest>) exits non-zero and leaves nothing at <dest>.
#
# https only, including redirects: GitHub release downloads redirect to another
# https host, and a redirect to http or any other scheme is refused. There is no
# test or insecure mode: the script's tests stand in a fake curl on PATH.
set -euo pipefail

fail() {
  echo "fetch-verified: $*" >&2
  exit 1
}

[ "$#" -eq 3 ] || fail "usage: fetch-verified.sh <https-url> <sha256> <dest>"
url=$1
expected=$2
dest=$3

[[ "$url" =~ ^https://[A-Za-z0-9._~:/?#@\&+,%=-]+$ ]] || fail "the URL must be a plain https:// URL: '${url}'"
[[ "$expected" =~ ^[0-9a-f]{64}$ ]] || fail "the checksum must be 64 lowercase hex digits"
[ -n "$dest" ] || fail "the destination is empty"
[ ! -e "$dest" ] || fail "${dest} already exists"
dir=$(dirname "$dest")
[ -d "$dir" ] || fail "${dir} is not a directory"

if command -v sha256sum >/dev/null 2>&1; then
  digest() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  digest() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  fail "neither sha256sum nor shasum is installed"
fi

tmp=$(mktemp "${dest}.XXXXXX")
trap 'rm -f "$tmp"' EXIT

curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --silent --show-error \
  --location --max-redirs 5 --retry 3 --output "$tmp" "$url" || fail "could not download ${url}"

actual=$(digest "$tmp")
[ "$actual" = "$expected" ] || fail "checksum mismatch for ${url}: expected ${expected}, got ${actual}"

mv "$tmp" "$dest"
trap - EXIT
echo "fetch-verified: ${url} -> ${dest} (sha256 ${actual})"

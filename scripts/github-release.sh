#!/usr/bin/env bash
# Creates (or updates) the GitHub Release for $TAG with notes from
# docs/releases/<base version>.md, the image digest and verification steps,
# and attaches the release assets:
#   open-model-gateway-<tag>-<os>-<arch>.sbom.spdx.json  the image's SPDX SBOM per
#                                                        platform (BuildKit attestations)
#   open-model-gateway-<tag>.digest.txt                  the image reference by digest
#   checksums.txt                                        SHA-256 of the files above
# Run by the `release` job in .github/workflows/image.yml (needs GH_TOKEN, TAG,
# DIGEST and read access to the image). Tags with a hyphen (v0.3.0-rc.1)
# become pre-releases and use the base version's notes. A missing notes file,
# SBOM or upload stops the script with a message saying what went wrong.
# RELEASE_DRY_RUN=1 writes the assets and notes to RELEASE_OUT (default: a
# temporary directory) and skips every GitHub call.
set -euo pipefail

fail() {
  echo "::error::github-release: $*" >&2
  exit 1
}

: "${TAG:?TAG is not set}" "${DIGEST:?DIGEST is not set}"
[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || fail "TAG is not a vX.Y.Z tag: '${TAG}'"
[[ "$DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]] || fail "DIGEST is not a sha256 digest: '${DIGEST}'"
dry_run="${RELEASE_DRY_RUN:-0}"
tools=(docker jq sha256sum)
[ "$dry_run" = 1 ] || tools+=(gh)
for cmd in "${tools[@]}"; do
  command -v "$cmd" >/dev/null || fail "${cmd} is not installed"
done

repo_slug=ncecere/open-model-gateway
image="ghcr.io/${repo_slug}"
ref="${image}@${DIGEST}"
base="${TAG%%-*}" # v0.3.0-rc.1 -> v0.3.0
notes_src="docs/releases/${base}.md"
[ -f "$notes_src" ] || fail "no release notes: ${notes_src}"
if [ "$dry_run" = 1 ]; then
  work="${RELEASE_OUT:-$(mktemp -d)}"
  mkdir -p "$work"
else
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
fi
assets="${work}/assets"
mkdir -p "$assets"

# ---- assets ------------------------------------------------------------------

# For a multi-platform index, .SBOM maps each platform ("linux/amd64") to its
# attestations; a single-platform image has .SPDX at the top.
sboms="${work}/sboms.json"
docker buildx imagetools inspect "$ref" --format '{{ json .SBOM }}' >"$sboms" ||
  fail "could not read the SBOM attestations of ${ref}"
if jq -e 'type == "object" and has("SPDX")' "$sboms" >/dev/null; then
  jq '{"image": .}' "$sboms" >"${sboms}.tmp" && mv "${sboms}.tmp" "$sboms"
fi
platforms="$(jq -r 'if type == "object" then keys[] else empty end' "$sboms")"
[ -n "$platforms" ] || fail "${ref} has no SBOM attestation (was it built with an SBOM attestation?)"
files=()
while IFS= read -r platform; do
  name="open-model-gateway-${TAG}-${platform//\//-}.sbom.spdx.json" # linux/amd64 -> linux-amd64
  [ "$platform" = image ] && name="open-model-gateway-${TAG}.sbom.spdx.json"
  jq --arg p "$platform" '.[$p].SPDX' "$sboms" >"${assets}/${name}"
  jq -e '.spdxVersion | type == "string" and startswith("SPDX-")' "${assets}/${name}" >/dev/null ||
    fail "the SBOM for ${platform} is not an SPDX document"
  files+=("$name")
done <<<"$platforms"

echo "$ref" >"${assets}/open-model-gateway-${TAG}.digest.txt"
files+=("open-model-gateway-${TAG}.digest.txt")
(cd "$assets" && sha256sum -- "${files[@]}" >checksums.txt)
files+=(checksums.txt)

# ---- notes -------------------------------------------------------------------

notes="${work}/notes.md"
{
  if [ "$TAG" != "$base" ]; then
    echo "> **Release candidate** for ${base}. Test it before relying on it; the final release follows."
    echo
  fi
  echo "## Image"
  echo
  echo '```'
  echo "${image}:${TAG}"
  echo "$ref"
  echo '```'
  echo
  echo "Deploy by digest. Verify the signature with cosign v3 or newer (keyless, GitHub Actions OIDC):"
  echo
  echo '```sh'
  echo "cosign verify ${ref} \\"
  echo "  --certificate-identity https://github.com/${repo_slug}/.github/workflows/image.yml@refs/tags/${TAG} \\"
  echo "  --certificate-oidc-issuer https://token.actions.githubusercontent.com"
  echo '```'
  echo
  echo "## Release assets"
  echo
  echo "- \`open-model-gateway-${TAG}-<os>-<arch>.sbom.spdx.json\`: the image's SBOM (SPDX JSON) for each platform, as attached to the image."
  echo "- \`open-model-gateway-${TAG}.digest.txt\`: the image reference by digest."
  echo "- \`checksums.txt\`: SHA-256 of the files above (\`sha256sum -c checksums.txt\`)."
  echo
  echo "The SBOM and build provenance are also attached to the image as attestations:"
  echo "\`docker buildx imagetools inspect ${ref} --format '{{ json .SBOM }}'\`."
  echo
  # Relative links in docs/releases/ would break on the release page: point
  # them at the files as of this tag.
  blob="https://github.com/${repo_slug}/blob/${TAG}"
  sed -E \
    -e "s#\]\(\.\./\.\./#](${blob}/#g" \
    -e "s#\]\(\.\./#](${blob}/docs/#g" \
    -e "s#\]\((v[0-9]+\.[0-9]+\.[0-9]+\.md)#](${blob}/docs/releases/\1#g" \
    "$notes_src"
} >"$notes"

if [ "$dry_run" = 1 ]; then
  echo "dry run: notes ${notes}, assets in ${assets}"
  ls -l "$assets"
  exit 0
fi

# ---- release -----------------------------------------------------------------

# Name the repository: the asset upload runs from the assets directory, where
# gh can't infer it from a git checkout.
repo="${GITHUB_REPOSITORY:-$repo_slug}"
title="Open Model Gateway ${TAG}"
if gh release view "$TAG" --repo "$repo" >/dev/null 2>&1; then
  gh release edit "$TAG" --repo "$repo" --title "$title" --notes-file "$notes" || fail "could not update the release ${TAG}"
else
  flags=(--repo "$repo" --title "$title" --notes-file "$notes" --verify-tag)
  [ "$TAG" != "$base" ] && flags+=(--prerelease)
  gh release create "$TAG" "${flags[@]}" || fail "could not create the release ${TAG}"
fi
(cd "$assets" && gh release upload "$TAG" --repo "$repo" --clobber "${files[@]}") || fail "could not upload the release assets"
echo "release ${TAG}: ${ref}"
ls -l "$assets"

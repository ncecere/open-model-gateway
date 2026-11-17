# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.18.0](v0.18.0.md). They become the next release's notes.

## Distribution

The gateway repository is now [bitop-dev/open-model-gateway](https://github.com/bitop-dev/open-model-gateway) (#623). New gateway and loadtest builds publish to `ghcr.io/bitop-dev/open-model-gateway` and `ghcr.io/bitop-dev/open-model-gateway-loadtest`. The next release will be the first published under the organization. This change does not add a migration, runtime grant or API change.

## Upgrade

For the next release, change the image repository to **`ghcr.io/bitop-dev/open-model-gateway`** and pin the verified release digest. For Helm, set `image.repository` and `image.digest` together (a digest takes precedence over `image.tag`). Do the same for explicit migration jobs and any separate gateway workloads; changing only the version tag on an old repository override will not pull the new release. Verify the signature with the exact `bitop-dev/open-model-gateway/.github/workflows/image.yml@refs/tags/<tag>` certificate identity and GitHub Actions OIDC issuer shown in the generated GitHub Release. See [image paths and signature identities](../staging.md#image-paths-and-signature-identities).

Published releases through **v0.18.0** stay pullable at `ghcr.io/ncecere/open-model-gateway`, with the `ncecere/open-model-gateway` signature identity. Their tags, digests and release notes are unchanged; they were not copied or re-signed. Keep the original repository plus digest for rollback, along with the database backup required by that release. Spark automatically selects the original repository for these release tags and the organization repository for newer releases and development images; it never retries a failed lookup against the other account.

Release preparation must update the chart's default repository to `ghcr.io/bitop-dev/open-model-gateway` **together with** its next `tag`, `appVersion` and chart version. Until then, the chart defaults still describe v0.18.0 and use its original repository, so they remain pullable. The release-metadata test enforces this boundary. Confirm both native architecture builds, package visibility/Actions access under organization policy, the exact release signature identity, and a Spark candidate journey run from the new path before tagging. Historical release notes and changelog entries remain as written; repository compare links now use `bitop-dev`.

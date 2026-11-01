# No `# syntax=` line: BuildKit's built-in Dockerfile frontend, so builds do
# not pull docker/dockerfile from Docker Hub. The node and rust build stages
# come from mirror.gcr.io, Google's Docker Hub mirror (same image digests),
# because Docker Hub's anonymous pull limit and outages failed CI builds.
FROM mirror.gcr.io/library/node:22.23.4-bookworm-slim@sha256:4b150b48b4116e2a22f2c39cc27025fbf4f153e2078fd3b14d159fdb5c7279da AS web
WORKDIR /build
COPY package.json package-lock.json ./
COPY apps/web/package.json apps/web/package.json
RUN npm ci
COPY apps/web/ apps/web/
RUN npm run build:web

# All locked dependency manifests currently require Rust <= 1.88. This pinned
# compiler also supports edition 2024; do not use the manifest MSRV blindly when
# updating Cargo.lock. Bookworm sets the build's glibc baseline; compatibility
# with the supported Debian 13 runtime must pass native build/runtime gates.
FROM mirror.gcr.io/library/rust:1.99.1-bookworm@sha256:9181b56c2a21ab1d5e73cffe04fc762426d30aa7fac3c91141dd033f49a7fc8b AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY apps/gateway/ apps/gateway/
# Embedded by `open-model-gateway grants apply|verify`.
COPY deploy/staging/runtime-grants.sql deploy/staging/verify-privileges.sql deploy/staging/
# Workspace members (load-test tools) must exist for Cargo to load the
# workspace; only the gateway package is built into this image.
COPY tools/ tools/
# SQLx migrations are compiled into the binary; no live database is needed.
ARG CARGO_BUILD_JOBS=2
RUN cargo build --locked --release --jobs "$CARGO_BUILD_JOBS" -p open-model-gateway

# Distroless: no shell, package manager, curl or perl. The binary links glibc
# and libgcc_s (Rust unwinding), so `cc` (not `base`/`static`); it ships
# ca-certificates, which the AWS SDK's rustls-native-certs reads. Pinned by
# multi-arch index digest (cc-debian13:nonroot); refresh it deliberately with
#   docker buildx imagetools inspect gcr.io/distroless/cc-debian13:nonroot
# The binary itself does what the former shell entrypoint did: umask 077,
# `*_FILE` secret import, and never migrating or bootstrapping implicitly.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:3ee00291cf386ec52a552e6a858d555debd74776eb4bfa53fa3df2cd6677515e AS runtime
WORKDIR /app
COPY --from=build --chmod=0555 /build/target/release/open-model-gateway /usr/local/bin/open-model-gateway
COPY --from=web /build/apps/web/dist/ /app/web/
COPY --from=web /build/apps/web/src/components/ui/LICENSE /usr/share/licenses/open-model-gateway/bitop-ui-LICENSE
# HOME matches the former passwd entry; nothing is written there.
ENV GATEWAY_ENV=production \
    GATEWAY_LISTEN=0.0.0.0:8080 \
    GATEWAY_WEB_DIR=/app/web \
    HOME=/nonexistent
# UID/GID 10001 (not distroless's 65532): staging, Compose and Kubernetes
# manifests and file ownership already assume it.
USER 10001:10001
EXPOSE 8080
STOPSIGNAL SIGTERM
# Exec form; the probe follows GATEWAY_LISTEN's port over loopback, ignores
# proxies and redirects, and exits 0 only for a 2xx from /health/ready.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/open-model-gateway", "healthcheck", "--timeout", "3s"]
ENTRYPOINT ["/usr/local/bin/open-model-gateway"]
CMD ["serve"]

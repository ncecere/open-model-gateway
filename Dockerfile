# syntax=docker/dockerfile:1

FROM node:22.22.0-bookworm-slim AS web
WORKDIR /build
COPY package.json package-lock.json ./
COPY apps/web/package.json apps/web/package.json
RUN npm ci
COPY apps/web/ apps/web/
RUN npm run build:web

# All locked dependency manifests currently require Rust <= 1.88. This pinned
# compiler also supports edition 2024; do not use the manifest MSRV blindly when
# updating Cargo.lock. Build and runtime share Debian's glibc baseline.
FROM rust:1.90.0-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY apps/gateway/ apps/gateway/
# SQLx migrations are compiled into the binary; no live database is needed.
ARG CARGO_BUILD_JOBS=2
RUN cargo build --locked --release --jobs "$CARGO_BUILD_JOBS" -p open-model-gateway

# Distroless: no shell, package manager, curl or perl. The binary links glibc
# and libgcc_s (Rust unwinding), so `cc` (not `base`/`static`); it ships
# ca-certificates, which the AWS SDK's rustls-native-certs reads. Pinned by
# multi-arch index digest (cc-debian12:nonroot); refresh it deliberately with
#   docker buildx imagetools inspect gcr.io/distroless/cc-debian12:nonroot
# The binary itself does what the former shell entrypoint did: umask 077,
# `*_FILE` secret import, and never migrating or bootstrapping implicitly.
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f AS runtime
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

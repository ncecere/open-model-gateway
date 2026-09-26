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

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 gateway \
    && useradd --uid 10001 --gid 10001 --no-create-home \
        --home-dir /nonexistent --shell /usr/sbin/nologin gateway
WORKDIR /app
COPY --from=build --chmod=0555 /build/target/release/open-model-gateway /usr/local/bin/open-model-gateway
COPY --from=web /build/apps/web/dist/ /app/web/
COPY --from=web /build/apps/web/src/components/ui/LICENSE /usr/share/licenses/open-model-gateway/bitop-ui-LICENSE
COPY --chmod=0555 deploy/container-entrypoint.sh /usr/local/bin/container-entrypoint
ENV GATEWAY_ENV=production \
    GATEWAY_LISTEN=0.0.0.0:8080 \
    GATEWAY_WEB_DIR=/app/web
USER 10001:10001
EXPOSE 8080
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["curl", "--fail", "--silent", "--show-error", "--noproxy", "*", "--max-time", "3", "http://127.0.0.1:8080/health/ready"]
ENTRYPOINT ["/usr/local/bin/container-entrypoint"]
CMD ["serve"]

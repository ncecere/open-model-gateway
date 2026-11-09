# Load-test image (docs/operations.md "Capacity baseline"): the gateway plus
# the test-only mock upstream and load generator, API only (no SPA). Never a
# production image. Build from the repository root:
#   docker build -f deploy/loadtest/Dockerfile -t omg-loadtest:local .
# No Docker Hub pulls (docs/verification.md): Google's mirror and distroless.
FROM mirror.gcr.io/library/rust:1.99.1-bookworm@sha256:9181b56c2a21ab1d5e73cffe04fc762426d30aa7fac3c91141dd033f49a7fc8b AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY apps/gateway/ apps/gateway/
# Embedded by `open-model-gateway grants apply|verify`.
COPY deploy/staging/runtime-grants.sql deploy/staging/verify-privileges.sql deploy/staging/
COPY tools/ tools/
ARG CARGO_BUILD_JOBS=8
# Same release profile as the production image (thin LTO, one codegen unit).
RUN --mount=type=cache,id=omg-loadtest-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=omg-loadtest-target,target=/build/target \
    cargo build --locked --release --jobs "$CARGO_BUILD_JOBS" \
      -p open-model-gateway -p omg-mock-upstream -p omg-loadgen \
 && mkdir -p /out \
 && cp target/release/open-model-gateway target/release/mock-upstream target/release/loadgen /out/

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:cc66d222f0a7ec996b78a99fe76a969bc80b539a5016ad866c14d1949bc6d0ab
COPY --from=build --chmod=0555 /out/ /usr/local/bin/
ENV GATEWAY_ENV=production \
    GATEWAY_LISTEN=0.0.0.0:8080 \
    HOME=/nonexistent
USER 10001:10001
EXPOSE 8080 9464 8000
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/open-model-gateway"]
CMD ["serve"]

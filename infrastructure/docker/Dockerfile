# Multi-stage build for the Dice backend bins (M4). One image carries all four
# bins — the monolith/gateway and the three split-mode service responders — so a
# single build feeds every Deployment/StatefulSet (the entrypoint picks the bin).
#
# Offline + ring-only: SQLX_OFFLINE uses the committed .sqlx cache (no DB at build
# time); the workspace pins the ring rustls provider, so there's no aws-lc-sys /
# NASM/CMake toolchain to install. protoc is needed for prost-build.

# ---- builder ----------------------------------------------------------------
FROM rust:1.83-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
# Copy the whole server workspace (the desktop client is a separate workspace and
# is NOT part of this image).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY services ./services
COPY benchmarks ./benchmarks
COPY proto ./proto
COPY .sqlx ./.sqlx

ENV SQLX_OFFLINE=true
RUN cargo build --release \
    -p dice-monolith -p auth-service -p chat-service -p presence-service \
    && strip target/release/dice-monolith target/release/auth-service \
       target/release/chat-service target/release/presence-service

# ---- runtime ----------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home dice

COPY --from=builder /build/target/release/dice-monolith   /usr/local/bin/dice-monolith
COPY --from=builder /build/target/release/auth-service    /usr/local/bin/auth-service
COPY --from=builder /build/target/release/chat-service    /usr/local/bin/chat-service
COPY --from=builder /build/target/release/presence-service /usr/local/bin/presence-service

USER dice
# REST/WSS (TLS), QUIC (UDP), admin /metrics.
EXPOSE 8443/tcp 8444/udp 9600/tcp
# Default to the gateway/monolith; the k8s manifests override `command` per bin.
ENTRYPOINT ["/usr/local/bin/dice-monolith"]

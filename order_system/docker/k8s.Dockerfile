# Native Linux image for the standalone demo order system.
#
# Build from the repository root so the path dependency on match_engine is
# available:
#   docker build --platform linux/arm64 \
#     -f order_system/docker/k8s.Dockerfile \
#     -t matchengine-order-system:arm64 .

# syntax=docker/dockerfile:1

FROM rust:1.93-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libssl-dev \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY match_engine ./match_engine
COPY order_system ./order_system

RUN cargo build --release --locked \
    --manifest-path order_system/Cargo.toml

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 orders \
    && useradd --uid 10001 --gid 10001 --no-create-home \
        --shell /usr/sbin/nologin orders

COPY --from=builder /src/order_system/target/release/matchengine-raft-demo /usr/local/bin/matchengine-order-system

ENV HOME=/tmp
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/matchengine-order-system"]

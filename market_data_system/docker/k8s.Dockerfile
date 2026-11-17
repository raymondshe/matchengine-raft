# Native Linux image for the standalone market-data service.
#
# Build from the repository root so both path dependencies are available:
#   docker build --platform linux/arm64 \
#     -f market_data_system/docker/k8s.Dockerfile \
#     -t matchengine-marketdata:aeron-arm64 .
#
# The rusteron 0.2.13 prebuilt Linux libraries are x86_64-only. This image
# builds Aeron from source with CMake >= 3.30, which works on arm64 without
# Rosetta.

# syntax=docker/dockerfile:1

FROM rust:1.93-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libssl-dev \
        ca-certificates \
        clang \
        libclang-dev \
        uuid-dev \
        libbsd-dev \
        python3 \
        python3-pip \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m pip install --break-system-packages --no-cache-dir cmake==3.31.6

RUN rustup component add rustfmt

WORKDIR /src
COPY match_engine ./match_engine
COPY market_data_system ./market_data_system

RUN cargo build --release \
    --manifest-path market_data_system/Cargo.toml \
    --features aeron

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        libssl3 \
        libbsd0 \
        libatomic1 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 marketdata \
    && useradd --uid 10001 --gid 10001 --no-create-home \
        --shell /usr/sbin/nologin marketdata

COPY --from=builder /src/market_data_system/target/release/matchengine-marketdata /usr/local/bin/matchengine-marketdata

ENV HOME=/tmp
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/matchengine-marketdata"]

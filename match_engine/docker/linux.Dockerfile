# Linux image used by the OrbStack four-container Raft lab.
#
# The builder compiles the engine for the container architecture; the runtime
# image keeps only the binary plus the tools needed for health checks and
# iptables-based partition tests.

# syntax=docker/dockerfile:1

FROM rust:1.93-bookworm AS toolchain

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libssl-dev \
        ca-certificates \
        clang \
        python3 \
        python3-pip \
        libclang-dev \
        uuid-dev \
        libbsd-dev \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m pip install --break-system-packages --no-cache-dir cmake==3.31.6

RUN rustup component add rustfmt

WORKDIR /src

FROM toolchain AS builder

COPY . .

RUN cargo build --manifest-path Cargo.toml --release --bin matchengine-raft

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        iproute2 \
        iptables \
        libssl3 \
        libatomic1 \
        procps \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/matchengine-raft /usr/local/bin/matchengine-raft

ENTRYPOINT ["/usr/local/bin/matchengine-raft"]

# Non-root engine image for the OrbStack Kubernetes test tier.
#
# The runtime user matches the StatefulSet security context in
# docs/KUBERNETES_CONTROL_PLANE.md (UID/GID 10001). The engine writes only to
# /data (PVC) and /tmp (emptyDir); the root filesystem stays read-only.

# syntax=docker/dockerfile:1

FROM rust:1.93-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libssl-dev \
        ca-certificates \
        cmake \
        clang \
        libclang-dev \
        uuid-dev \
        libbsd-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

RUN cargo build --release --bin matchengine-raft --bin match_supervisor

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 matchengine \
    && useradd --uid 10001 --gid 10001 --no-create-home \
        --shell /usr/sbin/nologin matchengine

COPY --from=builder /src/target/release/matchengine-raft /usr/local/bin/matchengine-raft
COPY --from=builder /src/target/release/match_supervisor /usr/local/bin/match_supervisor

ENV HOME=/tmp
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/matchengine-raft"]

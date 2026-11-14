# Non-root manager image for the OrbStack Kubernetes test tier.
#
# The manager serves the API and embedded GUI on 8099. It keeps its JSON
# catalog in /data; mount an emptyDir or PVC there.

# syntax=docker/dockerfile:1

FROM rust:1.93-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static

RUN cargo build --release --bin k8s-control-plane

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 controlplane \
    && useradd --uid 10001 --gid 10001 --no-create-home \
        --shell /usr/sbin/nologin controlplane

COPY --from=builder /src/target/release/k8s-control-plane /usr/local/bin/k8s-control-plane

ENV HOME=/tmp
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/k8s-control-plane"]

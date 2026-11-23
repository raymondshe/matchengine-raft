# Linux/arm64 image for the NATS + TDengine-history market-data service.
#
# This is the same binary as the Aeron image built without the optional `aeron`
# feature: the global terminal service consumes NATS, and the historical tier
# only needs reqwest/taosAdapter. Build from the repository root:
#   docker build --platform linux/arm64 \
#     -f market_data_system/docker/k8s-history.Dockerfile \
#     -t matchengine-marketdata:history-arm64-v1 .

# syntax=docker/dockerfile:1

FROM rust:1.93-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY match_engine ./match_engine
COPY market_data_system ./market_data_system

RUN cargo build --release --manifest-path market_data_system/Cargo.toml

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 marketdata \
    && useradd --uid 10001 --gid 10001 --no-create-home \
        --shell /usr/sbin/nologin marketdata

COPY --from=builder /src/market_data_system/target/release/matchengine-marketdata /usr/local/bin/matchengine-marketdata

ENV HOME=/tmp
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/matchengine-marketdata"]

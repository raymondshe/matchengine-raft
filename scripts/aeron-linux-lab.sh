#!/bin/sh

# Linux external-driver/multicast validation for the market-data feed.
#
# This runs on OrbStack's x86_64 Linux userland (Rosetta on Apple silicon) in a
# dedicated container. The engine and both market-data service instances use a
# separate, supervised Aeron media driver process; no embedded driver is
# started. The multicast scenario runs one driver per process and publishes to
# a multicast group over `lo`, so the two services are independent subscribers.
#
# Usage:
#   scripts/aeron-linux-lab.sh [all|ipc|multicast] [--build] [--keep]
#
# The first run builds the amd64 toolchain image and both binaries; later runs
# reuse the cargo target volume.

set -eu

cd "$(dirname "$0")/.."

MODE=all
BUILD=0
KEEP=0
while [ "$#" -gt 0 ]; do
    case $1 in
    all | ipc | multicast)
        MODE=$1
        ;;
    --build)
        BUILD=1
        ;;
    --keep)
        KEEP=1
        ;;
    -h | --help)
        sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "error: unknown argument: $1" >&2
        exit 1
        ;;
    esac
    shift
done

CONTAINER=matchengine-aeron-lab
IMAGE=matchengine-raft:builder-amd64
TARGET_VOLUME=matchengine-raft-cargo-target-amd64
REGISTRY_VOLUME=matchengine-raft-cargo-registry-amd64
CHANNEL_MC='aeron:udp?endpoint=239.1.2.3:24001|interface=lo'

die() {
    echo "error: $*" >&2
    exit 1
}

log() {
    printf '\n== %s\n' "$*"
}

require_docker() {
    command -v docker >/dev/null 2>&1 || die "docker is not in PATH"
    docker info >/dev/null 2>&1 || die "Docker is not ready; run 'orb start' first"
}

ensure_image() {
    if [ "$BUILD" -eq 1 ] || ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
        log "building $IMAGE"
        DOCKER_BUILDKIT=${DOCKER_BUILDKIT:-1} \
            docker build --platform linux/amd64 \
                -f docker/linux.Dockerfile --target toolchain -t "$IMAGE" .
    fi
}

ensure_container() {
    if docker inspect "$CONTAINER" >/dev/null 2>&1; then
        docker start "$CONTAINER" >/dev/null 2>&1 || true
    else
        log "starting $CONTAINER"
        docker run -d --name "$CONTAINER" --platform linux/amd64 \
            --shm-size=1g --cap-add NET_ADMIN \
            -v "$PWD:/src" \
            -v "$REGISTRY_VOLUME:/usr/local/cargo/registry" \
            -v "$TARGET_VOLUME:/target" \
            -e CARGO_TARGET_DIR=/target \
            -w /src \
            "$IMAGE" sleep infinity >/dev/null
    fi
    docker exec "$CONTAINER" true
}

ensure_tools() {
    if docker exec "$CONTAINER" sh -c \
        'command -v ip >/dev/null 2>&1 && command -v curl >/dev/null 2>&1 && python3 -c "import websockets" >/dev/null 2>&1'; then
        return 0
    fi
    log "installing container tools"
    docker exec "$CONTAINER" sh -c \
        'apt-get update >/dev/null && apt-get install -y --no-install-recommends iproute2 procps curl python3 python3-websockets >/dev/null'
}

build_binaries() {
    log "building engine with market-data-aeron"
    docker exec "$CONTAINER" cargo build --release --features market-data-aeron

    log "building market-data service and aeron_driver"
    docker exec "$CONTAINER" cargo build --release \
        --manifest-path demos/market_data_system/Cargo.toml --features aeron
}

clean_lab() {
    docker exec "$CONTAINER" sh -c '
        for path in /proc/[0-9]*; do
            pid=${path#/proc/}
            [ "$pid" = "$$" ] && continue
            args=$(tr "\0" " " < "/proc/$pid/cmdline" 2>/dev/null || true)
            case $args in
                /target/release/matchengine-*|/target/release/aeron_driver*) kill -9 "$pid" 2>/dev/null || true ;;
            esac
        done
        rm -rf /dev/shm/aeron-* /tmp/aeron-lab
        mkdir -p /tmp/aeron-lab
    '
}

wait_http() {
    port=$1
    path=$2
    i=0
    while [ "$i" -lt 150 ]; do
        if docker exec "$CONTAINER" curl -fsS "http://127.0.0.1:$port$path" >/dev/null 2>&1; then
            return 0
        fi
        i=$((i + 1))
        sleep 0.1
    done
    die "timed out waiting for $path on $port"
}

http_code() {
    port=$1
    path=$2
    docker exec "$CONTAINER" curl -s -o /dev/null -w '%{http_code}' \
        "http://127.0.0.1:$port$path" 2>/dev/null || true
}

metric() {
    port=$1
    key=$2
    docker exec "$CONTAINER" curl -fsS "http://127.0.0.1:$port/metrics" |
        python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$key"
}

start_driver() {
    name=$1
    dir=$2
    docker exec -d -e AERON_DIR="$dir" "$CONTAINER" sh -c \
        'exec /target/release/aeron_driver > "$1" 2>&1' sh "/tmp/aeron-lab/driver-$name.log"
    i=0
    while [ "$i" -lt 150 ]; do
        if docker exec "$CONTAINER" test -f "$dir/cnc.dat" 2>/dev/null; then
            return 0
        fi
        i=$((i + 1))
        sleep 0.1
    done
    die "media driver $name did not create $dir/cnc.dat"
}

start_engine() {
    data=$1
    name=$2
    channel=$3
    dir=$4
    docker exec -d -e RAFT_WAL_FSYNC_MS=0 "$CONTAINER" sh -c '
        exec /target/release/matchengine-raft \
            --id 1 \
            --http-addr 127.0.0.1:21001 \
            --data-dir "$1" \
            --instance-prefix "aeron-$2" \
            --snapshot-interval-ms 0 \
            --market-data-aeron-channel "$3" \
            --market-data-aeron-dir "$4" \
            --market-data-aeron-snapshot-every-ms 0 \
            --market-data-aeron-heartbeat-ms 500 \
            > "$5" 2>&1
    ' sh "$data" "$name" "$channel" "$dir" "/tmp/aeron-lab/engine-$name.log"

    wait_http 21001 /health
    docker exec "$CONTAINER" curl -fsS -X POST \
        -H 'Content-Type: application/json' -d '[]' \
        http://127.0.0.1:21001/init >/dev/null
    wait_http 21001 /ready

    docker exec "$CONTAINER" curl -fsS -X POST \
        -H 'Content-Type: application/json' \
        -d '{"ConfigureSymbol":{"config":{"symbol_id":0,"base_asset":{"code":"EUR","class":"fiat","decimals":2},"quote_asset":{"code":"USD","class":"fiat","decimals":2},"price_decimals":5,"price_tick":"0.00001","base_lot":"0.01","fee_asset_policy":"received"}}}' \
        http://127.0.0.1:21001/write >/dev/null
    place_order 1 100.0 Buy
    place_order 2 101.0 Sell
    # Let the market-data tap apply the depth journal before a service asks for
    # a snapshot.
    sleep 1
}

start_service() {
    port=$1
    channel=$2
    dir=$3
    name=$4
    docker exec -d "$CONTAINER" sh -c '
        exec /target/release/matchengine-marketdata \
            --addr "127.0.0.1:$1" \
            --aeron-channel "$2" \
            --aeron-dir "$3" \
            --symbol eurusd:0:EUR:USD \
            > "$4" 2>&1
    ' sh "$port" "$channel" "$dir" "/tmp/aeron-lab/service-$name.log"
    wait_http "$port" /health
}

wait_ready() {
    port=$1
    i=0
    while [ "$i" -lt 150 ]; do
        if [ "$(http_code "$port" /ready)" = 200 ]; then
            return 0
        fi
        i=$((i + 1))
        sleep 0.1
    done
    die "service on $port did not become ready"
}

ws_depth_snapshot() {
    port=$1
    docker exec -i "$CONTAINER" python3 - "$port" <<'PY'
import asyncio
import sys
import time
import websockets

port = int(sys.argv[1])

async def main():
    started = time.time()
    async with websockets.connect(
        f"ws://127.0.0.1:{port}/ws/eurusd@depth@100ms"
    ) as ws:
        for _ in range(12):
            message = await asyncio.wait_for(ws.recv(), timeout=5)
            if '"e":"depthSnapshot"' in message and '"bids":[]' not in message:
                print(f"ws {port}: depthSnapshot at {time.time() - started:.3f}s {message[:220]}")
                return
        raise SystemExit(f"ws {port}: no non-empty depthSnapshot")

asyncio.run(main())
PY
}

place_order() {
    id=$1
    price=$2
    side=${3:-Buy}
    docker exec "$CONTAINER" curl -fsS -X POST \
        -H 'Content-Type: application/json' \
        -d "{\"Place\":{\"order\":{\"live\":\"Limit\",\"side\":\"$side\",\"price\":$price,\"volume\":1.0,\"id\":$id,\"sequence\":0}}}" \
        http://127.0.0.1:21001/write >/dev/null
}

kill_service() {
    port=$1
    docker exec "$CONTAINER" sh -c '
        port=$1
        prefix="/target/release/matchengine-marketdata --addr 127.0.0.1:$port "
        for path in /proc/[0-9]*; do
            pid=${path#/proc/}
            [ "$pid" = "$$" ] && continue
            args=$(tr "\0" " " < "/proc/$pid/cmdline" 2>/dev/null || true)
            case $args in
                "$prefix"*) kill -9 "$pid" 2>/dev/null || true ;;
            esac
        done
    ' sh "$port"
}

setup_multicast() {
    docker exec "$CONTAINER" sh -c '
        ip link set lo multicast on
        ip route add 239.1.2.3/32 dev lo 2>/dev/null || true
        ip link show lo | head -1
    '
}

run_ipc() {
    log "external driver over IPC"
    clean_lab
    start_driver ipc /dev/shm/aeron-ipc
    start_engine /tmp/aeron-lab/engine-ipc ipc 'aeron:ipc' /dev/shm/aeron-ipc
    start_service 24700 'aeron:ipc' /dev/shm/aeron-ipc ipc

    [ "$(http_code 24700 /ready)" = 503 ] ||
        die "expected service to be not ready before its first depth snapshot"
    ws_depth_snapshot 24700
    wait_ready 24700
    [ "$(metric 24700 snapshot_requests)" -ge 1 ] ||
        die "service did not send a stream-2010 snapshot request"
    [ "$(metric 24700 messages)" -ge 1 ] ||
        die "service did not receive feed frames"
    echo "PASS: external driver IPC, snapshot request, ready, metrics"
}

run_multicast() {
    log "multicast fan-out with one external driver per process"
    clean_lab
    setup_multicast
    start_driver mc-engine /dev/shm/aeron-mc-engine
    start_driver mc-svc1 /dev/shm/aeron-mc-svc1
    start_driver mc-svc2 /dev/shm/aeron-mc-svc2
    start_engine /tmp/aeron-lab/engine-mc mc "$CHANNEL_MC" /dev/shm/aeron-mc-engine
    start_service 24700 "$CHANNEL_MC" /dev/shm/aeron-mc-svc1 mc1
    start_service 24701 "$CHANNEL_MC" /dev/shm/aeron-mc-svc2 mc2

    ws_depth_snapshot 24700
    ws_depth_snapshot 24701
    wait_ready 24700
    wait_ready 24701

    before1=$(metric 24700 messages)
    before2=$(metric 24701 messages)
    place_order 3 99.5
    sleep 1
    after1=$(metric 24700 messages)
    after2=$(metric 24701 messages)
    [ "$after1" -gt "$before1" ] || die "service 24700 metrics did not advance"
    [ "$after2" -gt "$before2" ] || die "service 24701 metrics did not advance"

    kill_service 24701
    sleep 1
    before1=$(metric 24700 messages)
    place_order 4 99.0
    sleep 1
    after1=$(metric 24700 messages)
    [ "$(http_code 24700 /ready)" = 200 ] ||
        die "surviving service is not ready after the peer was killed"
    [ "$after1" -gt "$before1" ] ||
        die "surviving service stopped receiving after the peer was killed"
    [ "$(http_code 24701 /health)" = 000 ] ||
        die "killed service is still responding"
    echo "PASS: multicast fan-out, independent drivers, peer kill, metrics"
}

cleanup() {
    if [ "$KEEP" -eq 0 ]; then
        clean_lab >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

require_docker
ensure_image
ensure_container
ensure_tools
build_binaries

case $MODE in
ipc)
    run_ipc
    ;;
multicast)
    run_multicast
    ;;
all)
    run_ipc
    run_multicast
    ;;
esac

log "PASS ($MODE)"

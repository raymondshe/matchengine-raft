#!/bin/sh

# Four-container Linux Raft lab on OrbStack.
#
# This is the container counterpart of scripts/test-cluster.sh. It builds a
# Linux image, runs one container per node on a fixed-IP Docker bridge, and
# initialises all voters in one /init call. OrbStack exposes container IPs to
# macOS directly, so the same IPs work for peer gRPC and host-side curl.
#
# Assumes OrbStack Free (personal, non-commercial use). Run `orb start` once.

set -eu

cd "$(dirname "$0")/.."

ACTION=start
NODES=4
IMAGE=matchengine-raft:linux
BUILDER_IMAGE=matchengine-raft:builder
NETWORK=matchengine-raft
SUBNET=172.30.0.0/24
BASE_IP=172.30.0.11
BASE_PORT=21001
WAIT_SECS=60
ORDERS=1
BUILD=auto
FRESH=1
CLEAN=0
FSYNC_MODE=default
FSYNC_EXPLICIT=0

usage() {
    cat <<'EOF'
Run a four-container Linux Raft lab on OrbStack.

Usage:
  scripts/orbstack-cluster.sh [start] [options]
  scripts/orbstack-cluster.sh stop [--clean]
  scripts/orbstack-cluster.sh status
  scripts/orbstack-cluster.sh partition <node> <peer>...
  scripts/orbstack-cluster.sh heal [node...]
  scripts/orbstack-cluster.sh shell <node>
  scripts/orbstack-cluster.sh logs <node>
  scripts/orbstack-cluster.sh build
  scripts/orbstack-cluster.sh cargo <cargo args...>
  scripts/orbstack-cluster.sh linux-check

Commands:
  start        Build the image if needed, start the containers, initialise
               the cluster and run a small write/read smoke test (default).
  stop         Remove the lab containers. --clean also removes volumes and
               the Docker network.
  status       Show container, health/readiness and leader state.
  partition    Drop traffic from <node> to each <peer> and from each <peer>
               back to <node> using iptables inside the node container.
               Run it on both sides of a partition for a symmetric split.
  heal         Flush INPUT/OUTPUT iptables rules on all nodes (or the listed
               nodes).
  shell        Open a shell in a node container.
  logs         Show the last 200 log lines from a node.
  build        Build both the runtime and builder images.
  cargo        Run cargo in the Linux builder image with this repo mounted.
               Examples:
                 cargo test
                 cargo bench --bench apply_path -- --check
                 cargo bench --bench e2e -- --pin 0
  linux-check  Run the Linux-only checks: full cargo test, async-WAL tests,
               apply_path --check and the pinned e2e pass in both fsync modes.

Options:
  -n, --nodes N          Number of containers (default 4).
      --image NAME       Runtime image (default matchengine-raft:linux).
      --builder-image N  Builder image (default matchengine-raft:builder).
      --network NAME     Docker network (default matchengine-raft).
      --subnet CIDR      Network subnet (default 172.30.0.0/24).
      --base-ip IP       IP for node 1; node i increments the last octet
                         (default 172.30.0.11).
      --base-port PORT   HTTP port inside every container (default 21001).
                         Raft gRPC uses port + 1000.
      --orders N         Resting orders to place on the leader (default 1).
      --wait SECONDS     Per-step readiness timeout (default 60).
      --build            Rebuild the runtime image even if it exists.
      --no-build         Never build; fail if the image is missing.
      --fresh            Remove node volumes before starting (default).
      --keep-data        Reuse existing node volumes.
      --fsync            Run with RAFT_WAL_FSYNC_MS=0 (default).
      --no-fsync         Unset RAFT_WAL_FSYNC_MS (requires >= 4 nodes).
  -h, --help             Show this help.

Notes:
  * The containers share OrbStack's Linux kernel. They are separate network
    namespaces and have separate filesystems, but this is not four independent
    kernels. Use OrbStack machines, Lima or Apple container for per-node kernels.
  * Build the Linux binary in the container; the macOS target/debug binary is
    Mach-O and cannot run in these containers.
  * The image is built from docker/linux.Dockerfile. The repository is mounted
    at /src for `cargo` commands, with cargo registry/target in named volumes.
  * An explicit --fsync also passes RAFT_WAL_FSYNC_MS=0 into a `cargo` container.
EOF
}

die() {
    echo "error: $*" >&2
    exit 1
}

require_value() {
    [ "$#" -ge 2 ] || die "option $1 requires a value"
}

# --- argument parsing

while [ "$#" -gt 0 ]; do
    case $1 in
    start | stop | status | build | linux-check)
        ACTION=$1
        ;;
    partition | heal | shell | logs | cargo)
        # Everything after these commands is positional: node ids or cargo
        # arguments. Stop option parsing so `cargo bench --bench e2e` works.
        ACTION=$1
        shift
        break
        ;;
    -n | --nodes)
        require_value "$@"
        NODES=$2
        shift
        ;;
    --image)
        require_value "$@"
        IMAGE=$2
        shift
        ;;
    --builder-image)
        require_value "$@"
        BUILDER_IMAGE=$2
        shift
        ;;
    --network)
        require_value "$@"
        NETWORK=$2
        shift
        ;;
    --subnet)
        require_value "$@"
        SUBNET=$2
        shift
        ;;
    --base-ip)
        require_value "$@"
        BASE_IP=$2
        shift
        ;;
    --base-port)
        require_value "$@"
        BASE_PORT=$2
        shift
        ;;
    --orders)
        require_value "$@"
        ORDERS=$2
        shift
        ;;
    --wait)
        require_value "$@"
        WAIT_SECS=$2
        shift
        ;;
    --build)
        BUILD=always
        ;;
    --no-build)
        BUILD=never
        ;;
    --fresh)
        FRESH=1
        ;;
    --keep-data)
        FRESH=0
        ;;
    --clean)
        CLEAN=1
        ;;
    --fsync)
        FSYNC_MODE=fsync
        FSYNC_EXPLICIT=1
        ;;
    --no-fsync)
        FSYNC_MODE=none
        FSYNC_EXPLICIT=1
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    -*)
        die "unknown option: $1 (try --help)"
        ;;
    *)
        die "unknown command or argument: $1 (try --help)"
        ;;
    esac
    shift
done

case $NODES in
'' | *[!0-9]*) die "--nodes must be a positive integer" ;;
esac
[ "$NODES" -ge 1 ] || die "--nodes must be at least 1"

case $BASE_PORT in
'' | *[!0-9]*) die "--base-port must be an integer" ;;
esac
[ "$BASE_PORT" -ge 1 ] && [ "$BASE_PORT" -le 64535 ] ||
    die "--base-port must leave room for the derived gRPC port (+1000)"

case $ORDERS in
'' | *[!0-9]*) die "--orders must be a non-negative integer" ;;
esac

case $WAIT_SECS in
'' | *[!0-9]*) die "--wait must be a positive integer" ;;
esac
[ "$WAIT_SECS" -ge 1 ] || die "--wait must be at least 1"

BASE_PREFIX=${BASE_IP%.*}
BASE_LAST=${BASE_IP##*.}
case $BASE_LAST in
'' | *[!0-9]*) die "--base-ip must be an IPv4 address" ;;
esac
LAST_OCTET=$((BASE_LAST + NODES - 1))
[ "$LAST_OCTET" -le 254 ] || die "--base-ip leaves no room for $NODES nodes"

case $FSYNC_MODE in
default | fsync)
    export RAFT_WAL_FSYNC_MS=0
    ;;
none)
    [ "$NODES" -ge 4 ] ||
        die "--no-fsync requires at least 4 nodes; the binary enforces min_voters=4"
    unset RAFT_WAL_FSYNC_MS
    ;;
esac

# --- helpers

node_name() {
    echo "matchengine-raft-node$1"
}

node_volume() {
    echo "matchengine-raft-node$1-data"
}

node_ip() {
    echo "$BASE_PREFIX.$((BASE_LAST + $1 - 1))"
}

node_http() {
    echo "http://$(node_ip "$1"):$BASE_PORT"
}

require_docker() {
    command -v docker >/dev/null 2>&1 || die "docker is not in PATH"
    docker info >/dev/null 2>&1 || die "Docker is not ready; run 'orb start' first"
}

docker_network_exists() {
    docker network inspect "$NETWORK" >/dev/null 2>&1
}

build_runtime_image() {
    if [ "$BUILD" = never ]; then
        docker image inspect "$IMAGE" >/dev/null 2>&1 ||
            die "image $IMAGE is missing and --no-build was given"
        return 0
    fi
    if [ "$BUILD" = always ] || ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
        echo "building $IMAGE from docker/linux.Dockerfile"
        DOCKER_BUILDKIT=${DOCKER_BUILDKIT:-1} \
            docker build -f docker/linux.Dockerfile -t "$IMAGE" .
    fi
}

build_builder_image() {
    if [ "$BUILD" = never ]; then
        docker image inspect "$BUILDER_IMAGE" >/dev/null 2>&1 ||
            die "image $BUILDER_IMAGE is missing and --no-build was given"
        return 0
    fi
    if [ "$BUILD" = always ] || ! docker image inspect "$BUILDER_IMAGE" >/dev/null 2>&1; then
        echo "building $BUILDER_IMAGE from docker/linux.Dockerfile"
        DOCKER_BUILDKIT=${DOCKER_BUILDKIT:-1} \
            docker build -f docker/linux.Dockerfile --target toolchain -t "$BUILDER_IMAGE" .
    fi
}

ensure_network() {
    if ! docker_network_exists; then
        echo "creating network $NETWORK ($SUBNET)"
        docker network create --subnet "$SUBNET" "$NETWORK" >/dev/null
    fi
}

stop_containers() {
    i=1
    while [ "$i" -le "$NODES" ]; do
        docker rm -f "$(node_name "$i")" >/dev/null 2>&1 || true
        i=$((i + 1))
    done
}

remove_volumes() {
    i=1
    while [ "$i" -le "$NODES" ]; do
        docker volume rm "$(node_volume "$i")" >/dev/null 2>&1 || true
        i=$((i + 1))
    done
}

clean_lab() {
    stop_containers
    remove_volumes
    docker network rm "$NETWORK" >/dev/null 2>&1 || true
}

wait_until() {
    description=$1
    shift
    deadline=$(($(date +%s) + WAIT_SECS))
    while ! "$@" >/dev/null 2>&1; do
        if [ "$(date +%s)" -ge "$deadline" ]; then
            echo "error: timed out after ${WAIT_SECS}s waiting for $description" >&2
            echo "inspect with: $0 logs <node> or $0 status" >&2
            return 1
        fi
        sleep 0.2
    done
    printf 'ok: %s\n' "$description"
}

health_ok() {
    curl -fsS --max-time 2 "http://$1:$BASE_PORT/health" >/dev/null 2>&1
}

ready_ok() {
    curl -fsS --max-time 2 "http://$1:$BASE_PORT/ready" >/dev/null 2>&1
}

all_healthy() {
    i=1
    while [ "$i" -le "$NODES" ]; do
        health_ok "$(node_ip "$i")" || return 1
        i=$((i + 1))
    done
}

all_ready() {
    i=1
    while [ "$i" -le "$NODES" ]; do
        ready_ok "$(node_ip "$i")" || return 1
        i=$((i + 1))
    done
}

find_leader() {
    best_term=-1
    best_leader=
    i=1
    while [ "$i" -le "$NODES" ]; do
        metrics=$(curl -fsS --max-time 2 "$(node_http "$i")/metrics" 2>/dev/null || true)
        if [ -n "$metrics" ]; then
            term=$(
                printf '%s' "$metrics" |
                    sed -n 's/.*"current_term":\([0-9][0-9]*\).*/\1/p' |
                    head -n 1
            )
            leader=$(
                printf '%s' "$metrics" |
                    sed -n 's/.*"current_leader":\([0-9][0-9]*\).*/\1/p' |
                    head -n 1
            )
            if [ -n "$term" ] && [ -n "$leader" ] && [ "$term" -gt "$best_term" ]; then
                best_term=$term
                best_leader=$leader
            fi
        fi
        i=$((i + 1))
    done
    if [ -n "$best_leader" ]; then
        echo "$best_leader"
        return 0
    fi
    return 1
}

book_has_order() {
    ip=$1
    order_id=$2
    body=$(
        curl -fsS --max-time 2 -X POST \
            -H "Content-Type: application/json" -d '"orderbook_orders"' \
            "http://$ip:$BASE_PORT/read" 2>/dev/null
    ) || return 1
    printf '%s' "$body" | grep -Eq '\\"id\\"[[:space:]]*:[[:space:]]*'"$order_id"'([^0-9]|$)'
}

start_node() {
    node_id=$1
    ip=$(node_ip "$node_id")
    name=$(node_name "$node_id")
    volume=$(node_volume "$node_id")

    env_args=
    if [ "$FSYNC_MODE" != none ]; then
        env_args="-e RAFT_WAL_FSYNC_MS=0"
    fi

    # NET_ADMIN is needed for the iptables partition helpers.
    # shellcheck disable=SC2086
    docker run -d --name "$name" --hostname "$name" \
        --network "$NETWORK" --ip "$ip" \
        --cpus 2 --memory 2g \
        --cap-add NET_ADMIN \
        -v "$volume:/data" \
        $env_args \
        "$IMAGE" --id "$node_id" --http-addr "$ip:$BASE_PORT" --data-dir /data \
        >/dev/null
    echo "started $name on $ip:$BASE_PORT"
}

init_cluster() {
    members=
    i=1
    while [ "$i" -le "$NODES" ]; do
        [ -n "$members" ] && members="$members,"
        members="$members[$i, \"$(node_ip "$i"):$BASE_PORT\"]"
        i=$((i + 1))
    done
    echo "initialising all $NODES voters in one /init call"
    curl -fsS --max-time "$WAIT_SECS" -X POST \
        -H "Content-Type: application/json" \
        -d "[$members]" \
        "$(node_http 1)/init" >/dev/null
}

seed_orders() {
    [ "$ORDERS" -gt 0 ] || return 0
    leader=$(find_leader || true)
    [ -n "$leader" ] || die "cannot place orders without a leader"
    echo "placing $ORDERS resting buy order(s) on node $leader"
    i=1
    while [ "$i" -le "$ORDERS" ]; do
        price=$((4000 + i))
        curl -fsS --max-time "$WAIT_SECS" -X POST \
            -H "Content-Type: application/json" \
            -d '{"Place":{"order":{"live":"Limit","side":"Buy","price":'"$price"'.0,"volume":4.4,"id":'"$i"',"sequence":0}}}' \
            "$(node_http "$leader")/write" >/dev/null
        i=$((i + 1))
    done

    i=1
    while [ "$i" -le "$NODES" ]; do
        wait_until "node $i to apply order $ORDERS" book_has_order "$(node_ip "$i")" "$ORDERS"
        i=$((i + 1))
    done
}

print_status() {
    i=1
    while [ "$i" -le "$NODES" ]; do
        name=$(node_name "$i")
        ip=$(node_ip "$i")
        state=$(
            docker inspect -f '{{.State.Status}}' "$name" 2>/dev/null |
                tr -d '\n' || true
        )
        [ -n "$state" ] || state=missing
        actual_ip=$(
            docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' \
                "$name" 2>/dev/null | tr -d '\n' || true
        )
        [ -n "$actual_ip" ] || actual_ip=$ip
        [ "$actual_ip" = "invalid IP" ] && actual_ip=$ip
        health=$(
            curl -s -o /dev/null -w '%{http_code}' --max-time 2 \
                "http://$actual_ip:$BASE_PORT/health" 2>/dev/null || true
        )
        ready=$(
            curl -s -o /dev/null -w '%{http_code}' --max-time 2 \
                "http://$actual_ip:$BASE_PORT/ready" 2>/dev/null || true
        )
        [ "$health" = 000 ] && health=down
        [ "$ready" = 000 ] && ready=down
        [ -n "$health" ] || health=down
        [ -n "$ready" ] || ready=down

        metrics=$(curl -fsS --max-time 2 "http://$actual_ip:$BASE_PORT/metrics" 2>/dev/null || true)
        raft_state=unknown
        term=-
        leader_view=-
        if [ -n "$metrics" ]; then
            value=$(
                printf '%s' "$metrics" |
                    sed -n 's/.*"state":"\([^"]*\)".*/\1/p' |
                    head -n 1
            )
            [ -n "$value" ] && raft_state=$value
            value=$(
                printf '%s' "$metrics" |
                    sed -n 's/.*"current_term":\([0-9][0-9]*\).*/\1/p' |
                    head -n 1
            )
            [ -n "$value" ] && term=$value
            value=$(
                printf '%s' "$metrics" |
                    sed -n 's/.*"current_leader":\([0-9][0-9]*\).*/\1/p' |
                    head -n 1
            )
            [ -n "$value" ] && leader_view=$value
        fi
        printf 'node %s  %-12s  ip=%-15s  health=%s  ready=%s  state=%-8s term=%s  leader=%s\n' \
            "$i" "$state" "$actual_ip" "$health" "$ready" "$raft_state" "$term" "$leader_view"
        i=$((i + 1))
    done

    leader=$(find_leader || true)
    if [ -n "$leader" ]; then
        echo "leader: node $leader"
    else
        echo "leader: unknown"
    fi
}

partition_node() {
    node_id=$1
    shift
    [ "$#" -ge 1 ] || die "usage: partition <node> <peer>..."
    name=$(node_name "$node_id")
    docker inspect "$name" >/dev/null 2>&1 || die "unknown node: $node_id"
    for peer in "$@"; do
        case $peer in
        '' | *[!0-9]*) die "partition peers must be numeric node ids" ;;
        esac
        [ "$peer" -ge 1 ] && [ "$peer" -le "$NODES" ] ||
            die "peer $peer is outside 1..$NODES"
        [ "$peer" -ne "$node_id" ] || continue
        peer_ip=$(node_ip "$peer")
        docker exec "$name" iptables -w 5 -C INPUT -s "$peer_ip" -j DROP >/dev/null 2>&1 ||
            docker exec "$name" iptables -w 5 -I INPUT -s "$peer_ip" -j DROP >/dev/null
        docker exec "$name" iptables -w 5 -C OUTPUT -d "$peer_ip" -j DROP >/dev/null 2>&1 ||
            docker exec "$name" iptables -w 5 -I OUTPUT -d "$peer_ip" -j DROP >/dev/null
        echo "node $node_id: blocked $peer_ip"
    done
}

heal_node() {
    node_id=$1
    name=$(node_name "$node_id")
    docker inspect "$name" >/dev/null 2>&1 || return 0
    docker exec "$name" iptables -w 5 -F INPUT >/dev/null 2>&1 || true
    docker exec "$name" iptables -w 5 -F OUTPUT >/dev/null 2>&1 || true
    echo "node $node_id: flushed INPUT/OUTPUT"
}

run_cargo() {
    require_docker
    build_builder_image

    network_args=
    if docker_network_exists; then
        network_args="--network $NETWORK"
    fi

    cargo_env_args=
    if [ "$FSYNC_EXPLICIT" -eq 1 ] && [ "$FSYNC_MODE" = fsync ]; then
        cargo_env_args="-e RAFT_WAL_FSYNC_MS=0"
    fi

    # shellcheck disable=SC2086
    docker run --rm -i \
        $network_args \
        $cargo_env_args \
        -v "$PWD:/src" \
        -v matchengine-raft-cargo-registry:/usr/local/cargo/registry \
        -v matchengine-raft-cargo-target:/target \
        -e CARGO_TARGET_DIR=/target \
        -w /src \
        "$BUILDER_IMAGE" cargo "$@"
}

linux_check() {
    require_docker
    build_builder_image

    echo "=== cargo test (Linux) ==="
    run_cargo test

    echo "=== async-WAL cluster tests (Linux) ==="
    run_cargo test --features async-wal --test cluster async_wal

    echo "=== apply-path allocation check (Linux) ==="
    run_cargo bench --bench apply_path -- --check

    echo "=== pinned-core e2e (Linux, no fsync) ==="
    run_cargo bench --bench e2e -- --pin 0

    echo "=== pinned-core e2e (Linux, every-append fsync) ==="
    saved_mode=$FSYNC_MODE
    saved_explicit=$FSYNC_EXPLICIT
    FSYNC_MODE=fsync
    FSYNC_EXPLICIT=1
    run_cargo bench --bench e2e -- --pin 0
    FSYNC_MODE=$saved_mode
    FSYNC_EXPLICIT=$saved_explicit
}

# --- command dispatch

case $ACTION in
start)
    require_docker
    build_runtime_image
    echo "stopping any existing lab containers"
    stop_containers
    if [ "$FRESH" -eq 1 ]; then
        echo "removing node volumes"
        remove_volumes
    fi
    ensure_network

    echo "starting $NODES-container Linux lab on $SUBNET"
    i=1
    while [ "$i" -le "$NODES" ]; do
        start_node "$i"
        i=$((i + 1))
    done

    wait_until "all $NODES containers to become healthy" all_healthy

    if all_ready; then
        echo "cluster is already initialised; skipping /init"
    else
        init_cluster
    fi
    wait_until "cluster to become ready" all_ready

    leader=$(find_leader || true)
    [ -n "$leader" ] || die "cluster has no leader"
    echo "leader: node $leader ($(node_http "$leader"))"

    seed_orders

    echo
    echo "nodes:"
    i=1
    while [ "$i" -le "$NODES" ]; do
        echo "  node $i  http=$(node_http "$i")  grpc=$(node_ip "$i"):$((BASE_PORT + 1000))"
        i=$((i + 1))
    done
    echo
    echo "next:"
    echo "  $0 status"
    echo "  $0 partition 1 2 3"
    echo "  $0 heal"
    echo "  $0 cargo bench --bench e2e -- --pin 0"
    ;;
stop)
    require_docker
    if [ "$CLEAN" -eq 1 ]; then
        clean_lab
        echo "removed containers, volumes and network $NETWORK"
    else
        stop_containers
        echo "removed lab containers (volumes and network kept)"
    fi
    ;;
status)
    require_docker
    print_status
    ;;
partition)
    require_docker
    [ "$#" -ge 2 ] || die "usage: partition <node> <peer>..."
    partition_node "$@"
    ;;
heal)
    require_docker
    if [ "$#" -ge 1 ]; then
        for node_id in "$@"; do
            heal_node "$node_id"
        done
    else
        i=1
        while [ "$i" -le "$NODES" ]; do
            heal_node "$i"
            i=$((i + 1))
        done
    fi
    ;;
shell)
    require_docker
    [ "$#" -eq 1 ] || die "usage: shell <node>"
    docker exec -it "$(node_name "$1")" sh
    ;;
logs)
    require_docker
    [ "$#" -eq 1 ] || die "usage: logs <node>"
    docker logs --tail 200 "$(node_name "$1")"
    ;;
build)
    require_docker
    BUILD=always
    build_runtime_image
    build_builder_image
    ;;
cargo)
    [ "$#" -ge 1 ] || die "usage: cargo <cargo args...>"
    run_cargo "$@"
    ;;
linux-check)
    linux_check
    ;;
esac

#!/bin/sh

# Start, inspect or stop a local matchengine-raft cluster.
#
# The defaults are a four-node cluster on 127.0.0.1:21001-21004; each node
# serves the HTTP API on that port and Raft gRPC on port + 1000. Every knob can
# be overridden; run with --help for the full list.

set -eu

# Always run from the repository root.
cd "$(dirname "$0")/.."

ACTION=start
NODES=4
BASE_PORT=21001
DEFAULT_DATA_DIR=/tmp/matchengine-raft-test-cluster
DATA_DIR=${RAFT_DATA_DIR:-$DEFAULT_DATA_DIR}
DATA_DIR_EXPLICIT=0
LOG_DIR=logs
BINARY=
BUILD=1
KEEP=0
FRESH=0
ORDERS=1
WAIT_SECS=30
FSYNC_MODE=default
RELEASE=0

usage() {
    cat <<'EOF'
Start, inspect or stop a local matchengine-raft cluster.

Usage:
  scripts/test-cluster.sh [start] [options]
  scripts/test-cluster.sh stop [--data-dir DIR]
  scripts/test-cluster.sh status [--nodes N] [--base-port PORT]

Commands:
  start    Build, start, initialise and smoke-test the cluster (default).
           Stops it on exit unless --keep is given.
  stop     Stop the nodes recorded in <data-dir>/pids by a previous start.
  status   Print health/readiness for each node and the current leader.

Options:
  -n, --nodes N          Number of nodes (default 4).
  -p, --base-port PORT   HTTP port for node 1; node i uses PORT+i-1.
                         Raft gRPC uses each HTTP port + 1000 (default 21001).
  -d, --data-dir DIR     State root (default /tmp/matchengine-raft-test-cluster).
                         The default directory is wiped on start; an explicit
                         directory is kept unless --fresh is passed.
  -l, --log-dir DIR      Per-node log directory (default logs).
  -b, --binary PATH      Binary to run (default target/debug/matchengine-raft).
      --release          Build/run target/release/matchengine-raft.
      --build            Run cargo build before starting (default).
      --no-build         Use an existing binary.
      --fresh            Wipe --data-dir before starting.
      --keep             Leave the cluster running after the smoke test.
      --orders N         Resting orders to place on the leader (default 1).
      --wait SECONDS     Per-step readiness timeout (default 30).
      --fsync            Run with RAFT_WAL_FSYNC_MS=0 (fsync every append).
      --no-fsync         Unset RAFT_WAL_FSYNC_MS (no fsync, production profile).
                         Requires at least 4 nodes because the binary then
                         enforces min_voters=4.
  -h, --help             Show this help.

Environment:
  RAFT_DATA_DIR          Default for --data-dir; never wiped implicitly.
  RAFT_WAL_FSYNC_MS      Preserved unless --fsync/--no-fsync is given. When
                         neither is given and the variable is unset, the
                         script uses 0 (fsync every append) so clusters with
                         fewer than four voters remain valid.

Examples:
  ./scripts/test-cluster.sh
  ./scripts/test-cluster.sh --nodes 3
  ./scripts/test-cluster.sh --no-fsync --orders 100
  ./scripts/test-cluster.sh --keep
  ./scripts/test-cluster.sh status
  ./scripts/test-cluster.sh stop
EOF
}

die() {
    echo "error: $*" >&2
    exit 1
}

require_value() {
    [ "$#" -ge 2 ] || die "option $1 requires a value"
}

while [ "$#" -gt 0 ]; do
    case $1 in
    start | stop | status)
        ACTION=$1
        ;;
    -n | --nodes)
        require_value "$@"
        NODES=$2
        shift
        ;;
    -p | --base-port)
        require_value "$@"
        BASE_PORT=$2
        shift
        ;;
    -d | --data-dir)
        require_value "$@"
        DATA_DIR=$2
        DATA_DIR_EXPLICIT=1
        shift
        ;;
    -l | --log-dir)
        require_value "$@"
        LOG_DIR=$2
        shift
        ;;
    -b | --binary)
        require_value "$@"
        BINARY=$2
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
        BUILD=1
        ;;
    --no-build)
        BUILD=0
        ;;
    --fresh)
        FRESH=1
        ;;
    --keep)
        KEEP=1
        ;;
    --release)
        RELEASE=1
        ;;
    --fsync)
        FSYNC_MODE=fsync
        ;;
    --no-fsync)
        FSYNC_MODE=none
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
LAST_PORT=$((BASE_PORT + NODES - 1))
[ "$LAST_PORT" -le 64535 ] ||
    die "node range $BASE_PORT-$LAST_PORT leaves no room for the derived gRPC port (+1000)"

case $ORDERS in
'' | *[!0-9]*) die "--orders must be a non-negative integer" ;;
esac

case $WAIT_SECS in
'' | *[!0-9]*) die "--wait must be a positive integer" ;;
esac
[ "$WAIT_SECS" -ge 1 ] || die "--wait must be at least 1"

# The default data dir is a disposable test root. Explicit directories are
# preserved unless the caller asks for a fresh start.
if [ "$FRESH" -eq 0 ] && [ "$DATA_DIR_EXPLICIT" -eq 0 ] && [ -z "${RAFT_DATA_DIR:-}" ]; then
    FRESH=1
fi

# The binary reads its state root from RAFT_DATA_DIR; keep the resolved path in
# the environment so start/stop always agree on the same instance.
export RAFT_DATA_DIR="$DATA_DIR"

case $FSYNC_MODE in
default)
    if [ -z "${RAFT_WAL_FSYNC_MS+x}" ]; then
        export RAFT_WAL_FSYNC_MS=0
    fi
    ;;
fsync)
    export RAFT_WAL_FSYNC_MS=0
    ;;
none)
    [ "$NODES" -ge 4 ] ||
        die "--no-fsync requires at least 4 nodes; the binary enforces min_voters=4"
    unset RAFT_WAL_FSYNC_MS
    ;;
esac

if [ -z "$BINARY" ]; then
    if [ "$RELEASE" -eq 1 ]; then
        BINARY=./target/release/matchengine-raft
    else
        BINARY=./target/debug/matchengine-raft
    fi
fi

PID_DIR=$DATA_DIR/pids
PORTS=
MEMBERS=
MEMBERS_JSON=
i=1
while [ "$i" -le "$NODES" ]; do
    port=$((BASE_PORT + i - 1))
    PORTS="$PORTS $port"
    if [ -n "$MEMBERS" ]; then
        MEMBERS="$MEMBERS,"
        MEMBERS_JSON="$MEMBERS_JSON,"
    fi
    MEMBERS="$MEMBERS$i"
    MEMBERS_JSON="$MEMBERS_JSON[$i, \"127.0.0.1:$port\"]"
    i=$((i + 1))
done

port_for() {
    echo $((BASE_PORT + $1 - 1))
}

url_for() {
    echo "http://127.0.0.1:$(port_for "$1")"
}

rpc() {
    method=$1
    url=$2
    body=${3-}

    if [ -n "$body" ]; then
        response=$(
            curl -sS --max-time "$WAIT_SECS" -X "$method" \
                -H "Content-Type: application/json" -d "$body" \
                -w '\n%{http_code}' "$url"
        )
    else
        response=$(
            curl -sS --max-time "$WAIT_SECS" -X "$method" \
                -w '\n%{http_code}' "$url"
        )
    fi

    status=$(printf '%s\n' "$response" | tail -n 1)
    payload=$(printf '%s\n' "$response" | sed '$d')
    case $status in
    2*)
        printf '%s\n' "$payload"
        ;;
    *)
        echo "HTTP $status from $method $url" >&2
        printf '%s\n' "$payload" >&2
        return 1
        ;;
    esac
}

health_ok() {
    curl -fsS --max-time 2 "http://127.0.0.1:$1/health" >/dev/null 2>&1
}

ready_ok() {
    curl -fsS --max-time 2 "http://127.0.0.1:$1/ready" >/dev/null 2>&1
}

all_healthy() {
    for port in $PORTS; do
        health_ok "$port" || return 1
    done
}

all_ready() {
    for port in $PORTS; do
        ready_ok "$port" || return 1
    done
}

wait_until() {
    description=$1
    shift
    deadline=$(($(date +%s) + WAIT_SECS))
    while ! "$@" >/dev/null 2>&1; do
        if [ "$(date +%s)" -ge "$deadline" ]; then
            echo "error: timed out after ${WAIT_SECS}s waiting for $description" >&2
            echo "see $LOG_DIR/n1.log ... $LOG_DIR/n$NODES.log" >&2
            return 1
        fi
        sleep 0.2
    done
    printf 'ok: %s\n' "$description"
}

find_leader() {
    for port in $PORTS; do
        metrics=$(curl -fsS --max-time 2 "http://127.0.0.1:$port/metrics" 2>/dev/null || true)
        [ -n "$metrics" ] || continue
        leader=$(
            printf '%s' "$metrics" |
                sed -n 's/.*"current_leader":\([0-9][0-9]*\).*/\1/p' |
                head -n 1
        )
        if [ -n "$leader" ]; then
            echo "$leader"
            return 0
        fi
    done
    return 1
}

book_has_order() {
    port=$1
    order_id=$2
    body=$(
        curl -fsS --max-time 2 -X POST \
            -H "Content-Type: application/json" -d '"orderbook_orders"' \
            "http://127.0.0.1:$port/read" 2>/dev/null
    ) || return 1
    # /read wraps the order array as a JSON string, so quotes are escaped.
    printf '%s' "$body" | grep -Eq '\\"id\\"[[:space:]]*:[[:space:]]*'"$order_id"'([^0-9]|$)'
}

node_command() {
    pid=$1
    ps -p "$pid" -o command= 2>/dev/null || true
}

terminate_pids() {
    pids=$1
    for pid in $pids; do
        kill "$pid" 2>/dev/null || true
    done

    # Give the nodes a few seconds to shut down, then force them.
    tries=0
    while [ "$tries" -lt 50 ]; do
        alive=0
        for pid in $pids; do
            if kill -0 "$pid" 2>/dev/null; then
                alive=1
            fi
        done
        if [ "$alive" -eq 0 ]; then
            break
        fi
        sleep 0.1
        tries=$((tries + 1))
    done
    for pid in $pids; do
        kill -9 "$pid" 2>/dev/null || true
    done
}

stop_cluster() {
    [ -d "$PID_DIR" ] || return 0

    pids=
    for pid_file in "$PID_DIR"/n*.pid; do
        [ -f "$pid_file" ] || continue
        pid=
        IFS= read -r pid <"$pid_file" || true
        rm -f "$pid_file"
        case $pid in
        '' | *[!0-9]*) continue ;;
        esac
        cmdline=$(node_command "$pid")
        case $cmdline in
        *matchengine-raft*)
            pids="$pids $pid"
            ;;
        esac
    done
    if [ -n "$pids" ]; then
        terminate_pids "$pids"
    fi
}

# Stops nodes already bound to our HTTP ports even when they were started with
# another data dir. This keeps a rerun from failing on a stale kept cluster
# without killing unrelated engines on other ports.
stop_port_owners() {
    pids=
    for port in $PORTS; do
        found=$(pgrep -f "matchengine-raft .*--http-addr 127\.0\.0\.1:$port" 2>/dev/null || true)
        for pid in $found; do
            cmdline=$(node_command "$pid")
            case $cmdline in
            *"--http-addr 127.0.0.1:$port "*) ;;
            *"--http-addr 127.0.0.1:$port") ;;
            *) continue ;;
            esac
            case " $pids " in
            *" $pid "*) continue ;;
            esac
            echo "stopping existing matchengine-raft on port $port (pid $pid)"
            pids="$pids $pid"
        done
    done
    if [ -n "$pids" ]; then
        terminate_pids "$pids"
    fi
}

start_node() {
    node_id=$1
    port=$(port_for "$node_id")
    log=$LOG_DIR/n$node_id.log

    nohup "$BINARY" --id "$node_id" --http-addr "127.0.0.1:$port" \
        >"$log" 2>&1 &
    echo "$!" >"$PID_DIR/n$node_id.pid"
}

print_status() {
    for port in $PORTS; do
        node_id=$((port - BASE_PORT + 1))
        health=$(
            curl -s -o /dev/null -w '%{http_code}' --max-time 2 \
                "http://127.0.0.1:$port/health" 2>/dev/null || true
        )
        ready=$(
            curl -s -o /dev/null -w '%{http_code}' --max-time 2 \
                "http://127.0.0.1:$port/ready" 2>/dev/null || true
        )
        [ "$health" = 000 ] && health=down
        [ "$ready" = 000 ] && ready=down
        [ -n "$health" ] || health=down
        [ -n "$ready" ] || ready=down
        printf 'node %s  port=%s  health=%s  ready=%s\n' "$node_id" "$port" "$health" "$ready"
    done
    leader=$(find_leader || true)
    if [ -n "$leader" ]; then
        echo "leader: node $leader"
    else
        echo "leader: unknown"
    fi
}

case $ACTION in
stop)
    stop_cluster
    echo "stopped cluster in $DATA_DIR"
    exit 0
    ;;
status)
    print_status
    exit 0
    ;;
esac

# --- start

if [ "$BUILD" -eq 1 ]; then
    if [ "$RELEASE" -eq 1 ]; then
        cargo build --release
    else
        cargo build
    fi
fi
[ -x "$BINARY" ] || die "binary not found or not executable: $BINARY (run without --no-build)"

cleanup() {
    status=$?
    set +e
    if [ "$KEEP" -eq 1 ] && [ "$status" -eq 0 ]; then
        return 0
    fi
    stop_cluster
    return "$status"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM
trap 'exit 141' PIPE

# Stop any nodes recorded for this data dir, then any other engine holding the
# target HTTP ports, before rebinding.
stop_cluster
stop_port_owners
if [ "$FRESH" -eq 1 ]; then
    rm -rf "$DATA_DIR"
fi
mkdir -p "$DATA_DIR" "$PID_DIR" "$LOG_DIR"

echo "Starting $NODES-node cluster on 127.0.0.1:$BASE_PORT-$LAST_PORT (gRPC $((BASE_PORT + 1000))-$((LAST_PORT + 1000)))"
echo "data dir: $DATA_DIR"
echo "logs:     $LOG_DIR/n1.log ... $LOG_DIR/n$NODES.log"

i=1
while [ "$i" -le "$NODES" ]; do
    start_node "$i"
    echo "started node $i on $(url_for "$i")"
    i=$((i + 1))
done

wait_until "all $NODES nodes to become healthy" all_healthy

if [ "$FSYNC_MODE" = none ]; then
    echo "initialising all $NODES nodes in one /init call (no WAL fsync requires four voters)"
    rpc POST "$(url_for 1)/init" "[$MEMBERS_JSON]" >/dev/null
else
    echo "initialising node 1 as a single-node cluster"
    rpc POST "$(url_for 1)/init" '[]' >/dev/null
    wait_until "node 1 leadership" ready_ok "$(port_for 1)"

    i=2
    while [ "$i" -le "$NODES" ]; do
        echo "adding node $i as a learner"
        rpc POST "$(url_for 1)/add-learner" "[$i, \"127.0.0.1:$(port_for "$i")\"]" >/dev/null
        i=$((i + 1))
    done

    if [ "$NODES" -gt 1 ]; then
        echo "changing membership to [$MEMBERS]"
        rpc POST "$(url_for 1)/change-membership" "[$MEMBERS]" >/dev/null
    fi
fi

wait_until "cluster to become ready" all_ready

LEADER_ID=$(find_leader || true)
[ -n "$LEADER_ID" ] || die "cluster has no leader"
LEADER_URL=$(url_for "$LEADER_ID")
echo "leader: node $LEADER_ID ($LEADER_URL)"

if [ "$ORDERS" -gt 0 ]; then
    echo "placing $ORDERS resting buy order(s) on the leader"
    i=1
    while [ "$i" -le "$ORDERS" ]; do
        price=$((4000 + i))
        rpc POST "$LEADER_URL/write" \
            '{"Place":{"order":{"live":"Limit","side":"Buy","price":'"$price"'.0,"volume":4.4,"id":'"$i"',"sequence":0}}}' \
            >/dev/null
        i=$((i + 1))
    done

    i=1
    while [ "$i" -le "$NODES" ]; do
        wait_until "node $i to apply order $ORDERS" book_has_order "$(port_for "$i")" "$ORDERS"
        i=$((i + 1))
    done
fi

echo "reading the order book from every node"
for port in $PORTS; do
    node_id=$((port - BASE_PORT + 1))
    echo "node $node_id:"
    rpc POST "http://127.0.0.1:$port/read" '"orderbook_orders"'
done

if [ "$KEEP" -eq 1 ]; then
    echo
    echo "cluster left running; stop it with:"
    echo "  ./scripts/test-cluster.sh stop --data-dir $DATA_DIR"
else
    echo
    echo "stopping the cluster"
fi

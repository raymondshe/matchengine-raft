#!/bin/sh

set -eu

# Run the standalone demo desk against two independent engines: a Spot
# EUR/USD orderbook and a Perpetual BTC/USDT orderbook. Only this script's
# children are stopped on exit; unrelated engine/demo processes are left alone.

cd "$(dirname "$0")/.."

DEMO_ADDR=${DEMO_ADDR:-127.0.0.1:24400}
SPOT_ADDR=${SPOT_ADDR:-127.0.0.1:24401}
PERP_ADDR=${PERP_ADDR:-127.0.0.1:24402}
DATA_DIR=${DEMO_PAIR_DATA_DIR:-/tmp/matchengine-raft-demo-pair}

# Engine binaries add RAFT_PORT_OFFSET (1000) to the HTTP port for gRPC.
SPOT_GRPC=$(( $(echo "$SPOT_ADDR" | sed 's/.*://') + 1000 ))
PERP_GRPC=$(( $(echo "$PERP_ADDR" | sed 's/.*://') + 1000 ))

SPOT_PID=
PERP_PID=
DEMO_PID=

cleanup() {
    trap - EXIT INT TERM
    for pid in "$DEMO_PID" "$SPOT_PID" "$PERP_PID"; do
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || true
        fi
    done
    wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

wait_http() {
    addr=$1
    name=$2
    i=0
    while [ "$i" -lt 100 ]; do
        if curl -sf "http://$addr/health" >/dev/null 2>&1; then
            return 0
        fi
        i=$((i + 1))
        sleep 0.1
    done
    echo "$name at $addr did not become healthy" >&2
    return 1
}

wait_ready() {
    addr=$1
    name=$2
    i=0
    while [ "$i" -lt 100 ]; do
        if curl -sf "http://$addr/ready" >/dev/null 2>&1; then
            return 0
        fi
        i=$((i + 1))
        sleep 0.1
    done
    echo "$name at $addr did not elect a leader" >&2
    return 1
}

write_config() {
    addr=$1
    name=$2
    config=$3
    response=$(curl -sf -X POST "http://$addr/write" \
        -H 'Content-Type: application/json' \
        -d "{\"ConfigureSymbol\":{\"config\":$config}}")
    case "$response" in
        *'"Err"'*)
            echo "failed to configure $name: $response" >&2
            exit 1
            ;;
        *configured*)
            echo "configured $name"
            ;;
        *)
            echo "failed to configure $name: $response" >&2
            exit 1
            ;;
    esac
}

cargo build
cargo build --manifest-path demos/order_system/Cargo.toml

if [ -z "${DEMO_PAIR_DATA_DIR:-}" ]; then
    rm -rf "$DATA_DIR"
fi
mkdir -p "$DATA_DIR/spot" "$DATA_DIR/perp" "$DATA_DIR/logs"

echo "Starting Spot engine on $SPOT_ADDR (gRPC $SPOT_GRPC)"
./target/debug/matchengine-raft \
    --id 1 \
    --http-addr "$SPOT_ADDR" \
    --data-dir "$DATA_DIR/spot" \
    --instance-prefix demo-spot \
    --snapshot-interval-ms 0 \
    >"$DATA_DIR/logs/spot.log" 2>&1 &
SPOT_PID=$!

echo "Starting Perpetual engine on $PERP_ADDR (gRPC $PERP_GRPC)"
./target/debug/matchengine-raft \
    --id 1 \
    --http-addr "$PERP_ADDR" \
    --data-dir "$DATA_DIR/perp" \
    --instance-prefix demo-perp \
    --snapshot-interval-ms 0 \
    >"$DATA_DIR/logs/perp.log" 2>&1 &
PERP_PID=$!

wait_http "$SPOT_ADDR" "Spot engine"
wait_http "$PERP_ADDR" "Perpetual engine"

curl -sf -X POST "http://$SPOT_ADDR/init" \
    -H 'Content-Type: application/json' -d '[]' >/dev/null
curl -sf -X POST "http://$PERP_ADDR/init" \
    -H 'Content-Type: application/json' -d '[]' >/dev/null

wait_ready "$SPOT_ADDR" "Spot engine"
wait_ready "$PERP_ADDR" "Perpetual engine"

# Spot demo instrument: EUR/USD, five price decimals, 0.01 EUR lot.
write_config "$SPOT_ADDR" "Spot EUR/USD" '{
  "symbol_id": 1,
  "base_asset": {"code": "EUR", "class": "fiat", "decimals": 2},
  "quote_asset": {"code": "USD", "class": "fiat", "decimals": 2},
  "price_decimals": 5,
  "price_tick": "0.00001",
  "base_lot": "0.01",
  "fee_asset_policy": "received"
}'

# The perpetual backend keeps a linear crypto pair and requires quote fees.
write_config "$PERP_ADDR" "Perpetual BTC/USDT" '{
  "symbol_id": 1,
  "base_asset": {"code": "BTC", "class": "crypto", "decimals": 8},
  "quote_asset": {"code": "USDT", "class": "stablecoin", "decimals": 2},
  "price_decimals": 2,
  "price_tick": "0.01",
  "base_lot": "0.0001",
  "fee_asset_policy": "quote"
}'

echo
echo "Spot engine:      http://$SPOT_ADDR  (EUR/USD)"
echo "Perpetual engine: http://$PERP_ADDR  (BTC/USDT)"
echo "Demo desk:        http://$DEMO_ADDR/demo/"
echo "Logs:             $DATA_DIR/logs"
echo "Press Ctrl-C to stop the demo and both engines."
echo

./demos/order_system/target/debug/matchengine-raft-demo \
    --addr "$DEMO_ADDR" \
    --engine "$SPOT_ADDR" \
    --engine-id 1 \
    --perp-engine "$PERP_ADDR" \
    --perp-engine-id 1 &
DEMO_PID=$!

wait "$DEMO_PID"

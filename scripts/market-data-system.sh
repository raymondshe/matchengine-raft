#!/bin/sh

set -eu

# Run the standalone market-data system against two engines over Aeron IPC.
# Each engine embeds its own media driver and publishes one symbol; the
# service consumes both and serves them from one WebSocket hub, so the test
# page can switch symbols over a single connection.
#
# Only this script's children are stopped on exit.

cd "$(dirname "$0")/.."

# These demos use fewer than the four voters required when WAL fsync is
# disabled; use every-append fsync unless the caller overrides it.
export RAFT_WAL_FSYNC_MS=${RAFT_WAL_FSYNC_MS:-0}

DATA_DIR=${MARKET_DATA_SYSTEM_DATA_DIR:-/tmp/matchengine-marketdata-demo}
DEMO_ADDR=${MARKET_DATA_SYSTEM_ADDR:-127.0.0.1:24800}
EUR_ADDR=${MARKET_DATA_EUR_ADDR:-127.0.0.1:24801}
BTC_ADDR=${MARKET_DATA_BTC_ADDR:-127.0.0.1:24802}
EUR_AERON_DIR="$DATA_DIR/aeron-eurusd"
BTC_AERON_DIR="$DATA_DIR/aeron-btcusdt"

EUR_PID=
BTC_PID=
SERVICE_PID=

cleanup() {
    trap - EXIT INT TERM
    for pid in "$SERVICE_PID" "$EUR_PID" "$BTC_PID"; do
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || true
        fi
    done
    wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

if [ -z "${MARKET_DATA_SYSTEM_DATA_DIR:-}" ]; then
    rm -rf "$DATA_DIR"
fi
mkdir -p "$DATA_DIR" "$EUR_AERON_DIR" "$BTC_AERON_DIR"

# rusteron is feature-gated; the first build downloads precompiled static libs.
cargo build --features market-data-aeron
cargo build --manifest-path demos/market_data_system/Cargo.toml --features aeron

start_engine() {
    id=$1
    addr=$2
    name=$3
    aeron_dir=$4
    echo "Starting $name engine on $addr (Aeron IPC, snapshots every 5s)"
    ./target/debug/matchengine-raft \
        --id "$id" \
        --http-addr "$addr" \
        --data-dir "$DATA_DIR/engine-$name" \
        --instance-prefix "marketdata-$name" \
        --snapshot-interval-ms 0 \
        --market-data-aeron-channel "aeron:ipc" \
        --market-data-aeron-embedded-driver \
        --market-data-aeron-dir "$aeron_dir" \
        --market-data-aeron-snapshot-every-ms 5000 \
        --market-data-status-every 10 \
        >"$DATA_DIR/engine-$name.log" 2>&1 &
    STARTED_PID=$!
}

wait_engine() {
    addr=$1
    name=$2
    i=0
    until curl -sf "http://$addr/health" >/dev/null 2>&1; do
        i=$((i + 1))
        if [ "$i" -ge 100 ]; then
            echo "$name engine did not become healthy; see $DATA_DIR/engine-$name.log" >&2
            exit 1
        fi
        sleep 0.1
    done
    curl -sf -X POST "http://$addr/init" \
        -H 'Content-Type: application/json' -d '[]' >/dev/null
    i=0
    until curl -sf "http://$addr/ready" >/dev/null 2>&1; do
        i=$((i + 1))
        if [ "$i" -ge 100 ]; then
            echo "$name engine did not elect a leader; see $DATA_DIR/engine-$name.log" >&2
            exit 1
        fi
        sleep 0.1
    done
}

configure_symbol() {
    addr=$1
    name=$2
    body=$3
    echo "Configuring $name"
    curl -sf -X POST "http://$addr/write" \
        -H 'Content-Type: application/json' \
        -d "$body" \
        >/dev/null
}

start_engine 1 "$EUR_ADDR" eurusd "$EUR_AERON_DIR"
EUR_PID=$STARTED_PID
start_engine 2 "$BTC_ADDR" btcusdt "$BTC_AERON_DIR"
BTC_PID=$STARTED_PID

wait_engine "$EUR_ADDR" eurusd
wait_engine "$BTC_ADDR" btcusdt

configure_symbol "$EUR_ADDR" "EUR/USD" \
    '{"ConfigureSymbol":{"config":{"symbol_id":0,"base_asset":{"code":"EUR","class":"fiat","decimals":2},"quote_asset":{"code":"USD","class":"fiat","decimals":2},"price_decimals":5,"price_tick":"0.00001","base_lot":"0.01","fee_asset_policy":"received"}}}'
configure_symbol "$BTC_ADDR" "BTC/USDT" \
    '{"ConfigureSymbol":{"config":{"symbol_id":1,"base_asset":{"code":"BTC","class":"crypto","decimals":8},"quote_asset":{"code":"USDT","class":"stablecoin","decimals":2},"price_decimals":2,"price_tick":"0.01","base_lot":"0.00000001","fee_asset_policy":"received"}}}'

echo "Starting market-data system on $DEMO_ADDR"
./demos/market_data_system/target/debug/matchengine-marketdata \
    --addr "$DEMO_ADDR" \
    --aeron-channel "aeron:ipc" \
    --aeron-channel "aeron:ipc" \
    --aeron-dir "$EUR_AERON_DIR" \
    --aeron-dir "$BTC_AERON_DIR" \
    --symbol "eurusd:0:EUR:USD" \
    --symbol "btcusdt:1:BTC:USDT" \
    >"$DATA_DIR/marketdata.log" 2>&1 &
SERVICE_PID=$!

i=0
until curl -sf "http://$DEMO_ADDR/health" >/dev/null 2>&1; do
    i=$((i + 1))
    if [ "$i" -ge 100 ]; then
        echo "market-data system did not start; see $DATA_DIR/marketdata.log" >&2
        exit 1
    fi
    sleep 0.1
done

echo
echo "Test page:          http://$DEMO_ADDR/"
echo "  raw stream:       ws://$DEMO_ADDR/ws/eurusd@bookTicker"
echo "  combined:         ws://$DEMO_ADDR/stream?streams=eurusd@bookTicker/btcusdt@depth@100ms"
echo "  health/ready:     http://$DEMO_ADDR/health, /ready"
echo "Engines:            http://$EUR_ADDR (EUR/USD), http://$BTC_ADDR (BTC/USDT)"
echo "Logs:               $DATA_DIR"
echo "Press Ctrl-C to stop the market-data system and both engines."
echo

# Keep the service in the foreground so Ctrl-C runs the cleanup trap.
wait "$SERVICE_PID"

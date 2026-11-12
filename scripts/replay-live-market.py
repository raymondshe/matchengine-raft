#!/usr/bin/env python3
"""Keep the demo market-data feeds alive with a two-symbol book.

The market-data test page at http://127.0.0.1:24800/ shows live depth, BBO,
trades and bars. This script seeds a ladder on each side once, then sends
several marketable IOC orders per interval that walk up to a few levels. Sweep
depth, size and side pressure vary, so the 1s/1m klines get real OHLC movement
instead of printing the same two prices; touched levels are replenished and
nothing is cancelled, so the shadow book stays clean.

Usage:
    ./scripts/replay-live-market.py
    ./scripts/replay-live-market.py --interval 0.5
    ./scripts/replay-live-market.py --iterations 20

The script only uses Python's standard library. It expects the two engines from
`./scripts/market-data-system.sh` on ports 24801 (EUR/USD) and 24802 (BTC/USDT).
"""

import argparse
import json
import math
import random
import time
import urllib.error
import urllib.request

RNG = random.Random()

EUR = {
    "port": 24801,
    "symbol_id": 0,
    "decimals": 5,
    "volume_decimals": 2,
    "bids": [f"{1.07990 - i * 0.00010:.5f}" for i in range(10)],
    "asks": [f"{1.08010 + i * 0.00010:.5f}" for i in range(10)],
    "maker_volume": 20.0,
    "refill_below": 4.0,
    "refill_volume": 20.0,
    "sweep_levels": 5,
    "trades_per_interval": 3,
}
BTC = {
    "port": 24802,
    "symbol_id": 1,
    "decimals": 2,
    "volume_decimals": 8,
    "bids": [f"{59990.00 - i * 10:.2f}" for i in range(10)],
    "asks": [f"{60010.00 + i * 10:.2f}" for i in range(10)],
    "maker_volume": 2.0,
    "refill_below": 0.4,
    "refill_volume": 2.0,
    "sweep_levels": 5,
    "trades_per_interval": 3,
}


def http_json(port, path, body, timeout=5.0):
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.load(response)


def read_depth(spec):
    response = http_json(spec["port"], "/read", "orderbook_depth")
    payload = response.get("Ok")
    if isinstance(payload, str):
        return json.loads(payload)
    return payload if isinstance(payload, dict) else {"bids": [], "asks": []}


def place(spec, side, price, volume, order_id, tif="GTC"):
    body = {
        "Place": {
            "order": {
                "live": "Limit",
                "side": side,
                "symbol_id": spec["symbol_id"],
                "price": price,
                "volume": volume,
                "id": order_id,
                "sequence": 0,
                "tif": tif,
            }
        }
    }
    return http_json(spec["port"], "/write", body)


def level_volume(levels, price):
    target = float(price)
    return sum(
        float(level["volume"])
        for level in levels
        if abs(float(level["price"]) - target) < 1e-9
    )


def ensure_book(spec, next_id):
    depth = read_depth(spec)
    for side, prices, levels in (
        ("Buy", spec["bids"], depth.get("bids") or []),
        ("Sell", spec["asks"], depth.get("asks") or []),
    ):
        present = {round(float(level["price"]), 8) for level in levels}
        for price in prices:
            if round(float(price), 8) not in present:
                place(spec, side, price, spec["maker_volume"], next_id())


def replenish(spec, side, price, next_id):
    depth = read_depth(spec)
    levels = depth["bids"] if side == "Buy" else depth["asks"]
    if level_volume(levels, price) < spec["refill_below"]:
        place(spec, side, price, spec["refill_volume"], next_id())


def walk_volume(levels, spec):
    """Volume that consumes the first levels and part of the last one."""
    volume = sum(float(level["volume"]) for level in levels[:-1])
    volume += float(levels[-1]["volume"]) * RNG.uniform(0.15, 0.95)
    return round(max(volume, 0.01), spec["volume_decimals"])


def replay_symbol(spec, next_id, bias):
    ensure_book(spec, next_id)

    for _ in range(spec["trades_per_interval"]):
        depth = read_depth(spec)
        bids = depth.get("bids") or []
        asks = depth.get("asks") or []
        if not bids or not asks:
            ensure_book(spec, next_id)
            depth = read_depth(spec)
            bids = depth.get("bids") or []
            asks = depth.get("asks") or []
            if not bids or not asks:
                continue

        # Side pressure drifts with a slow wave, so the tape alternates
        # between buy-dominated and sell-dominated runs.
        buy = RNG.random() < 0.5 + bias
        side = "Buy" if buy else "Sell"
        levels = asks if buy else bids
        max_levels = min(len(levels), spec["sweep_levels"])
        weights = [max_levels - index for index in range(max_levels)]
        count = RNG.choices(range(1, max_levels + 1), weights=weights, k=1)[0]
        walked = levels[:count]
        # The IOC limit sits at the deepest touched level so the order walks
        # the book; the maker levels supply the trade prices.
        place(
            spec,
            side,
            walked[-1]["price"],
            walk_volume(walked, spec),
            next_id(),
            tif="IOC",
        )
        maker_side = "Sell" if buy else "Buy"
        for level in walked:
            replenish(spec, maker_side, level["price"], next_id)

    depth = read_depth(spec)
    bid = depth["bids"][0]["price"] if depth.get("bids") else "—"
    ask = depth["asks"][0]["price"] if depth.get("asks") else "—"
    return bid, ask


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--interval", type=float, default=1.0)
    parser.add_argument(
        "--iterations",
        type=int,
        default=0,
        help="stop after N iterations; 0 runs until interrupted",
    )
    parser.add_argument("--skip-eur", action="store_true")
    parser.add_argument("--skip-btc", action="store_true")
    parser.add_argument("--seed", type=int, default=7)
    args = parser.parse_args()

    RNG.seed(args.seed)
    counter = [1000]

    def next_id():
        value = counter[0]
        counter[0] += 1
        return value

    iteration = 0
    while args.iterations == 0 or iteration < args.iterations:
        iteration += 1
        started = time.monotonic()
        # Slow side-pressure wave: buy runs followed by sell runs.
        bias = math.sin(iteration / 8.0) * 0.25
        try:
            parts = []
            if not args.skip_eur:
                eur_bid, eur_ask = replay_symbol(EUR, next_id, bias)
                parts.append(f"EURUSD {eur_bid}/{eur_ask}")
            if not args.skip_btc:
                btc_bid, btc_ask = replay_symbol(BTC, next_id, bias)
                parts.append(f"BTCUSDT {btc_bid}/{btc_ask}")
            print(f"[{iteration}] " + "  ".join(parts), flush=True)
        except (OSError, urllib.error.URLError, KeyError, ValueError) as error:
            print(f"[{iteration}] replay error: {error}", flush=True)
        elapsed = time.monotonic() - started
        time.sleep(max(args.interval - elapsed, 0.05))


if __name__ == "__main__":
    main()

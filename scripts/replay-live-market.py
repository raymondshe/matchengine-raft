#!/usr/bin/env python3
"""Keep the demo market-data feeds alive with a fixed two-symbol book.

The market-data test page at http://127.0.0.1:24800/ shows live depth, BBO,
trades and bars. This script seeds a small ladder on each side once, then sends
one IOC buy and one IOC sell through the spread every interval and replenishes
the best levels when they run low. Nothing is cancelled, so the shadow book has
no stale levels and the page's book stays clean.

Usage:
    ./scripts/replay-live-market.py
    ./scripts/replay-live-market.py --interval 0.5
    ./scripts/replay-live-market.py --iterations 20

The script only uses Python's standard library. It expects the two engines from
`./scripts/market-data-system.sh` on ports 24801 (EUR/USD) and 24802 (BTC/USDT).
"""

import argparse
import json
import time
import urllib.error
import urllib.request

EUR = {
    "port": 24801,
    "symbol_id": 0,
    "decimals": 5,
    "bids": [f"{1.07990 - i * 0.00010:.5f}" for i in range(10)],
    "asks": [f"{1.08010 + i * 0.00010:.5f}" for i in range(10)],
    "maker_volume": 100.0,
    "taker_volume": 0.1,
    "refill_below": 2.0,
    "refill_volume": 20.0,
}
BTC = {
    "port": 24802,
    "symbol_id": 1,
    "decimals": 2,
    "bids": [f"{59990.00 - i * 10:.2f}" for i in range(10)],
    "asks": [f"{60010.00 + i * 10:.2f}" for i in range(10)],
    "maker_volume": 10.0,
    "taker_volume": 0.01,
    "refill_below": 0.2,
    "refill_volume": 2.0,
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
    if depth.get("bids") or depth.get("asks"):
        return
    for side, prices in (("Buy", spec["bids"]), ("Sell", spec["asks"])):
        for price in prices:
            place(spec, side, price, spec["maker_volume"], next_id())


def replenish(spec, side, price, next_id):
    depth = read_depth(spec)
    levels = depth["bids"] if side == "Buy" else depth["asks"]
    if level_volume(levels, price) < spec["refill_below"]:
        place(spec, side, price, spec["refill_volume"], next_id())


def replay_symbol(spec, next_id):
    ensure_book(spec, next_id)
    bid = spec["bids"][0]
    ask = spec["asks"][0]

    # Cross the fixed best levels; IOC orders leave no resting remainder.
    place(spec, "Buy", ask, spec["taker_volume"], next_id(), tif="IOC")
    place(spec, "Sell", bid, spec["taker_volume"], next_id(), tif="IOC")

    replenish(spec, "Buy", bid, next_id)
    replenish(spec, "Sell", ask, next_id)
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
    args = parser.parse_args()

    counter = [1000]

    def next_id():
        value = counter[0]
        counter[0] += 1
        return value

    iteration = 0
    while args.iterations == 0 or iteration < args.iterations:
        iteration += 1
        try:
            parts = []
            if not args.skip_eur:
                eur_bid, eur_ask = replay_symbol(EUR, next_id)
                parts.append(f"EURUSD {eur_bid}/{eur_ask}")
            if not args.skip_btc:
                btc_bid, btc_ask = replay_symbol(BTC, next_id)
                parts.append(f"BTCUSDT {btc_bid}/{btc_ask}")
            print(f"[{iteration}] " + "  ".join(parts), flush=True)
        except (OSError, urllib.error.URLError, KeyError, ValueError) as error:
            print(f"[{iteration}] replay error: {error}", flush=True)
        time.sleep(max(args.interval, 0.05))


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Replay the checked-in tests/replay cases into a running engine.

The replay fixtures assume a fresh engine sequence and a fresh book. This
driver clears the demo book at the start of each pass, keeps every case in one
price band, and adds the engine's current sequence to the Cancel/Amend keys so
the cases still address the orders they created. It is a demo driver, not a
replacement for `cargo test` or the replay comparison examples.

Run it after `./scripts/market-data-system.sh`; the WebSocket page will show
the resulting depth, trades, stop/iceberg/post-only lifecycles and bars.

Usage:
    ./scripts/replay-test-cases.py
    ./scripts/replay-test-cases.py --port 24801 --delay 0.03
"""

import argparse
import json
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CASES = [
    "limit_orders",
    "ioc_orders",
    "fok_orders",
    "market_orders",
    "post_only_orders",
    "stop_market_orders",
    "stop_limit_orders",
    "iceberg_orders",
    "gtd_orders",
    "day_orders",
    "amend_orders",
    "cancel_orders",
    "fee_orders",
    "fee_quote_orders",
]
# Keep every case near the same price so the demo book stays coherent. The
# quote-fee fixture is written for a ~400 USD share price.
PRICE_OFFSETS = {"fee_quote_orders": -300.0}


def http_json(port, path, body, timeout=5.0):
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.load(response)


def read_list(port, key):
    response = http_json(port, "/read", key)
    payload = response.get("Ok")
    if isinstance(payload, str):
        try:
            payload = json.loads(payload)
        except json.JSONDecodeError:
            return []
    return payload if isinstance(payload, list) else []


def orderbook_sequence(port):
    response = http_json(port, "/read", "orderbook_sequence")
    return int(response.get("Ok"))


def cancel_all(port):
    """Best-effort clear of the demo book before replaying one case."""
    for key in ("orderbook_orders", "orderbook_stops"):
        for order in read_list(port, key):
            try:
                http_json(port, "/write", {"Cancel": {"order": order}})
            except (OSError, urllib.error.URLError, KeyError, ValueError):
                pass


def adjust_price(value, offset):
    if isinstance(value, str):
        return f"{float(value) + offset:.5f}"
    if isinstance(value, (int, float)) and value > 0:
        return round(float(value) + offset, 5)
    return value


def place(port, side, price, volume, order_id):
    body = {
        "Place": {
            "order": {
                "live": "Limit",
                "side": side,
                "symbol_id": 0,
                "price": f"{price:.5f}",
                "volume": volume,
                "id": order_id,
                "sequence": 0,
            }
        }
    }
    return http_json(port, "/write", body)


def seed_book(port):
    """Leave a visible two-sided top of book between passes."""
    response = http_json(port, "/read", "orderbook_depth:5")
    payload = response.get("Ok")
    depth = json.loads(payload) if isinstance(payload, str) else (payload or {})
    best_bid = float(depth["bids"][0]["price"]) if depth.get("bids") else None
    best_ask = float(depth["asks"][0]["price"]) if depth.get("asks") else None
    # Skip a side that would cross the current book.
    if best_ask is None or best_ask > 99.5:
        place(port, "Buy", 99.5, 1, 0)
    if best_bid is None or best_bid < 100.5:
        place(port, "Sell", 100.5, 1, 0)


def adjust_request(request, sequence_offset, price_offset):
    """Offset this case's prices and sequence keys into the running engine."""
    key = next(iter(request))
    if key not in {"Place", "Cancel", "Amend"}:
        return request

    payload = request[key]
    order = payload["order"]
    order["symbol_id"] = 0
    order["price"] = adjust_price(order.get("price"), price_offset)
    if "stop_price" in order:
        order["stop_price"] = adjust_price(order["stop_price"], price_offset)
    if key in {"Cancel", "Amend"}:
        order["sequence"] = int(order.get("sequence", 0)) + sequence_offset
    if key == "Amend" and "price" in payload:
        payload["price"] = adjust_price(payload["price"], price_offset)
    return request


def replay_case(port, case, delay):
    path = ROOT / "tests" / "replay" / case / "orders.jsonl"
    lines = [line.strip() for line in path.read_text().splitlines()]
    lines = [line for line in lines if line and not line.startswith("#")]
    price_offset = PRICE_OFFSETS.get(case, 0.0)
    sequence_offset = orderbook_sequence(port)
    sent = 0

    for line in lines:
        request = json.loads(line)
        request = adjust_request(request, sequence_offset, price_offset)
        try:
            response = http_json(port, "/write", request)
            if "Err" in response:
                print(f"  {case}: {response['Err']}", flush=True)
        except urllib.error.HTTPError as error:
            print(f"  {case}: HTTP {error.code}", flush=True)
        except (OSError, urllib.error.URLError) as error:
            print(f"  {case}: {error}", flush=True)
        sent += 1
        if delay > 0:
            time.sleep(delay)

    return sent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=24801)
    parser.add_argument("--delay", type=float, default=0.03)
    args = parser.parse_args()

    # Start each pass from a clean book so leftovers cannot grow without
    # bound, but keep the book populated while the pass runs.
    cancel_all(args.port)
    for index, case in enumerate(CASES):
        print(f"[{index + 1}/{len(CASES)}] replaying {case}", flush=True)
        count = replay_case(args.port, case, max(args.delay, 0.0))
        print(f"  {case}: {count} requests", flush=True)
        time.sleep(0.3)

    seed_book(args.port)
    print("all test cases replayed", flush=True)


if __name__ == "__main__":
    main()

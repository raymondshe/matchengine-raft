#!/usr/bin/env python3
"""Replay the checked-in match_engine/tests/replay cases through the demo order system.

The sibling `replay-test-cases.py` posts engine requests straight to `/write`.
This driver posts **order tickets** to the demo order desk
(`POST /demo/api/orders`, `/orders/cancel`, `/orders/amend`) exposed by the
control-plane Ingress, so the replay exercises the BFF, the user-ID shard
routing, leader forwarding and the matching engine together.

Each replay case runs under one demo user, and the users are spread across the
`OrderSystem` shards, so every order-system pod receives traffic. Prices are
scaled from the fixtures' ~100 band into the target's band; the fixtures assume
a fresh engine, so Cancel/Amend keys are offset by the live book sequence.

Usage:
    ./scripts/replay-order-system.py --dry-run
    ./scripts/replay-order-system.py --target eurusd --price-scale 0.012
    ./scripts/replay-order-system.py --hosts http://127.0.0.1:24300 --users 1

Live OrbStack demo (four ingress hosts, one per user shard):
    ./scripts/replay-order-system.py \
      --hosts http://users-0.orders.match-eu-1.example \
              http://users-1.orders.match-eu-1.example \
              http://users-2.orders.match-eu-1.example \
              http://users-3.orders.match-eu-1.example \
      --users 1,300,600,900 --target eurusd --price-scale 0.012

Cases that need their own `ConfigureSymbol` (the quote-fee and fee-overflow
fixtures) are skipped: the live symbol is locked. `PurgeExpired` has no desk
endpoint and is skipped too; a GTD order past its deadline is rejected or
expires on intake instead.
"""

import argparse
import json
import sys
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
]
SKIP_CASES = {
    "fee_quote_orders": "needs a quote-fee ETF symbol config",
    "fee_overflow_orders": "needs a quote-fee ETF symbol config",
    "fee_overflow_base_orders": "overflow fixture, not a live-tradable book",
}
ACTION_HEADER = "x-demo-action"


def http_json(base, method, path, body=None, timeout=15.0):
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(
        f"{base.rstrip('/')}{path}",
        data=data,
        headers={"Content-Type": "application/json", ACTION_HEADER: "replay"},
        method=method,
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        raw = error.read()
        try:
            return error.code, json.loads(raw or b"{}")
        except json.JSONDecodeError:
            return error.code, {"error": {"kind": "http", "message": raw.decode(errors="replace")}}
    except (OSError, urllib.error.URLError) as error:
        return 0, {"error": {"kind": "transport", "message": str(error)}}


def state(base, target):
    _, body = http_json(base, "GET", f"/demo/api/state?target={target}&depth=5")
    return body


def book_sequence(base, target):
    return int(state(base, target).get("book", {}).get("sequence", 0))


def scale_price(value, scale, offset, decimals):
    return f"{float(value) * scale + offset:.{decimals}f}"


def volume_str(value):
    if isinstance(value, str):
        return value
    text = f"{float(value):.8f}".rstrip("0").rstrip(".")
    return text or "0"


def base_ticket(order, user_id, target):
    ticket = {"side": order["side"], "user_id": user_id, "target": target}
    ticket["type"] = order.get("live", "Limit")
    tif = order.get("tif", "")
    if isinstance(tif, dict):
        ((name, expiry),) = tif.items()
        ticket["tif"] = name.upper()
        if name.upper() == "GTD":
            ticket["expiry_ms"] = int(expiry)
    elif tif:
        ticket["tif"] = str(tif).upper()
    if order.get("volume_unit"):
        ticket["volume_unit"] = order["volume_unit"]
    if order.get("post_only"):
        ticket["post_only"] = True
    for field in ("maker_fee_rate", "taker_fee_rate"):
        if order.get(field) not in (None, "", "0", 0):
            ticket[field] = str(order[field])
    return ticket


def place_ticket(order, user_id, target, options, run_id, case, index):
    ticket = base_ticket(order, user_id, target)
    if ticket["type"] != "Market":
        ticket["price"] = scale_price(
            order.get("price", 0), options.price_scale, options.price_offset,
            options.price_decimals,
        )
    ticket["volume"] = volume_str(order["volume"])
    if float(order.get("stop_price") or 0) > 0:
        ticket["stop_price"] = scale_price(
            order["stop_price"], options.price_scale, options.price_offset,
            options.price_decimals,
        )
    if float(order.get("display_volume") or 0) > 0:
        ticket["display_volume"] = volume_str(order["display_volume"])
    ticket["client_order_id"] = f"replay-{run_id}-{case}-{index}"
    return ticket


def cancel_ticket(order, user_id, target, sequence, options):
    return {
        "user_id": user_id,
        "target": target,
        "side": order["side"],
        "price": scale_price(
            order.get("price", 0), options.price_scale, options.price_offset,
            options.price_decimals,
        ),
        "sequence": sequence,
    }


def amend_ticket(payload, user_id, target, sequence, options):
    order = payload["order"]
    ticket = cancel_ticket(order, user_id, target, sequence, options)
    if payload.get("price") is not None:
        ticket["new_price"] = scale_price(
            payload["price"], options.price_scale, options.price_offset,
            options.price_decimals,
        )
    if payload.get("volume") is not None:
        ticket["new_volume"] = volume_str(payload["volume"])
    return ticket


def owners_by_host(base, target, hosts, users):
    """Map each open order/stop to the shard host that owns its user."""
    items = []
    for key in ("orders", "stops"):
        for entry in state(base, target).get(key, {}).get("items", []):
            order = entry["order"]
            items.append(order)
    routed = []
    for order in items:
        owner = order.get("owner_id")
        if owner is None:
            continue
        host = hosts[users.index(owner)] if owner in users else hosts[0]
        routed.append((host, order))
    return routed


def cancel_all(base, target, hosts, users, options):
    cancelled = 0
    for host, order in owners_by_host(base, target, hosts, users):
        ticket = cancel_ticket(order, order["owner_id"], target, order["sequence"], options)
        status, body = http_json(host, "POST", "/demo/api/orders/cancel", ticket)
        if status == 200 and not body.get("error"):
            cancelled += 1
        else:
            print(f"  cleanup: {order['sequence']} -> {describe(status, body)}", flush=True)
    return cancelled


def describe(status, body):
    if status == 0 or status >= 400:
        error = body.get("error") or {}
        return f"HTTP {status} {error.get('kind', '')}: {error.get('message', '')}".strip()
    events = body.get("events") or []
    rejected = [event for event in events if event.get("kind") == "rejected"]
    filled = [event for event in events if event.get("kind") == "fill"]
    if rejected:
        return f"rejected {rejected[0].get('reason', '')}"
    if filled:
        return f"filled {len(filled)}"
    return "ok"


def host_for(user_id, hosts, users):
    return hosts[users.index(user_id)] if user_id in users else hosts[0]


def replay_case(base, case, user_id, hosts, users, options, run_id, stats):
    path = ROOT / "match_engine" / "tests" / "replay" / case / "orders.jsonl"
    lines = [line.strip() for line in path.read_text().splitlines()]
    lines = [line for line in lines if line and not line.startswith("#")]
    offset = book_sequence(base, options.target)
    host = host_for(user_id, hosts, users)
    sent = 0

    for index, line in enumerate(lines):
        request = json.loads(line)
        key = next(iter(request))
        payload = request[key]

        if key == "Place":
            ticket = place_ticket(
                payload["order"], user_id, options.target, options, run_id, case, index
            )
            path_name = "/demo/api/orders"
        elif key == "Cancel":
            ticket = cancel_ticket(
                payload["order"], user_id, options.target,
                int(payload["order"].get("sequence", 0)) + offset, options,
            )
            path_name = "/demo/api/orders/cancel"
        elif key == "Amend":
            ticket = amend_ticket(
                payload, user_id, options.target,
                int(payload["order"].get("sequence", 0)) + offset, options,
            )
            path_name = "/demo/api/orders/amend"
        else:
            stats["skipped"] += 1
            print(f"  {case}: skip {key} (no desk endpoint)", flush=True)
            continue

        if options.dry_run:
            print(f"  {case}: {key} {json.dumps(ticket)}", flush=True)
            sent += 1
            continue

        status, body = http_json(host, "POST", path_name, ticket)
        detail = describe(status, body)
        if status != 200 or body.get("error"):
            stats["http_errors"] += 1
        for event in body.get("events") or []:
            if event.get("kind") == "rejected":
                stats["rejections"] += 1
            elif event.get("kind") == "fill":
                stats["fills"] += 1
        if detail not in ("ok",) and not detail.startswith("filled"):
            print(f"  {case}: {key} -> {detail}", flush=True)
        stats["per_user"][user_id] = stats["per_user"].get(user_id, 0) + 1
        sent += 1
        if options.delay > 0:
            time.sleep(options.delay)

    return sent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--hosts",
        nargs="+",
        default=[
            "http://users-0.orders.match-eu-1.example",
            "http://users-1.orders.match-eu-1.example",
            "http://users-2.orders.match-eu-1.example",
            "http://users-3.orders.match-eu-1.example",
        ],
        help="one order-system base URL per user shard (default: the lab ingress)",
    )
    parser.add_argument(
        "--users",
        default="1,300,600,900",
        help="comma-separated demo user per host; must match the shard range",
    )
    parser.add_argument("--target", default="eurusd", help="order-system target id")
    parser.add_argument("--price-scale", type=float, default=0.012)
    parser.add_argument("--price-offset", type=float, default=0.0)
    parser.add_argument("--price-decimals", type=int, default=5)
    parser.add_argument("--delay", type=float, default=0.03)
    parser.add_argument("--dry-run", action="store_true", help="print tickets only")
    parser.add_argument(
        "--keep-book",
        action="store_true",
        help="skip the pre-pass cleanup of open orders and stops",
    )
    args = parser.parse_args()

    users = [int(value) for value in args.users.split(",") if value.strip()]
    if len(users) != len(args.hosts):
        parser.error("--hosts and --users must have the same length")

    base = args.hosts[0]
    snapshot = state(base, args.target)
    targets = [entry["id"] for entry in snapshot.get("backends", {}).get("targets", [])]
    if args.target not in targets:
        sys.exit(f"target {args.target} is not served; available: {', '.join(targets)}")
    symbol = snapshot.get("symbol", {}).get("config", {})
    print(
        f"target {args.target} symbol_id={symbol.get('symbol_id')} "
        f"tick={symbol.get('price_tick')} lot={symbol.get('base_lot')} "
        f"policy={symbol.get('fee_asset_policy')}",
        flush=True,
    )

    run_id = time.strftime("%Y%m%d-%H%M%S")
    stats = {"fills": 0, "rejections": 0, "http_errors": 0, "skipped": 0, "per_user": {}}

    if not args.keep_book and not args.dry_run:
        cancelled = cancel_all(base, args.target, args.hosts, users, args)
        print(f"cleared {cancelled} open order(s)/stop(s)", flush=True)

    total = 0
    for index, case in enumerate(CASES):
        user_id = users[index % len(users)]
        print(f"[{index + 1}/{len(CASES)}] {case} as user {user_id}", flush=True)
        total += replay_case(
            base, case, user_id, args.hosts, users, args, run_id, stats
        )
        if not args.dry_run:
            time.sleep(0.2)

    for case, reason in SKIP_CASES.items():
        print(f"skipped {case}: {reason}", flush=True)

    print(f"\nrun {run_id}: {total} tickets", flush=True)
    print(
        f"fills={stats['fills']} rejections={stats['rejections']} "
        f"http_errors={stats['http_errors']} skipped_lines={stats['skipped']}",
        flush=True,
    )
    for user_id, count in sorted(stats["per_user"].items()):
        print(f"  user {user_id}: {count}", flush=True)

    if not args.dry_run:
        final = state(base, args.target)
        book = final.get("book", {})
        depth = book.get("depth", {})
        best_bid = (depth.get("bids") or [{}])[0].get("price")
        best_ask = (depth.get("asks") or [{}])[0].get("price")
        print(
            f"book sequence={book.get('sequence')} "
            f"best_bid={best_bid} best_ask={best_ask} "
            f"open_orders={final.get('orders', {}).get('total')} "
            f"stops={final.get('stops', {}).get('total')}",
            flush=True,
        )


if __name__ == "__main__":
    main()

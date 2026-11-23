#!/usr/bin/env python3
"""Compare 30m klines with the rollup of their traded 5m children.

Reads the Binance-shaped GET /klines through the terminal's same-origin proxy
and asserts that every traded 30m window equals the OHLCV/n rollup of the 5m
bars in that window. Empty 5m windows are ignored (sparse storage rule).

Usage:
  TERMINAL_URL=http://terminal.match-eu-1.example \
    SYMBOL=gbpusd ./scripts/check-30m-rollup.py
"""
import json
import os
import urllib.request
from decimal import Decimal

BASE = os.environ.get("TERMINAL_URL", "http://terminal.match-eu-1.example").rstrip("/")
SYMBOL = os.environ.get("SYMBOL", "gbpusd")
WINDOW_MS = 30 * 60_000


def klines(interval, limit):
    url = f"{BASE}/md/klines?symbol={SYMBOL}&interval={interval}&limit={limit}"
    with urllib.request.urlopen(url, timeout=30) as response:
        return json.load(response)


def main():
    fives = klines("5m", 500)
    thirties = klines("30m", 100)
    by_window = {}
    for bar in fives:
        if int(bar[8]) == 0:
            continue
        by_window.setdefault(int(bar[0]) // WINDOW_MS * WINDOW_MS, []).append(bar)

    got = {int(bar[0]): bar for bar in thirties}
    mismatches = []
    for window, children in sorted(by_window.items()):
        if window not in got:
            mismatches.append((window, "missing 30m bar"))
            continue
        bar = got[window]
        expected = [
            children[0][1],
            max(c[2] for c in children),
            min(c[3] for c in children),
            children[-1][4],
            str(sum(Decimal(c[5]) for c in children)),
            str(sum(Decimal(c[7]) for c in children)),
            sum(int(c[8]) for c in children),
        ]
        actual = [bar[1], bar[2], bar[3], bar[4], bar[5], bar[7], int(bar[8])]
        if expected != actual:
            mismatches.append((window, expected, actual))

    print(f"symbol={SYMBOL} 5m traded={sum(1 for b in fives if int(b[8]) > 0)} "
          f"30m traded={sum(1 for b in thirties if int(b[8]) > 0)} "
          f"windows_compared={len(by_window)} mismatches={len(mismatches)}")
    for mismatch in mismatches[:10]:
        print(mismatch)
    if mismatches:
        raise SystemExit(1)


if __name__ == "__main__":
    main()

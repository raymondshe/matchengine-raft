#!/usr/bin/env python3
"""Seed the lab TDengine with dense demo bars so the terminal has history.

Lab only. DROPS and recreates the configured database. Point TDENGINE_URL at a
port-forward of the in-cluster taosAdapter, for example:

  kubectl -n match-eu-1 port-forward svc/tdengine 16042:6041 &
  TDENGINE_URL=http://127.0.0.1:16042 ./scripts/seed-history-lab.py
"""
import base64
import http.client
import json
import os
import random
import time
from urllib.parse import urlparse

_URL = urlparse(os.environ.get("TDENGINE_URL", "http://127.0.0.1:16042"))
URL = _URL.hostname or "127.0.0.1"
PORT = _URL.port or 80
DB = os.environ.get("TDENGINE_DATABASE", "market_bars")
AUTH = "Basic " + base64.b64encode(
    f"{os.environ.get('TDENGINE_USER', 'root')}:{os.environ.get('TDENGINE_PASSWORD', 'taosdata')}".encode()
).decode()
SYMBOLS = os.environ.get(
    "SEED_SYMBOLS", "gbpusd,audusd,usdjpy,eurgbp,nzdusd,btcusdt,eurchf"
).split(",")
SCALE = 10**18

COLS = (
    "ts TIMESTAMP, open DECIMAL(38,18), high DECIMAL(38,18), low DECIMAL(38,18), "
    "close DECIMAL(38,18), volume DECIMAL(38,18), quote_volume DECIMAL(38,18), "
    "taker_base DECIMAL(38,18), taker_quote DECIMAL(38,18), trades BIGINT UNSIGNED, "
    "last_ts TIMESTAMP, first_log BIGINT, first_ord INT, last_log BIGINT, last_ord INT, "
    "rev INT, source TINYINT, sealed BOOL, seq BIGINT"
)


def sql(stmt, db=None, retries=3):
    for attempt in range(retries):
        try:
            c = http.client.HTTPConnection(URL, PORT, timeout=60)
            path = "/rest/sql" + ("/" + db if db else "")
            c.request("POST", path, stmt, {"Authorization": AUTH})
            body = json.loads(c.getresponse().read())
            c.close()
            if body.get("code") != 0:
                raise RuntimeError(f"{body} for {stmt[:120]}")
            return body
        except Exception:
            if attempt == retries - 1:
                raise
            time.sleep(1)


def fmt(units):
    neg = units < 0
    whole, frac = divmod(abs(units), SCALE)
    return ("-" if neg else "") + f"{whole}.{frac:018d}"


def row(ts_us, o, h, l, c, vol, last_us, log_id, interval_us):
    quote = vol * o // SCALE
    return (
        f"({ts_us},{fmt(o)},{fmt(h)},{fmt(l)},{fmt(c)},{fmt(vol)},{fmt(quote)},"
        f"{fmt(vol)},{fmt(quote)},3,{last_us},{log_id},0,{log_id},2,"
        f"0,1,true,0)"
    )


def main():
    sql("DROP DATABASE IF EXISTS market_bars")
    sql(f"CREATE DATABASE market_bars PRECISION 'us' KEEP 3650 DURATION 10d VGROUPS 1 REPLICA 1 CACHEMODEL 'last_row'")
    for interval in ("1s", "1m"):
        sql(f"CREATE STABLE bars_{interval} ({COLS}) TAGS (symbol VARCHAR(32))", DB)
    for symbol in SYMBOLS:
        for interval in ("1s", "1m"):
            sql(f"CREATE TABLE bars_{interval}_{symbol} USING bars_{interval} TAGS ('{symbol}')", DB)

    now_s = int(time.time())
    for symbol in SYMBOLS:
        rng = random.Random(sum(ord(ch) for ch in symbol))
        px = 1_100_000_000_000_000_000 + rng.randrange(0, 10**15)
        for interval, step_s, count in (("1s", 1, 500), ("1m", 60, 500)):
            end_s = (now_s // step_s) * step_s
            rows = []
            for i in range(count):
                ts_s = end_s - (count - i) * step_s
                o = px
                hi = o
                lo = o
                for _ in range(3):
                    px += rng.randrange(-2_000_000_000_000, 2_000_000_000_000)
                    hi = max(hi, px)
                    lo = min(lo, px)
                vol = rng.randrange(50, 600) * 10**15
                rows.append(row(ts_s * 1_000_000, o, hi, lo, px, vol,
                                ts_s * 1_000_000 + (step_s - 1) * 1_000_000,
                                i + 1, step_s * 1_000_000))
            for i in range(0, len(rows), 200):
                sql(f"INSERT INTO bars_{interval}_{symbol} VALUES " + " ".join(rows[i:i+200]), DB)
        print("seeded", symbol, flush=True)
    print("done")


if __name__ == "__main__":
    main()

#!/usr/bin/env bash
# Run the market-data fan-out/slow-consumer soak described in
# docs/MARKET_DATA_SOAK_DESIGN.md.
#
#   MODE=local  scripts/market-data-soak.sh
#   MODE=remote LABEL=lab-1 scripts/market-data-soak.sh
#
# Mode "local" starts the reference service in-process and uses a synthetic
# channel source. Mode "remote" drives the deployed global terminal service
# through its ingress. Each phase writes JSONL samples under dist/soak/<label>/.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

MODE="${MODE:-local}"
LABEL="${LABEL:-$(date -u +%Y%m%dT%H%M%SZ)}"
DURATION="${DURATION:-60}"
CLIENTS="${CLIENTS:-64}"
STREAMS="${STREAMS:-32}"
RATE="${RATE:-5000}"
PHASES="${PHASES:-steady churn storm slow reconnect}"
WS_URL="${WS_URL:-ws://terminal.match-eu-1.example/md/ws}"
METRICS_URL="${METRICS_URL:-http://terminal.match-eu-1.example/md/metrics}"
# cargo executes benches with the crate directory as cwd, so pass absolute paths.
OUT_DIR="$ROOT/dist/soak/$LABEL"
mkdir -p "$OUT_DIR"

echo "mode=$MODE label=$LABEL clients=$CLIENTS streams=$STREAMS duration=${DURATION}s"
echo "output: $OUT_DIR"

EXTRA_URLS=""
if [ "$MODE" = "remote" ]; then
  bash scripts/check-observability.sh >"$OUT_DIR/observability.before.txt" 2>&1 \
    || { echo "FAIL: observability before soak"; tail -20 "$OUT_DIR/observability.before.txt"; exit 1; }
  # Engine node snapshots through the manager console proxy: apply/raft state
  # and the market-data tap counters are sampled alongside service metrics so
  # the soak can show that fan-out load does not disturb the engine path.
  python3 - <<'PY' >"$OUT_DIR/engine-urls.txt"
import json,urllib.request
base="http://console.match-eu-1.example"
print(base+"/api/v1/health")
items=json.load(urllib.request.urlopen(base+"/api/v1/shards"))["items"]
for item in items:
    detail=json.load(urllib.request.urlopen(base+"/api/v1/shards/"+item["name"]))
    for console in detail.get("engineConsoles",[]):
        proxy=console["proxyUrl"].rstrip("/")
        print(base+proxy+"/api/node")
PY
  EXTRA_URLS="$(paste -sd, "$OUT_DIR/engine-urls.txt")"
fi

run_phase() {
  local phase="$1" clients="$2" out="${3:-$OUT_DIR/$phase.jsonl}"
  local args=(
    --mode "$MODE"
    --phase "$phase"
    --clients "$clients"
    --streams "$STREAMS"
    --duration "$DURATION"
    --rate "$RATE"
    --label "$LABEL"
    --out "$out"
  )
  if [ "$MODE" = "remote" ]; then
    args+=(--ws-url "$WS_URL" --metrics-url "$METRICS_URL" --extra-urls "$EXTRA_URLS")
  fi
  echo "=== phase $phase ($clients clients) ==="
  cargo bench --manifest-path market_data_system/Cargo.toml --bench ws_soak -- "${args[@]}" \
    | tee "${out%.jsonl}.log"
}

# P0: no WebSocket clients, just service/engine metrics.
run_phase steady 0 "$OUT_DIR/baseline.jsonl"

failed=0
for phase in $PHASES; do
  run_phase "$phase" "$CLIENTS" || { echo "FAIL: $phase driver"; failed=1; continue; }
  python3 - "$OUT_DIR/$phase.jsonl" "$phase" <<'PY' || failed=1
import json,sys
path, phase = sys.argv[1], sys.argv[2]
lines = [json.loads(l) for l in open(path) if l.strip()]
summary = [l for l in lines if "summary" in l]
if not summary:
    print(f"FAIL {phase}: no summary"); raise SystemExit(1)
s = summary[-1]["summary"]
print(f"  {phase}: frames={s['frames']} gaps={s['sequence_gaps']} reconnects={s['reconnects']} errors={s['errors']}")
if s["frames"] == 0 or s["errors"] > 0:
    print(f"FAIL {phase}: driver errors"); raise SystemExit(1)
if phase != "slow" and s["sequence_gaps"] > 0:
    print(f"FAIL {phase}: unexpected sequence gaps"); raise SystemExit(1)
samples = [l for l in lines if "metrics" in l and l["metrics"]]
if samples:
    peak = samples[-1]["metrics"].get("queue_peak", 0)
    if peak > 1024:
        print(f"FAIL {phase}: queue_peak {peak} > 1024"); raise SystemExit(1)
PY
done

python3 - "$OUT_DIR" "$LABEL" "$MODE" <<'PY'
import glob,json,pathlib,sys
out,label,mode=sys.argv[1],sys.argv[2],sys.argv[3]
rows=[]
for path in sorted(glob.glob(f"{out}/*.jsonl")):
    lines=[json.loads(l) for l in open(path) if l.strip()]
    sums=[l["summary"] for l in lines if "summary" in l]
    if sums:
        rows.append((pathlib.Path(path).stem,sums[-1]))
report=[f"# Market-data soak — {label}", "", f"Mode: {mode}", "",
        "| Phase | Frames | Gaps | Reconnects | Errors | Elapsed |",
        "|---|---:|---:|---:|---:|---:|"]
for name,s in rows:
    report.append(f"| {name} | {s['frames']} | {s['sequence_gaps']} | {s['reconnects']} | {s['errors']} | {s['elapsed_s']}s |")
report += ["", "Raw samples and service metrics are in the JSONL files in this directory.", ""]
pathlib.Path(out,"report.md").write_text("\n".join(report))
print(f"report: {out}/report.md")
PY

if [ "$MODE" = "remote" ]; then
  bash scripts/check-observability.sh >"$OUT_DIR/observability.after.txt" 2>&1 \
    || { echo "FAIL: observability after soak"; tail -20 "$OUT_DIR/observability.after.txt"; failed=1; }
fi

if [ "$failed" -eq 0 ]; then echo SOAK_OK; else echo SOAK_FAILED; fi
exit "$failed"

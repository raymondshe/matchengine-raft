# Market-data fan-out and slow-consumer soak — design

- **Date:** 2026-11-25
- **Status:** design for TODO Top priorities #8 (P2)
- **Scope:** `market_data_system` WebSocket fan-out, the engine market-data tap, and the OrbStack
  deployment. Does **not** change matching semantics or prove multi-host Aeron/NATS behaviour.

## 1. Goal

Prove that the market-data tier stays bounded and that a hostile or slow WebSocket population cannot
affect the matching engine. The three questions the soak must answer:

1. **Bounded fan-out:** with many symbols, 1,024-stream connections, control churn, snapshot-request
   storms, slow consumers and reconnect bursts, do queue depth, memory, broadcast lag and close
   rates stay within the configured caps?
2. **Graceful degradation:** when a consumer falls behind, does the service isolate it (`1013`,
   `GAP`/resync, snapshot refresh) without head-of-line blocking other consumers?
3. **Engine isolation:** does `apply`, Raft commit/apply rate and write latency remain unchanged
   compared with an identical load that has no WebSocket clients?

The result belongs in [`docs/PERFORMANCE.md`](./PERFORMANCE.md) next to the existing
`ws_fanout` table, and a short run-report should be attached as JSONL.

## 2. Current design (evidence)

```text
engine apply ──► depth journal + MatchEvents ──► MarketDataTap::publish (try_send, bounded)
                                                  │  off-thread MarketDataProcessor
                                                  ▼
                                        NATS / Aeron / JSONL source
                                                  │
                                                  ▼
                              per-symbol Feed (shadow book, bars, reset snapshots)
                                                  │  Registry::publish → broadcast::Sender<Bytes>
                                                  ▼
                        one forward task per (connection, stream) subscription
                                                  │  coalesce latest-value streams, conflate, budget
                                                  ▼
                              per-connection mpsc::Sender<Outbound> → WS writer
```

### 2.1 Local cache is the serving path (design target — already implemented)

Confirmed target: **the market-data service answers clients from its own per-symbol cache and must
not issue per-client or periodic HTTP queries to the engine.** A newly connected client may trigger
one asynchronous snapshot *request*, but it never waits for the engine before the WebSocket is
usable. Evidence in the current code:

- `feed::SharedState` holds the shadow depth, BBO, bar history, `last_update_id`, `snapshot_seen`,
  `depth_stale`, heartbeat deadline, epoch and leader id for every symbol. `session::subscribe`
  attaches to the broadcast **first**, then builds the response from
  `snapshot_payload(&runtime.shared, ...)`; `/ready` and `/metrics` also read this cache. There is no
  engine HTTP client in `market_data_system` — the only `reqwest::Client` is the TDengine history
  writer.
- When a depth/BBO subscriber joins with an empty or stale cache, `subscribe` fires **one**
  `requester.request(depth)` on stream 2010 and immediately pushes whatever the cache holds; it does
  not block the session. The client sees live frames as they arrive and a reset snapshot when it
  lands.
- `Feed::run` requests a snapshot once at startup and retries every 5 s **only while**
  `snapshot_seen` is false. The engine also publishes a periodic reset snapshot (default 30 s) and a
  fresh one on leadership gain, so a healthy feed is refreshed without being polled.
- Consequently `snapshot_requests` should be **O(1) per symbol per cold/stale episode**, never
  proportional to the number of connected clients. The soak must verify that plateau. The snapshot
  request channel is unbounded for NATS and `try_send`-bounded for Aeron; if P4 shows joins while
  stale can enqueue an unbounded burst, the fix is a per-symbol debounce, not more engine polling.

Facts that constrain the soak:

- The engine tap is a bounded `tokio::sync::mpsc` (`queue_capacity = 8,192` by default) and
  `MarketDataTap::publish` uses `try_send`; a full queue sets a gap marker and increments
  `dropped_frames`. Publishing happens **after** the state-machine lock is released
  (`StateMachineStore::publish_pending`), so WebSocket backpressure never enters `apply`.
- `market_data_system::feed::Registry` holds one lazy `broadcast::Sender<Bytes>` per stream key;
  payloads are encoded once per publication and shared as `Bytes` with all receivers. Broadcast lag
  is a signal, not a queue that grows without bound.
- Each subscription owns a forward task and a per-connection `mpsc` queue. `latest_value` streams
  (`bookTicker`, partial depth) drain newer broadcast values with `try_recv`; terminal connections
  conflate at 100 ms and enforce a 2,000 frame/s budget by default (`fast` connections stay exact).
  A full per-connection queue increments `slow_closes` and closes that consumer with `1013`.
- The stream catalogue and cheapest caps are enforced in `session.rs`: terminal `max_streams`,
  fast `fast_max_streams`, global `max_connections`, a 5 control-message/s limiter per connection,
  subscription limits checked before any mutation, and a 24 h connection lifetime.
- A depth subscriber joining with an empty/stale shadow cache calls `request_snapshot`; the feed also
  retries a snapshot every 5 s until `snapshot_seen`. `snapshot_requests`, `resyncs`, `gaps` and
  `slow_closes` are already exported through `/metrics` and the JSON→Prometheus bridge.
- Current lab deployment (`global-marketdata`, rendered from the CRD): 8 symbols,
  `--max-connections 4096`, `--max-streams 1024`, `--queue 1024`, `--fast-max-streams 32`,
  `--terminal-conflation-ms 100`, HPA min 1 / max 2, NATS transport, history enabled.
- Existing baseline: `docs/PERFORMANCE.md` §“WebSocket fan-out” records 110–186k aggregate frames/s
  on loopback with 16–64 clients on one `bookTicker` stream. That is a microbenchmark: no Aeron/NATS
  ingestion, no multi-symbol catalogue, no control churn, no engine under load, no slow clients.

## 3. Workload model

The soak uses the deployed global terminal service (8 symbols) as the primary target and one
fast-only shard service as the control. All load originates inside the lab via the ingress hosts; no
Internet client is used.

| Phase | Connections | Streams/client | Symbols | Control churn | Intent |
|---|---:|---:|---|---|---|
| P0 baseline | 0 | 0 | 8 | none | Record engine and service metrics with the continuous replay running but no WS clients. |
| P1 warm | 16 | 8 mixed | 8 | none | Verify steady state and collect per-stream fan-out/queue baselines. |
| P2 scale | 64, 128 | 32 mixed | 8 | 0.5 SUBSCRIBE/UNSUBSCRIBE per client/s | Reach realistic terminal concurrency; exercise registry add/remove and rate limiter. |
| P3 maximum | 256 | 1,024 terminal / 32 fast | 8 | 1 control msg/s/client | Approach the configured caps; bounded-time stress, abort on stop conditions. |
| P4 snapshot storm | 128 | depth + bookTicker | 8 | burst joins with stale cache | Drive `snapshot_requests`; verify the async request stays episode-scoped (not per client), queue bounds hold and engine request load is bounded. |
| P5 slow consumers | 128 | mixed | 8 | 25% stop reading for 10 s | Verify `1013`, `GAP`, resync recovery and isolation of healthy clients. |
| P6 reconnect burst | 256 | mixed | 8 | restart 50% simultaneously | Measure resync time, snapshot requests, HPA reaction and stream-key cleanup. |
| P7 cooldown | 0 | 0 | 8 | disconnect all | Verify queues drain, registry keys return to baseline, HPA returns to min, no restarts. |

Stream mix per terminal connection: `bookTicker`, `depth@100ms`, `depth5@100ms`, `trade`,
`kline_1s`, `status` for each assigned symbol, repeated until the target stream count is reached.
At least 20% of streams should be sequenced (`depth*`, `trade`, `kline`) so conflation cannot hide
queue growth. P3 is deliberately short (5 minutes) because 256 × 1,024 is 262k receivers, the
amplification case called out in the security review.

Offered market-data rate is the existing continuous replay harness (`scripts/replay-continuous.sh`)
plus the normal lab tape; do not synthesise order flow directly into the engine. A local-only
variant of P1–P3 uses `channel_source` for deterministic frame rates and is run first, before the
deployed soak.

## 4. Harness design

### 4.1 Code

Add `market_data_system/benches/ws_soak.rs` (or a `soak` subcommand in `ws_fanout.rs`) with two
modes:

- **local** — start the service in-process with a `channel_source`, like `ws_fanout`, for
  deterministic offered rate and direct access to internal stats. Used in CI-friendly smoke runs.
- **remote** — connect to `WS_BASE_URL` (terminal ingress `/md/ws` or a fast-service `/ws`) and read
  metrics from `METRICS_URL` (the JSON `/metrics` of the target service through the bridge or a
  port-forward). Used for the OrbStack soak.

CLI:

```text
--mode local|remote
--clients N --streams-per-client N --symbols a,b,c
--duration SECONDS --rate FRAMES_PER_SECOND   # local only
--churn-per-sec FLOAT --slow-fraction FLOAT --reconnect-burst PCT
--snapshot-storm BOOL --label NAME --out PATH.jsonl
```

Each client task records: frames received by stream kind, sequence gaps detected (for depth),
`1013`/policy/pong close codes, reconnect count, and client-side p50/p99 frame latency. The driver
samples service and engine metrics every second and writes one JSON object per sample with a run
label and phase tag.

### 4.2 Deployment wrapper

Add `scripts/market-data-soak.sh` to:

1. assert the lab is healthy (`kubectl get pods`, manager health, `scripts/check-observability.sh`);
2. snapshot HPA/PDB state and current metrics for P0;
3. run the phases in order with stop conditions, collecting JSONL under `dist/soak/<label>/`;
4. query Prometheus through `kubectl -n match-observability exec deploy/prometheus -- wget ...`
   for the trend queries below, plus `/api/v1/query` for HPA scaling events from kube-state metrics
   if available (otherwise record replica counts directly);
5. generate a Markdown table into `docs/PERFORMANCE.md` or a standalone run report and exit non-zero
   on any hard failure.

### 4.3 Metrics to collect

Existing metrics are sufficient for a first pass; these additions are recommended before the final
run so the "slow consumer" and CPU claims are measurable rather than inferred:

| Metric | Source | Why |
|---|---|---|
| `marketdata_queue_depth` / `marketdata_queue_peak` | `feed::Stats`, sampled in each forward task from `out_tx.capacity()` | Prove per-connection queue stays ≤ `--queue`; detect head-of-line buildup. |
| `marketdata_encode_ms_total` / `marketdata_frames_encoded` | time `frame_bytes`/`wrap` in the forward task, aggregate atomically | Per-instance encode CPU attribution; distinguish JSON cost from broadcast cost. |
| `marketdata_close_code_total{code}` | `session.rs` close path | Replaces the single `slow_closes` counter with labelled `1013`, policy, pong, lifetime, client-close reasons. |
| `marketdata_broadcast_lag_total` | `RecvError::Lagged` in `forward` | Quantify broadcast overrun before it becomes a client-visible `GAP`. |
| `marketdata_subscribers{stream,symbol}` | `Registry` counts | Prove stream keys are released after mass disconnect. |
| engine tap counters | `/console/api/node` → `market_data` or `/read marketdata_status` | `queued_peak`, `queue_capacity`, `dropped_frames`, `gaps`, `frames` must not move under WS load. |
| engine apply/Raft | `/metrics` via bridge | `matchengine_last_applied`, `matchengine_last_log_index`, `matchengine_term`, transport stream counts, `runtime_stats` when profiling is enabled. |
| client write latency | replay harness or `e2e --duration --clients` | p50/p99 with and without WS load, same offered write rate. |
| pod resource use | `kubectl top pod`, cgroup CPU, RSS, Prometheus | HPA max, OOMKill, restart count, memory growth. |

Prometheus queries used by the wrapper (examples; bridge flattens JSON to these prefixes):

```promql
rate(marketdata_published[1m])
rate(marketdata_gaps[1m]) + rate(marketdata_resyncs[1m])
rate(marketdata_slow_closes[1m])
rate(marketdata_snapshot_requests[1m])
max(marketdata_queue_peak) <= 1024
rate(matchengine_last_applied[1m])
histogram_quantile(0.99, sum(rate(e2e_write_latency_bucket[5m])) by (le))
```

The bridge caches for 5 s and Prometheus scrapes every 15 s, so these are trend signals. Per-second
decisions in the harness use the direct JSON `/metrics` endpoints.

## 5. Pass / fail criteria

**Hard failures (stop the soak immediately):**

- Any engine pod restarts, loses leadership unexpectedly, or stops advancing
  `last_applied`/`last_log_index` for more than 5 s under continued writes.
- Engine tap `dropped_frames`, `gaps` or `queued_peak` increase relative to P0 while WS load changes.
  Any increase means backpressure reached `apply`, which the design forbids.
- Market-data pod OOMKill, restart, `CrashLoopBackOff`, or HPA pinned at max with `/ready` false.
- `marketdata_queue_depth` exceeds the configured `--queue` cap, or healthy clients receive
  `GAP`/`1013` outside the injected slow-consumer phase.
- Snapshot request rate grows without bound after a storm (no plateau within 30 s).

**Success criteria:**

- Engine write p50/p99 and commit/apply rate within **±10%** of P0 under the same offered write load;
  no change in Raft transport stream counts beyond normal election/heartbeat variance.
- Bounded fan-out: queue peak ≤ cap, per-stream broadcast lag returns to zero after each burst, and
  RSS at the end of P6 is within 20% of the P1 steady-state value at the same connection count.
- Slow consumers are isolated: injected clients are closed with `1013` and receive a resync/snapshot
  on reconnect; healthy clients on the same pod keep contiguous depth sequences and no `GAP`.
- Snapshot storms are absorbed: `snapshot_requests` plateaus at a small value per symbol (not one
  per joining client, not a rising 5 s retry after `snapshot_seen`), engine snapshot traffic shows no
  sustained spike, and `/ready` remains true for all 8 symbols. Client response time is measured from
  the local cache path, not from an engine round trip.
- Mass reconnect recovers within 30 s: subscriber/stream counts return to pre-burst levels, no
  leaked stream keys in `Registry`, HPA returns to min during P7.
- All results recorded in a JSONL run report with the exact image tags, HPA settings and lab state.

Thresholds are deliberately relative to a same-host baseline rather than absolute frame rates: the
existing 110–186k frames/s table already covers local peak throughput, and third-party comparisons
must not be claimed without a pinned host.

## 6. Safety and stop conditions

- Run only in the lab namespace; use the existing ingress hosts and the current images. Never point
  the soak at a production overlay.
- Keep the HPA max at 2 during P0–P6; do not raise it to "make the test pass" without recording a new
  config. P3 is capped at 5 minutes.
- The harness reserves a separate admin WebSocket client that stays healthy; if it sees a `GAP` or
  `1013` outside an injected phase, it aborts the run.
- Before each phase, snapshot Pod/HPA state. On abort, disconnect all clients, wait for cooldown and
  re-run `scripts/check-observability.sh`; record any residue.
- Do not change matching, WAL or snapshot settings. The soak is observational except for the
  proposed metrics additions in §4.3.

## 7. Execution plan

1. Land the metric additions (§4.3) and a unit test for close-reason counting; rebuild the market-data
   image and roll the lab.
2. Implement `ws_soak.rs` and run the **local** variant in CI-like form (P1–P3, 60 s each) to catch
   driver bugs cheaply.
3. Run the deployed soak once with the continuous replay at its normal rate; publish the run report
   and a short section in `docs/PERFORMANCE.md`.
4. If P4/P5 fail, fix the root cause in the service (likely snapshot-request debouncing per symbol or
   earlier close of slow consumers) and repeat from that phase; do not hide it by raising caps.
5. If P0–P2 fail, stop and treat it as a real fan-out defect, not a tuning exercise; file a P1/P2
   item with the captured JSONL and re-prioritise.

## 8. Risks / open questions

- **Single host:** all pods share one OrbStack kernel and one NATS broker, so network and CPU
  contention are shared. Results prove per-instance boundedness, not multi-host fabric behaviour
  (which remains a P1 production gate).
- **Per-IP quotas are not implemented** (P1 public-edge item). The soak can quantify the risk of one
  client opening many connections, but must not claim the risk is mitigated.
- **Snapshot-request amplification:** the request is async and episode-scoped today, but a burst of
  depth joins while the cache is stale can still enqueue one request per join. P4 will quantify it;
  a per-symbol debounce is the proposed fix if the request count tracks client count.
- **Fast vs terminal conflation:** fast connections are exact, so they will expose queue pressure
  earlier than terminal connections. Keep both in the mix.
- **Metrics cardinality:** labelled close codes and per-stream subscriber gauges are bounded by the
  stream catalogue; do not add per-client labels.
- **History tier:** the per-symbol fast services write TDengine bars; the global service is read-only
  (`--history-read-only`) and serves `/klines` from the same store. P0 should include a note on
  `history_dropped_events`/`history_dropped_rows` from the writer pods to ensure fan-out load is not
  silently starving the historical writer, and verify no writer runs in the global pod.

## 9. Deliverables

- `market_data_system/benches/ws_soak.rs` with local/remote modes.
- `scripts/market-data-soak.sh` wrapper and stop-condition logic.
- Metric additions in `market_data_system/src/{feed.rs,session.rs}` if required.
- `docs/PERFORMANCE.md` section with the run table and a JSONL report under `dist/soak/` (gitignored).
- Updated TODO item #8 with the measured outcome and any follow-up item created from a failure.

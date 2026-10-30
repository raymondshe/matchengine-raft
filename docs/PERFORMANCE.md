# Performance measurements

Measured on **2026-10-30** on the reference development machine:

- Apple M4 (`arm64`)
- Rust `1.93.1`
- Release/bench profile
- Local loopback networking, single process for the end-to-end test

These are engineering baselines, not a capacity guarantee. Run the commands below
on the deployment hardware before using the numbers for sizing.

## Reproduce

```shell
cargo bench --bench matchengine   # order-book microbenchmarks
cargo bench --bench e2e           # submit -> commit -> apply -> response
cargo bench --bench feed_load     # market-data feed at 50k/500k fills/s
cargo bench --bench storage       # WAL append/read/recovery
```

## Order-book microbenchmarks

Best-of-5 per round; each scenario performs the operation 1,000 times unless
noted. These exclude HTTP, Raft, WAL and the feed.

| Scenario | Time per round |
|---|---:|
| Sweep 1k asks / 100 levels | 105.6 µs |
| Sweep 20k asks / 1 level | 2.538 ms |
| Rest 1k bids | 97.7 µs |
| Rest 1k bids + market-data journal | 100.3 µs |
| Partial fills 1k | 119.2 µs |
| IOC cancels 1k | 201.0 µs |
| Iceberg replenish 1k | 116.8 µs |
| Amend 1k | 130.8 µs |
| Cancel 1k | 148.1 µs |

The market-data journal adds about 2.6 µs to 1,000 rests, confirming that the
matching path does not pay for shadow-book derivation, encoding or sink I/O.

## End-to-end write path

Single node, HTTP JSON `/write`, including:

1. HTTP request decode and request validation
2. Raft proposal and commit
3. WAL append and `sync_data`
4. State-machine apply and order-book mutation
5. Response serialization and HTTP response

It excludes cross-node network replication, so a 3-node cluster adds one
majority round trip on top of these numbers.

| Measurement | Result |
|---|---:|
| Sequential writes | 500 |
| Sequential throughput | 253 writes/s |
| Mean latency | 3.96 ms |
| p50 latency | 3.99 ms |
| p95 latency | 4.90 ms |
| p99 latency | 5.17 ms |
| Max latency | 5.36 ms |
| Concurrent writes (1,000 in flight) | 18,350 writes/s |

The gap between sequential and concurrent throughput is the expected batching
effect: sequential clients pay one WAL fsync per request, while concurrent
requests share append batches. Matching itself is measured separately above;
the single-node e2e path is dominated by HTTP plus durable append.

## Market-data feed load

Real tokio tap, bounded queue and processor; null sink, so this measures the
feed pipeline without JSONL or network I/O. Each frame carries **10 fills**.

| Target | Offered | Processed | Dropped frames | GAPs | Peak queue | Publish / drain |
|---|---:|---:|---:|---:|---:|---:|
| 50k fills/s, queue 8,192 | 49,480 fills/s | 49,480 fills/s | 0 | 0 | 67 / 8,192 | 1.002 s / 208 ns |
| 500k fills/s, queue 8,192 | 495,072 fills/s | 495,072 fills/s | 0 | 0 | 169 / 8,192 | 1.002 s / 250 ns |
| 2M fills/s overload, queue 128 | 1,978,685 fills/s | 1,876,238 fills/s | 10,261 | 7,956 | 129 / 128 | 1.002 s / 292 ns |

At 500k fills/s the 8,192-frame queue stays far below capacity and the processor
keeps up. The deliberate 2M fills/s overload shows the bounded-queue contract:
frames are dropped rather than blocking `apply`, and the processor emits `GAP`
markers. Peak queue includes one frame handed to the processor but not yet
ingested, so it can show `capacity + 1`.

## Storage benchmark

`cargo bench --bench storage` writes 20,000 real `MatchEngineRequest::place`
entries in batches of 100, with 512 KiB segments:

| Operation | Result |
|---|---:|
| Append 20k entries | 767.8 ms |
| Append throughput | 26,049 entries/s |
| Read 20k entries | 8.38 ms |
| Read throughput | 2,386,089 entries/s |
| Reopen/recover 20k entries | 6.33 ms |
| Recovered entries | 20,000 |

Append throughput is fsync-bound (one durable batch per 100 entries); read and
recovery benefit from sequential segment scanning and coalesced `pread` calls.

## Storage-path improvements

The 2026-10-30 audit removed several avoidable copies and stalls from the storage path:

- **WAL append** serializes a borrowed `WalRecordRef` instead of cloning every entry into an owned
  `WalRecord`; the on-disk format is unchanged.
- **WAL recovery** scans each segment sequentially instead of issuing two `pread` calls per record.
- **Log reads** coalesce contiguous records from one append batch into a single `pread`, while
  still handling commit-marker gaps.
- **Snapshot build** captures a consistent content image under the state-machine lock, then
  encodes and writes it after releasing the lock, so apply can continue during large snapshots.
- **Snapshot writes** stream the postcard body through an incremental CRC writer, so no full
  body `Vec` is allocated; snapshot install moves its buffer into the write task instead of
  cloning it.

The storage integration suite (`test_storage`) still covers framing, torn tails, rotation, purge,
truncate, snapshot install/reopen and 20k-record recovery.

## Interpretation

- Matching and feed derivation are not the current bottleneck.
- Sequential write latency is fsync-dominated; batching concurrent submissions
  improves throughput substantially.
- The feed has ample headroom at 500k fills/s with a null sink. A real sink
  (JSONL or Aeron) must be measured separately because it adds encoding and I/O.
- No per-level bucket or compact resting-order change is justified by these
  measurements yet. Revisit after a 3-node e2e run and a real feed sink.

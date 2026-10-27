# Choosing a transport for the Raft network

Status: **gRPC adopted; benchmarks still missing.** The gRPC/tonic transport described here is
implemented (`src/network/grpc/`, `src/network/transport.rs`) and is the default for peer-to-peer
Raft RPCs; HTTP/JSON remains selectable with `--transport http`. **No benchmark harness has been
built**, so every performance figure below is guidance, not a result.

Date: 2026-10-27 · Baseline: `openraft` 0.10.0-alpha.37, gRPC/tonic default with HTTP/JSON
selectable, segmented WAL + atomic snapshots.

Implementation snapshot:

| Item | State |
|---|---|
| gRPC service | `proto/raft.proto` + `src/network/grpc/server.rs`; unary Vote/AppendEntries, client-streaming snapshot, bidirectional append stream |
| gRPC client | `src/network/grpc/client.rs`; `postcard` payloads, `soft_ttl` deadlines, `stream_append`, cancel-aware snapshot |
| Selection | `--transport grpc\|http` (default `grpc`); the server always exposes both |
| Addressing | Raft port = API port + `RAFT_PORT_OFFSET` (1000); `BasicNode.addr` stays the API address |
| Tests | `test_cluster` runs over both transports; `test_snapshot` proves purge → snapshot install over gRPC |

---

## 1. Summary

**Adopted default: gRPC via `tonic`.** It is what openraft's own guide recommends, it is what
the reference production deployment (databend-meta) uses, and it is the only candidate that gives
multiplexed streams and streaming RPCs without writing a framing layer. It is implemented and is
what the node dials unless `--transport http` is passed.

**Keep measuring.** The transport is one line item in the latency budget, and the log fsyncs once
per append batch, so the remaining question is how much of the p99 commit path each part actually
costs. That is what the harness in §5 has to answer.

Order of work:

1. Build the harness (§5) and measure the current gRPC path and the HTTP/JSON fallback (§4).
2. Split p99 commit latency into transport, serialization, fsync and state-machine time.
3. Change the transport only if it beats the current gRPC path by ≥20% on the dominant metric (§7).
4. If not, spend the time on TLS, snapshot streaming and matching-engine work instead.

If you only read one thing: **hold the codec constant when comparing transports, and hold the
transport constant when comparing codecs.** Most "transport" benchmarks in this space are codec
benchmarks in disguise.

---

## 2. What the transport has to carry

openraft does not have one network workload; it has three with opposite requirements.

| Traffic | Typical size | Frequency | Requirement |
|---|---|---|---|
| `vote` / `pre_vote` | ~100 B | rare, bursty during elections | tail latency; must not queue behind data |
| `append_entries` | 1 KB – 1 MB | constant | throughput and p99; benefits from batching and pipelining |
| `full_snapshot` | 1 MB – GB | rare | must not block the other two; needs streaming and backpressure |

Contract beyond raw speed:

- honours `RPCOption::soft_ttl()` and the `cancel` future passed to `full_snapshot`
- maps failures onto `RPCError::{Timeout, Unreachable, Network}` and `StreamingError`
- never blocks the Raft core on a slow or stalled peer
- handles payloads above the transport's limit by splitting, not failing
- TLS/mTLS when the order flow crosses a trust boundary
- runs on macOS for development and Linux in production with normal ops tooling
- observable: per-peer latency, error rate, bytes, in-flight requests

A transport that is 100 µs faster but stalls a vote behind a snapshot is worse than a slower one
that isolates the three classes.

---

## 3. Current state in this repo

| Aspect | Today | File |
|---|---|---|
| Client (default) | gRPC/tonic; unary vote/append, bidirectional `stream_append`, client-streaming snapshot | `src/network/grpc/client.rs` |
| Server (default) | tonic service on API port + 1000: `AppendEntries`, `Vote`, `InstallSnapshot`, `AppendStream` | `src/network/grpc/server.rs`, `proto/raft.proto` |
| Client (legacy) | `reqwest`, one HTTP POST per RPC, `.timeout(soft_ttl)`, sequential `stream_append` | `src/network/raft_network_impl.rs` |
| Server (legacy) | `actix-web` handlers for `/append`, `/vote`, `/snapshot` | `src/network/raft.rs` |
| Codec | `postcard` inside protobuf envelopes over gRPC; `serde_json` over HTTP and on the public API | `src/network/grpc/`, `src/network/raft_network_impl.rs` |
| Pipelining | gRPC pipelines appends over `AppendStream`; HTTP is one at a time | `src/network/grpc/client.rs` |
| Snapshot | gRPC streams 256 KiB chunks with offsets, a terminal marker and cancellation; HTTP sends one JSON body | `src/network/grpc/client.rs`, `src/network/grpc/server.rs` |
| Node address | `BasicNode { addr }` is the public API address; the Raft port is `addr` + 1000 | `src/network/mod.rs` |
| Timing | heartbeat 500 ms, election 1500–3000 ms | `src/lib.rs` |
| Storage | segmented WAL + atomic snapshots; one fsync per append batch | `src/store/` |
| Tests | cluster (both transports), snapshot install over gRPC, storage formats, restart recovery; unique temp dirs | `tests/cluster/` |
| Benchmarks | **none** | — |

Two consequences for any evaluation:

- **Append now pays an fsync.** Under load the leader's commit latency includes disk sync time, so
  the transport's share is smaller than it was when the log was in memory. Measure storage and
  transport separately.
- **Measure batches, not just round trips.** gRPC pipelines via `stream_append`; a benchmark with a
  single entry per RPC measures latency, not replication throughput. Use batch sizes 1, 10, 100,
  300.

---

## 4. Measure the budget before choosing

Instrument the current path and split p99 commit latency into:

```
client → leader            (HTTP + JSON, request overhead)
leader encode → socket     (serialization + syscall)
network                    (RTT; 0 on loopback)
follower decode → append   (serialization + WAL write/fsync)
follower → leader response
leader commit → apply
```

Run the split once with `--transport grpc` and once with `--transport http`; the two paths differ in
the middle three rows, not in the client or apply rows.

How to get the split without guessing:

- `tracing` spans around encode / send / await / decode in `src/network/grpc/client.rs` and
  `src/network/raft_network_impl.rs`.
- openraft `RaftMetrics`: `last_log_index` vs `last_applied`, and per-peer matched index.
- `std::time::Instant` deltas recorded into an HDR histogram, not averaged.

Decision input: if transport <10–15% of p99, fix the codec and batching first; a new transport
cannot return more than its share.

---

## 5. Evaluation harness

### 5.1 The seam already exists

Every candidate is one `RaftNetworkFactory` + network impl. The repo already has two, selected at
runtime by `TransportNetworkFactory` (`src/network/transport.rs`):

```rust
// One impl per candidate, selected by CLI arg today.
impl<C: RaftTypeConfig<Node = BasicNode>> RaftNetworkFactory<C> for HttpJsonFactory { ... }   // exists
impl<C: RaftTypeConfig<Node = BasicNode>> RaftNetworkFactory<C> for GrpcFactory { ... }       // exists, default
impl<C: RaftTypeConfig<Node = BasicNode>> RaftNetworkFactory<C> for HttpPostcardFactory { ... } // hypothetical
```

Wrap the chosen impl in a recording layer that timestamps send/receive and emits a histogram, so
the harness measures the transport and not the test client.

Labels used from here on: **baseline** = HTTP/JSON (legacy), **A** = gRPC via `tonic` (the current
default), **B** = HTTP +
`postcard`, **C** = HTTP/2 h2c + `postcard` (only if A and B both fail a gate).

Implementation note from databend-meta: if you need a custom `stream_append`, implement the split
sub-traits (`NetAppend`, `NetVote`, `NetSnapshot`, `NetStreamAppend`, `NetTransferLeader`,
`NetBackoff`) rather than the umbrella `RaftNetworkV2` — `NetStreamAppend` is blanket-implemented
for every `RaftNetworkV2`, so the two cannot coexist. Overriding `stream_append` directly in the
`RaftNetworkV2` impl also works; pick one and be explicit.

### 5.2 Two measurement levels

**L1 — micro, isolates the transport.** Construct the network impl directly and call
`append_entries` / `vote` against a peer that answers without Raft. Fixed byte payloads, no
consensus. Answers: encode + send + decode + syscall cost.

**L2 — macro, what users feel.** Real 3-node cluster, `Raft::client_write` load, metrics from
`RaftMetrics`. Answers: time-to-commit including batching and consensus.

Report both. L1 ranks transports; L2 decides whether the ranking matters.

### 5.3 Workloads

| Workload | Shape | Why |
|---|---|---|
| vote storm | 1 000 votes/s, tiny, concurrent with appends | election tail latency |
| steady append | 1 KB entries at 1k / 10k / 50k writes/s | latency vs load curve |
| batch sweep | 1 / 10 / 100 / 300 entries per append | batching efficiency |
| burst | 10k writes in 1 s, measure drain | queueing and backpressure |
| snapshot under load | install 10 MB / 100 MB / 1 GB while appending | head-of-line blocking |
| cold start | follower joins empty, catches up | streaming and snapshot path |

### 5.4 Metrics

- p50 / p99 / p99.9 / max, **open-loop** (schedule sends on a timer; do not measure from
  completion to completion — that hides coordinated omission)
- throughput at a fixed SLO, not peak throughput
- CPU µs per message (user + sys), and bytes on the wire per RPC
- allocation count per RPC if the codec allocates
- append p99 **during** a snapshot transfer
- election time and catch-up time after failures
- RSS and mapped memory (Aeron term buffers are the outlier here)
- error rate and recovery time under §6

### 5.5 Pitfalls that invalidate comparisons

| Pitfall | Effect | Avoid by |
|---|---|---|
| Changing codec and transport together | attributes JSON cost to HTTP | hold codec constant; separate codec experiment |
| Loopback only | hides RTT and loss behaviour | repeat at 1 ms RTT |
| Closed-loop client | hides queueing | open-loop, fixed rate, record schedule drift |
| No warm-up | measures connection setup | discard first 10 % or a fixed 30 s |
| Single run | noise looks like a result | 5 runs, median of p99 |
| Different batch sizes | incomparable throughput | fix batch size per row |
| HTTP/1.1 pool vs gRPC channel defaults | confounds connection reuse | pin pool/channel settings |
| TLS on for one candidate only | penalises one side | same security config for all |
| One transport with fsync, the other cached | attributes disk time to the transport | keep the WAL enabled for both; record fsync separately |

### 5.6 Environment

1. Loopback, same host (development signal).
2. Two hosts, 1 ms RTT, 0.1 % loss (Linux `tc netem`; on macOS `dnctl`/`pfctl` or Toxiproxy).
3. Target deployment topology.

Record CPU model, core count, governor, and whether nodes share a machine.

---

## 6. Candidates and gates

### 6.1 Shortlist

Versions checked on crates.io, 2026-10-27.

| Candidate | Crates | Prior art | Strengths | Weaknesses |
|---|---|---|---|---|
| **gRPC** | `tonic` 0.14.7, `prost` 0.14.4 | openraft guide recommends Tonic; databend-meta uses it in production; implemented here | HTTP/2 multiplexing (no HOL between vote/append/snapshot); native streaming for appends and snapshots; deadlines; TLS/mTLS; mature tooling; this repo raises the 4 MB receive default to 64 MiB | protobuf schema; second listener |
| **HTTP/1.1 + binary** | `reqwest` 0.12.28, `actix-web` 4.13, `postcard` 1.1.3 / `bincode` 3.0.0 | this repo | smallest change; keeps the stack; removes JSON cost | no multiplexing — snapshot blocks appends unless separate connections; streaming and chunking are hand-rolled |
| **HTTP/2 h2c + binary** | `h2` 0.4.20, `actix-web` `bind_auto_h2c`, `reqwest::http2_prior_knowledge` | — | multiplexing with the existing stack; no protobuf | streaming and framing are yours; h2c support varies across proxies |
| **Raw TCP, length-prefixed** | std + `tokio` | TiKV-style custom transports | full control; low overhead; easy framing | TLS, reconnect, flow control, observability all yours |
| **Aeron** | `rusteron-client` 0.2.13 | Aeron Cluster's own (Java) consensus | lowest latency ceiling; native flow control; IPC | `!Send + !Sync`; media driver process; 2 MiB default message cap; no RPC semantics; ~4 weeks — see `AERON_SOLUTION.md` |
| **QUIC** | `quinn` 0.11.12 | — | loss resilience; multiplexed streams | CPU cost; UDP may be blocked; unnecessary on a LAN |
| **ZeroMQ** | `zeromq` 0.6.0 (2026-05), `zmq` 0.10.0 (2022, stale) | — | simple message patterns | no RPC reliability semantics by default; weak observability; stale legacy crate |
| **Unix sockets / shm** | `interprocess` 2.4.7 | — | fastest same-host | not a cluster transport |

Reference point: the in-process path (no serialization, direct function call) is the floor. If a
transport is within noise of the floor at L1, it is not the bottleneck.

gRPC (candidate A) is already implemented and is the default; HTTP/JSON (the former baseline) is
still selectable with `--transport http`. The harness has not been built, so none of these
candidates has a measured score yet.

### 6.2 Hard gates

Any failure eliminates the candidate regardless of score:

1. **Oversized payload** — an append larger than the transport limit is split, not dropped.
2. **Snapshot larger than the limit** — installs successfully while appends continue.
3. **Stalled follower** — `SIGSTOP` a follower; votes and other peers' appends are unaffected, and
   the leader does not block.
4. **Deadline** — an unresponsive peer produces `Timeout` within `soft_ttl`, not a hang.
5. **Recovery** — leader kill elects a new leader; a wiped follower rejoins via snapshot.
6. **Security** — TLS/mTLS available if the deployment needs it.
7. **Ops** — restartable, observable, no privileged or kernel-bypass requirements.
8. **Portability** — builds and runs on macOS (dev) and Linux (prod).

### 6.3 Scoring

| Criterion | Weight |
|---|---|
| p99 append at target load | 30 % |
| Throughput at SLO | 20 % |
| Snapshot impact on live traffic | 15 % |
| Operational burden | 15 % |
| Security / compliance | 10 % |
| Ecosystem health and maintenance | 10 % |

Decision rule: **a replacement must beat the current default (gRPC) by ≥20% on the dominant
metric**; otherwise keep it and spend the time on codec, batching, TLS or storage.

---

## 7. Outcome and remaining risk

gRPC was chosen and is in the tree, for three reasons that are specific to Raft rather than
generic:

1. **Pipelining and isolation come for free.** HTTP/2 streams let `stream_append` keep several
   appends in flight per peer while a snapshot transfer proceeds on its own stream. Over HTTP/1.1
   you get neither without extra connections and hand-rolled chunking.
2. **Error semantics line up.** gRPC status codes map cleanly onto `RPCError` / `StreamingError`,
   and deadlines propagate, which is exactly the contract `RaftNetworkV2` asks for.
3. **There is a production reference.** databend-meta splits oversized appends by an advisory
   message size, streams appends over a bidirectional gRPC call (`append_v002`, falling back to
   unary when the stream cannot be established), and streams snapshots as KV entries in batches of
   64 followed by a commit record — the same problems this repo would otherwise solve from
   scratch.

HTTP/JSON is the **fallback**, kept compiled and selectable (`--transport http`), not the plan of
record. Two measurements could still reopen the choice:

- gRPC's tail is worse than HTTP/1.1 on the target network → raw TCP (§6, B′) becomes the
  fallback, not Aeron.
- The transport share is small (< 15 % of p99) and gRPC's operational surface (second HTTP stack,
  TLS config, message limits) is not buying anything → simplify, but only with numbers.

Aeron wins only if the gate in `AERON_SOLUTION.md` §9.4 is met, i.e. the transport is provably the
bottleneck after the gRPC path has been measured.

---

## 8. Execution plan (what is left)

The implementation half of the original one-week plan is done. What remains is measurement:

| Day | Work | Deliverable |
|---|---|---|
| 1 | Instrument the budget split; recording wrapper | latency breakdown for the current transport |
| 2 | L1 + L2 harness, workload matrix | reproducible command; numbers for both `--transport grpc` (default) and `--transport http` |
| 3 | Failure injection: stall, kill, loss, snapshot under load | tail behaviour per transport |
| 4 | Score against §6, write the decision, delete the losers | decision record (§10) |

Deliverables stay in the repo: the harness, a results table, and the decision. Until the harness
exists, treat every number in this document as guidance and leave the default alone.

---

## 9. Integration notes (as implemented)

- **Separate port:** the gRPC Raft server binds `http_port + 1000`, so the public API stays on
  `--http-addr` and both transports coexist.
- **Runtime selection:** `--transport grpc|http` (default `grpc`) selects only the outbound
  transport; every node accepts both.
- **Addressing:** `BasicNode.addr` remains the public API address. The Raft address is derived by
  `raft_addr_from_http()`; client forwarding is unchanged.
- **Dependencies:** `tonic` + `prost` pull in a second HTTP stack (`hyper`/`h2`) alongside
  actix-web. That is accepted; it adds build time but no runtime conflict.
- **Tests:** the cluster test uses fixed ports 21001–21003 (gRPC 22001–22003) and unique temp data
  dirs; the snapshot and restart tests use their own ports. None of them share storage, but they
  still share ports, so kill leftovers before rerunning.
- **CI:** still missing; nothing gates commits, so a transport regression can land silently.

---

## 10. Decision record (fill in when the harness runs)

| Field | Value |
|---|---|
| Date | |
| Baseline p99 append (1 KB, batch 100, 10k writes/s) | |
| Transport share of p99 commit latency | |
| Winner | |
| Improvement over baseline (dominant metric) | |
| Gate failures | |
| Reason not to adopt (if any) | |
| Re-open trigger | |

---

## 11. Open questions

1. **Topology** — same host, same rack, or cross-AZ? (Decides whether QUIC and Aeron are even in
   play.)
2. **Target** — peak orders/s and acceptable p99 commit latency?
3. **Durability** — the log fsyncs each append batch before the leader responds. How much of the
   target p99 does that consume?
4. **Snapshot size and rate** — how large does the order-book snapshot get, and how often?
5. **Security** — is mTLS required between nodes?
6. **Ops** — is a media-driver process acceptable? (Only matters for Aeron.)
7. **Cluster size** — 3 nodes, 5, more? Affects connection and publication counts.

---

## Appendix A — verification log

Checked, not assumed (2026-10-27):

- crates.io: `tonic` 0.14.7, `prost` 0.14.4, `quinn` 0.11.12, `h2` 0.4.20, `postcard` 1.1.3,
  `bincode` 3.0.0, `zeromq` 0.6.0 (2026-05-04), `zmq` 0.10.0 (2022-11-04),
  `rusteron-client` 0.2.13, `interprocess` 2.4.7.
- tonic source: `DEFAULT_MAX_RECV_MESSAGE_SIZE = 4 * 1024 * 1024` (4 MB receive limit), send limit
  unlimited by default.
- actix-web 4.13 `HttpServer`: `bind_auto_h2c` exists; reqwest 0.12.28 `ClientBuilder`:
  `http2_prior_knowledge` and `http2_adaptive_window` exist. Both are gated on the crate's `http2`
  feature, which is on by default in actix-web 4.13.0 and reqwest 0.12.28.
- openraft guide recommends Tonic and points at databend-meta as a real-world example.
- databend-meta `crates/server/service/src/network.rs`: tonic-based network impl, append requests
  split by `raft_grpc_advisory_message_size`, `stream_append` implemented as a bidirectional
  `append_v002` gRPC stream with fallback to `stream_append_sequential`, snapshots sent as streamed
  KV entries in `try_chunks(64)` batches plus a final commit record (`install_snapshot_v004`),
  retry with fewer entries when the payload exceeds the gRPC limit, and the note that
  `NetStreamAppend` is blanket-implemented for `RaftNetworkV2`.
- Aeron properties and `!Send + !Sync` handles: see `AERON_SOLUTION.md` Appendix A.

Not verified, and therefore not relied on:

- gRPC and HTTP/JSON are both compiled and tested for correctness, but **neither has been
  benchmarked** in this repo; no candidate numbers exist.
- No latency budget was measured; all percentages in this document are guidance, not results.
- macOS behaviour of `dnctl`/Toxiproxy under load is untested here.
- Whether actix-web's h2c and reqwest's h2c interoperate cleanly through the project's middleware
  stack is untested.

## Appendix B — references

- openraft network API: <https://docs.rs/openraft/0.10.0-alpha.37/openraft/network/>
- openraft guide (Tonic recommendation): <https://databendlabs.github.io/openraft/getting-started.html>
- databend-meta reference network impl:
  <https://github.com/databendlabs/databend-meta/blob/main/crates/server/service/src/network.rs>
- tonic: <https://docs.rs/tonic/0.14.7/tonic/>
- reqwest HTTP/2: <https://docs.rs/reqwest/0.12.28/reqwest/struct.ClientBuilder.html>
- actix-web HTTP/2: <https://docs.rs/actix-web/4.13.0/actix_web/struct.HttpServer.html>
- Aeron deep dive in this repo: `AERON_SOLUTION.md`

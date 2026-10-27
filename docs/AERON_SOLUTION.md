# Aeron as the Raft transport — assessment and design

Status: **investigation only.** No Aeron code exists in this repo, nothing was benchmarked, and no
dependency was added. This document records what was verified, what the integration would look
like, and what should happen before anyone commits to it.

Date: 2026-10-27 · Baseline: `openraft` 0.10.0-alpha.37, gRPC/tonic transport (HTTP/JSON remains
selectable), segmented WAL + atomic snapshots.

> **Update after this investigation:** two of the prerequisites below have since landed. Peer-to-peer
> Raft RPCs now default to gRPC with `stream_append` and chunked snapshots
> (`src/network/grpc/`, `src/network/transport.rs`), and the log/state machine are durable
> (`src/store/log_store.rs`, `src/store/state_machine.rs`). Aeron is still **not** adopted. The
> remaining gate is a benchmark that shows the transport is the bottleneck.

---

## 1. Recommendation

**Do not move the Raft transport to Aeron yet.** The cheap wins that this document originally
recommended — a binary codec, pipelined appends, snapshot chunking and persistence — have been
implemented over gRPC. Re-open the Aeron decision only when a measured transport-bound target is
missed (§9.4).

If Aeron is a hard requirement, use it as a **transport-only layer under openraft** (§5) and not
as a replacement for openraft. Do not use Aeron Cluster: its Raft implementation is Java and there
is no Rust server-side equivalent (§3.3).

Why:

1. **It is not a drop-in.** Aeron is publish/subscribe, not request/response. openraft's
   `RaftNetworkV2` is a request/response API; bridging it needs a correlation layer, timeout
   handling, snapshot chunking, and a backpressure strategy — roughly 4 weeks of work (§7.1).
2. **The transport is probably not the bottleneck, and nobody has measured it.** After the gRPC
   port, replication is pipelined and snapshots stream, so the remaining cost is split between
   consensus, serialization and fsync. Switching the socket to Aeron without a harness would
   measure the wrong layer; run the Phase 0/1 benchmarks first (§8, §9.2).
3. **The operational cost is real.** Aeron needs a media-driver process per host, the `rusteron`
   handles are `!Send + !Sync` so every node needs a dedicated OS thread owning the client, and
   the failure modes (backpressure, `NOT_CONNECTED`, term-buffer wrap) all have to be mapped onto
   openraft's error model by hand.
4. **Durability came first and is now done.** The log is a segmented WAL and the state machine is
   snapshotted atomically, so restart recovery no longer blocks transport tuning. The next
   question is whether the optimized gRPC path is still transport-bound; that needs data.

---

## 2. Scope: three different things called "adopt Aeron"

| # | Interpretation | What it means here | Verdict |
|---|---|---|---|
| A | **Aeron as transport only** | Replace the gRPC/HTTP Raft RPC implementations with Aeron publications/subscriptions under the existing `RaftNetworkV2` impl. openraft stays. | The only version that is technically coherent. Design in §5. |
| B | **Aeron Cluster as the consensus layer** | Delete openraft and run the replication inside Aeron Cluster's Consensus Module. | Not possible in Rust. The cluster server is Java-only; a Rust process can only be a client. Would mean rewriting the service in Java. Out of scope. |
| C | **Aeron for client ingress only** | Gateways publish orders over Aeron; nodes subscribe and propose them to Raft. Raft RPC stays HTTP. | Viable later, orthogonal to this question, and does not satisfy "Raft transport". |

This document covers A, explains why B is a dead end, and parks C.

---

## 3. Verified facts

Everything in this section was checked against crate metadata, docs.rs for `rusteron` 0.2.13, or
the Aeron source tree (`aeron-io/aeron`, master, Oct 2026). Unverified or environment-specific
claims are labelled as such.

### 3.1 Rust ecosystem

| Crate | Version | Last update | Notes |
|---|---|---|---|
| `rusteron-client` | 0.2.13 | 2026-10-19 | FFI wrapper over the Aeron **C client**. Publications, subscriptions, handlers, counters. Actively maintained (GSR). |
| `rusteron-media-driver` | 0.2.13 | 2026-10-19 | Wraps the Aeron **C media driver**. Standalone or embedded. |
| `aeron-rs` | 0.1.8 | 2025-01-15 | Client only, stale relative to `rusteron`. |
| `aeron` | 0.2.0 | 2024-09-15 | Client only, stale. |
| `aeron-glide` | 0.4.0 | 2026-10-04 | C++ wrapper via `cxx`, different trade-off. |
| `ergo-aeron-cluster` | 0.1.34 | 2026-10-27 | Experimental **client** for Aeron Cluster. Explicitly client-only, "LLM-assisted and less tested than the Java reference". |

Conclusion: `rusteron` 0.2.13 is the only realistic building block, and it is a thin, largely
generated FFI layer (docs.rs reports ~35% documented). Nothing in the ecosystem provides
Raft-over-Aeron or a Rust cluster server.

### 3.2 Aeron transport properties that matter here

| Property | Value / behaviour | Consequence for Raft |
|---|---|---|
| Model | One-way pub/sub on `(channel, stream_id)` | No request/response. Correlation IDs and a pending-request map are ours to build. |
| Delivery | In-order, gap-detected and NAK-retransmitted **while both ends are active**. No store-and-forward when the subscriber is absent. | Raft already retries, so this is acceptable — but a down follower silently loses messages rather than queueing them. |
| Max message | `min(term_length / 8, 16 MiB)` | 16 MiB term → 2 MiB max message. AppendEntries batches and snapshots must be sized or split against this. |
| Defaults | term length 16 MiB, MTU 1408 | 18 publications at default size ≈ 288 MB of mapped buffers for a 3-node cluster (§5.8). |
| Term length bounds | 64 KiB … 1 GiB, power of two | Vote streams can use 64 KiB terms; snapshot streams need the big ones. |
| Flow control | Channel URI `fc` selects the strategy (min / max / preferred variants) | `offer` returns `BackPressured` when the window is full; the retry policy is ours. |
| Oversized offer | Rejected by Aeron (the publication has a `max_payload_length()`); not truncated | Must pre-check and split. This is a failure mode HTTP does not have. |
| Media driver | Separate process per host (JVM or C). `rusteron`'s embedded driver is documented as "best suited for integration tests". | New deployable, new supervision, new failure mode. |
| Security | Transport is plaintext; no auth/TLS in the transport itself | Parity with today's plain HTTP, but no improvement either. |
| `rusteron` handles | `Aeron`, `AeronPublication`, `AeronSubscription` are all `!Send + !Sync` (docs.rs auto-trait list) | Mandatory thread confinement. The async `RaftNetworkV2` impl cannot hold them. |

### 3.3 Aeron Cluster is not a drop-in consensus layer

Aeron Cluster ships a Raft implementation inside its Java Consensus Module. `ergo-aeron-cluster`
0.1.34 states plainly that it implements the **client side only** and "does not implement — and
never will — the cluster server: no consensus module (Raft), no clustered-service container, no
leader election, no snapshots/recovery". Adopting it would mean running the Java cluster and
writing this service in Java. That is a rewrite, not a transport change, and it is out of scope.

Vendor performance claims for Aeron Cluster (≈40 µs P99 at 500 k messages/s) are for that Java
stack on tuned hardware and are **not** a prediction for this repo.

### 3.4 Current baseline in this repo

- Transport: gRPC/tonic is the default (`src/network/grpc/`); HTTP/JSON is selectable with
  `--transport http` (`src/network/raft_network_impl.rs`). Both server sides stay mounted.
- Pipelining: gRPC implements `stream_append` over the bidirectional `AppendStream`; the HTTP
  transport still sends one AppendEntries at a time per peer.
- Codec: `postcard` payloads inside protobuf envelopes on gRPC; `serde_json` on the HTTP transport
  and the public API.
- Snapshot: 256 KiB chunks over a client-streaming `InstallSnapshot`, bounded by the `cancel`
  future; the HTTP transport sends one JSON body to `POST /snapshot`.
- Node address: `BasicNode { addr: String }` is used for **two** purposes — the public API /
  HTTP Raft endpoint and HTTP client forwarding (`MatchEngineClient::send_with_forwarding`,
  `src/client.rs`). The gRPC address is derived as `addr` port + 1000.
- Timing: heartbeat 500 ms, election 1500–3000 ms (`src/lib.rs`).
- Storage: durable segmented WAL + atomic snapshots; restart recovery is covered by
  `tests/cluster/test_restart.rs`.
- Benchmarks: none. Every latency comparison in this document is guidance.

---

## 4. Fit and gaps

**Aeron gives you:** brokerless, ordered per-stream messaging over UDP or shared memory (IPC),
per-peer flow control, message batching, and a natural fit for pipelined replication.

**Aeron does not give you:** RPC semantics, request/response correlation, retries with deadlines,
snapshot transfer, membership discovery, persistence, or a Rust consensus implementation.

Mapping the required openraft surface:

| openraft requirement | Aeron provides | Gap |
|---|---|---|
| `append_entries` → response | one-way broadcast | correlation + pending map + deadline |
| `vote` → response | one-way broadcast | same |
| `full_snapshot` (whole snapshot) | message up to 2 MiB by default | chunking + reassembly + acks + cancel |
| `RPCOption::soft_ttl()` deadline | none | timer per pending request |
| `RPCError::{Timeout,Unreachable,Network}` | offer result codes | explicit mapping table (§5.7) |
| ordered delivery, idempotent RPCs | ordered while connected | Raft RPCs are idempotent, so retransmits are safe |
| backpressure | `BackPressured` | retry with idle strategy, bounded by soft TTL |

---

## 5. Design (Option A, if it is ever built)

### 5.1 Threading model

```
┌─────────────────────── actix/tokio runtime ───────────────────────┐
│  RaftNetworkV2 impl (Send + Sync)                                  │
│      Network { transport: Arc<AeronTransport>, target: NodeId }    │
│      append_entries(): postcard-encode → transport.request(...)    │
└───────────────────────────────┬────────────────────────────────────┘
                                │ mpsc::Sender<Command> / oneshot
┌───────────────────────────────▼────────────────────────────────────┐
│  Aeron agent thread (owns every !Send handle)                      │
│    Aeron client · publications per (peer, kind) · subscriptions     │
│    loop: poll subscriptions → dispatch responses / Raft requests    │
│          drain command channel → offer() with retry/idle            │
└───────────────────────────────┬────────────────────────────────────┘
                                │ shared memory
                        ┌───────▼────────┐
                        │  media driver  │  (separate process; UDP)
                        └────────────────┘
```

`AeronTransport` is `Send` because it holds only channels and an `AtomicU64`; the Aeron handles
never leave the agent thread. One agent thread per **node**, not per peer — `new_client` must not
call `Aeron::connect` per peer (each client owns a conductor thread and its own shared-memory
mappings). Use the `async_add_publication` / `async_add_subscription` variants inside the agent
loop; the blocking `add_publication(uri, stream_id, timeout)` helpers are documented as
"convenience for examples and tests only".

### 5.2 Addressing and ports

`BasicNode.addr` must stay an HTTP address, because client forwarding uses it (`src/client.rs`).
Two options:

- **Pilot:** convention — `aeron_udp_port = http_port + 2000` (21001 → 23001), because
  `http_port + 1000` is already the gRPC Raft port. No API change, but duplicated knowledge and
  fragile.
- **Production:** a custom `MatchEngineNode { http_addr, grpc_addr, aeron_addr }` implementing
  `openraft::Node` (replacing `BasicNode`), plus a serde-compatible upgrade path for `/init` and
  `/add-learner` payloads, which today take `(NodeId, String)`.

Each node subscribes on its own UDP endpoint (`aeron:udp?endpoint=0.0.0.0:23001`) per stream, and
publishes to `aeron:udp?endpoint=<peer_host>:23001`. Note the asymmetry: the subscription URI
carries the *local* bind address, the publication URI the *remote* one.

For the local 3-node test, one embedded driver can serve all three nodes with distinct UDP ports;
production needs one driver process per host, started before the nodes.

### 5.3 Stream layout

One request stream and one response stream per RPC class, so a large append batch cannot delay a
vote (head-of-line blocking during elections), and so snapshot traffic gets its own term size.

| Stream ID | Direction | Kind | Suggested term length | Max message |
|---|---|---|---|---|
| 1001 | peer → node | append request | 4 MiB | 512 KiB |
| 1002 | node → peer | append response | 4 MiB | 512 KiB |
| 1003 | peer → node | vote request | 64 KiB | 8 KiB |
| 1004 | node → peer | vote response | 64 KiB | 8 KiB |
| 1005 | peer → node | snapshot chunk | 16 MiB | 2 MiB |
| 1006 | node → peer | snapshot ack | 64 KiB | 8 KiB |

Alternative: one bidirectional pair per peer with a `kind` byte in the header. Six times less
mapped memory, but a snapshot chunk can block a vote behind it. Not recommended.

### 5.4 Wire format

Little-endian fixed header, then a `postcard`-encoded payload (JSON only for `data` inside the
snapshot, where it already is today):

```text
offset  size  field
0       4     magic  "ARPC"
4       1     version (1)
5       1     kind    (1=append, 2=vote, 3=snapshot_chunk, 4=response, 5=snapshot_ack)
6       2     flags   (bit 0 = first chunk, bit 1 = last chunk)
8       8     correlation_id (u64)
16      8     deadline_ns    (u64, sender's monotonic clock; informational)
24      4     payload_len    (u32)
28      ...   payload
```

Publishing with `publication.offer_parts(&[&header, &payload])` avoids concatenating header and
body. Replies carry the request's `correlation_id` and `kind=4`; the payload is
`postcard::to_allocvec(&Result<Response, RaftError>)` so the remote error survives the trip and can
be mapped exactly as the HTTP code maps it today.

### 5.5 Request/response correlation

```rust
// PSEUDOCODE — illustrates the shape, not compilable.
struct AeronTransport {
    cmd: mpsc::Sender<Command>,
    next_id: AtomicU64,
}

async fn request(&self, target: NodeId, kind: Kind, payload: Vec<u8>, ttl: Duration)
    -> Result<Vec<u8>, RpcError>
{
    let id = self.next_id.fetch_add(1, Relaxed);
    let (tx, rx) = oneshot::channel();
    self.cmd.send(Command::Send { target, kind, id, payload, reply: tx }).await?;
    match timeout(ttl, rx).await {
        Ok(Ok(res)) => res,
        Ok(Err(_))  => Err(RpcError::Unreachable),   // agent gone
        Err(_)      => Err(RpcError::Timeout),        // soft_ttl elapsed
    }
}
```

The agent keeps `HashMap<(NodeId, u64), oneshot::Sender<...>>`. Responses are matched on
`(peer, correlation_id)`; unknown ids are dropped and logged (late response after a timeout).
Raft RPCs are idempotent, so duplicate delivery is safe; duplicate *responses* are dropped by the
same lookup.

### 5.6 Snapshot transfer

`full_snapshot` receives the entire `Cursor<Vec<u8>>` and a cancel future. Chunking is internal to
the network implementation:

1. Split `data` into `chunk = publication.max_payload_length() - HEADER` slices (typically ~1 MiB
   with a 16 MiB term).
2. First chunk carries `vote + meta`; every chunk carries `transfer_id` (= `last_log_id` index +
   an attempt counter) and `offset`/`total_len`.
3. Keep a bounded window (e.g. 8 chunks) in flight; the follower acks each chunk with the next
   expected offset so a stalled follower applies backpressure instead of filling the term buffer.
4. Poll the `cancel` future between chunks; on cancel, stop and return
   `StreamingError::ReplicationClosed`.
5. Follower reassembles by `transfer_id`, validates `total_len` and the meta, then calls the
   existing `Raft::install_full_snapshot` path. Partial state is discarded on timeout or a new
   `transfer_id`.
6. On any failure the leader restarts at offset 0 — openraft retries `full_snapshot` anyway, and
   the whole snapshot is in memory at the leader, so a restart is cheap here.

A single-message fast path is worth keeping for snapshots smaller than
`max_payload_length()`; the test cluster's snapshots are tiny.

### 5.7 Error mapping

| Aeron condition | openraft error | Rationale |
|---|---|---|
| `NotConnected` (no image yet) | `RPCError::Unreachable` | openraft backs off, avoids log spam; matches the current `is_connect()` branch. |
| `BackPressured`, `AdminAction` | retry until `soft_ttl`, then `RPCError::Timeout` | Bounded; never blocks the Raft core. |
| `Closed`, `MaxPositionExceeded` | `RPCError::Network` + recreate publication | Transport is broken; next attempt gets a fresh handle. |
| no response within `soft_ttl` | `RPCError::Timeout` | Deadline is per request, mirroring `reqwest`'s `.timeout()`. |
| decode failure | `RPCError::Network` | Same as a malformed HTTP body today. |
| remote `RaftError` (higher term, etc.) | `RPCError::Network(NetworkError::new(&e))` | Exactly what `src/network/raft_network_impl.rs` does now; `RPCError<C>` has no remote variant. |

### 5.8 Memory sizing (why term lengths matter)

Aeron maps a log buffer per publication, sized by term length. With six streams per peer pair:

- Default 16 MiB terms, 3 nodes: 3 × 2 peers × 3 outbound streams × 16 MiB ≈ **288 MiB** just for
  publications (round to ~300 MB, plus the driver's own mappings).
- Tuned as in §5.3: 6 × 64 KiB + 6 × 4 MiB + 6 × 16 MiB ≈ **120 MiB**.
- For 5 nodes the tuned figure grows to ~400 MB (20 directed pairs).

A single-stream-per-peer design divides this by six but reintroduces head-of-line blocking.

### 5.9 `RaftNetworkV2` sketch

```rust
// PSEUDOCODE — the real impl must respect the trait's exact signatures (openraft 0.10).
impl<C: RaftTypeConfig<Node = BasicNode>> RaftNetworkV2<C> for Network<C> {
    type SnapshotData = Cursor<Vec<u8>>;

    async fn append_entries(&mut self, rpc: AppendEntriesRequest<C>, opt: RPCOption)
        -> Result<AppendEntriesResponse<C>, RPCError<C>>
    {
        let bytes = postcard::to_allocvec(&rpc).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        let raw = self.transport.request(self.target, Kind::Append, bytes, opt.soft_ttl()).await?;
        postcard::from_bytes(&raw).map_err(|e| RPCError::Network(NetworkError::new(&e)))
    }

    // vote: identical shape with Kind::Vote.
    // full_snapshot: §5.6.
    // stream_append: implement over the same publication once the unary path is stable.
}
```

### 5.10 Repo changes

| File | Change |
|---|---|
| `Cargo.toml` | `rusteron-client`, `rusteron-media-driver` (`static` + `precompile`); `postcard`/`crc32fast` are already dependencies |
| `src/network/grpc/client.rs` | Where the Aeron transport would plug in today; keep the gRPC and HTTP impls selectable |
| `src/network/raft.rs` | `POST /append` / `/vote` / `/snapshot` stay for dual-transport migration |
| `src/network/aeron/{mod,agent,rpc,codec,snapshot}.rs` | New module |
| `src/network/mod.rs` | Add `Transport::Aeron`; media-driver dir and endpoint config |
| `src/client.rs` | Forwarding still uses `BasicNode.addr` (HTTP) — unchanged |
| `tests/cluster/test_cluster.rs` | Must start a media driver; fixed ports 21001–21003 plus gRPC 22001–22003 and UDP 23001–23003 |

This table was written before the gRPC port; the integration point is now the gRPC transport, not a
new HTTP feature flag.

---

## 6. Options

| Option | Latency | Throughput | Complexity | Ops burden | Time to value |
|---|---|---|---|---|---|
| **A. HTTP/JSON baseline** (legacy, still selectable) | baseline | baseline | none | none | — |
| **B. gRPC + postcard + `stream_append` + chunked snapshots** (**implemented**) | large serialization win; pipelined replication | large gain on bursts | medium | none | done |
| **B′. Raw TCP** — length-prefixed frames, own connection pool | near-B | high, pipelined | medium | low | ~1 week |
| **D. Aeron for Raft** | marginal vs B/B′ on loopback; better tail under load | high, natural pipelining | **high** | media driver + FFI | ~4 weeks |
| **E. Aeron Cluster** | vendor-claimed best | vendor-claimed best | rewrite in Java | Java cluster | months |
| **F. Aeron at ingress** | helps order entry | helps ingress | medium | media driver | 2–3 weeks |

Option B is where the measurable wins were, and it is now in the tree. Note that Aeron's strongest
Raft-specific advantage is not raw socket latency — it is that pipelining many AppendEntries per
peer is natural. gRPC's `stream_append` already provides most of that without a media driver or FFI.

---

## 7. Costs and risks

### 7.1 Effort (one experienced Rust engineer, no prior Aeron experience)

| Work item | Days |
|---|---|
| Media driver up, two clients exchanging messages, build on macOS | 1–2 |
| Envelope, codec, correlation map, deadlines | 3–5 |
| `append_entries` + `vote` end to end, error mapping | 3–4 |
| Snapshot chunking, acks, cancel, install | 3–5 |
| Addressing/config/dual-transport flags | 2 |
| Failure injection (kill follower, stall follower, drop driver, restart) | 3–5 |
| Benchmarks and tuning | 2–3 |
| **Total** | **≈20 engineer-days (~4 weeks)** |

Plus permanent cost: a media driver to package and supervise, `unsafe` FFI in the dependency tree,
and one more moving part in every failure investigation.

### 7.2 Risks

| Risk | Severity | Mitigation |
|---|---|---|
| `rusteron` is a thin FFI wrapper, partially documented; the sibling cluster crate warns of "undefined behaviour, segfaults, or data loss" | High | Pin the version, isolate it behind one module, run under ASan in CI, keep the HTTP transport selectable |
| `!Send + !Sync` forces a thread hop per RPC | Medium | One agent thread per node; measure the channel cost before committing |
| Backpressure interacts with election timeouts: a stalled follower must not stall votes | High | Separate vote stream; bounded retries; always return `Timeout` rather than block |
| Snapshot chunking is new, failure-prone code on the recovery path | High | Keep the single-message fast path; property-test reassembly; test with a real 4-node cluster as was done for the current snapshot path |
| Term-buffer wrap loses messages while a follower is down | Low | Raft retries; document that Aeron is not store-and-forward |
| Media driver becomes a single point of failure per host | Medium | Supervise it, alert on Aeron counters, fail the node fast if the driver dies |
| UDP blocked / NAT in cloud deployments | Medium | Validate on the target network before committing |
| Memory: mapped term buffers per publication per peer | Medium | Tuned term lengths (§5.8); cap cluster size assumptions |
| Two transports in one cluster cannot interoperate | High | Dual-serve both during migration; cut over per node only after all nodes listen on both |
| `BasicNode.addr` serves Raft RPC *and* HTTP forwarding | Medium | Port convention for the pilot, structured node type for production |
| No baseline means no way to prove the win | High | Phase 0 benchmarks before any Aeron code |

---

## 8. Staged plan (only if the gate in §9.4 is met)

**Phase 0 — measure (1–2 days).** Harness that drives `RaftNetworkV2` directly on 3 nodes,
loopback and 1 ms RTT, 1 KiB and 64 KiB payloads, 1 and 300 entries per batch, recording p50/p99/p999
for `append_entries` and `vote`, plus writes/s through `Raft::client_write`. Run it against both the
gRPC and HTTP transports.

**Phase 1 — optimise the existing transport (done, but unmeasured).** `postcard` payloads,
`stream_append`, and chunked snapshots now exist over gRPC; the public API stays JSON. The missing
deliverable is the Phase 0 numbers for this path.

**Phase 2 — Aeron spike (5 days, throwaway).** Embedded driver, two processes, one stream,
`offer`/`poll`, correlation map, kill/restart a peer. Re-run the Phase 0 harness through the same
interface. Decision point: if p99 is not clearly better than Phase 1, stop.

**Phase 3 — integrate (2–3 weeks).** `append_entries`, `vote`, then snapshot chunking. Add
`Transport::Aeron` and dual-serve both during rollout.

**Phase 4 — cut over.** Only after a full cluster test that includes a follower joining via
snapshot, a leader kill, and a driver restart.

**Rollback:** keep the gRPC and HTTP impls compiled and selectable; the flag flips back without a
rebuild.

---

## 9. Decision gate

### 9.1 Preconditions for adopting Aeron

All of these must hold:

1. Persistence exists and is benchmarked (otherwise latency numbers are meaningless). **Persistence
   is done; its cost has not been measured in isolation.**
2. Phase 0 and Phase 1 baselines exist, with p99 numbers on the target network. **Phase 1 code is
   done; the numbers do not exist yet.**
3. The transport is measurably the bottleneck — not serialization, not fsync, not the state
   machine.
4. Someone owns media-driver operations (packaging, supervision, upgrades, counters).
5. The team accepts `unsafe` FFI in the dependency tree and a new failure domain.

### 9.2 Signals that would justify it

- Sustained replication throughput is capped by per-RPC overhead even with `stream_append`, after
  storage and codec costs are subtracted.
- p99 `append_entries` latency remains dominated by HTTP/TLS/JSON overhead after Phase 1.
- The deployment is latency-critical order entry where ingress (Option F) and replication both
  need sub-millisecond tails.

### 9.3 Signals that would not

- "Aeron Cluster does 40 µs" — different stack, different hardware, different consensus code.
- "Aeron is faster than HTTP" — true in the abstract, irrelevant if JSON + fsync dominate.
- "We want to remove a dependency" — Aeron adds several.

### 9.4 Concrete trigger to re-open this document

Re-open when **either**:

- Phase 1 p99 `append_entries` (64 KiB, 300 entries) is **> 2×** the p99 transport-independent
  floor measured in-process, **or**
- write throughput plateaus below the target while CPU is idle on the leader (i.e. waiting on the
  network rather than working).

Otherwise, close this as "investigated, not adopted".

---

## 10. What to do instead, now

Ordered by value per day:

1. ~~**Persist the log and state machine.**~~ Done: segmented WAL + atomic snapshots, with restart
   tests.
2. ~~**Binary codec for Raft RPCs.**~~ Done: `postcard` inside protobuf envelopes; JSON stays at the
   public API edge.
3. ~~**`stream_append`.**~~ Done over gRPC (`AppendStream`); the HTTP transport stays sequential.
4. ~~**Snapshot chunking + streaming.**~~ Done: 256 KiB chunks over client-streaming
   `InstallSnapshot`.
5. **Benchmark harness.** Still missing, and now the only gate in §9.4. Build the L1/L2 harness in
   `docs/TRANSPORTS.md` §5 and record gRPC vs HTTP/JSON.
6. **TLS/mTLS for the gRPC listener.** Not transport tuning, but the next production blocker.

---

## Appendix A — verification log

Checked, not assumed:

- `rusteron-client` / `rusteron-media-driver` 0.2.13: crates.io metadata, docs.rs API pages
  (`Aeron::connect`, `add_publication(uri, stream_id, timeout)`, `add_subscription(...)`,
  `AeronPublication::offer(&[u8]) -> Result<i64, AeronOfferError>`, `max_payload_length()`,
  `AeronSubscription::poll_fn(...)`, `AeronOfferError::{NotConnected, BackPressured, AdminAction,
  Closed, MaxPositionExceeded}` with `is_retryable`/`is_fatal`), auto-trait lists (`!Send`,
  `!Sync`), installation notes (dynamic vs `static` + `precompile`), media-driver guidance
  ("embedded … best suited for integration tests").
- `ergo-aeron-cluster` 0.1.34 docs: client-only, no consensus module, "Java process is the
  cluster".
- Aeron source (`aeron-io/aeron` master): `FrameDescriptor.MAX_MESSAGE_LENGTH = 16 MiB`,
  `computeMaxMessageLength = min(termLength >> 3, MAX_MESSAGE_LENGTH)`,
  `LogBufferDescriptor.TERM_MIN_LENGTH = 64 KiB`, `TERM_MAX_LENGTH = 1 GiB`,
  driver defaults `TERM_BUFFER_LENGTH_DEFAULT = 16 MiB`, `MTU_LENGTH_DEFAULT = 1408`,
  flow-control URI param `fc`.
- Aeron wiki, *Message Delivery Assurances*: reliable in-order delivery while both ends are
  active, NAK recovery, no store-and-forward when the subscriber is absent.
- openraft 0.10.0-alpha.37 source: `RaftNetworkV2` required/optional methods, `RPCOption::soft_ttl`,
  `SnapshotData` on `RaftStateMachine`/`NetSnapshot`, `RPCError` variants.

Not verified, and therefore not relied on:

- No `rusteron` code was compiled or run in this repo.
- No performance measurement of any kind was taken.
- macOS behaviour of the C media driver under this workload is untested.
- Exact openraft message sizes for real order-book workloads are unknown.
- C driver vs Java driver parity for the counters used in the proposed monitoring.

## Appendix B — references

- rusteron: <https://github.com/gsrxyz/rusteron> · <https://docs.rs/rusteron-client/0.2.13/>
- Aeron: <https://github.com/aeron-io/aeron> · <https://aeron.io>
- Aeron delivery assurances: <https://github.com/aeron-io/aeron/wiki/Message-Delivery-Assurances>
- Aeron Cluster (Java): <https://aeron.io/docs/distributed-systems-basics/consensus-algorithms/>
- ergo-aeron-cluster (client-only prototype): <https://docs.rs/ergo-aeron-cluster/0.1.34/>
- openraft 0.10 network API: <https://docs.rs/openraft/0.10.0-alpha.37/openraft/network/>

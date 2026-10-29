# Aeron in matchengine-raft — Raft transport assessment and market-data egress design

Status: **investigation and design only.** No Aeron code exists in this repo, nothing was
benchmarked, and no dependency was added. Part I (sections 1–10) assesses Aeron as the Raft
transport and says not yet. Part II (sections 11–26) designs the market-data egress path — trades,
spot/top-of-book, market depth and lifecycle events published to downstream subscribers — with all
derivation, encoding and IO decoupled from the matching thread, and recommends Aeron there. Both parts record what was verified, what the integration would look like,
and what should happen before anyone commits to it.

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

**For market-data egress the answer is the opposite: adopt Aeron (Part II).** Publishing one-way,
ordered updates to many subscribers is exactly Aeron's model; there is no RPC correlation or
consensus code to bridge, the feed is derived from state the engine already computes, and a slow
consumer can be isolated behind a bounded queue. That decision is independent of the Raft-transport
gate in §9.4 and should not be read as one.

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

This document covers A, explains why B is a dead end, and parks C. Part II adds a fourth use —
**Aeron for market-data egress** — which is orthogonal to all three: it leaves consensus and the Raft
RPC transport untouched and only publishes derived data.

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
7. **Market data.** Independent of the items above and recommended regardless of how §9.4 resolves:
   see Part II for the egress design (Aeron, leader-published, snapshot + deltas).

---


---

# Part II — Market-data egress over Aeron

Status: **core implemented; Aeron adapter pending.** The deterministic pipeline below exists in
`src/market_data/`: depth journal, tap, shadow book, OHLC bars, entry-driven sequencing, snapshots,
leader gating, JSONL sink, the binary wire encoder and `/read marketdata_status`. U8 (deterministic
`market_seq`/`sub_seq` keys) is implemented and tested; the Aeron transport (`rusteron` + media
driver), Archive replay and multi-symbol routing remain. Progress and acceptance criteria:
`docs/MARKET_DATA_PLAN.md`. Every Aeron transport property quoted here comes
from §3.2; nothing in this part has been benchmarked.

## 11. Recommendation

**Publish the engine's market data over Aeron.** Unlike the Raft transport, this workload matches
Aeron's design point:

1. **One-way, ordered, multi-consumer.** Trades, top-of-book and depth are a fan-out feed, not an
   RPC. Aeron gives ordered delivery per stream, NAK recovery and per-subscriber flow control with
   no broker process in the hot path.
2. **Generation can be fully decoupled from matching.** `apply` already computes every fill and
   lifecycle event, and every book mutation already knows the level and volume it changes. The
   matching thread therefore only appends those already-known values to a preallocated journal —
   no book scans, no aggregation, no serialization, no IO. A separate market-data thread owns the
   derived state (shadow level book, BBO, snapshots), does all encoding, and talks to Aeron. With
   the feed disabled the tap is one predictable branch, so matching cost is unchanged; that
   isolation is a hard requirement of the design, not an optimization (§13, §14).
3. **Back-pressure is isolatable.** A slow subscriber must never stall `apply`; the queue plus a
   dedicated publisher thread enforces that, matching the existing rule that a sink failure never
   fails `apply`.
4. **Loss is recoverable.** Aeron does not store-and-forward (§3.2), so the design pairs the feed
   with periodic snapshots, gap markers and an optional Archive — and the Raft log remains the
   authoritative record, so nothing is lost even when the feed drops a frame.

Scope:

| In scope | Out of scope |
|---|---|
| Trades / last trade, spot/top-of-book, OHLC bars, L2 depth (MBP-N) snapshots and deltas, lifecycle events, feed status | Replacing the Raft transport (Part I) |
| Leader-published feed with deterministic, log-derived sequencing | Private order entry over Aeron (Option C in §2) |
| Optional Archive replay and resync | Durable clearing-grade fill delivery (`docs/MATCH_RESULTS.md` owns that) |

Keep the two Aeron decisions separate: Part I is gated on a transport benchmark; this part is gated
on wanting a low-latency public feed.

## 12. Products and their source of truth

| Product | Contents | Delivery mode | Boundary / conflation |
|---|---|---|---|
| `TRADE` | price, volume, aggressor side, taker/maker sequences | complete within a session | one message per taker, all fills, uncapped (§17) |
| `BBO` | last trade ("spot"), best bid/ask with **level** volume, mid, spread | latest-value | one per book-changing frame |
| `BAR` | OHLCV over a fixed interval, plus trade count | aggregated | closed + live bars from event-time windows |
| `DEPTH_SNAPSHOT` | top-N levels per side: price, total visible volume, order count | periodic | takeover reset boundary |
| `DEPTH_DELTA` | post-batch **total** visible volume for each touched level | per frame | one message per entry with all touched levels |
| `LIFECYCLE` | every match event, including `rejected` with its reason | per frame | one message per entry; `rested` shed above the cap |
| `STATUS` | feed position, applied index, book sequence, pending stops | diagnostic | every `status_every` (default 100) sequences |

Message boundaries are entry-driven and deterministic; there is no wall-clock throttling on the
data streams. Downstream conflation (Kafka compaction, latest-value caches, OHLC roll-ups) is where
rate control belongs.

**One time source.** Every timestamp in the feed — trade prints, depth and BBO frames, lifecycle
events and OHLC windows — is the taker order's `timestamp` (§13), not a publish-time clock. The
receiving leader **stamps it at intake and overwrites any client-supplied value**, so there is one
engine clock and clients cannot skew event time; GTD validation uses the same value. §17 lists which
request supplies it when there is no taker (cancel, expiry).

**"Spot price" is ambiguous, so `BBO` carries all candidates:** `last_trade` (the engine's canonical
price — it is also what elects stops), `mid = (bid + ask) / 2`, and the best bid/ask. Consumers pick
their definition; the engine's own notion of spot is the last trade price.

**Depth counts visible volume only.** An iceberg's `hidden_volume` is invisible to matching and to
the book, so it is invisible to the feed too (consistent with `OrderBook::resting_volume`). Parked
stops are not depth either; they appear only as `LIFECYCLE` events (`triggered`).

## 13. Constraints this design respects

1. **Determinism and a single time source.** `apply` runs on every replica and may not read a
   clock or the network. Feed content is a pure function of applied state (through the tap journal)
   plus the entry's events, and **every market-data timestamp is the taker order's `timestamp`**,
   carried in the replicated request. The leader stamps it at intake and overwrites any
   client-supplied value, before sequencing; `apply` itself never reads a clock. Trades, depth
   changes, BBO states and OHLC windows are therefore identical on every replica.
   `EventId { log_index, ordinal }` remains the canonical audit key of the applied log; the feed's
   dedupe identity is `(market_seq, sub_seq)` (§16.1). The timestamp buckets and labels events, it
   never orders them (a stalled or skewed clock cannot reorder the tape). The two-replica
   byte-diff test (§22) is the guard.
2. **`apply` holds the state-machine lock, so the feed must not extend that critical section.**
   `publish_match_events` runs inside `apply` under `inner.lock().await`, and Aeron `offer` can
   back-pressure, block on the client conductor or return retryable errors. The matching thread
   therefore does no derivation at all: it hands over the event batch and a small depth journal
   through a preallocated bounded ring (`try_push`, never blocking, never awaiting). The
   market-data thread does everything else. A feed failure is logged and never fails `apply`.
3. **Every replica applies; the leader publishes.** The identity is frame-derived
   `(symbol_id, stream_id, market_seq, sub_seq)` (§16.1), so it is identical on every replica and
   across a leader change; the brief dual-publisher window during a handover is harmless under
   dedupe. The tap is gated at the source on leader-only nodes.
4. **No store-and-forward.** A subscriber that is absent or falls behind the term buffer loses
   messages (§3.2). Periodic snapshots, gap markers and optional Archive replay are therefore part
   of the design, not extras.
5. **Reading the live book from the feed path would tax matching.** `bids`/`asks` are per-order
   maps (`BTreeMap<(price, sequence), Order>`) with no aggregated level index, so a level total or
   a top-N snapshot requires tree scans. The design never does that inside `apply`: the market-data
   thread keeps **shadow level books** (one per instrument) fed by the tap's journal, and a seeder
   task captures the
   real book only at startup, on leadership gain and (optionally) for reconciliation. The per-price
   bucket index in `TODO.md` §9 stays a matching-side optimization; the feed does not depend on it.
6. **Prices and quantities are scale-18 integers.** Wire v3 encodes every financial value as a
   little-endian `i128` at 18 fractional digits; no public financial output crosses `f64`.
7. **Multi-symbol is planned.** Today the state machine owns one book, but the next step is one
   book per instrument inside the same state machine. The feed is designed for that from day one:
   every message carries `symbol_id`, and all derived state — shadow books, OHLC aggregators, depth
   snapshots, BBO — is keyed by it. Until the engine learns symbols, every message carries
   `symbol_id = 0`; adding symbols later changes routing, not the schema. (Prerequisite on the
   engine side: `symbol_id` on the order/request and a per-symbol book; the feed does not invent
   it.)
8. **The disabled feed must be free.** The journal is optional and transient (`#[serde(skip)]`,
   not in snapshots); with no market-data sink configured each recording point is one predictable
   branch and nothing else. A benchmark gate (§22) compares matching with the feed on and off, so a
   regression is caught in CI rather than discovered in production.

## 14. Architecture

```
                     matching thread (state-machine lock held)
┌──────────────────────────────────────────────────────────────────────────┐
│ StateMachineStore::apply                                                 │
│   MatchEvent batch ──────────────► MatchResultSink ──► JSONL/…  [as is]  │
│   depth journal (side, price, Δ) ─┐   ← O(1) per book mutation, no scans  │
│   events (Copy) ─────────────────┤                                      │
│        try_push: preallocated, bounded, non-blocking, never awaited      │
└──────────────────────────────────┼───────────────────────────────────────┘
                                   │ lock-free ring
┌──────────────────────────────────▼───────────────────────────────────────┐
│ market-data thread (owns every !Send rusteron handle; takes no locks)    │
│   shadow books per symbol ← journal (authoritative for the feed)         │
│   BBO/spot · depth totals · entry batching · snapshot assembly · encoding │
│   leader gate · snapshot boundary · gap policy · Archive (optional)      │
│   TRADE 2001 · BBO 2002 · DEPTH 2003/2004 · LIFECYCLE 2005 · STATUS 2006  │
└──────────────────────────────────┬───────────────────────────────────────┘
                                   │ shared memory
                          ┌────────▼─────────┐   IPC / UDP / multicast
                          │   media driver   │ ──────────────► subscribers
                          └──────────────────┘

seeder task (rare: startup, leadership gain, optional reconciliation)
   lock state machine → aggregate N levels → hand to the market-data thread
```

**Three parts, only one of them touches matching:**

- **Tap (matching thread, inside `apply`).** Copies the entry's `MatchEvent` batch (which is `Copy`
  and already built), the request's timestamp (the taker order's, or the cancel/expiry request's),
  and appends `(side, price, delta)` entries to a reusable journal at the points where the book
  already changes. O(1) per mutation, no tree walks, no serialization, no clock, no
  allocation once warm. The journal is cleared per batch; the queue is preallocated.
- **Market-data thread.** Owns the per-symbol shadow books, derives BBO/last trade, applies deltas,
  assembles snapshots, encodes messages, emits one batch per applied entry, offers to Aeron, retries
  back-pressure, counts gaps, and (optionally) records to an Archive. All CPU-heavy and IO work sits
  here.
- **Seeder task.** The only component that reads the engine's book: it locks the state machine at
  startup, on leadership gain, and (optionally) on a reconciliation timer, aggregates the top-N
  levels and hands the map to the market-data thread. It never runs per entry.

**Isolation rules (the "must not downgrade matching" contract):**

1. `apply` never awaits the queue, the publisher or Aeron: `try_push` only; a full queue drops the
   frame and marks a gap, it never blocks the state machine.
2. Derivation is one-way. Nothing on the market-data side can write to the book, propose a log
   entry, or fail `apply`; the queue is the only shared object.
3. The matching loop does no aggregation, no snapshotting, no encoding and no allocation for the
   feed. Everything expensive is off-thread and recoverable from the snapshot/log.
4. The feed can lag or drop without correctness consequences; the Raft log remains the record.
5. With the feed disabled the tap is inert: one branch per recording point, no journal writes.
   `cargo bench --bench matchengine` gates this (§22).

Why a shadow book instead of reading the engine's book: it removes lock contention and all per-batch
tree scans from the critical section, and it is the only way to keep depth exact for iceberg
refills, which the event stream alone does not describe (the sweep knows the refilled tranche; the
shadow book receives it as a journal delta). A dedicated thread also keeps the `!Send` `rusteron`
handles off the Raft runtime (§3.2).

## 15. Channels and streams

| Stream | Product | Message size | Term length | Notes |
|---|---|---|---|---|
| 2001 | `TRADE` | ~69 B per fill | 2 MiB | one message per taker, all fills (uncapped; size the Aeron payload limit) |
| 2002 | `BBO` | ~115 B | 1 MiB | one per book-changing entry |
| 2003 | `DEPTH_SNAPSHOT` | ≤ ~1 KiB @ N=20 | 4 MiB | takeover/startup reset + resync |
| 2004 | `DEPTH_DELTA` | ~37 B per touched level | 4 MiB | one message per entry with touched levels |
| 2005 | `LIFECYCLE` | ~71 B | 2 MiB | may be private (see §21) |
| 2006 | `STATUS` | ~64 B | 256 KiB | deterministic cadence, gap flags |
| 2007 | `BAR` | ~120 B | 1 MiB | OHLCV, one message per interval/window emitted |
| 2010 | snapshot request (subscriber → engine) | ~24 B | 256 KiB | phase 3, optional |

**Ports:** `http_port + 3000 + stream index` (21001 → 24001…24006) keeps the existing conventions
(`+1000` gRPC, `+2000` reserved for the Part I pilot). Encoded in `MarketDataConfig`, overridable by
flag; tests pin them.

**Channel choice:**

- `aeron:ipc` when the consumer is co-located — lowest latency, no UDP path.
- unicast `aeron:udp?endpoint=<host>:<port>` for a single downstream collector.
- multicast `aeron:udp?endpoint=<group>:<port>|interface=<iface>` for many consumers. Cloud VPCs
  often disable multicast; validate before promising fan-out, otherwise run N unicast publications
  or a single collector that bridges to the bus (§19).

One publication per stream so a large depth batch cannot head-of-line-block a trade or status.
Streams are **multiplexed by `symbol_id`** — the six streams serve every instrument, so a
multi-symbol engine does not multiply the channel count. Publications are created lazily on
leadership gain (`add_exclusive_publication`, single writer) and closed on loss. Memory: six
publications × 1–4 MiB terms ≈ 8–24 MiB per node plus the driver's mappings; followers keep theirs
closed but still run a driver because leadership can move.

**Fan-out at 10–100 subscribers.** A unicast publication is one-to-one, so the publisher must not
create one per subscriber. Use multicast when the network allows it; otherwise run a single
collector/relay (or the Kafka/NATS bridge in §19) that subscribes once and fans out. Members of the
10–100 range are exactly where this choice matters: 100 unicast publications from the engine is a
configuration error, not a scaling plan.

## 16. Wire format

Common header (64 bytes, little-endian) then the body:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic `AMKT` |
| 4 | 1 | `version` = 3 |
| 5 | 1 | `msg_type` (1 trade, 2 bbo, 3 depth snapshot, 4 depth delta, 5 lifecycle, 6 status, 7 gap, 8 bar) |
| 6 | 2 | `flags` (bit0 snapshot, bit1 reset, bit2 gap, bit3 end-of-batch, bit4 reserved, bit5 partial bar, bit6 bar close) |
| 8 | 8 | `market_seq` — replicated order/mutation sequence (§16.1) |
| 16 | 8 | `log_index` |
| 24 | 4 | `ordinal` |
| 28 | 4 | `body_len` |
| 32 | 8 | `epoch` — Raft term (leader generation) |
| 40 | 8 | `leader_id` |
| 48 | 8 | `ts_ms` — **authoritative** event time: the taker order's `timestamp`, or the max in a batch |
| 56 | 4 | `symbol_id` (0 until multi-symbol lands; see §13.7) |
| 60 | 2 | `sub_seq` — index within `(stream, market_seq)`; 0 for a frame message, 1 for the reset BBO, `u16::MAX` for `GAP` |
| 62 | 2 | reserved (0) |
| 64 | … | body |

Rules:

- `TRADE` and `LIFECYCLE` messages carry one entry's records (ascending `(log_index, ordinal)`);
  the header's position is the last record's, each body record carries its own;
- one message per entry for `DEPTH_DELTA` (post-entry totals) and for `BBO`;
- `(log_index, ordinal)` remains the audit position; the dedupe identity is
  `(symbol_id, stream_id, market_seq, sub_seq)` (§16.1). `market_seq` is the replicated
  order/mutation sequence and `sub_seq` is deterministic within `(stream, market_seq)`;
- `ts_ms` is never generated at publish time: it is the mutating request's log-carried intake
  `timestamp` (a bar carries its window end);
- bodies are fixed structs; variable arrays are `u32` counts followed by packed records, to keep
  golden tests simple and leave no truncation risk for an uncapped `TRADE`;
- additive fields are allowed only in `STATUS`; anything else bumps `version`.

Bodies:

| Type | Fields |
|---|---|
| `TRADE` | `count u32`, then per fill: `log_index u64`, `ordinal u32`, `ts_ms u64`, `price i128`, `volume i128`, `taker_side u8` (0 = buy aggressor, 1 = sell), `taker_seq u64`, `maker_seq u64` |
| `BBO` | `present u8` + `last_trade i128`, `bid i128`, `bid_volume i128`, `ask i128`, `ask_volume i128`, `present u8` + `mid i128`, `present u8` + `spread i128` |
| `DEPTH_DELTA` | `count u32`, then `[side u8, price i128, total_visible i128, orders u32]` per touched level |
| `DEPTH_SNAPSHOT` | `bid_levels u32`, `ask_levels u32`, then level records, best first |
| `LIFECYCLE` | `count u32`, then per event: `log_index u64`, `ordinal u32`, `ts_ms u64`, `kind u8`, `order_sequence u64`, `counterparty u64` (0 = none), `price i128`, `volume i128`, `reason u8` |
| `STATUS` | `last_market_seq u64`, `last_sub_seq u16`, `last_log_index u64`, `last_ordinal u32`, `applied_index u64`, `book_sequence u64`, `pending_stops u32` |
| `GAP` | `last_good_log_index u64`, `last_good_ordinal u32`, `dropped u32`, `product u8` |
| `BAR` | `interval_ms u32`, `window_start_ms u64`, `last_ts_ms u64`, `open i128`, `high i128`, `low i128`, `close i128`, `volume i128`, `trade_count u32`, `bar_seq u64` |

**Why post-batch totals instead of increments:** a duplicated delta is harmless, a delta applied on
top of a dropped predecessor is not. Totals plus per-stream ordering make the feed idempotent and
reduce gap recovery to "get a snapshot".

### 16.1 Sequencing and deduplication (U8, implemented)

**Identity.** Each message carries `(symbol_id, stream_id, market_seq, sub_seq)`; that quadruple is
the downstream dedupe key. `(log_index, ordinal)` stays the audit position and snapshot boundary,
not the message identity.

- **`market_seq`** is the replicated mutation sequence (`OrderBook::sequence`, already part of the
  snapshot). A `Place` uses it as the taker order sequence; `Cancel`, `Amend` and `PurgeExpired`
  each consume one when they arrive, so every mutating request has a fresh anchor. Stop cascades
  anchor to the electing taker. Rejections also consume a sequence: ids are sparse, not counts.
- **`sub_seq`** is assigned per stream within the anchor. One frame emits at most one message per
  data stream, so it is 0 except for `BAR` (several intervals/windows can close in one frame). Two
  replicas derive the same value because message boundaries are entry-driven. Two reserved values
  keep local/reset messages from colliding with the frame they precede: `GAP` uses
  `u16::MAX` (diagnostic, node-local) and the reset snapshot's `BBO` uses `1`, because the first
  post-snapshot frame starts at the snapshot's boundary `market_seq` and emits its own `BBO` at
  `sub_seq = 0`.
- **TRADE is uncapped**: all fills of the taker go into one message. This is the conflation unit
  downstream asked for; the Aeron adapter must size its payload/term limits accordingly (or split
  at the transport edge without changing the key).

**Time and expiry.** Every admissible mutating request runs the automatic GTD purge before it is
applied, using its intake timestamp as the trigger time; the expired orders' events share that
request's `market_seq` and timestamp. `Day` still needs a market close, so only the explicit
`PurgeExpired { now_ms, day_close_ms }` evicts it (that request has its own sequence and timestamp).

**Deterministic boundaries.** Emission is a pure function of the applied entry; there are no
wall-clock flush windows in the data path. `STATUS` is emitted when
`market_seq % status_every == 0` (default 100); `GAP` is diagnostic and only reports a hard queue
overflow. The full per-product rules are in §17.

**Snapshot boundary.** A leadership snapshot carries the **next** sequence (an exclusive boundary).
The processor emits it with `SNAPSHOT | RESET` and drops any frame with
`market_seq < snapshot.market_seq`; data above it follows normally. This makes a takeover a clean
reset without a counter.

**Publication and failover.** Keys are derived from the frame, not from publisher state.
Leader-only publication gates the tap at the source; a new leader resets from its snapshot and then
produces the same keys for the same later entries. Deriving on every replica is not required for key
equality (the snapshot supplies state); with `leader_only = false` every replica publishes and keys
still match, though a bar window that straddles a snapshot can be partial on each side and is merged
by consumers.

**Restart.** `market_seq` is replicated and snapshotted, so a restarted leader continues
monotonically; `sub_seq` is per frame and needs no state.

**Responses.** `/write` returns the assigned `market_seq` for `Place`, `Cancel`, `Amend` and
`PurgeExpired` (previously `Cancel` returned null and `PurgeExpired` a count).

**Wire version 3.** `market_seq` occupies the former `stream_seq` slot; `sub_seq` uses two of the
reserved header bytes at offset 60. Version 3 encodes all prices/quantities as scale-18 `i128`
(16 bytes each) instead of `f64`; the encoder and its golden tests cover the layout.

## 17. Publishing rules

The processor emits from one applied entry, in a fixed order (depth, BBO, trade, lifecycle, bars,
status). All messages carry the entry's `market_seq` and their per-stream `sub_seq`:

| Product | When it is emitted | Size / conflation |
|---|---|---|
| `TRADE` | every entry with fills: all fills of the taker in **one** message | uncapped (52 B per fill); no chunking |
| `BBO` | every entry whose book changed (depth delta or fill), after applying it | latest-value; downstream conflates |
| `DEPTH_DELTA` | every entry with touched levels: post-batch totals | one message per entry |
| `LIFECYCLE` | every entry with events: expired, fills, rests, cancels, triggers | one message; `rested` shed first above `lifecycle_batch_max` |
| `BAR` | event-time windows: closed when a trade rolls the window, live after 1 s of event time | one message per interval/window |
| `DEPTH_SNAPSHOT` | takeover / startup reset | top-N per side, anchored at the next sequence |
| `STATUS` | `market_seq % status_every == 0` (default 100) | diagnostic (sub_seq 0) |
| `GAP` | before the next frame after a hard queue overflow | diagnostic (`sub_seq = u16::MAX`) |

There are **no wall-clock intervals, token buckets or cross-entry batching** in the data path: any
of those would make message boundaries node-dependent and break the dedupe key. Rate control belongs
downstream (Kafka compaction, latest-value caches, chart aggregation) or must be deterministic
(chunking at a fixed fill count) if a safety cap is ever added.

**OHLC bars.** Bars are aggregated from trade prints per `(symbol, interval)`:

- windows are Unix-epoch aligned on the taker timestamp, so they are deterministic across replicas;
- `open`/`high`/`low`/`close`/`volume`/`trade_count` come from the fills; `partial` marks a live bar;
- a closed bar is emitted in the entry whose trade rolls the window; a live bar after 1 s of event
  time since the previous live update;
- after a takeover snapshot the open window restarts (bars are reset), so a window that straddles
  the handover can be published as partial on both sides; OHLC merge is associative and consumers
  stitch by `(symbol, interval, window_start_ms)`.

**Backpressure.** The matching thread never blocks: a full tap queue drops the frame, counts it and
sets a `GAP` flag for the next frame. If the sink or Aeron publication stalls, the processor slows,
the queue fills and the feed degrades to `GAP` while consensus continues. `apply` latency is
independent of the feed.

## 18. Leader changes, restarts, recovery

- **Leader change.** New publication session, `RESET`, snapshot at the new leader's `last_applied`,
  then deltas. The deposed leader may still publish a few frames for entries it had already applied;
  those are committed, so they are not wrong — consumers dedupe on
  `(symbol_id, stream_id, market_seq, sub_seq)` (§16.1) and honour the `RESET`/snapshot ordering
  rule. The keys are replicated-state derived, so the handover does not reset the sequence space.
- **Leader restart.** State is restored from the v6 snapshot plus log replay (v6 stores the last
  trade and match events as fixed-point; v5 stored order prices/quantities; v4 carries the
  match-event window and submission dedupe map; v3 added stops and `last_trade_price`); the seeder
  captures the restored book and the snapshot-then-deltas rule applies. Replayed entries are below
  the reset boundary (the next `market_seq`) and are therefore suppressed.
- **Follower installs a snapshot.** It is not publishing, but its shadow book is now stale (an
  installed snapshot skips the entries the shadow would have consumed). Its open OHLC windows are
  stale for the same reason. It keeps serving nothing; the mandatory seeder run on the next
  leadership gain reconciles the book, and bars for the handover window are republished as
  `partial` (the merge rule in §17 stitches them). That is why seeding on takeover is a correctness
  requirement, not an optimization.
- **Consumer reconnect.** Subscribe and wait for the next `RESET` snapshot (startup or a leadership
  gain today; the request stream 2010 is phase 3), discard frames with
  `market_seq < snapshot.market_seq`, then apply. `log_index` remains the tape/audit position.
  Consumers that cannot tolerate the wait use the Archive (or the phase-3 request stream).

## 19. Delivery contract

| Property | The feed gives | The consumer must |
|---|---|---|
| Ordering | total order per stream while connected | apply in stream order; join streams on `(market_seq, sub_seq)` (§16.1) |
| Completeness | best effort at the transport level | detect `GAP`/staleness and resync from the snapshot stream |
| Idempotence | deltas carry totals; messages carry `(market_seq, sub_seq)` | dedupe on `(symbol_id, stream_id, market_seq, sub_seq)`; totals are safe to re-apply |
| Durability | none on the wire (Aeron is not store-and-forward) | use the Archive for replay, the Raft log for authoritative recovery |
| Exactly-once | not provided | at-least-once plus dedupe, or Archive replay plus dedupe |

Per-product completeness is the table in §12 and the policies in §17: `TRADE`,
`DEPTH_SNAPSHOT` and `STATUS` are complete within their guarantees; `BBO` is latest-value;
`DEPTH_DELTA` carries post-entry totals; `BAR` is aggregated; `LIFECYCLE` is one message per entry
and may shed low-value kinds above its record cap.

**Bridging to a durable message bus.** If downstream is Kafka/NATS/Pulsar rather than Aeron
subscribers, run a bridge that subscribes to streams 2001–2007 and produces to the bus keyed by
`(symbol_id, stream_id, market_seq, sub_seq)` (§16.1). Aeron provides the low-latency hop; the bus provides
retention and fan-out; the sequence fields make the bridge idempotent. This also covers "many consumers" when
multicast is unavailable.

## 20. Repo integration

| File | Change |
|---|---|
| `Cargo.toml` | optional `rusteron-client` + `rusteron-media-driver` behind a `market-data-aeron` feature (default off, so the default build stays pure Rust and `cargo test` needs no driver) |
| `src/matchengine/mod.rs` | transient, optional depth journal: `rest`, `sweep` (partial fill, full consume, iceberg refill), `cancel` and `purge_expired` append `(symbol_id, side, price, delta)`; `#[serde(skip)]` so snapshots and the log are unchanged; zero work when disabled |
| `src/market_data/mod.rs` | **new**: `MarketDataTap` (journal + event hand-off + bounded queue), `DepthJournal`, `ShadowBooks` (per symbol), `MarketDataConfig`, publisher trait |
| `src/market_data/aeron.rs` | **new**: `AeronPublisher` (feature-gated) — publications, per-entry batching, retries, gap counters |
| `src/match_result/mod.rs` | add `side: OrderSide` to `MatchEvent` (the order's own side; the maker's side on a fill is its opposite). Additive JSON field, needed for `TRADE.taker_side` and useful to every consumer. The frame carries the request's `timestamp` separately; events do not each need a copy |
| `src/store/state_machine.rs` | after the event batch, hand events plus the drained journal to the tap (`try_push`); `capture_levels()` helper for the seeder; everything is a no-op when no tap is configured |
| `src/lib.rs` | wire the tap, market-data thread, leader watch and seeder in `build_match_engine_node`; join them in `MatchEngineNode::shutdown` |
| `src/store/config.rs` | `MarketDataConfig { log_path, depth_levels, status_every, lifecycle_batch_max, bar_intervals_ms, queue_capacity, leader_only }` |
| `src/main.rs` | `--market-data-*` flags and env equivalents, following `--match-event-log` (§20 config block) |
| `src/network/api.rs` | `marketdata_status` read key (gate state, last published position, queue depth, shadow divergence counter) |
| `tests/cluster/test_market_data.rs` | **new**: embedded driver, subscriber, failover, resync and shadow-book equivalence tests |
| `benches/market_data.rs` | **new**: tap cost with the feed on/off, shadow-book update cost, end-to-end latency |

The hot path records only what the operation already knows:

```rust
// src/matchengine/mod.rs (sketch, not compilable)
fn rest(&mut self, order: &Order, total: Fixed) -> Fixed {
    ...
    self.journal.push(order.side, order.price, +visible); // one append, no lookup
    ...
}

// src/store/state_machine.rs (sketch)
let outcome = inner.orderbook.execute_order(&mut o);
tap.try_publish(&events, inner.orderbook.take_journal()); // bounded, non-blocking
```

- **No scans in `apply`.** Touched levels are not looked up in the book; deltas come from the
  operations themselves: `rest` adds the visible tranche, a partial fill subtracts the matched
  volume, a fully consumed tranche subtracts it and adds the iceberg refill, `cancel` and
  `purge_expired` subtract the visible open volume (a cancelled parked stop never contributed
  depth, so it records nothing). Hidden iceberg volume is never recorded.
- **Side is recorded with each delta**, which is also why `MatchEvent` needs the new `side` field:
  `TRADE` publishes the aggressor side and `LIFECYCLE` consumers need to know which side an event
  is about.
- **The shadow level book lives on the market-data thread.** The wire still carries post-batch
  totals: the publisher applies its own deltas and emits the resulting total, which preserves the
  idempotence property in §16.
- **Seeding is the seeder's job.** It locks the state machine only at startup, on leadership gain
  and (optionally) on a reconciliation timer, aggregates N levels and hands them over; it is the
  only O(book) operation and never runs per entry.
- **Allocation budget.** The journal is a reusable buffer (`Vec` with reserved capacity, cleared per
  batch), the queue is preallocated, and `try_publish` moves one small record. No serialization, no
  IO, no clock, no lock beyond the one `apply` already holds.
- **Feed off.** `OrderBook::record_depth` is `false`; each recording point is one predictable
  branch and the journal stays empty. `cargo bench --bench matchengine` must stay within noise of
  the current numbers (§22).

Config (following the `--match-event-log` precedent, all off by default):

```
--market-data-log /tmp/node1-market-data.jsonl   # enables the feed (JSONL sink today)
--market-data-depth 10                  # snapshot levels per side
--market-data-status-every 100          # deterministic status cadence
--market-data-bar-intervals-ms 1000,60000
--market-data-leader-only true           # false publishes from every replica
```

The Aeron channel/stream mapping and sink live in the adapter (`MarketDataSink`); the shipped sink
is the JSONL file above.

## 21. Operations

- **Media driver.** One per node host, started before the node (the embedded driver stays for tests,
  §3.2). `aeron-dir` on tmpfs, supervised, alert on death: `NotConnected` is retryable, a dead
  driver is not.
- **Tuning.** Trades/deltas 2–4 MiB terms, BBO/status 1 MiB, snapshots 4 MiB; MTU 1408 unless the
  whole path does jumbo; `fc` min/max per stream; sender/receiver windows sized to the slowest
  consumer you promise to keep.
- **Monitoring.** Offer failures, back-pressure waits, tap queue high-water mark, publish lag
  (`applied_index - last_market_seq`), `GAP` count, snapshots, frames, trade/lifecycle/bar counts,
  lifecycle shedding, and subscriber positions via Aeron counters. Expose as JSON alongside
  `/metrics` (today `/read marketdata_status`); alert on lag and gaps.
- **Security.** Aeron transport is plaintext with no auth (§3.2): isolate the network/VPN, keep
  `LIFECYCLE` (which carries sequences and reasons) on a separate channel if its audience differs
  from the public depth/trade audience, and never treat stream IDs as authorization.
- **Capacity.** Only the leader publishes, but every node can become leader, so every node runs a
  driver and creates publications on demand. Six streams × 1–4 MiB terms plus driver overhead is
  tens of MiB per node; the Archive, if used, is disk. At the target peak, budget ~26 MB/s of tape
  before batching (~15.6k msg/s at 32 fills/message) and plan fan-out for 10–100 subscribers via
  multicast or one relay — not 100 unicast publications.

## 22. Testing and benchmarks

**Unit (no driver):**

- journal application: per-level totals, iceberg refill deltas, cancel/expiry removal, BBO
  aggregation, snapshot truncation at N, stop and rejection handling;
- deterministic sequencing: two replicas fed the same frames emit identical
  `(market_seq, sub_seq)` sequences and bodies; a takeover snapshot continues the same keys; one
  TRADE message carries every fill; status cadence is `market_seq % status_every`; lifecycle
  shedding order;
- timestamps: every message's `ts_ms` is the mutating request's replicated intake timestamp
  (cancel/amend use their key order's; expiry uses `now_ms`); nothing is re-timed at publish time,
  and bar windows are aligned to that timestamp;
- multi-symbol: interleaved fills for two symbols produce independent shadows, BBO and bars with no
  cross-talk, and `symbol_id` routes every record;
- OHLC: reference aggregation over a random tape, epoch-aligned windows, multiple intervals,
  live-bar `partial` updates, and the window roll on the entry whose trade opens a new window;
- encoder/decoder golden tests per message type, header flags, versioning, and a `u32` trade count
  that encodes an uncapped sweep;
- processor policy: gap emission (with a distinct `sub_seq` when it shares the frame with status),
  BBO emission per changed entry, snapshot-boundary discards.

**Integration (`tests/cluster/test_market_data.rs`, embedded driver, ports 24001+):**

- scripted book (rest, fill, cancel, iceberg refill, stop cascade, expiry) → subscriber sees the
  expected trades, BBO and deltas with exact `(log_index, ordinal)`;
- new subscriber mid-stream: next snapshot plus deltas ≥ snapshot index reconstructs the book;
- leader kill in the 3-node cluster: `RESET` + snapshot from the new leader, no committed fill lost
  after dedupe;
- slow consumer / tiny term buffer: `GAP` observed, resync via snapshot;
- node restart: replay does not duplicate published frames (snapshot boundary);
- overload: a tap queue small enough to force drops observes `GAP` and resynchronizes from a
  snapshot; matching latency is unaffected;
- bars end-to-end: subscriber receives closed bars whose OHLCV equals an independent aggregation of
  the `TRADE` stream for the same window.

**Determinism:** apply the same entries to two `StateMachineStore`s and diff every frame byte for
byte, bars included — every timestamp is log-carried, so there is nothing to exclude. The U8 tests
do this for the emitted messages: identical `(symbol_id, stream_id, market_seq, sub_seq)` keys and
bodies, across a takeover snapshot and (because the sequence is snapshotted) a restart. That is the
guard against a clock read or a publisher-local counter slipping into the feed.

**Shadow-book equivalence:** after a scripted workload, compare every level of the shadow book with
a fresh seeder capture (and repeat over randomized op mixes in a unit test). Any divergence is a
journal bug; the publisher also counts a divergence on periodic reconciliation and forces a
snapshot resync.

**Performance gates (the "matching must not regress" contract):**

- feed **off** vs the pre-change baseline: every `cargo bench --bench matchengine` scenario within
  noise; a regression blocks the change;
- feed **on**: tap overhead per apply batch (journal appends + one `try_push`) measured separately,
  with a target budget set before implementation (guidance: single-digit nanoseconds per mutation,
  zero allocations after warmup); frame building, aggregation, encoding and Aeron entirely
  off-thread;
- the market-data thread must be able to fall behind arbitrarily without changing apply latency —
  demonstrated by a test that stalls the publisher while submitting writes.

**Benchmarks** (extend `benches/matchengine.rs` or add `benches/market_data.rs`): tap cost per
mutation and per apply batch with the feed on and off, shadow-book update cost (deltas × depth),
snapshot cost vs N, and end-to-end apply → offer → subscriber latency p50/p99/p999 over IPC and
loopback UDP, plus offers/s at saturation. The JSONL sink is the baseline. **No numbers are claimed
until measured** — same rule as Part I.

## 23. Staged plan and effort

| Phase | Deliverable | Exit criterion | Days |
|---|---|---|---|
| 1 | Depth journal + tap + per-symbol shadow books + in-process publisher (no Aeron); `symbol_id` plumbing (0 until the engine is multi-symbol); on/off benchmark gate; unit, determinism and shadow-equivalence tests | journal/journal-free benchmarks show no regression; shadow books match a fresh capture; default build unchanged | 3–4 |
| 2 | `rusteron` behind a feature; trades + BBO streams; embedded driver in tests | subscriber receives scripted trades/BBO over IPC and UDP; apply latency unchanged under a stalled publisher | 3–5 |
| 3 | Depth snapshots + post-batch totals, seeder on takeover, status/gap streams, deterministic sequencing (§16.1/U8), OHLC bars, `/read` status | mid-stream subscriber reconstructs the book; gap → resync works; identical keys across replicas; bars match an independent aggregation | 4–6 |
| 4 | Leader gating, failover + back-pressure tests, metrics, reconciliation alarm | leader kill produces `RESET` + snapshot, no loss after dedupe | 3–4 |
| 5 (optional) | Archive recording/replay, request stream, lifecycle entitlements | replay window proves gap recovery without snapshots | 3–6 |

Rollback: the feed is a sink. Turning it off cannot change consensus state, the log format or the
book.

## 24. Risks

| Risk | Severity | Mitigation |
|---|---|---|
| Dual publisher during a leader change | Low | dedupe on `(symbol_id, stream_id, market_seq, sub_seq)` (U8); `RESET` ordering rule; short window by construction |
| Queue overflow during a burst drops frames | Medium | size for peak, emit `GAP`, use Archive for recovery, alert on the drop counter |
| Shadow book grows with the instrument's level count (memory, reconciliation cost) | Low | it lives off-thread, is bounded by distinct prices, and is only reconciled on a timer; matching never touches it |
| `rusteron` FFI risk (thin, partially documented) | Medium (lower than Part I) | feature-gated, isolated in one module, pinned version, ASAN in CI; no consensus path depends on it |
| Media driver becomes another per-host process | Medium | supervise, alert on `NotConnected`/driver death, run it in the same deployment unit |
| Plaintext transport, no auth | Medium | isolated network, separate private streams, no secrets in payloads |
| Multicast unavailable in cloud | Medium | unicast + bridge to the durable bus; validate on the target network |
| Consumers forget the snapshot rule and apply a delta first | Medium | `RESET` flag documented in the schema; reference consumer in tests; snapshots on the same stream family |
| Schema drift breaks consumers | Medium | version byte, golden tests, additive-only `STATUS`, bump for everything else |
| Anyone adds a clock read to the feed and breaks replica determinism | High | every timestamp is the mutating request's intake time, carried through the log; the two-replica byte-diff test covers bars too |
| The tap creeps into the matching loop (scans, allocations, encoding) | High | isolation rules in §14; journal only records values the operation already has; on/off benchmark gate in §22; code review checklist for the matching module |
| Shadow book diverges from the engine | Medium | journal is exact by construction; equivalence test over scripted and randomized workloads; reconciliation counter plus snapshot resync |
| Multi-symbol fan-out multiplies derived state and traffic | Medium | shared streams with `symbol_id` routing; per-symbol shadow books sized for the actual symbol count; consider sharding a busy symbol onto its own channel set |
| Shedding silently hides lifecycle records | Medium | only `rested` is shed above `lifecycle_batch_max`, the count is exposed in `STATUS`, and the tape is recoverable from the log |
| Intake timestamps can be skewed or non-monotonic (clock step, leader handover) | Medium | the leader stamps the timestamp at intake and it is replicated with the log entry; ordering never uses it; bars expose `window_start_ms`/`last_ts_ms` and merge associatively across handovers |
| Tap queue capacity is wrong for a deployment | Medium | configurable; overflow is counted and marked with `GAP`; size it with the load test |

## 25. Alternatives

| Option | Latency | Fan-out | Durability | Ops | Fit here |
|---|---|---|---|---|---|
| **Aeron (this design)** | lowest of the options; shared memory + UDP | multicast/unicast, per-subscriber flow control | none on the wire; Archive optional | driver per host | Good: one-way ordered feed, many consumers, no broker |
| gRPC server-streaming (tonic already in tree) | good; HTTP/2 + protobuf overhead | one stream per consumer | none | none new | Fine for a handful of consumers and for kicking the tires; weaker tail and no multicast |
| Kafka / NATS / Pulsar | ms-class in practice | excellent | yes | brokers + ops | Good *behind* a bridge (§19), not as the engine's last hop |
| Raw UDP multicast | lowest possible | yes | none | build sequencing/flow control yourself | Reinvents Aeron's gaps |
| ZeroMQ pub/sub | low | yes | no | low | No flow control or delivery guarantees |
| Aeron Cluster | vendor-claimed lowest | — | replicated | Java cluster | Not applicable: consensus is openraft (Part I §3.3) |

## 26. Open questions

- Does `rusteron` 0.2.13 expose exclusive publications and an Archive client? Part I verified
  `offer`, `max_payload_length` and subscription polling; not these.
- Unicast UDP: can two subscriptions share one port with different stream IDs in the C driver, or
  should every stream get its own port? Port-per-stream is the safe default assumed here.
- Is multicast available in the target network? If not, fan-out is N unicast publications or a
  single collector bridging to the bus.
- Do consumers prefer post-batch totals (chosen) or true diffs? Totals cost bandwidth on busy levels
  but survive duplication and simplify recovery.
- Archive deployment: every node, or only the current leader? Retention and disk budget?
- Are lifecycle events and order sequences public? If not, split streams and networks.
- ~~Price encoding~~ Resolved: wire v3 carries scale-18 `i128` values; old v2 frames are not
  silently reinterpreted.
- Journal vs event-only reconstruction: the journal is exact (iceberg refills, hidden volume) but
  touches the book module; events alone would leave the tap fully outside `matchengine/` but need
  extra fields (`side`, maker remaining, refill) to reconstruct depth. Which boundary do we prefer?
- Reconciliation cadence for the shadow book: never, on a timer, or only on a divergence alarm?
- Where does `symbol_id` come from (client-supplied on the order, or assigned by the gateway), and
  who owns the symbol table? How many symbols per node at launch, and what is the expected split of
  traffic between them?
- Do bars belong on the same feed or on a separate analytics feed with different retention?
- Default bar intervals per venue: is `1s,1m` enough, or does the first consumer need `5m/1h` too?
- Default rates and bursts per venue: what is the target consumer's capacity, and who owns the
  profile (engine team or downstream)?
- May `LIFECYCLE` shed `rested` under pressure, or does a downstream system need every rest?
- Should the tap also serve `/read` depth views (`orderbook_depth`) so the HTTP API and the feed
  share one derivation path?

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

Added by the Part II design and also not verified:

- `rusteron` API for exclusive publications (`add_exclusive_publication`) and for the Aeron Archive
  client; the design works without the Archive, and publication exclusivity can fall back to a
  single-writer convention if the binding lacks it.
- Whether unicast UDP requires one port per stream (assumed above) or can mux stream IDs on one
  port; port-per-stream is safe either way.
- Multicast behaviour in the target cloud/VPC network.
- Any conflation facility in the C media driver; the design batches per entry in the publisher, so it does
  not depend on one.

## Appendix B — references

- rusteron: <https://github.com/gsrxyz/rusteron> · <https://docs.rs/rusteron-client/0.2.13/>
- Aeron: <https://github.com/aeron-io/aeron> · <https://aeron.io>
- Aeron delivery assurances: <https://github.com/aeron-io/aeron/wiki/Message-Delivery-Assurances>
- Aeron Cluster (Java): <https://aeron.io/docs/distributed-systems-basics/consensus-algorithms/>
- Aeron Archive: <https://github.com/aeron-io/aeron/wiki/Archive>
- Aeron C driver: <https://github.com/aeron-io/aeron/tree/master/aeron-driver>
- ergo-aeron-cluster (client-only prototype): <https://docs.rs/ergo-aeron-cluster/0.1.34/>
- openraft 0.10 network API: <https://docs.rs/openraft/0.10.0-alpha.37/openraft/network/>

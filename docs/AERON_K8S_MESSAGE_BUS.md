# Market-data message bus on Kubernetes: Profile A Aeron multicast default and fallbacks

Status: **Profile A implemented end-to-end in the single-node lab; fallbacks are design-level.**
The `MarketDataService` CRD, manager API/GUI, deterministic renderer and operator Deployment/
Service/PDB/NetworkPolicy/ConfigMap rollout exist and are live in the OrbStack cluster. The engine
Aeron sink, market-data Aeron source and single-node pod multicast path are implemented and tested;
the engine -> service feed is Ready with `interface={pod_ip}`. Cross-node multicast is not yet
validated. There is no direct-NATS sink/source or bridge implementation.
Date: 2026-11-16.

This document designs the transport between the replicated match-engine clusters and the standalone
market-data cluster on Kubernetes. It answers four questions:

1. Is Aeron a good candidate for this bus?
2. Can Aeron use multicast on Kubernetes?
3. Is NATS-only simpler and better on EKS?
4. What topology scales out without turning the matching engine into a per-subscriber publisher?

The short answer:

- **Aeron is a strong engine-edge transport, not a complete Kubernetes message bus.** It is excellent
  for the one-way, ordered, low-latency hop out of the leader and for pod-local IPC. It has no
  persistence, service discovery, authentication or cross-node multicast guarantee.
- **Profile A — direct Aeron multicast — is the default implementation.** One leader publication
  per feed group, independent market-data subscriptions, no bridge and no per-subscriber engine
  work. It requires a multicast-capable pod network.
- **On default AWS EKS this profile cannot run on the VPC CNI.** EKS must be provisioned with a
  multicast path: Cilium/Isovalent Enterprise multicast, or Multus + Transit Gateway multicast. If
  the platform team will not provide either, use the direct-NATS fallback or the Aeron bridge.
- **Do not assume multicast works in Kubernetes.** Kubernetes Services/Ingress do not expose
  multicast; standard overlay CNIs and many cloud VPCs drop it. The current OrbStack cluster passed a
  single-node pod multicast test, not a multi-node test.
- **The per-engine-pod Aeron bridge remains a compatibility profile** for deployments that already
  run Aeron or cannot expose multicast directly to service pods.
- **Never create one Aeron unicast publication per market-data pod**; that is a configuration error
  at 10–100 instances.

## 1. Requirements

The bus must:

- publish the engine's deterministic feed (`TRADE`, `BBO`, `DEPTH_SNAPSHOT`, `DEPTH_DELTA`,
  `LIFECYCLE`, `STATUS`, `BAR`) without touching the matching/apply path;
- preserve the wire-v7 identity `(symbol_id, stream_id, market_seq, sub_seq)` so failover, restart and
  multiple service instances produce identical content;
- make the leader's publish cost independent of market-data instance count;
- let the market-data cluster scale horizontally and restart statelessly;
- work with one-symbol-per-engine-process shards and packed pods;
- survive leader changes, missed frames, slow consumers and pod restarts by snapshot/reset and dedupe;
- be operable from the `MarketDataService` control-plane object: image, symbols/feed groups,
  transport, replicas, rollout, readiness, lag and drop status;
- keep the Aeron port and control-plane network private.

Non-goals: durable event storage, exactly-once delivery, replacing the Raft log, or carrying engine
RPC/consensus traffic.

## 2. Why a bus at all?

The engine already replicates the feed and the market-data service already consumes Aeron directly in
the demo. A bus is needed for deployment concerns rather than matching concerns:

- **Decoupling:** the engine must not know how many WebSocket instances exist or where they run.
- **Fan-out:** one symbol may feed many market-data pods and many clients.
- **Independent scale:** WebSocket CPU, connections and TLS terminate in a separate deployment.
- **Failure isolation:** a restarted or slow market-data pod must not back-pressure `apply`.
- **Routing:** cells, symbol groups and gateway routes change without restarting engines.

## 3. Aeron fit

### 3.1 What Aeron gives us

- Brokerless ordered pub/sub over shared memory (IPC), UDP unicast or UDP multicast.
- One exclusive publication per stream, multiple independent subscriptions.
- Term buffers, flow control and back-pressure signals; loss detection.
- Very low, predictable latency when the receiver is on the same host or the same L2 multicast
  domain.
- A mature Rust path through `rusteron` 0.2.13, already used by the engine's optional Aeron egress and
  the market-data service.

### 3.2 What Aeron does not give us

| Missing capability | Consequence for the bus |
|---|---|
| Persistence / replay | Aeron is not store-and-forward. A new or lagging service must resync from a reset snapshot, or a separate archive/bus must retain frames. |
| Service discovery | Endpoints are addresses and stream IDs, not Kubernetes Services. A unicast design needs a headless/stable endpoint plan; a multicast design needs a group/port plan. |
| Authentication / encryption | Aeron is plaintext. NetworkPolicy/private network only; WebSocket TLS/auth belongs at the edge. |
| Cross-node multicast guarantee | Depends entirely on the CNI, VPC and node network. Must be tested in the target cluster. |
| Per-subscriber flow control over multicast | A slow receiver falls behind, detects gaps and resyncs; it does not slow the publisher. |
| A Kubernetes operator | The media driver and bridge/sidecar lifecycle must be designed, monitored and supervised. |

**Verdict:** Aeron is a good candidate for the **engine → market-data edge**, especially with
multicast. It is not a substitute for a durable, discoverable cluster bus when the network cannot
provide multicast.

## 4. Can we use multicast on Kubernetes?

Kubernetes has no native multicast Service or Ingress abstraction. Multicast is forwarded (or
dropped) by the **pod network/CNI**, and sometimes by the underlying VPC or firewall.

### 4.1 What we tested

On the current OrbStack Kubernetes cluster (single node, 2026-11-16), two pods ran Python UDP
sockets:

- receiver joined `239.1.2.3:25000` with `IP_ADD_MEMBERSHIP`;
- sender set `IP_MULTICAST_TTL=1` and sent `hello-mcast`;
- the receiver printed the payload from the sender pod's IP.

Result: **pod-to-pod multicast works within this single-node OrbStack cluster**. A second test on
an OrbStack Docker bridge (`172.31.77.0/24`) also delivered multicast between two containers. This
confirms the macOS kernel/OrbStack bridge forwarding path, but it does **not** prove forwarding
between Kubernetes nodes, a production CNI overlay, NetworkPolicy/IGMP behavior, or a cloud VPC
with multicast disabled.

Aeron-specific finding (2026-11-16): the rusteron prebuilt Linux libraries are x86_64-only. Native
arm64 images build Aeron from source with CMake 3.31 and `rustfmt`; `market_data_system/docker/k8s.Dockerfile`
and the engine Dockerfiles use that path, while macOS/Linux x86_64 can opt into `aeron-precompiled`.
A channel with `|interface=eth0` starts the driver and creates publications but never emits multicast;
the publication stays backpressured. Each market-data Aeron source also re-subscribes after 5 s
without a fragment (backing off to 30 s), covering the image-registration race at pod start and
engine leader transfers. Engine **publishers** therefore keep `|interface={pod_ip}`.
Market-data **subscribers** omit `interface=` and bind the wildcard address: pinning a receiver to its
pod IP makes the IGMP membership interface-specific, and a second subscriber on the same node (for
example a shard fast service plus the global terminal service in a single-node lab) can then miss a
group. With a wildcard receiver bind both get a copy; on a multi-node cluster each pod has its own
kernel and either form works. Omitting `interface`,
or using the pod's literal IPv4 address, works in the Docker lab and in a pod once the control plane
substitutes `{pod_ip}`. The static rusteron/Aeron build does not resolve the interface name here.

### 4.2 What must be validated in the target cluster

Before selecting the direct multicast profile, run the same two-pod test on **different nodes**, then
add:

1. the target CNI and encapsulation mode (VXLAN/IPIP, MTU, multicast snooping);
2. a NetworkPolicy that allows only UDP group traffic between engine and service pods;
3. packet loss and pod-restart tests;
4. 10, 50 and 100 service subscribers;
5. a leader transfer while subscribers are connected.

Common overlays (Calico/Flannel and standard Cilium configurations) are primarily unicast fabrics;
do not treat pod multicast as portable. Cilium has a multicast feature (with IGMP/MulticastGroup
support in current/enterprise versions), and Multus with macvlan/SR-IOV can provide an L2 network,
but each requires explicit validation and often a separate network attachment. Public cloud VPC
multicast support is provider- and region-specific; AWS VPC, for example, does not provide native
multicast without a dedicated multicast service.

**Decision:** Profile A (direct Aeron multicast) is the default implementation, but it is
**supported only after a multi-node acceptance test in the target environment**. The deployment
object must record the validated profile; a cluster that does not provide pod multicast uses the
direct NATS or per-pod bridge fallback. Aeron also requires an **odd** multicast data address and
`|ttl=1` (or higher) for traffic to leave the pod network namespace. Use
`|interface={pod_ip}` in a `MarketDataService`; the renderer substitutes each pod's `POD_IP` from
the downward API. Do not hard-code `interface=eth0`: Aeron's interface resolver does not accept the
interface name in the static Linux build, and the implicit default can bind the wrong address.

### 4.3 AWS EKS: Profile A requires a multicast-capable platform

Default Amazon EKS/VPC CNI gives pods VPC IPs but does **not** provide native pod multicast.
Kubernetes Services and Ingress do not expose multicast, and VPC routing is unicast-first. Profile A
therefore requires the platform team to provide one of:

1. **Cilium / Isovalent Enterprise with multicast** — VPC CNI opt-out, Cilium as the cluster CNI,
   `MulticastGroup`/IGMP support. This is a cluster-wide networking decision, not a
   `MarketDataService` toggle.
2. **Multus secondary network + AWS Transit Gateway multicast** — keep VPC CNI as primary, attach a
   host-device/macvlan secondary ENI, and route IGMP/multicast through a TGW multicast domain. AWS
   documents this pattern, but it adds ENI/instance-type limits, secondary-subnet planning, security
   group/NACL work and TGW multicast-group operations.

If neither is available, the supported alternatives are the direct-NATS fallback (§5.1) or the
Aeron bridge (§5.3). The default VPC-CNI cluster cannot run Profile A by configuration alone.

For an EKS cluster that has validated multicast, the Profile A topology is:

```
engine pod (leader)                                      market-data pods
┌──────────────────────────────┐   UDP multicast        ┌───────────────────────────┐
│ matchengine-raft             │ ──────────────────────▶│ market-data instance 1    │
│  AeronSink (embedded driver) │   239.x.y.z:24001..    │  AeronSource per symbol   │
│  IPC → local media driver    │                        │  assembler → WS hub       │
└──────────────────────────────┘                        ├───────────────────────────┤
                                                        │ market-data instance N    │
                                                        │  same subscriptions       │
                                                        └───────────────────────────┘
```

- The leader publishes one exclusive publication per stream per feed group; each market-data pod
  joins the group and subscribes independently.
- The publisher cost is independent of subscriber count.
- Snapshot/reset on leader change; periodic resets cover late joiners.
- No bridge process on the engine pod.
- The same topology works on OrbStack's single-node cluster after the multi-node acceptance test
  required for EKS.

The exact EKS proof must cover multiple nodes, the chosen CNI/TGW multicast path, NetworkPolicy,
MTU, packet loss and 10/50/100 subscribers. Until then, Profile A is the default implementation but
not a claim that every EKS cluster can host it.

## 5. Recommended topology

### 5.1 Profile N — direct NATS (fallback when multicast is unavailable)

```
engine pod (leader)                              EKS cell (3 AZs)
┌──────────────────────────────┐
│ matchengine-raft             │
│  market-data task            │
│   wire v7 encoder            │
│   NatsSink (async-nats)      │──TCP/TLS──▶ NATS JetStream cluster
│   leader-gated, bounded      │             (3 nodes, one per AZ,
│   publish queue              │              EBS gp3, short retention)
└──────────────────────────────┘                       │
                                                       │ subjects
                                                       ▼
                                          market-data Deployment (N pods)
                                           NatsSource → assembler → WS hub
                                                       │
                                                       ▼
                                          ALB/NLB (TLS, WebSocket)
```

This is the fallback profile for clusters that cannot provide pod multicast. It is **not** the default implementation; it remains the safest option for a default EKS/VPC-CNI cluster or a cloud network that blocks multicast:

- **No Aeron, no media driver, no bridge sidecar, no multicast.** The engine's market-data task
  publishes raw wire-v7 frames to `md.<cell>.<feed_group>.<stream_id>` through an `async-nats`
  client.
- **Only the leader connects.** The publisher is created on leadership gain and closed on loss; a
  follower holds no NATS connection. This keeps active publishers at one per symbol, not one per
  engine process.
- **The engine stays decoupled from WebSocket scale.** NATS fans out to every market-data pod; the
  engine's publish cost is independent of client or service instance count.
- **The market-data service adds a `NatsSource`** beside the existing `AeronSource`. The assembler,
  conflation, wire decoder and WebSocket hub are unchanged.
- **NATS is shared cell infrastructure**, deployed separately with three nodes across three AZs,
  EBS gp3 and anti-affinity. `MarketDataService` references the NATS cluster and owns the
  market-data Deployment, not the NATS lifecycle.

Use **NATS Core** for the live WebSocket path when latency matters (at-most-once, lowest overhead)
and an optional **JetStream** stream on the same subjects for short retention/replay. Start with
JetStream if restart continuity is required; measure whether its ack/disk overhead is acceptable
before promising the same p99 as Core. The feed's delivery contract is already lossy with
snapshot/reset, so Core NATS plus reset is valid; JetStream is a recovery convenience, not a
correctness requirement.

### 5.2 Profile A — direct Aeron multicast (default implementation)

```
engine pod (leader)                                      market-data pods
┌──────────────────────────────┐                        ┌───────────────────────────┐
│ matchengine-raft             │   UDP multicast        │ market-data instance 1    │
│  AeronSink (embedded driver) │ ──────────────────────▶│  AeronSource per symbol   │
│  IPC → local media driver    │   239.x.y.z:24001..    │  assembler → WS hub       │
└──────────────────────────────┘                        ├───────────────────────────┤
                                                        │ market-data instance N    │
                                                        │  same subscriptions       │
                                                        └───────────────────────────┘
```

- The leader publishes one exclusive publication per stream per **feed group**.
- Every market-data pod joins the group and subscribes independently.
- Publisher cost is independent of subscriber count.
- Snapshot/reset on leader change; periodic reset snapshots cover late joiners.
- No bridge process on the engine pod.

This is the default implementation. It gives the lowest latency and the cleanest publisher cost, but it requires a multicast-capable pod network: the cluster must be provisioned for multicast before this profile is enabled.

### 5.3 Profile B — per-pod Aeron bridge + durable bus (compatibility)

```
engine pod (leader)
┌──────────────────────────────────────────────┐
│ matchengine-raft                             │
│  --market-data-aeron-embedded-driver         │
│  IPC 2001..2007 on /dev/shm                  │
│        │                                     │
│  aeron-bridge sidecar (same pod IPC ns)      │
│   subscribe IPC → publish bus subjects       │
└──────────────────────────────────────────────┘
                         │ NATS JetStream / Kafka / Redpanda
                         ▼
              market-data Deployment (N pods)
               per-symbol durable consumer → assembler → WS
```

- A small sidecar consumes the engine's **IPC** publications and republishes the same wire-v7
  frames to a durable, K8s-native bus.
- One media driver per engine pod; the sidecar and engine share the pod IPC namespace and
  `/dev/shm` (mount a memory-backed `emptyDir` at `/dev/shm` if the default 64 MiB is too small).
- The bridge never blocks `apply`; it is downstream of the same bounded tap and drop/`GAP` policy.
- The bus provides retention/replay and portable unicast/TCP fan-out; Aeron remains the low-latency
  edge. Use NATS JetStream first: subject-per-`(cell, feed_group, stream)`, sequential
  `(market_seq, sub_seq)` keys, short retention, and per-symbol consumers.
- The market-data service can restart or replay without contacting the engine.

This is a compatibility profile for deployments that already run Aeron or need an Aeron edge. It is **not the default**; Profile A removes the sidecar, media driver and IPC hop where multicast is available, and direct NATS remains the TCP/unicast fallback.

### 5.4 Profile C — Aeron unicast relay (small/isolated only)

For a handful of service pods, a bridge can publish Aeron unicast to each endpoint. This is not the
target scale-out design:

- each unicast publication consumes engine/bridge memory and bandwidth per subscriber;
- endpoint discovery and pod IP churn become part of the control plane;
- 10–100 publications is already an operational mistake.

Use it only for a small fixed service, a test rig, or as a transitional step to Profile A/B.

### 5.5 Do not use

- **IPC across pods.** Aeron IPC is host/pod-local; `/dev/shm` is not shared between pods. A
  `hostIPC` or shared host path is unsafe and couples pods to a node.
- **One multicast group per symbol at large scale.** Group management and filtering become
  expensive. Use feed groups of bounded size (for example 16–64 symbols) and multiplex by
  `symbol_id`.
- **One unicast publication per market-data pod from the engine.**
- **A public Aeron listener.** Aeron is plaintext; keep it ClusterIP/private and let only the
  market-data tier subscribe.

## 6. Feed groups and stream layout

Keep the existing wire format and stream IDs:

| Stream | Product | Notes |
|---|---|---|
| 2001 | TRADE | one message per taker |
| 2002 | BBO | latest value |
| 2003 | DEPTH_SNAPSHOT | reset / resync |
| 2004 | DEPTH_DELTA | post-entry totals |
| 2005 | LIFECYCLE | may be private |
| 2006 | STATUS | idle heartbeat/gap |
| 2007 | BAR | OHLCV |
| 2010 | snapshot request | bidirectional control, optional but required for fast joins |

A **feed group** is a stable set of symbols with:

- a deterministic multicast channel or bus subject prefix;
- a base stream ID (one process uses `base..base+6`);
- a cell-unique `symbol_id` range.

Recommended allocation:

- Within a packed pod, process slot `s` gets `stream_base = 2001 + 7 * s`.
- Feed groups are cell-scoped and assigned by the manager; the group mapping is stored in the
  `MarketDataService` spec/status.
- A service instance subscribes only to the groups it serves; the manager can rebalance groups when
  instances scale.
- Feed group size is chosen from fan-in and CPU budgets. Start with 32 symbols per group for a
  direct multicast or bridge profile, and validate per-instance encode/WS CPU.

Direct NATS publishes the same raw frames; do not transform them in the engine or the
market-data service. For the Aeron bridge profile, forward the same frames and keys unchanged.
Any transformation would make the edge and the bus diverge and break dedupe/replay.

### 6.1 EKS NATS mapping

For the confirmed EKS profile (engine `NatsSink` → NATS):

- **Subjects:** `md.<cell>.<feed_group>.<stream_id>` (for example
  `md.eu-1.fx-a.2001`). The raw wire-v7 payload stays unchanged; `symbol_id`, `stream_id`,
  `market_seq` and `sub_seq` remain in the payload.
- **Streams:** one `MD_<CELL>` stream (or one per feed group if retention differs), with
  `storage=file`, `replicas=3`, `discard=old`, a short `max_age` (start with 10 minutes) and a
  byte cap. This is a restart/incident buffer, not the authoritative history; the engine's Raft log
  and match-event window remain authoritative.
- **Consumers:** each market-data replica needs the **full** feed, so do not use a shared queue
  group for the data path. A live `NatsSource` can use a Core subscription with `New`/reset
  behavior. For restart continuity use a stable replica identity (StatefulSet ordinal) and one
  durable JetStream consumer per replica/feed group; otherwise rebuild from the next reset
  snapshot. Document which guarantee the `MarketDataService` profile promises.
- **Publishers:** one NATS connection per bridge, asynchronous publish with a bounded in-flight
  window. A slow/unavailable NATS connection is a bridge backpressure event, never an engine
  blocking event; the engine tap degrades to `GAP` and the next reset repairs the service state.
- **Placement:** NATS 3 nodes across 3 AZs with EBS gp3; market-data replicas spread across AZs;
  engine shards keep their four voters spread as usual. Use anti-affinity/PDB and a
  topology-aware service where available to keep bridge/consumer traffic in-zone.


## 7. Scale-out model

### Producers

- Only the leader's engine pod publishes. Followers keep the driver/publications closed and open them
  on leadership gain.
- In Profile A, one publication per stream per feed group; multicast fans out in the network.
- In Profile B, one bridge per engine pod; the bridge publishes the same frames to the bus. Cost is
  proportional to feed groups, not service instances.

### Consumers

- Market-data instances are stateless; each instance receives the groups it serves and keeps a
  shadow book per symbol.
- Add instances for WebSocket connection/CPU capacity; no sticky sessions are required because every
  instance can rebuild the same state.
- Scale by **feed group** and **connection count**, not by per-client subscriptions.
- A slow consumer is disconnected with `1013`; a lagging instance resyncs from a snapshot rather than
  slowing the publisher.

### Capacity shape

- Aeron multicast/Bus ingress bandwidth is roughly `feed bytes/s × number of subscribed service
  pods`; the publisher sends once but every pod receives the group.
- WebSocket fan-out (JSON encode + socket writes) is the service bottleneck; size instances by
  connections × streams and message rate, not by symbol count alone.
- Keep market-data CPU/memory requests separate from engine pods. Do not co-locate the WS fan-out
  with the matching process except for a sidecar bridge (which must stay lightweight).

### HPA

Use HPA on CPU plus a custom WebSocket-connection/queue-depth metric. A low `minReplicas` is required
so a restart does not leave zero subscribers; a PDB protects drain windows.

## 8. Kubernetes deployment and control plane

Add a separate `MarketDataService` CRD. It is owned by the same manager/operator but scales
independently of `SymbolShard`.

```yaml
apiVersion: matching.io/v1
kind: MarketDataService
metadata:
  name: me-btcusdt-marketdata
  namespace: match-eu-1
spec:
  cell: eu-1
  feedGroups:
    - name: btc
      symbols: [btcusdt]
      streamBase: 2001            # one unique base per engine process; streams base..base+6
  image: registry/matchengine-marketdata@sha256:...
  release:
    wireVersion: 7
    snapshotVersion: 11
    mixedVersionSafe: false
  replicas: 2
  transport:
    mode: multicast               # multicast (default) | nats | bridge
    channel: "aeron:udp?endpoint=239.1.2.51:24001|interface={pod_ip}|ttl=1"
  websocket:
    port: 24800
    ingress: ws.match-eu-1.example
    tlsSecret: market-data-eu-1-tls
    maxConnections: 4096
    maxStreamsPerConnection: 1024
  resources:
    requests: { cpu: 250m, memory: 256Mi }
    limits: { cpu: "2", memory: 1Gi }
```

The reference operator renders Deployment + Service + PDB + NetworkPolicy + ConfigMap +
`autoscaling/v2` HPA (minimum one replica, maximum from `spec.replicas`/`spec.autoscaling`,
70% CPU), gates on a matching wire version, warns on a mutable image tag, rolls with
`maxUnavailable: 0` / one surge pod, and drains through a `preStop` sleep before SIGTERM. The
cluster must run a metrics server for CPU-based HPA scaling. It writes per-symbol readiness, last-frame age,
subscribers and `GAP` counts to `status`. `ServiceMonitor`, HPA/canary policy and explicit
WebSocket close reasons (`1001`/`1013`) remain follow-ups.

When a `MarketDataService` references a symbol, the **shard renderer** also adds the engine-side
egress flags to that symbol's container: `--market-data-aeron-channel`,
`--market-data-aeron-embedded-driver`, a per-process `--market-data-aeron-dir` under `/dev/shm`,
the matching `--market-data-aeron-stream-base`, a 1 s heartbeat and status every 10 sequences. The
engine pod gets a memory-backed `/dev/shm` emptyDir. This requires an engine image built with
`--features market-data-aeron`; the reference `k8s.Dockerfile` accepts
`--build-arg CARGO_FEATURES=market-data-aeron`. Supervisor packing forwards the same settings
through `processes.json`, including the per-process `marketData` channel and stream base.

The rendered service exposes two profiles on the same port and deployment:

- **fast** — `/ws/<symbol>@<stream>`, pinned to one symbol, exact sequenced delivery, no display
  conflation; for API/algo clients.
- **terminal** — `/ws` plus `SUBSCRIBE`, or `/stream?streams=...`, multi-symbol up to 1024 streams,
  with `--terminal-conflation-ms` (default 100 ms) applied to `bookTicker` and partial depth.

Set `spec.websocket.fastOnly: true` to render `--fast-only`; the terminal `/ws` and `/stream`
routes then return `404` and only `/ws/<symbol>@<stream>` is served. Shard-local services use this
mode so they are predictable single-symbol feeds, while the global service keeps the terminal
profile.

The reference demo runs four **fast-only** shard services plus one **global terminal** service:

| Service | Symbols | Channel | WS |
|---|---|---|---|
| `fx-eu-01-marketdata` | GBP/USD | `239.1.2.67:24041` | 24801 |
| `fx-eu-02-marketdata` | AUD/USD + USD/JPY | `239.1.2.63:24021` | 24802 |
| `fx-sup-01-marketdata` | EUR/GBP + NZD/USD | `239.1.2.65:24031` | 24803 |
| `me-btcusdt-marketdata` | BTC/USDT | `239.1.2.51:24001` | 24800 |
| `global-marketdata` | all of the above | per feed group | 24804 |

`global-marketdata` declares one `feedGroup` per source shard and each group carries its own
`channel`; the renderer emits one `--aeron-channel` per symbol, so a single market-data pod
subscribes to every shard multicast group. Its terminal endpoint is
`wss://ws-fx-eu-02.match-eu-1.example/stream?streams=gbpusd@bookTicker/btcusdt@trade`; a client can
then `SUBSCRIBE`/`UNSUBSCRIBE` any symbol on that one socket. The shard services publish the same
symbols on `fast-ws-fx-eu-0N.match-eu-1.example` (or their port-forward) for API/algo clients.

```yaml
apiVersion: matching.io/v1
kind: MarketDataService
metadata: { name: global-marketdata, namespace: match-eu-1 }
spec:
  cell: eu-1
  image: matchengine-marketdata:aeron-arm64-v2
  release: { wireVersion: 7, snapshotVersion: 11 }
  replicas: 1
  feedGroups:
    - { name: fx-eu-01-gbpusd, symbols: [gbpusd], streamBase: 2001,
        channel: "aeron:udp?endpoint=239.1.2.67:24041|interface={pod_ip}|ttl=1" }
    - { name: fx-eu-02-audusd, symbols: [audusd], streamBase: 2001,
        channel: "aeron:udp?endpoint=239.1.2.63:24021|interface={pod_ip}|ttl=1" }
    - { name: fx-eu-02-usdjpy, symbols: [usdjpy], streamBase: 2011,
        channel: "aeron:udp?endpoint=239.1.2.63:24021|interface={pod_ip}|ttl=1" }
    - { name: fx-sup-01-eurgbp, symbols: [eurgbp], streamBase: 2001,
        channel: "aeron:udp?endpoint=239.1.2.65:24031|interface={pod_ip}|ttl=1" }
    - { name: fx-sup-01-nzdusd, symbols: [nzdusd], streamBase: 2011,
        channel: "aeron:udp?endpoint=239.1.2.65:24031|interface={pod_ip}|ttl=1" }
    - { name: me-btcusdt, symbols: [btcusdt], streamBase: 2001,
        channel: "aeron:udp?endpoint=239.1.2.51:24001|interface={pod_ip}|ttl=1" }
  transport: { mode: multicast }
  websocket:
    port: 24804
    ingress: ws-fx-eu-02.match-eu-1.example
    tlsSecret: global-marketdata-tls
    fastOnly: false
  autoscaling: { minReplicas: 1, maxReplicas: 2, targetCpuUtilization: 70 }
```

The renderer keeps `interface={pod_ip}` on the engine publication and strips it from the
market-data subscriber args, so both the global terminal service and the shard fast services can
subscribe to the same group on a single-node lab.

Supervisor children receive per-process `marketData` settings from `processes.json` and forward the
engine-side `--market-data-aeron-*` flags. Each service has its own multicast group, feed groups,
WS port and per-symbol status; the manager derives the source shard (`fx-eu-01`, `fx-eu-02`, etc.)
from the symbol/deployment catalog. Binance itself offers WebSocket Streams (push, raw/combined,
1024 streams) and a separate WebSocket API (request/response); it does not define a fast/terminal
split. The fast/terminal classes are our server-side policy.

Only the multicast profile is rendered today. The market-data binary has no `NatsSource` yet, so a
`nats` or `bridge` service is rejected by the renderer with a clear error instead of starting pods
that cannot serve a feed.

The manager exposes `GET/POST /api/v1/market-data-services`, a detail endpoint with the WS endpoint
and scoped validation, and `GET /api/v1/market-data-services/{name}/manifests` for the read-only
preview. The GUI **market data** view, with **terminal** and **fast** sub-tabs, lists services,
per-symbol status and links to the manifests.
Symbol create/migrate/stop coupling and the guarded create/scale/upgrade flow remain P1 in
`TODO.md`.

### Snapshot/control path

For Profile A, retain the stream-2010 request path between service pods and the engine leader (or a
bridge control channel). If that is not possible, the periodic reset snapshot must be short enough
for join latency, and the service must mark a symbol not-ready until the first reset. For Profile B,
the bridge owns the request channel or the service waits for the bus's retained snapshot/reset; do not
expose the engine's HTTP API to the market-data tier just to fetch a snapshot.

## 9. Failure and recovery

| Event | Behavior |
|---|---|
| Leader change | New leader NatsSink/publication starts with a RESET; old frames may still arrive; consumers dedupe by `(symbol_id, stream_id, market_seq, sub_seq)` |
| Pod restart | Service rebuilds from the next reset + deltas and reconnects to NATS; the engine keeps matching |
| Missed frames | `NatsSource` detects a `market_seq` discontinuity, marks the symbol not-ready and waits for reset; do not serve stale depth as ready |
| NATS unavailable | The engine publisher queue is bounded and drops/`GAP`s; it reconnects and a reset repairs the service; matching never blocks |
| Slow WebSocket client | Bounded queue, `1013` close, reconnect and snapshot; never back-pressure the engine |
| CNI multicast loss | Applies only to Profile A; health check degrades the direct profile |
| Symbol migration | New feed group publishes first; service route swaps; old group drains and is removed |
| Bridge crash | Applies only to Profile B; the bus retains/replays, Profile N has no bridge |

## 10. Observability

Minimum metrics:

- engine publisher/bridge: offer/publish failures, publish lag (`applied_index - market_seq`), tap queue high-water,
  `GAP` count, snapshot count, bytes/frames per stream;
- bus: NATS publish/consume rate, consumer lag, reconnects, stream retention;
- service: per-symbol last-frame age, subscribers, queue depth, drops, conflation ratio, encode CPU,
  WS close reasons, connection count;
- Kubernetes: pod readiness, restarts, HPA scale events, PDB/drain state.

Manager status should show the same per-symbol view; a pod being Ready is not sufficient if one symbol
has no recent frame.

## 11. Security

- On the EKS NATS profile, use TLS, NKey/JWT or mTLS between engine, market-data and NATS; NATS is
  the authenticated internal bus. NetworkPolicy allows only TCP 4222 from engine leaders and
  market-data pods, and 6222 between NATS nodes.
- Aeron, where still used, is plaintext and unauthenticated. Keep it on a private pod network and
  enforce NetworkPolicy:
  - engine egress → feed group/port (UDP for multicast/bridge; TCP if the bus uses TCP);
  - market-data ingress → feed group/port;
  - no public Aeron listener; no `hostNetwork` unless a validated L2 design requires it.
- WebSocket ingress terminates TLS, authenticates clients and rate-limits connections/streams at the
  gateway.
- Do not put `LIFECYCLE` (sequences/reasons) on a public stream if its audience differs from
  trade/depth; separate the group/channel or filter it.
- Do not treat NATS subjects, stream IDs or multicast group membership as authorization.

## 12. Alternatives

| Option | Latency | Fan-out | Durability/replay | K8s portability | Verdict |
|---|---|---|---|---|---|
| Aeron direct multicast | lowest | excellent | none | Requires validated multicast | **Default implementation** |
| Engine direct NATS Core/JetStream | low (one network hop) | excellent | Core none / JetStream short | highest | Fallback when multicast is unavailable |
| Aeron IPC + bridge + NATS JetStream | low edge + bus hop | excellent | retention/replay | high | Compatibility for existing Aeron edges |
| Aeron unicast to every service pod | low | poor at scale | none | high | Small deployments only |
| Kafka/Redpanda | higher | excellent | strong | high | Good if retention/ecosystem dominates |
| gRPC stream per service | medium | engine work per subscriber | none | high | Not for fan-out |

Profile A direct Aeron multicast is the default because it is the lowest-latency one-way fan-out:
one leader publication per stream, no bridge, no per-subscriber engine work and no media driver in
the service pod. It needs a multicast-capable pod network. Direct NATS is the TCP/unicast fallback
because it is small, Kubernetes-native, supports TLS/auth, fan-out and optional JetStream retention,
and removes the media driver/IPC hop. Kafka is appropriate if the organisation already operates it
and needs long retention.

## 13. Rollout plan

**Phase 0 — validate the default multicast platform (blocking).**
- Run the two-pod multicast acceptance test across nodes on the target CNI/VPC (Cilium/Isovalent
  multicast, or Multus + Transit Gateway multicast); record MTU, IGMP/snooping and packet loss.
- If it fails, select the direct-NATS or bridge fallback and record that decision in the
  `MarketDataService` profile.

**Phase 1 — Profile A direct multicast.**
- `MarketDataService` CRD, operator renderer/rollout and manager view (implemented).
- Engine Aeron publication and market-data Aeron ingestion (implemented).
- Multi-node CNI validation, stream-base allocation, reset/snapshot behavior, drain and per-symbol
  readiness (partially implemented; cross-node validation open).

**Phase 2 — fallback profiles.**
- Direct NATS: engine `NatsSink`, feed-group subject allocation, leader-gated connection, bounded
  publish queue, market-data `NatsSource`, reset/reconnect/gap handling.
- `aeron-bridge` sidecar (IPC → NATS) only for deployments that require an Aeron edge.
- Bridge/multicast tests, feed group selection/rebalance and symbol migration.

**Phase 3 — scale and soak.**
- Many symbols/groups, 1,024-stream connections, slow consumers, reconnect storms.
- HPA/PDB/drain and canary.
- Prove the engine's `apply`/Raft p99 is unchanged with the feed on/off.

## 14. Acceptance criteria

- Profile A passes a multi-node multicast acceptance test on the target platform: engine publish
  cost and `apply` p99 are unchanged, and every service instance consumes at the target feed rate
  with acceptable lag.
- A selected fallback (direct NATS or bridge) passes its own 3-AZ EKS-like benchmark and does not
  stall the engine on node loss, reconnect or stream restart; a service instance rebuilds from
  reset.
- One feed group fans out to all service instances without per-instance engine publications.
- Leader transfer produces no duplicate content after dedupe and no ready-but-stale symbol.
- WebSocket drain preserves capacity; no subscriber sees a gap longer than the configured reset
  interval.
- The manager can create, scale, upgrade and roll back the `MarketDataService` with per-symbol
  readiness and lag visible.
- NetworkPolicy/TLS tests prove only engine leaders and market-data pods can reach NATS, and no
  Aeron bus is exposed when the optional profile is unused.
- If the optional Aeron profile is enabled, its multi-node multicast test passes and the cluster is
  explicitly marked for it.

## 15. Open questions

- What are the measured NATS Core vs JetStream p99/throughput numbers at the target fill rate and
  feed-group count?
- How many active publisher connections (one per symbol leader) and market-data consumers should one
  NATS cluster serve before splitting by cell?
- Do we need JetStream retention for restart/incident replay, or is Core NATS + reset snapshots
  sufficient? If JetStream is used, what `max_age`/`max_bytes` and consumer identity?
- What is the EKS topology for NATS: 3 nodes across 3 AZs with EBS gp3, or a managed/operator
  deployment with zone-aware routing?
- Is a binary or compressed NATS payload needed, or is raw wire v7 plus NATS headers sufficient?
- Is the optional Aeron bridge still worth maintaining once direct NATS is benchmarked?
- Can the stream-2010 snapshot request be carried through the bridge, or must the service wait for
  periodic resets?
- What is the maximum supported WebSocket connections/streams per instance before HPA/PDB need a
  dedicated connection router?
- Does the gateway terminate WebSocket TLS, or does the market-data service serve TLS directly?

## 16. References

- AWS EKS, multiple network interfaces and Multus:
  <https://docs.aws.amazon.com/eks/latest/userguide/pod-multiple-network-interfaces.html>,
  <https://docs.aws.amazon.com/eks/latest/userguide/pod-multus.html>
- AWS, multicast on EKS with VPC CNI, Multus and Transit Gateway:
  <https://aws.amazon.com/blogs/networking-and-content-delivery/modernizing-financial-networks-huatai-securities-multicast-implementation-on-aws/>
- Isovalent, enabling multicast on EKS with Isovalent Enterprise for Cilium:
  <https://repost.aws/articles/ARzptt1ZToR0CEjiv0Fjz5iw/enabling-multicast-on-amazon-eks-with-isovalent-enterprise-for-cilium>
- Cilium multicast command/reference:
  <https://docs.cilium.io/en/latest/cmdref/cilium_multicast/>

## 17. Decision

**Use Profile A — direct Aeron multicast — as the default market-data bus.** The engine publishes
one exclusive publication per stream per feed group; market-data pods subscribe independently. This
gives the lowest latency and the cleanest publisher cost, and it matches the existing
`AeronSink`/`AeronSource` implementation.

**Profile A has a mandatory platform prerequisite:** a multicast-capable pod network. Default AWS
EKS/VPC CNI does not provide it. Production EKS must use Cilium/Isovalent Enterprise multicast or
Multus + Transit Gateway multicast, with a multi-node acceptance test for MTU, NetworkPolicy, packet
loss and 10/50/100 subscribers. If the platform cannot provide multicast, use the direct-NATS
fallback (Profile N) or the per-pod Aeron bridge (Profile B); those are supported fallbacks, not the
default.

Reject end-to-end Aeron unicast for the general market-data fleet. Reject cross-pod IPC and shared
`/dev/shm`. The control plane owns `MarketDataService`, feed groups, transport profile and
per-symbol status.

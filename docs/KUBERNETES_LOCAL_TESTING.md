# Local Kubernetes test plan (OrbStack, k3d, minikube)

Status: **proposed test plan** for the control plane in
[`docs/KUBERNETES_CONTROL_PLANE.md`](./KUBERNETES_CONTROL_PLANE.md). No test manifests are checked
in yet.

Last updated: 2026-11-13.

## 1. Short answer

- **Use OrbStack Kubernetes for the first tier.** It is already installed, starts in seconds, is a
  single-node cluster, and shares the local Docker image store, so a locally built engine image can
  run without a registry.
- **Use k3d (or kind) on OrbStack's Docker engine for the second tier.** OrbStack Kubernetes is
  single-node, so it cannot test four voters on four nodes, required pod anti-affinity, PDB drains,
  node loss or zone spread. k3d is a thin wrapper that runs k3s nodes as Docker containers and
  supports `k3d image import`; it is lighter than minikube on this laptop.
- **Minikube is a valid but redundant alternative.** It works with the Docker driver and supports
  multi-node (`minikube start --driver=docker --nodes=4`), but it adds another cluster lifecycle
  and image-loading path on top of OrbStack. Use it only if the team already standardizes on it.
  Do not run OrbStack Kubernetes, minikube and k3d at the same time; they compete for the same
  CPU/memory.

OrbStack Kubernetes on this laptop was verified as:

```
NAME       STATUS   ROLES           AGE   VERSION        INTERNAL-IP     CONTAINER-RUNTIME
orbstack   Ready    control-plane   ...   v1.36.5+orb1   192.168.139.2   docker://29.8.2
```

A local `debian:bookworm-slim` image started with `imagePullPolicy: IfNotPresent`; the kubelet
reported `Container image ... already present on machine`, confirming that OrbStack Kubernetes
shares the Docker image store.

## 2. What each tier can and cannot prove

| Tier | Tool | Proves | Does not prove |
|---|---|---|---|
| L0 | OrbStack Kubernetes (single node) | CRDs, operator reconcile, StatefulSet/headless DNS, per-pod PVC, probes, bootstrap, symbol config, apply/write/read/events, manager GUI port-forward, restart recovery | Required anti-affinity, four voters on four nodes, PDB drain, node/zone loss, cross-node PVC reattach |
| L1 | k3d 4 nodes on OrbStack Docker | Four pods on four nodes, required anti-affinity, topology spread, PDB drain, leader transfer, pod kill, node loss, quorum loss/recovery, logical snapshot/WAL export and replay | One-step cluster restore, real cloud block storage, real network latency, real zones, production fsync throughput |
| L2 | OrbStack or k3d + multi-container/supervisor | Packing 2–5 single-symbol engine processes per pod on this laptop (10–20 on a larger Linux host), port/data isolation, per-symbol leader routing, per-container limits, noisy-neighbour measurement | Multi-host storage/network effects; large-scale capacity |
| L3 | Two k3d clusters | Manager federation, cell registration, placement, cross-cell cutover, cell-down behavior | Production multi-region latency and failure domains |
| L4 | OrbStack + mock engine/CRs | 50–100 CRs, operator workqueue, API-server/GUI scale, list latency | Real Raft load; do not run 100 real engine processes on the laptop |

## 3. Prerequisites and resource budget

Host observed: 10 CPUs, 24 GiB RAM, ~48 GiB free disk. This is enough for a 4-node k3d cluster with
a few shards. Allocate roughly 6–8 CPUs and 12–16 GiB to OrbStack in its settings; keep at least
4 GiB for macOS.

Packing math matters on this host: 20 containers × 4 pods × `250m` CPU / `512Mi` memory requests
20 vCPU / 40 GiB, more than the laptop has. Use the 2–5-container overlay below (`50m` / `128Mi`
per container) for L2, and reserve 20-container pods for a larger Linux host.

```sh
# OrbStack Kubernetes (single node)
orbctl start k8s
orbctl k8s                         # prints the kubeconfig/context help
export KUBECONFIG="$HOME/.orbstack/k8s/config.yml"
export PATH="/Applications/OrbStack.app/Contents/MacOS/xbin:$PATH"   # bundled kubectl
kubectl get nodes -o wide

# Optional: install the usual clients (kubectl is bundled, this is for Helm/k3d)
brew install kubernetes-cli helm k3d
```

Stop it when done:

```sh
orbctl stop k8s
```

Build the engine image once. OrbStack Kubernetes can use it directly; k3d needs an explicit
import.

```sh
docker build -f docker/linux.Dockerfile -t matchengine-raft:k8s .
```

The lab image runs as **root** (there is no `USER` in `docker/linux.Dockerfile`). For a test that
matches the production security context, build a small non-root wrapper and use it in the
manifests:

```dockerfile
# Dockerfile.nonroot
FROM matchengine-raft:k8s
RUN useradd -u 10001 -m -s /usr/sbin/nologin engine \
 && mkdir -p /data && chown -R 10001:10001 /data
USER 10001
```

```sh
docker build -f Dockerfile.nonroot -t matchengine-raft:k8s-nonroot .
```

`fsGroup: 10001` makes the mounted PVC writable even though the image's `/data` is replaced by the
volume. The engine writes only under its data directory and `/tmp`. For a quick L0 smoke test you
may keep the root lab image and omit `runAsNonRoot`/`runAsUser`; do not carry that exception into
staging or production.

## 4. Tier L0: control plane on OrbStack Kubernetes

### 4.1 Apply the dedicated-shard manifest

Start from `deploy/test/orbstack/` (to be added) or the example in
`docs/KUBERNETES_CONTROL_PLANE.md` §5.3, with these single-node adjustments:

- Use `preferredDuringSchedulingIgnoredDuringExecution` anti-affinity instead of `required`; four
  pods must coexist on the one OrbStack node. Also drop the zone topology-spread constraint (or
  keep it `ScheduleAnyway`), because OrbStack nodes have no `topology.kubernetes.io/zone` label.
- Use the `local-path` storage class and a small PVC (e.g. `2Gi`) for the smoke test.
- Set `RAFT_WAL_FSYNC_MS=0` for durable writes in the test.
- Use `image: matchengine-raft:k8s` and `imagePullPolicy: IfNotPresent` for a quick smoke test, or
  `matchengine-raft:k8s-nonroot` for the production-shaped run.
- Set `automountServiceAccountToken: false` and do not create a ServiceAccount; the engine needs no
  Kubernetes API access.
- Add a writable `emptyDir` at `/tmp` when `readOnlyRootFilesystem: true` is set.
- If you use the root lab image, omit `runAsNonRoot`, `runAsUser` and `fsGroup` (test only). With
  the non-root image, use `runAsUser`/`runAsGroup`/`fsGroup: 10001` and
  `fsGroupChangePolicy: OnRootMismatch`.
- For a packed shard, add one container per symbol with distinct HTTP/gRPC ports and data
  directories; each container runs the existing single-symbol binary.

```sh
kubectl create namespace match-test
kubectl -n match-test apply -f deploy/test/orbstack/dedicated-shard.yaml

kubectl -n match-test get pods -o wide -w
# wait for four pods: me-eurusd-test-0..3 Running
```

### 4.2 Verify DNS and ports

From a peer or a debug pod in the same namespace:

```sh
kubectl -n match-test run netcheck --rm -it \
  --image=debian:bookworm-slim --restart=Never --command -- bash

# inside the pod
getent hosts me-eurusd-test-0.me-eurusd-test-headless.match-test.svc.cluster.local
# HTTP 21001 and Raft 22001 must both be reachable from another voter
```

From the host, port-forward pod 0:

```sh
kubectl -n match-test port-forward pod/me-eurusd-test-0 21001:21001
curl -s http://127.0.0.1:21001/health
curl -s http://127.0.0.1:21001/ready     # 503 until the cluster is initialised
curl -s http://127.0.0.1:21001/metrics | jq '{current_leader, last_log_index, last_applied}'
```

`/metrics` is JSON, not Prometheus text. Prometheus needs a bridge or a text endpoint (see the
control-plane doc §11); for L0, `jq` is enough.

### 4.3 Bootstrap once

The current engine uses the single-symbol `/init` shape with `[id, addr]` pairs:

```sh
curl -s -X POST http://127.0.0.1:21001/init \
  -H 'Content-Type: application/json' \
  -d '[[0,"me-eurusd-test-0.me-eurusd-test-headless.match-test.svc.cluster.local:21001"],
      [1,"me-eurusd-test-1.me-eurusd-test-headless.match-test.svc.cluster.local:21001"],
      [2,"me-eurusd-test-2.me-eurusd-test-headless.match-test.svc.cluster.local:21001"],
      [3,"me-eurusd-test-3.me-eurusd-test-headless.match-test.svc.cluster.local:21001"]]'
```

Then:

1. Wait for a leader and `/ready` on every pod.
2. Apply the symbol configuration through the leader's `/write`:
   ```sh
   curl -s -X POST http://127.0.0.1:21001/write \
     -H 'Content-Type: application/json' \
     -d '{"ConfigureSymbol":{"config":{
       "symbol_id":0,
       "base_asset":{"code":"EUR","class":"fiat","decimals":2},
       "quote_asset":{"code":"USD","class":"fiat","decimals":2},
       "price_decimals":5,"price_tick":"0.00001","base_lot":"0.01",
       "fee_asset_policy":"received"}}}'
   ```
3. Verify `/read symbol_config`, `/pretrade/status`, a write, `/read orderbook_orders`, and
   `POST /events`.
4. Call `/init` again and confirm it is rejected; the operator must never do this.

### 4.4 Restart and recovery

- Delete the leader pod: `kubectl -n match-test delete pod me-eurusd-test-0`; watch the new leader,
  `/ready` recovery and the PVC reattach.
- Delete a follower; the cluster stays writable with three of four voters.
- Restart the whole StatefulSet and verify WAL replay and that the book/sequence survive.
- Inspect `kubectl describe pod`, `/metrics` (`last_applied`, `last_log_index`) and the event stream.

**Zero-impact add (supervisor mode).** Create a supervisor-mode shard, record the pod UIDs and the
supervisor `/processes` PIDs, then add a second symbol and apply the updated ConfigMap. The pod
UIDs and existing child PIDs must not change; only the new child appears. Containers-mode additions
to a non-empty shard are rejected unless `allowRestart=true`; that path restarts every engine in
the shard one voter at a time and is not the zero-impact check.

### 4.5 Manager and GUI

Run the manager API/GUI as a local process (or deploy it into the test namespace) and port-forward
it. A Service-backed port-forward picks one pod, so target the Deployment for a stable UI:

```sh
kubectl -n match-test port-forward deploy/match-manager 8080:8080
open http://127.0.0.1:8080
```

If the manager runs on the host, point it at the OrbStack kubeconfig
(`KUBECONFIG="$HOME/.orbstack/k8s/config.yml"`) and let it read the test namespace. Leader
discovery must come from `SymbolShard.status.symbols[].leaderAddr` /
`SymbolDeployment.status.leaderAddr` (or the operator's route status), not from a pod label or a
leader Service. For L0, a dev auth mode is enough; wire OIDC only in the staging cell. Verify the
shard/symbol list, per-symbol leader/port, conditions, operations and audit entries.

### 4.6 What L0 deliberately does not test

- Required anti-affinity, four nodes, zone spread, PDB drain, node loss.
- Prometheus scraping: `/metrics` is JSON, so L0 checks it with `jq`; a bridge/text endpoint is a
  separate test.
- Cross-node PVC reattach. `local-path` PVs are node-local; a pod rescheduled to another node
  cannot see the old data. This is expected; production uses a block storage class with zone
  reattach.
- Real fsync latency and market-data Aeron. The engine should run with market data disabled for
  these tests.

## 5. Tier L1: multi-node Raft on k3d

Create a four-node cluster (one server, three agents) on OrbStack's Docker engine:

```sh
k3d cluster create match-test \
  --servers 1 --agents 3 \
  --port '8080:80@loadbalancer' \
  --port '8443:443@loadbalancer'

k3d image import matchengine-raft:k8s-nonroot -c match-test
kubectl config use-context k3d-match-test
kubectl get nodes -o wide
```

Apply the production-shaped manifest with **required** anti-affinity and a PDB
(`minAvailable: 3`). Then run the checks that L0 cannot:

### 5.1 Placement and DNS

- Four pods must land on four different nodes.
- Each pod resolves the other three FQDNs and reaches `:22001`.
- `kubectl get endpoints` for the headless Service lists all four pods.

### 5.2 PDB drain

```sh
kubectl drain <node> --ignore-daemonsets --delete-emptydir-data
```

One pod at a time must move; a second eviction for the same shard must be blocked while
`minAvailable: 3` is violated. Because `local-path` is node-local, a rescheduled pod may stay
Pending; for the drain test, use a storage class that can reattach or accept the limitation and
test scheduling/PDB separately from data recovery.

### 5.3 Leader transfer and failover

- Use the console/API to transfer leadership to a named follower; verify `/metrics`
  `current_leader`, the per-symbol leader map and a successful write after the transfer.
- Kill the leader pod; verify a new leader within the election budget and a retried write
  (with `client_order_id`) without duplication.

### 5.4 Node loss and quorum

Find the container names (`docker ps --filter name=k3d-match-test`), then:

```sh
docker stop k3d-match-test-agent-0
# 3/4 voters remain; writes continue; the stopped voter catches up after start
docker start k3d-match-test-agent-0
```

Stop two voters: writes must fail (no quorum), the manager must show `Degraded`, and restarting the
pods must recover. This is the most important Raft test L0 cannot do.

### 5.5 Topology and zones

k3d nodes are containers on one host, so real zones do not exist. Label the nodes to exercise the
scheduler:

```sh
kubectl label node k3d-match-test-agent-0 topology.kubernetes.io/zone=z1
kubectl label node k3d-match-test-agent-1 topology.kubernetes.io/zone=z2
...
```

Verify the topology-spread behavior and document that this is a scheduling test, not a network
failure-domain test. Label **every** schedulable node before switching the constraint to
`DoNotSchedule`; a required zone spread with an unlabeled node leaves pods `Pending`.

### 5.6 Backup/restore

- `local-path` does not provide CSI snapshots. For L1, trigger the engine's snapshot, stop the
  process (or quiesce writes), then copy the whole process directory: `snapshot/current.snap`, the
  WAL segments and `log/state`. A snapshot file alone is not a complete backup. Treat the copy as
  an export, not a one-step cluster restore; verify determinism by replaying the order input into
  a fresh cluster and comparing the book/sequence.
- Four independent PVC copies are crash-consistent per volume, not atomic across the shard. For
  L1, quiesce writes before copying; for production, use a CSI `VolumeGroupSnapshot` (control-plane
  doc §7.5). Do not restore one voter from an older copy while the others are newer.
- If you need `VolumeSnapshot`, install the CSI snapshot controller and a compatible driver; this
  is a separate lab, not required for the first pass. A group snapshot also needs a CSI driver that
  implements `VolumeGroupSnapshot`.

## 6. Tier L2: packing multiple engine processes per pod

### 6.1 Multi-container pod (primary, works today)

Run a packed pod with two engine containers, each one symbol; this is the production shape for
packed shards, just at a smaller scale. On this laptop, use a **2–5-container overlay** at `50m` CPU
/ `128Mi` memory per container: 5 containers × 4 pods is 1 vCPU / 2.5 GiB of requests, which fits.
Do not request the production 20 × `250m` / `512Mi` shape here (20 vCPU / 40 GiB); run that on a
larger Linux host.

- Container `eurusd`: HTTP `21001`, gRPC `22001`, data `/data` (same PVC),
  `--instance-prefix eurusd` → `/data/eurusd-<ordinal>`.
- Container `gbpusd`: HTTP `21002`, gRPC `22002`, data `/data`,
  `--instance-prefix gbpusd` → `/data/gbpusd-<ordinal>`.
- Do not mount a per-symbol subPath and also set a per-symbol data dir; that double-nests
  (`/data/eurusd/eurusd-0`).
- Pod network namespace is shared; ports must not collide.
- Each container has its own resource requests/limits; the pod total is the sum.
- Each container has its own probes and `/metrics` on its port.

Test:

- Both symbols bootstrap independently and reach `/ready` on their ports.
- Killing one container does not stop the other; the pod restarts only the failed container.
- The gateway routes each symbol to its own leader address (the deployment's `leaderAddr`).
- Add a third container with a rolling update; verify existing symbols restart one voter at a time
  and keep quorum, then initialise the new symbol.
- Measure `kubectl top pod` and per-container CPU/memory; this is the baseline for supervisor mode
  and for the production density limit.

This tier needs **no engine change** and is the fastest way to validate the packing CRD/operator
and GUI.

### 6.2 Supervisor container (optional, dynamic)

- Run a small supervisor container per pod that spawns/stops the existing single-symbol engine
  processes; start with 2–5 here, and only go to 20 on a larger Linux host.
- Add a process via the supervisor API (driven by a `SymbolDeployment`); verify no other symbol
  restarts and the new process initialises on all four pods.
- Kill one engine process; the supervisor restarts it and the other symbols keep serving.
- Stop a process and verify graceful shutdown, port release and data retention.
- Compare CPU/memory with containers mode: supervisor mode shares one container limit and has no
  per-process cgroup isolation unless the supervisor manages cgroups.
- No engine refactor and no special paths or group IDs: each process is the existing binary on its
  own port.

## 7. Tier L3: multi-cell and manager

- Create a second k3d cluster (`k3d cluster create match-eu-2 ...`).
- Register both cells in the manager; place 5 symbols in cell A and 5 in cell B.
- Verify the GUI aggregates shards and symbols, and that a cell API failure shows as `Unreachable`
  without blocking the other cell.
- Exercise a symbol move (process migration within a cell) and a cross-cell cutover (new
  `SymbolDeployment` in the target cell + `SymbolRoute` switch).
- Resource note: two k3d clusters plus the manager will use most of the laptop; stop one cluster
  between phases.

## 8. Tier L4: control-plane scale without hundreds of real engine processes

The laptop should not run 100 real four-voter clusters. Test the control plane with:

- a mock engine (a tiny HTTP server that answers `/health`, `/ready`, `/metrics`, `/init`,
  `/write`) or a fake source in the operator;
- 100 `SymbolShard`/`TradingSymbol`/`SymbolDeployment`/`SymbolRoute` CRs with 1–5 symbols each;
- measure operator reconcile latency, API-server QPS, manager list latency, and GUI rendering;
- separately run 10–20 real shards to validate the data path.

This separates "operator/GUI scale" from "Raft capacity", which are different risks.

## 9. Proposed test harness layout

```
deploy/test/
  orbstack/
    dedicated-shard.yaml       # single node, preferred anti-affinity, local-path
    packed-multi-container.yaml
    manager-dev.yaml
    metrics-bridge.yaml        # optional JSON -> Prometheus text sidecar
    Dockerfile.nonroot         # wraps matchengine-raft:k8s with UID 10001
  k3d/
    cluster.sh                 # create/delete/import images
    dedicated-shard.yaml       # required anti-affinity, PDB
    packed-containers.yaml     # N engine containers per pod
    packed-supervisor.yaml     # supervisor spawns N engine processes
  smoke/
    bootstrap.sh               # wait, init, configure, write/read/events
    chaos.sh                   # kill leader/node, drain, quorum loss
scripts/k8s-local.sh           # start/stop/apply/smoke wrapper
```

Acceptance for the first pass:

- Four pods ready; every symbol has a leader and `/ready`.
- Bootstrap is idempotent; a second `/init` is rejected.
- One pod kill and one leader kill recover without data loss (with fsync=0).
- PDB allows only one voluntary disruption; k3d node loss leaves quorum and recovers.
- The manager shows shard/symbol status, leaders, routes and audit entries.
- A two-container packed pod proves port/data isolation without an engine refactor.
- The non-root security context works (or the root exception is explicitly recorded for L0).

## 10. Minikube verdict

Minikube is **not required** on this laptop, but it is a reasonable fallback if the team already
uses it:

```sh
minikube start --driver=docker --nodes=4 --cpus=2 --memory=4096
minikube image load matchengine-raft:k8s        # or :k8s-nonroot
kubectl config use-context minikube
```

Those flags request 8 vCPU / 16 GiB in total across four nodes, plus the Docker/VM overhead; on a
24-GiB host, use two nodes or lower per-node memory, and do not run it alongside OrbStack
Kubernetes. `--nodes` is also a per-node resource multiplier, not a single pool.

Pros: mature multi-node Docker driver, addons (metrics-server, ingress), familiar workflow.
Cons: another VM/container lifecycle and context; image loading is a separate path; resource use is
higher than OrbStack Kubernetes + k3d; it duplicates what OrbStack already provides.

Recommendation:

- **L0/L2/GUI:** OrbStack Kubernetes.
- **L1/multi-node:** k3d on OrbStack Docker.
- **Minikube:** only if required by an existing team standard; do not run it concurrently with the
  OrbStack cluster.

## 11. Caveats specific to this laptop

- OrbStack Kubernetes is **single-node**. Required anti-affinity will leave pods Pending; use
  preferred anti-affinity or k3d. Zone topology constraints with `DoNotSchedule` also leave pods
  Pending because the node has no zone label.
- The engine's `/metrics` is JSON. Use `jq` for L0 and add a bridge/text endpoint before testing
  Prometheus scraping.
- The checked-in lab image runs as root. Use the non-root wrapper above for the production-shaped
  security context; omit `runAsNonRoot` only for a quick L0 smoke test.
- Packing is bounded by the laptop: 20 containers × 4 pods at `250m` / `512Mi` would request
  20 vCPU / 40 GiB. Use the 2–5-container `50m` / `128Mi` overlay here.
- OrbStack/K3d nodes share the Mac's CPU, memory and disk. Do not use these results for latency or
  throughput claims; use them for correctness, scheduling and failure behavior.
- `local-path` PVs are node-local. Pod rescheduling across nodes may not reattach data; use it to
  test scheduling, not cross-node storage recovery.
- No external LoadBalancer by default (`k8s.expose_services: false`); use `port-forward` or
  NodePort. The public gateway/TLS path needs a separate ingress test.
- CSI snapshots are not installed. Use logical snapshot/WAL copies for the first backup test, or
  install the snapshot controller as a separate lab.
- Aeron market data is not part of these tests. The engine should run with market data disabled;
  the rusteron 0.2.13 precompiled Linux libraries are x86_64-only and need the OrbStack x86_64 lab
  or the JSONL fallback.
- Fsync behavior on OrbStack virtual disks is not production-representative. Use `fsync=0` for
  durable test runs; do not infer p99 from this environment.
- Stop OrbStack Kubernetes and k3d clusters when not in use: they consume CPU/memory even when
  idle, and macOS sleep can break long-lived clusters.

## 12. Suggested order

1. L0 on OrbStack: one dedicated shard, bootstrap, config, write/read/events, restart, GUI.
2. L1 on k3d: placement, PDB, leader transfer, pod kill, node loss, quorum loss.
3. L2 containers: two engine containers in one pod; validate packing assumptions and the gateway
   leader map; then 5 containers at `50m` / `128Mi` and measure. Reserve 10–20 containers for a
   larger Linux host.
4. L2 supervisor: dynamic process start/stop and process-failure recovery.
5. L3: two cells and manager placement.
6. L4: 100 mock CRs for operator/GUI scale.

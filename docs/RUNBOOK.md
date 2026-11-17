# Runbook

> Engine commands below run from `match_engine/` or with
> `--manifest-path match_engine/Cargo.toml` from the repository root. Run the orchestration scripts
> from the repository root; standalone projects have their own manifests.

## Start a cluster

### Quick Start with Test Script

The easiest way to see the cluster in action is to run the provided test script:

```shell
./scripts/test-cluster.sh
```

This script builds a four-node cluster on `127.0.0.1:21001-21004` (gRPC Raft on `22001-22004`),
initializes it, adds learners, changes membership, places resting orders and reads the book back
from every node. It uses only `curl` against the HTTP API, showing the request/response flow. It
stores state in `/tmp/matchengine-raft-test-cluster` and clears that directory on start, so reruns
are reproducible; set `RAFT_DATA_DIR` or `--data-dir` to keep state somewhere else (add `--fresh`
to wipe an explicit directory).

Useful options:

```shell
./scripts/test-cluster.sh --nodes 3              # smaller cluster
./scripts/test-cluster.sh --no-fsync              # production no-fsync profile (4+ voters)
./scripts/test-cluster.sh --orders 100            # seed more resting orders
./scripts/test-cluster.sh --release                # build/run target/release/matchengine-raft
./scripts/test-cluster.sh --keep                  # leave the cluster running
./scripts/test-cluster.sh status                  # health/readiness and leader
./scripts/test-cluster.sh stop                    # stop the kept cluster
```

`--no-fsync` unsets `RAFT_WAL_FSYNC_MS`, so the binary requires at least four voters and the script
initializes all nodes in one `/init` call. Without it the script uses `RAFT_WAL_FSYNC_MS=0` (fsync
every append) so clusters with fewer than four nodes remain valid. `--base-port` moves the whole
port range, and `--wait` bounds each readiness step.

### Using the Rust Tests

`cargo test` runs the cluster integration suite; see [Testing](./TESTING.md) for the test matrix.

### Manual Cluster Setup

#### 1. Start Nodes

Start the first node:

```shell
./target/debug/matchengine-raft --id 1 --http-addr 127.0.0.1:21001
```

Start additional nodes (in separate terminals):

```shell
./target/debug/matchengine-raft --id 2 --http-addr 127.0.0.1:21002
./target/debug/matchengine-raft --id 3 --http-addr 127.0.0.1:21003
./target/debug/matchengine-raft --id 4 --http-addr 127.0.0.1:21004
```

Each node listens for gRPC Raft RPCs on `port + 1000` (`22001-22004` here). The HTTP listener is
reserved for the application and management API.

All state is written to `<data-dir>/<instance-prefix>-<id>/`; see
[Per-node data directory](./STORAGE.md#per-node-data-directory).

#### 2. Initialize the Cluster

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '[]' \
  http://127.0.0.1:21001/init
```

`/init` takes a JSON array. An empty array (`[]`) initializes the receiving node as a single-node
cluster; a non-empty array of `[id, "host:port"]` pairs initializes a whole cluster at once.
A bare POST or `{}` is rejected by the deserializer. This initializes node 1 as the leader.

#### 3. Add Learners

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '[2, "127.0.0.1:21002"]' \
  http://127.0.0.1:21001/add-learner

curl -X POST -H "Content-Type: application/json" \
  -d '[3, "127.0.0.1:21003"]' \
  http://127.0.0.1:21001/add-learner

curl -X POST -H "Content-Type: application/json" \
  -d '[4, "127.0.0.1:21004"]' \
  http://127.0.0.1:21001/add-learner
```

#### 4. Change Membership

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '[1, 2, 3, 4]' \
  http://127.0.0.1:21001/change-membership
```

#### 5. Write and Read Orders

Place an order:

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '{"Place":{"order":{"live":"Limit","side":"Buy","price":4000.0,"volume":4.4,"id":1,"sequence":0}}}' \
  http://127.0.0.1:21001/write
```

Read the order book from any node:

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '"orderbook_orders"' \
  http://127.0.0.1:21002/read
```

You should be able to see the order from any node!

### Using the Helper Script

The project also includes `scripts/test.sh` for more convenient cluster management:

```shell
# Start a node
./scripts/test.sh start-node 1

# Initialize node 1 and add the other two as learners
./scripts/test.sh build-cluster 1

# Promote the learners to voters
./scripts/test.sh change-membership

# Check metrics
./scripts/test.sh metrics 1

# Place orders
./scripts/test.sh place-order 1

# Read the sequence counter
./scripts/test.sh get-seq 1

# Clean up (the data directory is renamed, not deleted)
./scripts/test.sh clean
```

`scripts/test.sh` keeps all state under `RAFT_DATA_DIR` (default `/tmp/matchengine-raft`), so
`place-order` results survive `start-node`/`kill` cycles.

## Membership and leadership

Adding a node to a cluster involves 3 steps:

1. **Write the node info** through the Raft protocol to storage
2. **Add as Learner** to let it start receiving replication data from the leader
3. **Change membership** to promote the learner to a full voting member

> **Note**: Raft itself does not store node addresses. This implementation stores node information in the storage layer, and the network layer references the store to lookup target node addresses.

## Kubernetes control plane (OrbStack demo)

The Kubernetes control plane is the standalone manager in `k8s_control_plane` plus the
engine image's supervisor binary. It is a **desired-state writer**, not a Kubernetes controller: it
validates/places symbols, renders manifests, and returns operator steps. The operator (you) still
applies ConfigMaps/workloads and performs PVC operations.

Read [KUBERNETES_CONTROL_PLANE.md](./KUBERNETES_CONTROL_PLANE.md) for the design and
[KUBERNETES_LOCAL_TESTING.md](./KUBERNETES_LOCAL_TESTING.md) for the test tiers. The reference demo
details are in [`k8s_control_plane/README.md`](../k8s_control_plane/README.md).

> This is the local OrbStack reference tier. The manager writes desired state and returns operator
> steps; it is **not** a Kubernetes controller. You still apply ConfigMaps/workloads and perform
> PVC operations. Treat every `delete-storage` step as irreversible.

### Control-plane quick reference

| Task | Manager action | Notes |
|---|---|---|
| List shards | `GET /api/v1/shards` or GUI sidebar | `processes`, `disabledProcesses`, `freeSlots`, phase |
| Shard detail | `GET /api/v1/shards/{shard}` or click the card | deployments, routes, consoles, simulated observation |
| Render manifests | `GET /api/v1/shards/{shard}/manifests?profile=local` | save the List before a drop if you need `kubectl delete -f` |
| Add a symbol | `POST /api/v1/symbols` or **+ symbol** | supervisor is zero-impact; containers may return `409 restart_required` |
| Stop an engine | `POST /api/v1/symbols/{symbol}/shutdown` or **stop** | keeps the deployment record, slot/ports and PVC |
| Start an engine | `POST /api/v1/symbols/{symbol}/start` or **start** | reuses the record/slot/PVC; do not re-run `/init` |
| Drop a shard | `POST /api/v1/shards/{shard}/drop` or **drop shard** | catalog-only; requires every engine stopped; asks `retainStorage`; the operator prunes workloads/PVCs |
| Audit | `GET /api/v1/audit` | `shutdown-symbol`, `start-symbol` and `drop-shard` are recorded |

The GUI is served at `/` (and `/console`). From a workstation:

```sh
kubectl -n match-control port-forward deploy/match-manager 8099:8099
open http://127.0.0.1:8099/
```

### Safety model and data locations

- One engine process trades one symbol; a shard is four pods sharing a StatefulSet.
- **Supervisor** shards start/stop one child per pod without a pod-template change. **Containers**
  shards cannot add or remove a container without recreating every pod, so the manager returns
  `409 restart_required` unless `allowRestart: true`. The last active engine in a containers shard
  is zero-impact to stop or start.
- A stopped deployment stays in the catalog as `Disabled`. Its slot and ports stay reserved and
  `freeSlots` does not increase; this prevents a new symbol from reusing the data.
- Engine state lives under `/data/<symbol>-<pod>` inside the PVC named `data-<shard>-<pod>`.
  Dropping a shard with `retainStorage: true` keeps those PVCs; `false` deletes them.
- The manager never applies a workload or deletes a PVC. If you lose the catalog before applying
  the returned steps, inspect the cluster and prune by name.

### What runs where

| Component | Namespace | Address / image | Notes |
|---|---|---|---|
| Manager API + GUI | `match-control` | `matchengine-k8s-control-plane:local`, port `8099` | Embedded GUI at `/`; JSON demo at `/data/catalog.json` or CRD mode through the API server |
| Cell operator | `match-control` | same image, `/usr/local/bin/match-operator` | Lease-elected; watches CRDs, applies/prunes workloads, bootstraps/configures Raft, rolls containers one pod at a time, labels leaders and writes status |
| Engine shard | `match-eu-1` | `matchengine-raft:k8s-nonroot` | Four voters; one engine process per symbol |
| Supervisor pod | engine shard pods | `/health`, `/ready`, `/processes` on `9090` | One container per pod; children are one-process engines |
| Engine console | each engine HTTP port | `/console/` | The manager reverse-proxies it; the symbol is the hero label |

The Raft port is always `httpPort + 1000`. A `SymbolShard` is four pods. A supervisor shard hosts
several child processes per pod; containers mode hosts one engine container per symbol.

The reference OrbStack deployment used during validation (2026-11-14):

| Shard | Mode | Symbols / role |
|---|---|---|
| `fx-eu-01` | packed containers | `eurusd`, `gbpusd` |
| `fx-eu-02` | packed containers | `usdjpy`, `audusd` |
| `fx-eu-03` | reserved packed | no workload; safe target for a reserved-shard drop |
| `fx-sup-01` | packed supervisor | `eurgbp`, `nzdusd`; both children run in every pod |
| `me-btcusdt` | dedicated containers | `btcusdt` |

### Build the images

```sh
# Engine + supervisor (must be rebuilt after changes under match_engine/web/console/)
docker build -f match_engine/docker/k8s.Dockerfile -t matchengine-raft:k8s-nonroot match_engine

# Manager API + embedded GUI
docker build -f k8s_control_plane/Dockerfile \
  -t matchengine-k8s-control-plane:local k8s_control_plane

docker images | grep -E 'matchengine-raft|matchengine-k8s-control-plane'
```

OrbStack shares the local Docker image store with its Kubernetes node. Use
`imagePullPolicy: IfNotPresent`; a rebuilt `:local` tag is picked up when the pod is recreated.

### Deploy the manager

```sh
kubectl create namespace match-eu-1 --dry-run=client -o yaml | kubectl apply -f -
kubectl create namespace match-control --dry-run=client -o yaml | kubectl apply -f -
kubectl label namespace match-control matching.io/role=control-plane --overwrite

kubectl -n match-control create deployment match-manager \
  --image=matchengine-k8s-control-plane:local \
  -- /usr/local/bin/k8s-control-plane serve --addr 0.0.0.0:8099
kubectl -n match-control set env deploy/match-manager \
  RUST_LOG=info
kubectl -n match-control expose deployment match-manager --port=8099 --target-port=8099
kubectl -n match-control rollout status deploy/match-manager --timeout=120s
```

Add a writable volume at `/data` and pass `--state /data/catalog.json` for the JSON demo catalog;
the live demo uses an `emptyDir`, so a manager rollout reseeds the 3-shard demo catalog. For
production desired state, use CRD mode instead (see below).

#### CRD mode (recommended for production)

Desired state lives in `matching.io` CRDs and is persisted by the Kubernetes API server; the
manager never connects to etcd directly. Install the CRDs and RBAC first:

```sh
cargo run --manifest-path k8s_control_plane/Cargo.toml -- crds \
  | kubectl apply -f -
kubectl apply -f k8s_control_plane/deploy/rbac.yaml
```

The manager pod must use the `match-manager` ServiceAccount and mount its token. The demo manifest
currently sets `automountServiceAccountToken: false`, so patch it:

```sh
kubectl -n match-control patch deploy match-manager --type=merge \
  -p '{"spec":{"template":{"spec":{"serviceAccountName":"match-manager","automountServiceAccountToken":true}}}}'
```

Run the manager with `--crd --no-seed` for a real cluster. The cache is bootstrapped by listing the
CRDs and then refreshed by one informer per kind:

```sh
kubectl -n match-control set args deploy/match-manager -- \
  serve --addr 0.0.0.0:8099 --crd --crd-namespace match-eu-1 --no-seed
kubectl -n match-control rollout status deploy/match-manager --timeout=120s
```

`GET /api/v1/health` reports `"store":"crd"` in this mode. Status writes use the `/status`
subresource and are rate-limited per object; audit history remains in memory in the demo and must
move to an external append-only store in production. Engine WAL/snapshot, order-book, match-event
and market-data payloads never go into CRDs/etcd.

#### Deploy the cell operator

The operator is the reconciler that applies/prunes workloads, writes status, and drives engine
bootstrap, configuration and containers-mode rollouts. Install the CRDs and RBAC first, then the
two-replica operator Deployment (Lease handover):

```sh
kubectl apply -f k8s_control_plane/deploy/crds.yaml
kubectl apply -f k8s_control_plane/deploy/rbac.yaml
kubectl apply -f k8s_control_plane/deploy/operator.yaml
kubectl -n match-control rollout status deploy/match-operator --timeout=120s

# Only the Lease holder reconciles.
kubectl -n match-control get lease match-engine-operator \
  -o jsonpath='{.spec.holderIdentity} {.spec.renewTime}{"\n"}'
kubectl -n match-control logs deploy/match-operator --tail=20
```

For a local binary run, use `--namespace` to scope watches/pruning to one namespace and a local
Lease:

```sh
cargo run --manifest-path k8s_control_plane/Cargo.toml --bin match-operator -- \
  --namespace match-eu-1 \
  --lease-namespace match-control --lease-name match-engine-operator \
  --profile local --resync-seconds 10
```

What it reconciles:

- **Apply/prune** the rendered ConfigMap, Services, PDB, NetworkPolicy and StatefulSet. Pruning is
  limited to objects labeled `app.kubernetes.io/managed-by=match-engine-operator`; a shard that
  fails to render is not pruned. Shard PVCs are retained; only orphaned PVCs past the retention
  window are deleted.
- **Leader Service labels** for dedicated shards: the engine-reported leader pod gets
  `matching.io/role=leader`; the other pods lose it. The `<shard>-leader` Service selects that pod.
- **Status/conditions** for `SymbolShard`, `SymbolDeployment`, `TradingSymbol` and `SymbolRoute`,
  from each pod's `/health`: phase, readiness, leader pod/address, applied index/lag and
  `observedGeneration`. `SymbolRoute.status.leaderAddr` is only published when the deployment is
  Ready.

- **Bootstrap/config/membership** — once every pod for a deployment is healthy, run `/init` for an
  empty group, add missing learners, change membership to the desired voters and apply
  `ConfigureSymbol` on the leader. Routes only publish `leaderAddr` after the config matches.
- **Containers rolls** — replace non-running pods first, gate each running-pod deletion on a
  healthy quorum for every process it hosts, and transfer leadership only when the removal is
  already quorum-safe. The operator ServiceAccount needs `pods/delete` in addition to
  list/watch/patch.
- **Retention** — an orphaned shard PVC gets `matching.io/retain-until` (a Unix deadline or
  `manual`). After the deadline the operator deletes it; a returning shard clears the marker.
  Configure with `--retention-seconds` (`0` keeps the manual marker).
- **Migration** — a `SymbolDeployment` with `desiredState: Shadow` and a non-overlapping
  `raftIdOffset` is added to the source leader as a learner; after catch-up the operator changes
  membership, switches the route, activates the target and drains the source. Set route
  `state: Rollback` to reverse. The operator needs `patch`/`update` on the CRs for this.

`--dry-run` logs desired counts without applying, writing status, bootstrapping, rolling or
retention changes.

The manager needs network access to engine pods for the console proxy. The rendered NetworkPolicy
allows HTTP from namespaces labelled `matching.io/role=control-plane`; keep that label if you serve
the GUI from another namespace. It also allows same-cell engine pods to reach each other on all
ports for cross-shard migration.

Port-forward the manager for a workstation browser:

```sh
kubectl -n match-control port-forward deploy/match-manager 8099:8099
# http://127.0.0.1:8099/
```

When the manager runs on the workstation instead of in the cluster, point the console proxy at
loopback with `cargo run --manifest-path k8s_control_plane/Cargo.toml -- serve
--engine-console-host 127.0.0.1`; ports still come from each deployment's catalog placement.

#### Adopt an existing fleet (brownfield)

Use this when the engines and PVCs already exist and the manager has not yet been switched to CRD
mode. The goal is to reproduce the live placement exactly, adopt the objects, and only then let the
operator change anything. Keep `--retention-seconds 0` until the adoption is verified. Build the
demo binaries once so the commands below resolve:

```sh
cargo build --manifest-path k8s_control_plane/Cargo.toml \
  --bin k8s-control-plane --bin match-operator
```

1. **Back up the live state.** Never start an operator with an empty or incomplete CR catalog:
   live StatefulSets carry `app.kubernetes.io/managed-by=match-engine-operator`, so an empty
   catalog prunes them.

   ```sh
   mkdir -p /tmp/match-adopt
   kubectl -n match-eu-1 get sts,deploy,svc,cm,pdb,networkpolicy,pvc -o yaml \
     > /tmp/match-adopt/fleet.yaml
   kubectl -n match-eu-1 get pvc -o wide > /tmp/match-adopt/pvc.txt
   kubectl -n match-eu-1 get pods -o wide > /tmp/match-adopt/pods-before.txt
   ```

   Capture the data baseline for every symbol (`orderbook_sequence` and `orderbook_orders` on the
   leader or through the manager console proxy). Save the manager's current catalog too:
   `curl -s http://127.0.0.1:8099/api/v1/catalog > /tmp/match-adopt/catalog.json`.

2. **Rebuild and validate the catalog.** Reuse the old manager catalog if it matches the live
   placement; otherwise reconstruct it from the StatefulSets, ConfigMaps and symbol configs. Set
   `raftIdOffset: 0` on existing deployments, clear `status`, and validate:

   ```sh
   k8s-control-plane validate --data /tmp/match-adopt/catalog.json
   k8s-control-plane crd-restore --input /tmp/match-adopt/catalog.json --namespace match-eu-1
   ```

3. **Diff before adopting.** Render the full desired set and compare it with the live objects:

   ```sh
   k8s-control-plane render --data /tmp/match-adopt/catalog.json --all --profile local \
     > /tmp/match-adopt/rendered.json
   kubectl apply --server-side --dry-run=server -f /tmp/match-adopt/rendered.json
   ```

   Expected differences on an older fleet are the operator-owned fields (`--id-offset`,
   supervisor `--max-processes`), the same-cell NetworkPolicy rules, and PVC labels. Existing
   StatefulSets cannot gain labels on `volumeClaimTemplates`; the operator labels PVCs after
   creation instead, so do not add them back to the template. The operator force-applies and takes
   over fields previously owned by `kubectl-client-side-apply`.

4. **Start the operator conservatively.** Use a dedicated Lease name for the first run and
   `--retention-seconds 0`. A dry run first is safe:

   ```sh
   match-operator --namespace match-eu-1 --lease-namespace match-control \
     --lease-name match-engine-operator-adopt --profile local --resync-seconds 10 --dry-run
   ```

   Confirm the desired object count and that no workload is reported for pruning, then start it
   without `--dry-run`. Watch `reconcile complete` and `SymbolShard`/`SymbolDeployment` phases. The
   first apply updates the StatefulSet template; existing pods keep running until a rollout is
   triggered. If an adopted StatefulSet still has client-side list ownership, a container removal
   can fail server-side-apply validation; the operator retries that StatefulSet with a merge patch,
   which replaces the container array cleanly.

5. **Upgrade the image deliberately.** The new template carries `--id-offset`, which the old
   engine binary does not understand. Build and publish an image with `--id-offset` and SIGTERM
   handling, set `shard.spec.image` in the CR, and let the operator roll containers-mode shards one
   pod at a time. Supervisor shards are intentionally skipped by the rollout controller; transfer
   each hosted symbol's leadership first, then delete one supervisor pod at a time.

6. **Verify adoption.** Check that every `SymbolShard`, `SymbolDeployment` and `SymbolRoute` is
   `Ready`, all PVCs are still `Bound` and now carry `matching.io/shard`, and the saved
   `orderbook_sequence`/`orderbook_orders` values are unchanged. Then switch the manager to CRD
   mode (`--crd --no-seed --crd-namespace match-eu-1`) and confirm `"store":"crd"` from
   `/api/v1/health`.

7. **Roll back safely.** Scale the operator to zero before changing the catalog, so deleting or
   editing a CR cannot prune workloads. Restore the backed-up manager catalog and workload YAML
   with `kubectl apply --server-side --force-conflicts`, or set `shard.spec.image` back and let the
   operator roll. PVCs are never deleted by adoption or rollback; only the retention timer deletes
   an orphaned PVC.

### Create the catalog and apply shards

The built-in demo catalog has `fx-eu-01` (packed containers, `eurusd`/`gbpusd`), `fx-eu-02`
(reserved packed, 4 slots), and `me-btcusdt` (dedicated). Create additional supervisor or reserved
shards from the GUI **+ shard** form or the API.

Render one shard through the manager (the API returns a `v1/List` that `kubectl apply -f -`
accepts):

```sh
curl -s 'http://127.0.0.1:8099/api/v1/shards/fx-eu-01/manifests?profile=local' \
  | kubectl apply -f -
```

Repeat for every shard. A reserved containers shard renders only its ConfigMap, PDB and
NetworkPolicy; supervisor shards keep their supervisor StatefulSet even with no children.

Wait for the pods, then bootstrap each process once. The manager pod has `curl`; the pod FQDNs are
reachable from the control namespace:

```sh
for shard in fx-eu-01 fx-eu-02 fx-sup-01 me-btcusdt; do
  kubectl -n match-eu-1 get pods -l matching.io/shard="$shard"
done

# Example: initialize eurusd on fx-eu-01 port 21001 (repeat per process/port).
kubectl -n match-control exec deploy/match-manager -- \
  curl -s -X POST -H 'content-type: application/json' \
  -d '[[0,"fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001"],[1,"fx-eu-01-1.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001"],[2,"fx-eu-01-2.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001"],[3,"fx-eu-01-3.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001"]]' \
  http://fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001/init
```

Run `/init` exactly once per process. For supervisor shards, each child has its own port and is a
separate four-voter group. Then apply the replicated symbol config through the leader:

```sh
# Find the leader of one process.
kubectl -n match-control exec deploy/match-manager -- \
  curl -s http://fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001/metrics

# Write ConfigureSymbol to the leader.
kubectl -n match-control exec deploy/match-manager -- \
  curl -s -X POST -H 'content-type: application/json' \
  -d '{"ConfigureSymbol":{"config":{"symbol_id":1,"base_asset":{"code":"EUR","class":"fiat","decimals":2},"quote_asset":{"code":"USD","class":"fiat","decimals":2},"price_decimals":5,"price_tick":"0.00001","base_lot":"0.01","fee_asset_policy":"received","stp":"off"}}}' \
  http://fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001/write
```

The exact `ConfigureSymbol` request shape is the engine's `/write` payload; prefer the typed
`provision_symbol` client helper in `src/client.rs` for production tooling. After a manager
restart, use `POST /api/v1/shards/{shard}/simulate?scenario=ready` only to make the demo GUI's
synthesized route status honest; it does not read the real cluster or change engine state.

### Verify

```sh
curl -s http://127.0.0.1:8099/api/v1/health
curl -s http://127.0.0.1:8099/api/v1/shards | python3 -m json.tool | head
kubectl -n match-eu-1 get pods -o wide
kubectl -n match-eu-1 get pvc
kubectl -n match-eu-1 get statefulsets

# Supervisor children (one pod shown; inspect all four)
kubectl -n match-eu-1 exec fx-sup-01-0 -- curl -s http://127.0.0.1:9090/processes

# Engine console through the manager proxy (symbol is the large header)
open 'http://127.0.0.1:8099/api/v1/shards/fx-sup-01/processes/eurgbp/pods/0/console/'
```

The console's cluster tab must report four reachable peers. A stopped engine has no console link in
the manager and its proxy returns `409 process_disabled`.

### Reach the demo ingress hosts

The renderer emits one class-less `Ingress` per order-system shard and per market-data service, with
Hosts such as `users-0.orders.match-eu-1.example` and `ws-fx-eu-02.match-eu-1.example`. OrbStack has
no cloud provider, so those Ingresses get no address on their own, and macOS needs root to bind
port 80. The local demo therefore installs one ingress-nginx controller and publishes its ClusterIP
through a tiny Docker front door:

```sh
# One class-less controller for every Ingress (demo namespaces included)
kubectl apply -f k8s_control_plane/deploy/ingress-nginx.yaml
kubectl -n ingress-nginx rollout status deploy/ingress-nginx-controller --timeout=180s

# Publish the controller on the Mac's 127.0.0.1:80 (Docker can bind port 80 without root)
./scripts/ingress-frontdoor.sh start

# The shard links and market-data hosts now resolve through OrbStack DNS + the front door
curl -s -o /dev/null -w '%{http_code}\n' http://users-0.orders.match-eu-1.example/demo/
curl -s -o /dev/null -w '%{http_code}\n' http://ws-fx-eu-02.match-eu-1.example/
```

After the rollout, every Ingress shows the controller ClusterIP in `ADDRESS`:

```sh
kubectl get ingress -A
```

The front door keeps the `Host` header, forwards WebSocket upgrades, and targets the controller
Service by ClusterIP, so it survives controller pod restarts. Re-run `start` after the controller
Service is recreated (its ClusterIP changes); `status` checks the container and the listening port,
and `stop` removes it. If port 80 is already taken, pass another port:
`./scripts/ingress-frontdoor.sh start 8080`.

This is demo plumbing, not a production gateway: it serves plaintext HTTP with no authentication,
TLS termination, rate limits or NetworkPolicy for the public hosts. Use the platform ingress or
gateway for production, and see the P1 public-edge items in `TODO.md`.

### Add a symbol

Add a symbol to a **supervisor** shard with the GUI or:

```sh
curl -s -X POST http://127.0.0.1:8099/api/v1/symbols \
  -H 'content-type: application/json' -d '{
  "shard": "fx-sup-01",
  "symbol": {
    "name": "chfusd", "namespace": "",
    "spec": {
      "symbolId": 8,
      "base": { "code": "CHF", "class": "fiat", "decimals": 2 },
      "quote": { "code": "USD", "class": "fiat", "decimals": 2 },
      "priceDecimals": 5, "priceTick": "0.00001", "baseLot": "0.01",
      "feeAssetPolicy": "received", "stp": "off",
      "owner": "fx-team", "generation": 1
    }
  }
}'
```

Adding to a non-empty **containers** shard returns `409 restart_required`; only pass
`"allowRestart": true` when a one-voter-at-a-time pod roll is acceptable. The manager allocates the
lowest free slot and `httpBasePort + slot`; rename or migrate the symbol if you need different
placement.

### Stop and start an engine

```sh
# Graceful stop: keep the record, slot and PVC.
curl -s -X POST http://127.0.0.1:8099/api/v1/symbols/chfusd/shutdown \
  -H 'content-type: application/json' -d '{}'

# Re-enable from the retained data; no /init or ConfigureSymbol.
curl -s -X POST http://127.0.0.1:8099/api/v1/symbols/chfusd/start \
  -H 'content-type: application/json' -d '{}'
```

The manager sets `SymbolDeployment.spec.desiredState` and `SymbolRoute.spec.state` to
`Disabled`/`Active` and returns operator steps. In supervisor mode, apply only the updated
ConfigMap so the supervisor stops/starts that child without touching the other PIDs:

```sh
SHARD=fx-sup-01
curl -s "http://127.0.0.1:8099/api/v1/shards/$SHARD/manifests?profile=local" > "/tmp/$SHARD-manifests.json"
python3 - "$SHARD" <<'PY'
import json, sys
shard = sys.argv[1]
items = json.load(open(f"/tmp/{shard}-manifests.json"))["items"]
items = [item for item in items if item["kind"] == "ConfigMap"]
json.dump({"apiVersion": "v1", "kind": "List", "items": items},
          open(f"/tmp/{shard}-configmap.json", "w"))
PY
kubectl apply -f "/tmp/$SHARD-configmap.json"
```

Wait for the supervisor to read the mounted file (kubelet propagation can take up to a minute) and
verify that only the intended child changed:

```sh
for pod in 0 1 2 3; do
  echo "fx-sup-01-$pod"
  kubectl -n match-eu-1 exec "fx-sup-01-$pod" -- curl -s http://127.0.0.1:9090/processes
done
```

`desired` should list only the active children; the unaffected child keeps its PID and `restarts`
stays `0`. After a start, wait for `/ready`, `GET /read symbol_id` and the console cluster tab
(4/4 peers) before publishing the route. If the data directory was deleted, the engine must be
initialized and configured from scratch.

In containers mode, `shutdown`/`start` return `409 restart_required` while other engines are active
unless `allowRestart` is set. After the operator applies the rendered StatefulSet change it rolls
one pod at a time: non-running pods are replaced first, a running pod is removed only when every
process on it keeps a healthy quorum, and a leader is transferred only when its removal is already
quorum-safe. Watch the revision and operator actions:

```sh
kubectl -n match-eu-1 get statefulset fx-eu-01 \
  -o jsonpath='{.status.updateRevision} {.status.currentRevision} ready={.status.readyReplicas}{"\n"}'
kubectl -n match-eu-1 get pods -l matching.io/cluster=fx-eu-01 \
  -o custom-columns='NAME:.metadata.name,REV:.metadata.labels.controller-revision-hash,READY:.status.containerStatuses[*].ready'
kubectl -n match-control logs deploy/match-operator --tail=100 | grep -E 'deleted|holding|leadership'
```

A Pending new-revision pod is deleted even while the existing process is below full strength,
because it holds no live engine and otherwise blocks the StatefulSet. Do not raise capacity by
deleting a healthy old pod manually; the quorum gate will wait for the Pending pod to schedule.
The engine handles SIGTERM as PID 1, so pod termination is prompt; an older image without the
handler waits for the full grace period. After all pods show the new revision and both containers
`Ready`, the operator runs `/init` only for an empty Raft group, otherwise the process recovers
from its PVC, and it applies `ConfigureSymbol` only when the observed config differs.

The last active engine in a containers shard is zero-impact to stop or start.

### Migrate a symbol and roll back

`POST /api/v1/symbols/{name}/migrate` creates a shadow `SymbolDeployment` on the target shard
with a non-overlapping `raftIdOffset`; it does not move the route. The operator then adds the
shadow's Raft IDs as learners to the source leader, waits for catch-up, changes membership,
switches the route, activates the shadow and sets the source to `Draining`. Watch:

```sh
kubectl -n match-eu-1 get symboldeployment -o custom-columns='NAME:.metadata.name,STATE:.spec.desiredState,OFFSET:.spec.raftIdOffset,PHASE:.status.phase,LEADER:.status.leaderPod'
kubectl -n match-eu-1 get symbolroute <symbol> -o jsonpath='{.status.phase} {.spec.activeDeploymentRef} {.status.leaderAddr}{"\n"}'
kubectl -n match-control logs deploy/match-operator --tail=100 | grep -E 'migration|learner|membership|cutover|rollback'
```

To roll back before deleting the source PVCs:

```sh
kubectl -n match-eu-1 patch symbolroute <symbol> --type=merge -p '{"spec":{"state":"Rollback"}}'
```

The operator adds the source voters back as learners to the target leader, waits for catch-up,
switches the route back, reactivates the source and disables the target. The source PVCs are the
copy that lets this reverse path work.

### Upgrade a shard image (compatible release)

Use the manager **upgrade** tab's **Upgrade** button, or the API, to write a new image and
release to one `SymbolShard`. The operator does the rollout; do not patch the StatefulSet directly.
The **preview plan** tab is read-only and simulates the operator's reconcile steps.

The **Target image** dropdown lists engine images already cached on a Kubernetes node and images
used by running engine pods, with tags, short digests, `● running here` and `◆ configured`
markers. Picking one fills the digest-pinned reference; an image that is not cached can still be
typed into the collapsed **Custom image reference** field. `GET /api/v1/images?shard=<name>`
returns the same inventory, and
falls back to catalog references with a `warning` when the manager cannot list nodes or pods. The
manager only reads nodes and engine-labeled pods (`deploy/rbac.yaml`); it never mutates workloads.

```sh
curl -s -X POST http://127.0.0.1:8099/api/v1/shards/fx-eu-01/upgrade \
  -H 'content-type: application/json' \
  -d '{"image":"matchengine-raft@sha256:<digest>",
       "release":{"wireVersion":7,"snapshotVersion":11,"mixedVersionSafe":false}}'
```

The image must be digest-pinned. The endpoint returns `changed:false` when the shard already has
that image and release. It refuses to write when:

- the target release is incompatible with the current `spec.release`
  (`409 release_incompatible`);
- a `SymbolDeployment` in the shard pins its own `spec.image`
  (`409 deployment_image_override`);
- the shard is paused (`409 shard_paused`) or the mutated catalog fails validation (`422`).

After a successful write, watch progress and the safety gates:

```sh
kubectl -n match-eu-1 get symbolshard fx-eu-01 \
  -o jsonpath='{.status.phase}{" "}{.status.rollout}{"\n"}'
kubectl -n match-eu-1 get symbolshard fx-eu-01 \
  -o jsonpath='{.status.releaseBlock}{" | "}{.status.capacityBlock}{"\n"}'
kubectl -n match-eu-1 get pods -l matching.io/cluster=fx-eu-01 \
  -o custom-columns='NAME:.metadata.name,IMAGE:.spec.containers[0].image,READY:.status.containerStatuses[*].ready'
```

The operator applies the new template and replaces pods one at a time, transferring a leader only
when the removal is already quorum-safe and waiting for all active engines on the replacement pod.
While pods are being replaced the shard reports `phase: Progressing`; when every pod is updated and
ready it returns to `Ready` with `Progressing=False/Settled`.
A blocked roll leaves the pods untouched: `ReleaseCompatible=False` means the live pods report a
different wire/snapshot release than the target, and `CapacityAvailable=False` means the rendered
requests do not fit the nodes. Fix the metadata or free capacity; the next reconcile retries.
Same-version rollouts and same-version image changes are not blocked.

### Incompatible image upgrade (stop-the-world)

When an image change would cross the wire or snapshot version, the operator does **not** roll pods.
It writes `SymbolShard.status.releaseBlock` with `reason: stop_the_world_required` (or
`release_metadata_missing`/`mixed_release`) and leaves `ReleaseCompatible=False`. Do not delete
pods manually while old and new versions are mixed. A fleet adopted before release annotations
existed should first apply the same image with the new template (a metadata-only roll) so pods
record their wire/snapshot versions; then change the image/release.

1. Back up the CR catalog and take a volume-group snapshot of the shard's four PVCs.
2. Disable the affected `SymbolRoute`s at the gateway. Stop every symbol in the shard, or scale the
   StatefulSet to zero after disabling the routes, so no old-version process remains.
3. Patch `spec.image` and `spec.release` together on the `SymbolShard`:
   ```sh
   kubectl -n match-eu-1 patch symbolshard <shard> --type=merge \
     -p '{"spec":{"image":"<new-image>@sha256:<digest>","release":{"wireVersion":8,"snapshotVersion":12,"mixedVersionSafe":false}}}'
   ```
4. Recreate the StatefulSet (or start the symbols) so every pod starts with the new release at
   once. The engines recover their own WAL/snapshot and rejoin the same four-voter group.
5. Verify `/ready`, a leader, the applied index and the order book; then re-enable the routes.

The block clears on the next reconcile once the shard has one consistent release and no pending
image change. `mixedVersionSafe` is only valid when the target image explicitly supports mixed
peers; do not use it to bypass a real format change.

### Market-data service

`MarketDataService` is a separate desired-state CRD for the standalone WebSocket fan-out tier. The
operator renders Deployment/Service/PDB/NetworkPolicy/ConfigMap, an Ingress when
`websocket.ingress` is set, and an HPA with a minimum of one replica. Profile A (direct Aeron
multicast) is the only rendered transport. A shard-local service sets `websocket.fastOnly: true`
and serves only `/ws/<symbol>@<stream>`; a global service declares one `feedGroup` per shard with
its own `channel` and keeps the terminal `/ws` and `/stream?streams=...` routes.

```sh
curl -s -X POST http://127.0.0.1:8099/api/v1/market-data-services \
  -H 'content-type: application/json' -d '{
    "name": "me-btcusdt-marketdata",
    "spec": {
      "cell": "eu-1",
      "image": "registry.example.com/matchengine-marketdata@sha256:...",
      "replicas": 2,
      "feedGroups": [{"name": "btc", "symbols": ["btcusdt"], "streamBase": 2001}],
      "transport": {
        "mode": "multicast",
        "channel": "aeron:udp?endpoint=239.1.2.51:24001|interface={pod_ip}|ttl=1"
      },
      "websocket": {"port": 24800, "fastOnly": true,
        "ingress": "fast-ws-me-btcusdt.match-eu-1.example"}
    }
  }'
kubectl -n match-eu-1 get deploy,svc,pdb,networkpolicy | grep marketdata
kubectl -n match-eu-1 get marketdataservice me-btcusdt-marketdata \
  -o jsonpath='{.status.phase} {.status.readyReplicas}/{.status.replicas}{"\n"}'
curl -s http://127.0.0.1:8099/api/v1/market-data-services/me-btcusdt-marketdata/manifests

# Stateless image update: writes desired state, then the operator rolls the Deployment.
curl -s -X POST http://127.0.0.1:8099/api/v1/market-data-services/me-btcusdt-marketdata/upgrade \
  -H 'content-type: application/json' -d '{
    "image": "registry.example.com/matchengine-marketdata@sha256:...",
    "release": {"wireVersion": 7, "snapshotVersion": 11, "mixedVersionSafe": false}
  }'
curl -s 'http://127.0.0.1:8099/api/v1/images?marketDataService=me-btcusdt-marketdata'
```

The upgrade endpoint rejects a paused service and any release whose wire version differs from a
source shard. `release` is optional; omit it to keep the current release and change only the image.
The GUI exposes the same action from the **market data** view in its terminal and fast sub-tabs.

Status conditions: `Available`, `FeedReady`, `ImagePinned` and `ReleaseCompatible`. A service in
`Degraded` with `FeedNotReady` is running but has not seen a snapshot/heartbeat for a symbol; check
the engine's `--market-data-aeron-*` flags, the odd multicast address, `|ttl=1` and the CNI's
multicast forwarding. For a containers-mode shard, the renderer adds those flags automatically when
a `MarketDataService` references the symbol; build the engine image with
`--build-arg CARGO_FEATURES=market-data-aeron` and mount a writable `/dev/shm` (the renderer adds a
memory emptyDir). Supervisor children receive the same flags through `processes.json`. The renderer keeps `interface={pod_ip}` on the engine publication and strips it from the market-data subscriber args so several pods on one node can subscribe to the same group. A `Failed` service means the renderer rejected the desired state (unknown
symbol/deployment, wire-version mismatch or a non-multicast transport) — see the operator log.

Do not point the market-data tier at the engine's WAL/snapshot PVCs or put feed payloads in CRDs.

### Order systems

`OrderSystem` is a separate CRD for stateless order-entry BFFs. `spec.shards` partitions users by
inclusive numeric ranges; `spec.targets` lists engine symbols, and every shard receives the same
target list, so any user shard can trade every engine process/symbol. The renderer builds one
ConfigMap plus one Deployment/Service/PDB/HPA (and optional Ingress) per user shard, with engine HTTP
addresses derived from the symbol deployment's pod-0 headless DNS.

```sh
curl -s -X POST http://127.0.0.1:8099/api/v1/order-systems   -H 'content-type: application/json' -d '{
    "name": "orders",
    "spec": {
      "cell": "eu-1",
      "image": "matchengine-order-system:arm64-v2",
      "replicas": 1,
      "autoscaling": {"minReplicas": 1, "maxReplicas": 2, "targetCpuUtilization": 70},
      "port": 24300,
      "targets": [
        {"symbol": "gbpusd", "label": "GBP/USD"},
        {"symbol": "btcusdt", "label": "BTC/USDT"}
      ],
      "shards": [
        {"name": "users-0", "userIdMin": 0, "userIdMax": 499},
        {"name": "users-1", "userIdMin": 500, "userIdMax": 999}
      ],
      "ingress": "orders.match-eu-1.example"
    }
  }'
kubectl -n match-eu-1 get deploy,svc,pdb,hpa,ingress -l matching.io/order-system=orders
```

The HPA floor is always one replica; `maxReplicas: 0` derives the ceiling from `spec.replicas`.
The running Deployment omits `spec.replicas` so the HPA owns the count, and a paused system scales
to zero and drops its HPA. CPU targets need a metrics-server or a Prometheus adapter; without one
the HPA reports `<unknown>` but still enforces the floor.

Roll a new stateless image without touching Raft:

```sh
curl -s -X POST http://127.0.0.1:8099/api/v1/order-systems/orders/upgrade \
  -H 'content-type: application/json' -d '{"image":"matchengine-order-system:arm64-v3"}'
kubectl -n match-eu-1 rollout status deploy/orders-users-0
```

Each shard rolls independently with `maxUnavailable: 0` and `maxSurge: 1`; there is no quorum or
leader-transfer gate. The BFF enforces the shard's user range (`409 user_shard`), sets `owner_id`
on new orders, and rejects cancels/amends by a different user. Clients can still reach one shard
directly; a user-ID-routing edge is a follow-up. The manager **order systems** tab lists shards,
targets, HPA bounds, readiness and a **WebUI** link per shard
(`http://<shard>.<spec.ingress>/demo/`, `https` with `tlsSecret`), and has an **upgrade** action.
Order entry happens in the order-system WebUI; the manager neither renders an order ticket nor
proxies orders. For the shard Hosts to load in the local demo, install the ingress controller and
front door described in [Reach the demo ingress hosts](#reach-the-demo-ingress-hosts); without
them, port-forward a shard and open `http://127.0.0.1:<port>/demo/` instead.

### Retention window

An orphaned shard PVC is labeled by the renderer and marked with
`matching.io/retain-until` (Unix seconds) or `manual`. The operator deletes it after the configured
window; a shard that reappears before the deadline clears the marker. Inspect and tune with:

```sh
kubectl -n match-eu-1 get pvc -l matching.io/shard=<shard> \
  -o custom-columns='NAME:.metadata.name,UNTIL:.metadata.annotations.matching\\.io/retain-until'
# Operator deployment: --retention-seconds N (0 means mark manual, never auto-delete).
kubectl -n match-control logs deploy/match-operator --tail=100 | grep -E 'retention|deleted PVC'
```

Only PVCs labeled `app.kubernetes.io/managed-by=match-engine-operator` and
`matching.io/shard` participate.

### Drop a shard

Dropping removes the shard, its deployments, and routes that point at them from the **manager
catalog**. Symbols still referenced by a deployment in another shard are kept. Every engine in the
shard must be stopped first; the API returns `409 shard_not_idle` and names the active symbols.

> **Important: the manager does not delete Kubernetes resources.**
> A successful `drop` only removes catalog records. The StatefulSet, pods, Services, ConfigMap,
> PDB and NetworkPolicy remain until the operator prunes them. With `retainStorage: true`, the
> operator marks the orphaned PVCs and deletes them after `--retention-seconds` (or leaves a
> `manual` marker when it is `0`). With `retainStorage: false`, run the returned `delete-storage`
> step immediately. A **reserved** shard that never materialized has no Kubernetes objects to
> clean up; after the catalog 404, there is nothing else to delete.

The expected end state depends on what the shard contained:

| Shard at drop | After the operator applies the steps |
|---|---|
| Reserved (`0` deployments, never materialized) | No StatefulSet/pods/Services/ConfigMap/PDB/NetworkPolicy/PVCs existed; only the catalog record disappears. |
| Materialized, `retainStorage: true` | Workload and shard objects are gone; the four `data-<shard>-0..3` PVCs remain and are marked with a retention deadline. |
| Materialized, `retainStorage: false` | Workload and shard objects are gone; the PVCs/PVs are gone after the explicit `delete-storage` step. This is irreversible. |


#### 1. Save the catalog and manifests

Save these before changing the catalog; after the drop the manager can no longer render the shard:

```sh
SHARD=fx-eu-03

curl -s http://127.0.0.1:8099/api/v1/catalog > /tmp/catalog-before-drop.json
curl -s "http://127.0.0.1:8099/api/v1/shards/$SHARD" > "/tmp/$SHARD-detail.json"
curl -s "http://127.0.0.1:8099/api/v1/shards/$SHARD/manifests?profile=local" \
  > "/tmp/$SHARD-manifests.json"
```

#### 2. Stop every symbol and apply the ConfigMap

```sh
curl -s "http://127.0.0.1:8099/api/v1/shards/$SHARD" \
  | python3 -c 'import json,sys; print("\n".join(d["spec"]["symbolRef"] for d in json.load(sys.stdin)["deployments"]))' \
  | while read -r symbol; do
      echo "stopping $symbol"
      curl -s -X POST "http://127.0.0.1:8099/api/v1/symbols/$symbol/shutdown" \
        -H 'content-type: application/json' -d '{}'
    done
```

Apply the updated ConfigMap as in [Stop and start an engine](#stop-and-start-an-engine), then
verify the shard is idle:

```sh
curl -s "http://127.0.0.1:8099/api/v1/shards/$SHARD" | python3 -c '
import json, sys
d = json.load(sys.stdin)
print("active", d["activeProcesses"], "disabled", d["disabledProcesses"])
print([(x["spec"]["symbolRef"], x["spec"]["desiredState"]) for x in d["deployments"]])
'
```

#### 3. Choose whether to keep storage

The GUI dialog and the API both ask; **keep storage is the default and recommended**:

```sh
# Safe default: prune workloads but retain the PVCs so the shard can be brought up again.
curl -s -X POST "http://127.0.0.1:8099/api/v1/shards/$SHARD/drop" \
  -H 'content-type: application/json' \
  -d '{"retainStorage": true}' | tee "/tmp/$SHARD-drop.json"

# Destructive: delete the PVCs too. There is no in-cluster recovery after this.
curl -s -X POST "http://127.0.0.1:8099/api/v1/shards/$SHARD/drop" \
  -H 'content-type: application/json' \
  -d '{"retainStorage": false}'
```

The response includes `removedDeployments`, `removedRoutes`, `removedSymbols`, the predicted
`pvcs` (`data-<shard>-0..N-1`), and `keep-storage`/`delete-storage` steps. Show it before applying
the destructive steps:

```sh
python3 -m json.tool "/tmp/$SHARD-drop.json"
```

#### 4. Prune the Kubernetes workload

Use the manifests saved before the drop; this deletes the StatefulSet, Services, PDB, NetworkPolicy
and ConfigMap, but not the PVCs:

```sh
kubectl -n match-eu-1 delete -f "/tmp/$SHARD-manifests.json" --ignore-not-found
kubectl -n match-eu-1 get pods,statefulset,service,pdb,networkpolicy,configmap \
  | grep "$SHARD" || true
```

If you chose `retainStorage: false`, delete the PVCs separately and only after confirming the shard
name and the backup situation:

```sh
for pod in 0 1 2 3; do
  kubectl -n match-eu-1 delete pvc "data-$SHARD-$pod"
done
```

Deleting a namespace or all PVCs bypasses the manager's retention choice and is irreversible.

#### 5. Verify

Use this checklist after the drop and after the operator prune:

- [ ] Manager catalog returns `404` for the shard.
- [ ] No StatefulSet, pods, Services, ConfigMap, PDB or NetworkPolicy remains with the shard name.
- [ ] `retainStorage: true`: the `data-<shard>-0..N-1` PVCs still exist (and their PVs are Bound).
- [ ] `retainStorage: false`: no PVC or PV remains for the shard.
- [ ] The remaining StatefulSet/pod/PVC counts match the shards still in the catalog.

```sh
SHARD=fx-eu-03

# Catalog is gone.
curl -s -o /dev/null -w 'catalog HTTP %{http_code}\n' \
  "http://127.0.0.1:8099/api/v1/shards/$SHARD"   # 404
curl -s http://127.0.0.1:8099/api/v1/shards | python3 -m json.tool | head

# Workload objects are pruned.
kubectl -n match-eu-1 get statefulsets,deployments,pods,services,configmaps,pdb,networkpolicy \
  -o name | grep "$SHARD" || echo "no workload objects"

# Storage matches the retainStorage choice.
kubectl -n match-eu-1 get pvc -o name | grep "$SHARD" || echo "no PVCs"
kubectl get pv -o name | grep "$SHARD" || echo "no PVs"

# Remaining resources line up with the remaining catalog shards.
kubectl -n match-eu-1 get statefulsets --no-headers | wc -l
kubectl -n match-eu-1 get pods --no-headers | wc -l
kubectl -n match-eu-1 get pvc --no-headers | wc -l
```

Reference example: dropping reserved `fx-eu-03` with `retainStorage: false` recorded

```text
drop-shard | fx-eu-03 | removed 0 deployment(s), 0 route(s), 0 symbol(s); retainStorage=false
```

and left no `fx-eu-03` object of any kind. The remaining four shards accounted for exactly
4 StatefulSets, 16 pods, 9 Services, 4 shard ConfigMaps, 4 PDBs, 4 NetworkPolicies and 16 Bound
PVCs. Because the shard was reserved, there was no workload to prune; the catalog `404` was the
whole cleanup. For a materialized shard, the same check must show the workload objects gone and the
PVC state matching the storage choice.

### Restore a shard with retained storage

`retainStorage: true` keeps `data-<shard>-<pod>`. To bring the shard back, recreate **the same shard
shape** so the new pods bind the existing PVCs:

1. Confirm the PVCs exist and no old pod still holds them.
2. Recreate the shard with the same name, namespace, replica count, storage class and HTTP base
   port. The safest input is the original `ShardBundle` used to create it, or the `shards.<name>`
   object from `/tmp/catalog-before-drop.json`:

   ```sh
   # A bundle restores the shard plus its symbols, deployments and routes with exact slots/ports.
   curl -s -X POST http://127.0.0.1:8099/api/v1/bundles \
     -H 'content-type: application/json' --data-binary @fx-eu-03.json
   ```

3. Render and apply the manifests; the StatefulSet reuses the `data` claim template and its PVC
   names:

   ```sh
   curl -s "http://127.0.0.1:8099/api/v1/shards/$SHARD/manifests?profile=local" \
     | kubectl apply -f -
   ```

4. Wait for all pods Ready. **Do not run `/init` or re-apply `ConfigureSymbol`** if the retained
   data is intact: each engine opens its WAL/snapshot, rejoins the same four-voter group and
   recovers the applied symbol configuration.
5. Verify `GET /ready`, `POST /read` with `"symbol_id"`, `POST /read` with `"symbol_config"`, and
   the manager console cluster view (4/4 peers). Only then publish the route.

Reuse the original slots, HTTP ports and replica count. Raft membership stores the peer addresses;
a recreated shard on different ports will not rejoin the old group. If a PVC is missing, that
engine's data is gone and it must be initialized and configured from scratch.

### Desired-state persistence and manager restarts

**CRD mode (recommended).** Desired state lives in the `matching.io` CRDs, so a manager rollout or
pod replacement has no catalog to lose: the new pod lists the CRDs and rebuilds its cache from
informers. No PVC is needed for desired state.

Use the backup CLI before upgrades or destructive operations. It writes one self-verifying bundle:
`catalog.json` (CRD desired state), `backup.json` (snapshot names, indexes and checksums),
`SHA256SUMS` (every bundle file) and optionally `etcd.snapshot.db` (an externally produced
`etcdctl snapshot save` file). The tool never connects to etcd directly.

Prerequisites: the CSI snapshot controller and a `VolumeSnapshotClass`; for group mode also the
`VolumeGroupSnapshot` CRDs/controller. Apply `deploy/rbac.yaml` for the `match-backup`
ServiceAccount (or run with an equivalent kubeconfig).

```sh
# One group snapshot per shard, plus the full CRD catalog and an optional etcd snapshot.
cargo run --manifest-path k8s_control_plane/Cargo.toml --bin k8s-control-plane -- \
  backup fx-eu-01 \
  --namespace match-eu-1 \
  --mode volume-group \
  --volume-group-snapshot-class fast-ssd-vg \
  --etcd-snapshot /var/backups/etcd-20261117.db \
  --out /var/backups/fx-eu-01-20261117T120000Z

# Drivers without group snapshots: one VolumeSnapshot per PVC.
cargo run --manifest-path k8s_control_plane/Cargo.toml --bin k8s-control-plane -- \
  backup fx-eu-01 --namespace match-eu-1 --mode volume \
  --volume-snapshot-class csi-snap \
  --out /var/backups/fx-eu-01-20261117T120000Z
```

Verification is on by default: each snapshot is restored into a temporary PVC, every file is hashed
through a short-lived verification pod, and the hashes are recorded in `backup.json`. Use
`--no-verify` only for an emergency; it records no content checksums. `--verify-image` selects the
checksum image (default `busybox:1.36`). In a cluster without CSI snapshot support, `--crd-only`
still writes the checksummed CRD/etcd bundle so desired-state restore and drills work.

Verify an existing bundle without touching the cluster:

```sh
cargo run --manifest-path k8s_control_plane/Cargo.toml --bin k8s-control-plane -- \
  backup-verify --input /var/backups/fx-eu-01-20261117T120000Z
```

Run a restore drill. It restores every snapshot into a temporary PVC, compares the recorded file
hashes, then deletes the temporary claims; it never writes CRDs or touches live PVCs:

```sh
cargo run --manifest-path k8s_control_plane/Cargo.toml --bin k8s-control-plane -- \
  restore --input /var/backups/fx-eu-01-20261117T120000Z --drill
```

Selective CRD restore expands direct dependencies (cell, shard, symbols, deployments, routes,
market-data and order-system references) and merges the selected objects over the current catalog;
it never deletes objects that are absent from the backup. Status fields are not restored.

```sh
cargo run --manifest-path k8s_control_plane/Cargo.toml --bin k8s-control-plane -- \
  restore --input /var/backups/fx-eu-01-20261117T120000Z \
  --shard fx-eu-01 --apply-crds
```

`--cell`, `--symbol`, `--market-data-service` and `--order-system` select narrower scopes. Restore
CRDs first, let the operator materialize workloads, then recover engine PVCs. Volume restore creates
new PVCs with a `restore-` prefix (never overwriting live `data-*` claims), verifies the recorded
hashes and keeps them for the operator cutover:

```sh
cargo run --manifest-path k8s_control_plane/Cargo.toml --bin k8s-control-plane -- \
  restore --input /var/backups/fx-eu-01-20261117T120000Z \
  --restore-volumes --target-namespace match-eu-1
```

A full cluster restore still requires all four PVCs from the same snapshot set, the same node IDs
and a controlled StatefulSet cutover; restoring a subset of voters over newer committed state is
unsupported. The etcd snapshot is checksum-verified and preserved in the bundle, but restoring etcd
is the provider's control-plane procedure, not this tool. Snapshots are crash-consistent; the
reference CLI does not quiesce writes, so drain the gateway route first when a quiesced backup is
required. Copy the bundle to object storage after creation; the output directory is a staging path,
not a retention system.

Audit/action history is not part of CRDs. Use the external audit store for that.

**JSON demo fallback.** `serve --state /data/catalog.json` still works for local demos. The
reference manifest uses an `emptyDir`, so a Deployment rollout reseeds the 3-shard demo catalog and
loses symbols, routes, disabled states and audit history unless you back `/data` with a PVC:

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: match-manager-state
  namespace: match-control
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: local-path
  resources:
    requests:
      storage: 1Gi
```

```sh
kubectl apply -f manager-state-pvc.yaml
# Save the current catalog first; this patch rolls the manager.
curl -s http://127.0.0.1:8099/api/v1/catalog > /tmp/manager-catalog.json
kubectl -n match-control patch deploy match-manager --type=json \
  -p='[{"op":"replace","path":"/spec/template/spec/volumes/0","value":{"name":"data","persistentVolumeClaim":{"claimName":"match-manager-state"}}}]'
kubectl -n match-control rollout status deploy/match-manager --timeout=120s
```

In JSON mode, export the catalog before any manager rollout and recreate desired state through
`POST /api/v1/bundles`, `POST /api/v1/shards` and `POST /api/v1/symbols` if the file is lost. This
does not restart engines; it only rebuilds the manager's view.
`POST /api/v1/shards/{shard}/simulate?scenario=ready` only makes the demo GUI's synthesized route
status honest and never changes engine state.

### Troubleshooting

| Symptom | Check |
|---|---|
| Manager pod restarted and the catalog looks like the 3-shard demo | JSON mode only: `/data` is an `emptyDir` or `--state` was not set. Use CRD mode or recreate from the saved catalog/bundles. CRD mode rebuilds from CRDs. |
| `kubernetes_api` 403 in CRD mode | The manager ServiceAccount lacks RBAC or its token is not mounted. Apply `deploy/rbac.yaml`, set `serviceAccountName: match-manager`, and `automountServiceAccountToken: true`. |
| `kubernetes_api` 404 in CRD mode | The five `matching.io` CRDs are not installed. Run `cargo run -- crds \| kubectl apply -f -`. |
| CRD manager starts with an empty catalog | No CRs exist in the listed namespaces, or list/watch RBAC is missing. Restore with `crd-restore` or create symbols/shards through the API. |
| Operator has no Lease holder | Check `kubectl -n match-control get lease match-engine-operator`; verify the `match-operator` ServiceAccount has `coordination.k8s.io/leases` permissions and the pod token is mounted. |
| Operator cannot apply workloads | Check the `match-operator` ClusterRole for StatefulSet/Service/ConfigMap/PDB/NetworkPolicy create/update/delete, pods list/watch/patch/delete, and CR `patch`/`update` for migration. |
| Route status stays Pending | The operator could not reach `<pod>.<shard>-headless.<ns>.svc:httpPort/health` or no engine reports a leader. Check readiness, the NetworkPolicy and the console cluster view. |
| Leader Service has no endpoints | Only dedicated shards get a leader Service. Check `SymbolDeployment.status.leaderPod`; the operator removes `matching.io/role=leader` when no leader is reported. |
| Operator pruned objects unexpectedly | Check the `--namespace` scope and that the shard still exists in its CRDs. Objects without the managed-by label are never pruned; PVCs are never pruned by the operator. |
| `ImagePullBackOff` / old engine console without the symbol header | The image was rebuilt but the pod was not recreated. Use `imagePullPolicy: IfNotPresent`, then `kubectl -n match-eu-1 rollout restart statefulset/<shard>` and wait for `/ready`. |
| `409 restart_required` on add/stop/start | The shard is containers mode with other active engines; use supervisor mode, an empty shard, or explicitly opt into `allowRestart`. |
| Image update is blocked with `ReleaseCompatible=False` | The target wire/snapshot release is incompatible with the applied pod annotations, or the pods lack annotations. Inspect `SymbolShard.status.releaseBlock`; plan a stop-the-world upgrade, or set `spec.release.mixedVersionSafe: true` only when the target image can tolerate mixed peers. |
| Stop shows `Disabled` but the symbol container is still running | The manager only wrote desired state. Check `kubectl -n match-control logs deploy/match-operator` for a StatefulSet apply error. An adopted StatefulSet can keep `kubectl-client-side-apply` list ownership; a container removal then fails SSA validation (`containers[0].image: Required value`). Operator v6+ retries that StatefulSet with a merge patch automatically. Confirm the rollout with `kubectl -n match-eu-1 get sts <shard> -o jsonpath='{.status.updatedReplicas}'` and the operator `deleted one outdated pod` logs. |
| `409 shard_not_idle` on drop | Stop every symbol in the shard first; the error names the active ones. Apply the updated ConfigMap before retrying. |
| Drop returned `200` but pods/StatefulSet still exist | The manager only wrote desired state. Prune with the manifests saved before the drop: `kubectl -n match-eu-1 delete -f /tmp/<shard>-manifests.json --ignore-not-found`. |
| Reserved shard drop shows no PVCs or workloads | Expected: a reserved shard never materialized. Verify the catalog returns `404` and no object with the shard name exists; there is nothing else to clean up. |
| Drop with `retainStorage: true` but PVCs are gone | The operator or namespace cleanup deleted them; the manager cannot recreate data. Restore from backup or re-initialize the shard. |
| Restored shard pods stay `Pending` or do not see old data | Recreate the same shard name, replica count, slots/ports and storage class; PVC names are `data-<shard>-<pod>`. Different ports or names do not rebind the old claims or rejoin the old group. |
| `start` marks `Active` but the child is missing | The ConfigMap was not applied yet, or kubelet has not propagated it. Apply only the ConfigMap and check `/processes`; propagation can take up to a minute. |
| `start` returns `409 restart_required` | Containers mode has other active engines. Use `{"allowRestart": true}` only if a pod-by-pod roll is acceptable. |
| Manager shows a symbol `Active` but the engine is stopped | The catalog was rebuilt after a manager restart; use `/start` (or the GUI **start**) to make the desired state match the engine, then verify `/ready`. |
| Shard detail shows `freeSlots` unchanged after stop | Expected: disabled deployments retain their slot until the shard is dropped or the symbol is deleted. |
| `delete-storage` selected but PVCs remain | The manager does not delete PVCs. Apply the returned step with `kubectl delete pvc data-<shard>-0 ...`. |
| Supervisor does not start/stop a child after a ConfigMap change | Kubelet propagation can take up to a minute; the supervisor polls every 1s. Check `kubectl exec <pod> -- curl -s :9090/processes` and the mounted `processes.json`. |
| Engine console proxy returns `502` | The manager cannot resolve/dial `<pod>.<shard>-headless.<namespace>.svc:httpPort`; check the NetworkPolicy, DNS and that the pod is Ready. |
| Engine console proxy returns `409 process_disabled` | The deployment is `Disabled`; use **start** first. |
| `/init` fails with “already initialized” | Never re-run `/init`; a restarted engine recovers from its PVC and should not be re-bootstrapped. A retained shard restore must not re-initialize intact data. |

### Cleanup and teardown

Full demo wipe (deletes all engine data, retained PVCs and CRDs included):

```sh
# Export CRD desired state first if you may restore it later and use CRD mode.
cargo run --manifest-path k8s_control_plane/Cargo.toml -- \
  crd-backup --out /tmp/crd-catalog.json || true
kubectl delete namespace match-control
kubectl delete namespace match-eu-1
kubectl delete -f k8s_control_plane/deploy/crds.yaml --ignore-not-found
```

Safe teardown that keeps data:

1. Save `/api/v1/catalog` and each shard's manifests (see [Drop a shard](#drop-a-shard)).
2. Stop every symbol and apply the updated ConfigMaps.
3. Drop each shard with `{"retainStorage": true}`.
4. Delete the workloads with the saved manifests, but keep the `match-eu-1` namespace and its
   `data-<shard>-<pod>` PVCs.
5. Delete the manager Deployment/Service if you no longer need the GUI; recreating it later with
   `--state` or rebundling the saved catalog does not touch the engines or PVCs.

Do **not** delete the `match-eu-1` namespace when you intend to keep storage: namespace deletion
cascades to PVCs. Deleting a namespace or PVC bypasses the manager's retention choice and is
irreversible.

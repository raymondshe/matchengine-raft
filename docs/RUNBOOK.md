# Runbook

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

The Kubernetes control plane is the standalone manager in `demos/k8s_control_plane` plus the
engine image's supervisor binary. It is a **desired-state writer**, not a Kubernetes controller: it
validates/places symbols, renders manifests, and returns operator steps. The operator (you) still
applies ConfigMaps/workloads and performs PVC operations.

Read [KUBERNETES_CONTROL_PLANE.md](./KUBERNETES_CONTROL_PLANE.md) for the design and
[KUBERNETES_LOCAL_TESTING.md](./KUBERNETES_LOCAL_TESTING.md) for the test tiers. The reference demo
details are in [`demos/k8s_control_plane/README.md`](../demos/k8s_control_plane/README.md).

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
# Engine + supervisor (must be rebuilt after changes under web/console/)
docker build -f docker/k8s.Dockerfile -t matchengine-raft:k8s-nonroot .

# Manager API + embedded GUI
docker build -f demos/k8s_control_plane/Dockerfile \
  -t matchengine-k8s-control-plane:local demos/k8s_control_plane

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
cargo run --manifest-path demos/k8s_control_plane/Cargo.toml -- crds \
  | kubectl apply -f -
kubectl apply -f demos/k8s_control_plane/deploy/rbac.yaml
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

The manager needs network access to engine pods for the console proxy. The rendered NetworkPolicy
allows HTTP from namespaces labelled `matching.io/role=control-plane`; keep that label if you serve
the GUI from another namespace.

Port-forward the manager for a workstation browser:

```sh
kubectl -n match-control port-forward deploy/match-manager 8099:8099
# http://127.0.0.1:8099/
```

When the manager runs on the workstation instead of in the cluster, point the console proxy at
loopback with `cargo run --manifest-path demos/k8s_control_plane/Cargo.toml -- serve
--engine-console-host 127.0.0.1`; ports still come from each deployment's catalog placement.

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
unless `allowRestart` is set. Apply the rendered StatefulSet change, delete/recreate one pod at a
time, and wait for `/ready` before moving to the next pod. The last active engine in a containers
shard is zero-impact to stop or start.

### Drop a shard

Dropping removes the shard, its deployments, and routes that point at them from the **manager
catalog**. Symbols still referenced by a deployment in another shard are kept. Every engine in the
shard must be stopped first; the API returns `409 shard_not_idle` and names the active symbols.

> **Important: the manager does not delete Kubernetes resources.**
> A successful `drop` only removes catalog records. The StatefulSet, pods, Services, ConfigMap,
> PDB and NetworkPolicy remain until the operator prunes them. PVCs are deleted only if you chose
> `retainStorage: false` and then run the returned `delete-storage` step. A **reserved** shard that
> never materialized has no Kubernetes objects to clean up; after the catalog 404, there is nothing
> else to delete.

The expected end state depends on what the shard contained:

| Shard at drop | After the operator applies the steps |
|---|---|
| Reserved (`0` deployments, never materialized) | No StatefulSet/pods/Services/ConfigMap/PDB/NetworkPolicy/PVCs existed; only the catalog record disappears. |
| Materialized, `retainStorage: true` | Workload and shard objects are gone; the four `data-<shard>-0..3` PVCs remain Bound/Released so the shard can be restored. |
| Materialized, `retainStorage: false` | Workload and shard objects are gone; the PVCs/PVs are gone too. This is irreversible. |


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
informers. No PVC is needed for desired state. Back up before upgrades or destructive operations:

```sh
# Selective catalog export (JSON, desired state only).
cargo run --manifest-path demos/k8s_control_plane/Cargo.toml -- \
  crd-backup --out /tmp/crd-catalog.json

# Cluster-wide backup covers all CRs too.
# etcd snapshot procedure depends on the managed Kubernetes provider.
```

Restore is selective and ordered: restore CRs first, then let the operator materialize workloads,
then recover engine PVCs. `crd-restore` validates the JSON and writes the CRs through the API
server:

```sh
cargo run --manifest-path demos/k8s_control_plane/Cargo.toml -- \
  crd-restore --input /tmp/crd-catalog.json
```

Audit/action history is not part of CRDs. Use the external audit store for that. Engine data lives
on the PVCs and is backed up separately with `VolumeSnapshot`/`VolumeGroupSnapshot`.

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
| `ImagePullBackOff` / old engine console without the symbol header | The image was rebuilt but the pod was not recreated. Use `imagePullPolicy: IfNotPresent`, then `kubectl -n match-eu-1 rollout restart statefulset/<shard>` and wait for `/ready`. |
| `409 restart_required` on add/stop/start | The shard is containers mode with other active engines; use supervisor mode, an empty shard, or explicitly opt into `allowRestart`. |
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
cargo run --manifest-path demos/k8s_control_plane/Cargo.toml -- \
  crd-backup --out /tmp/crd-catalog.json || true
kubectl delete namespace match-control
kubectl delete namespace match-eu-1
kubectl delete -f demos/k8s_control_plane/deploy/crds.yaml --ignore-not-found
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

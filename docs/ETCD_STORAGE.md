# etcd storage used by the Kubernetes control plane

This note describes the etcd keyspace used by the `demos/k8s_control_plane` reference
deployment. It is a logical map of what the Kubernetes API server persists; neither the
manager nor the operator opens an etcd client.

## 1. Scope and rules

The control plane has one source of truth for desired state:

- the five `matching.io/v1` custom resources, stored by the Kubernetes API server in etcd;
- Kubernetes built-in objects (`Deployment`, `StatefulSet`, `Service`, `ConfigMap`, `PDB`,
  `NetworkPolicy`, `Pod`, `PVC`, `Lease`, RBAC) that the manager, operator and kubelet use;
- no application database for desired state in the live deployment.

**Never:**

- connect to etcd directly from the manager, operator, gateway or engine;
- put engine WAL, snapshots, order books, match events or market data in CRs, ConfigMaps or etcd;
- treat an etcd snapshot as an engine backup. It backs up Kubernetes state, not the order books
  stored on the PVCs.

The paths below use Kubernetes' default etcd prefix `/registry`. A cluster can change the prefix
with `--etcd-prefix`, and the API server may encode values as protobuf rather than JSON. Treat these
paths as an operational map, not a stable API.

## 2. CRD definitions

The five CRDs are cluster-scoped definitions stored at:

```text
/registry/apiextensions.k8s.io/customresourcedefinitions/cells.matching.io
/registry/apiextensions.k8s.io/customresourcedefinitions/symbolshards.matching.io
/registry/apiextensions.k8s.io/customresourcedefinitions/tradingsymbols.matching.io
/registry/apiextensions.k8s.io/customresourcedefinitions/symboldeployments.matching.io
/registry/apiextensions.k8s.io/customresourcedefinitions/symbolroutes.matching.io
```

All five kinds are `matching.io/v1` and namespaced. Their status is exposed through the `/status`
subresource. Kubernetes stores the status **inside the same object value** as the spec; `/status`
is an API subresource, not a second etcd key. The broader design also names `ClusterTemplate`,
`Backup` and `Operation`; those kinds are not installed by the reference implementation and
therefore have no live etcd keys.

## 3. Desired-state CRs

For a namespaced CR the key is:

```text
/registry/<group>/<plural>/<namespace>/<name>
```

The live `match-eu-1` catalog is:

```text
/registry/matching.io/cells/match-eu-1/eu-1
/registry/matching.io/symbolshards/match-eu-1/fx-eu-01
/registry/matching.io/symbolshards/match-eu-1/fx-eu-02
/registry/matching.io/symbolshards/match-eu-1/fx-sup-01
/registry/matching.io/symbolshards/match-eu-1/me-btcusdt
/registry/matching.io/tradingsymbols/match-eu-1/audusd
/registry/matching.io/tradingsymbols/match-eu-1/btcusdt
/registry/matching.io/tradingsymbols/match-eu-1/eurgbp
/registry/matching.io/tradingsymbols/match-eu-1/eurusd
/registry/matching.io/tradingsymbols/match-eu-1/gbpusd
/registry/matching.io/tradingsymbols/match-eu-1/nzdusd
/registry/matching.io/tradingsymbols/match-eu-1/usdjpy
/registry/matching.io/symboldeployments/match-eu-1/{audusd,btcusdt,eurgbp,eurusd,gbpusd,nzdusd,usdjpy}-g1
/registry/matching.io/symbolroutes/match-eu-1/{audusd,btcusdt,eurgbp,eurusd,gbpusd,nzdusd,usdjpy}
```

The live counts are 1 cell, 4 shards, 7 symbols, 7 deployments and 7 routes. Compact JSON for the
whole catalog is about 21 KB; individual objects are about 0.4–2.1 KB. Allow headroom for status,
annotations and `managedFields`. Braces above are shorthand: each expanded name is a separate etcd
key.

| Kind | Key family | Live count | Compact JSON total |
|---|---|---:|---:|
| `Cell` | `/registry/matching.io/cells/match-eu-1/<name>` | 1 | ~0.5 KB |
| `SymbolShard` | `/registry/matching.io/symbolshards/match-eu-1/<name>` | 4 | ~7.9 KB |
| `TradingSymbol` | `/registry/matching.io/tradingsymbols/match-eu-1/<name>` | 7 | ~4.1 KB |
| `SymbolDeployment` | `/registry/matching.io/symboldeployments/match-eu-1/<name>` | 7 | ~4.8 KB |
| `SymbolRoute` | `/registry/matching.io/symbolroutes/match-eu-1/<name>` | 7 | ~3.4 KB |

These sizes are the API JSON bodies, not the exact etcd value size. The API server adds its storage
envelope and may use protobuf; status and `managedFields` rewrite the whole value.

### 3.1 `Cell`

Stores the cell identity and policy inputs, not engine state:

- `spec.clusterId`, `region`, `kubeContext`, `namespaces`;
- capacity (`maxProcesses`, `maxShards`);
- storage classes, gateway URL and observability endpoints;
- status conditions and observed generation.

### 3.2 `SymbolShard`

Stores the shard's pod-template inputs and observed status:

- `spec.image`, `replicas`, `class`, `environment`;
- `spec.packing` (`containers` or `supervisor`, max processes, log settings); supervisor shards
  also set `resourcesSizedForAggregate` after sizing the container resources for all children;
- `spec.network.httpBasePort`, storage class/size, per-process resources and quotas;
- `spec.raft` read policy, snapshot interval and WAL fsync mode;
- `spec.release` wire/snapshot versions and `spec.upgrade.strategy`;
- `status.phase`, conditions, per-pod readiness, symbol list and leader/lag projection;
- `status.rollout` with `percent`, `updatedReplicas`, `readyReplicas`, `replicas`, `complete`;
- `status.releaseBlock` when an image roll is waiting for a stop-the-world or metadata fix;
  `ReleaseCompatible` is the matching condition;
- `status.capacityBlock` when rendered pod requests do not fit current node/allocatable capacity;
  `CapacityAvailable` is the matching condition.

`SymbolShard.spec` does **not** own slots or ports per symbol; that belongs to `SymbolDeployment`.

### 3.3 `TradingSymbol`

Stores the immutable symbol definition:

- `spec.symbolId`, base/quote asset code, class and decimals;
- price decimals/tick, base lot, fee-asset policy and STP mode;
- owner and generation;
- status phase, config lock/hash and applying deployment.

### 3.4 `SymbolDeployment`

Stores one generation's placement:

- `spec.symbolRef`, `spec.shardRef`, `spec.generation`;
- `spec.placement.slot` and `httpPort`;
- `spec.raftIdOffset` (migration shadow range);
- optional image/resources override, desired state and config hash;
- status phase, ready flag, leader pod/address, applied index, lag, conditions and bootstrap time.

### 3.5 `SymbolRoute`

Stores gateway routing intent:

- `spec.symbolRef`, `spec.cellRef`, `spec.activeDeploymentRef`;
- route `state` and optional shadow/rollback references;
- status phase, active deployment, leader address and observed generation.

The gateway should read the route from the API server (or a watch cache), not from etcd.

## 4. Control-plane built-ins

The manager and operator themselves are ordinary Kubernetes objects:

```text
/registry/namespaces/match-control
/registry/namespaces/match-eu-1
/registry/apps/deployments/match-control/match-manager
/registry/apps/deployments/match-control/match-operator
/registry/services/specs/match-control/match-manager
/registry/serviceaccounts/match-control/match-manager
/registry/serviceaccounts/match-control/match-operator
/registry/coordination.k8s.io/leases/match-control/match-engine-operator
/registry/rbac.authorization.k8s.io/clusterroles/match-manager-crd
/registry/rbac.authorization.k8s.io/clusterroles/match-operator
/registry/rbac.authorization.k8s.io/clusterrolebindings/match-manager-crd
/registry/rbac.authorization.k8s.io/clusterrolebindings/match-operator
```

The active Lease holder is the only operator replica that reconciles. The manager and operator
ServiceAccounts use projected tokens; this deployment does not create long-lived application
Secrets. The manager's audit log is in memory in the reference implementation and is not in etcd.

## 5. Engine workload metadata

The operator renders one set of objects per shard. Their etcd entries are metadata and pod-template
desired state only:

```text
/registry/apps/statefulsets/match-eu-1/<shard>
/registry/pods/match-eu-1/<shard>-<ordinal>
/registry/services/specs/match-eu-1/<shard>-headless
/registry/services/specs/match-eu-1/<shard>-metrics
/registry/services/specs/match-eu-1/<shard>-leader        # dedicated shards only
/registry/configmaps/match-eu-1/<shard>-shard
/registry/policy/poddisruptionbudgets/match-eu-1/<shard>
/registry/networking.k8s.io/networkpolicies/match-eu-1/<shard>
/registry/persistentvolumeclaims/match-eu-1/data-<shard>-<ordinal>
```

- The `StatefulSet` stores the pod template (containers or supervisor), volume claim template and
  update strategy.
- `Pod` entries are runtime metadata; kubelet updates their status frequently and etcd churn here is
  normal.
- `ConfigMap/<shard>-shard` stores `processes.json` (the desired child process list used by a
  supervisor) and `slots.json` (the slot/port/deployment ownership map), not engine data.
- PVC entries store the claim and bound volume metadata. The actual WAL, snapshot, order book,
  event window and market-data archives live on the volume, not in etcd.
- The gRPC/HTTP addresses, leader labels and Service selectors are Kubernetes metadata. A leader
  label is on the Pod, and the dedicated leader Service selects it.

## 6. What is not in etcd

| Data | Where it lives |
|---|---|
| Engine WAL, snapshots, order book, sequence, retained match events | Engine PVC under `/data/<symbol>-<ordinal>` |
| Market-data feed and JSONL archives | Feed/archive files or object storage |
| Manager audit/approval history | In-memory in the reference manager; external append-only store in production |
| Prometheus time series and logs | Prometheus/Loki or the configured observability backend |
| Large backups of engine PVCs | `VolumeSnapshot`/`VolumeGroupSnapshot` or object storage |
| Application secrets and TLS keys | External secret store or Kubernetes Secrets if explicitly configured |

## 7. Write and watch behaviour

The manager writes CRs; the operator watches them, renders workloads and writes status back through
the `/status` subresource. The manager never reads etcd directly and the operator never opens an
etcd connection.

Implications:

- **Status updates are the main control-plane write load.** The operator reconciles every few
  seconds, but `write_status` skips unchanged objects. Plan etcd capacity and watch fan-out for
  status changes, not for the small spec bodies.
- **Every spec update increments `metadata.generation` and `resourceVersion`.** `resourceVersion`
  changes on status writes too; do not use it as a business version. Use
  `SymbolDeployment.spec.generation`.
- **Status and spec share one etcd value.** A large status (per-pod readiness, conditions, rollout
  counters) increases the object size and rewrite cost. Keep status compact.
- **`managedFields` can be significant.** A StatefulSet adopted from `kubectl apply` can carry
  client-side ownership in addition to the operator's server-side apply fields. This is metadata,
  not engine state.
- **No direct etcd clients.** The manager/operator ServiceAccounts have RBAC only for the five CRDs,
  their status subresources, the Kubernetes resources the operator manages and the manager's
  read-only `nodes`/`pods` image inventory. A direct etcd connection is unsupported and would
  bypass the API server's validation, watch and audit paths.

## 8. Backup and restore

- A **cluster etcd snapshot** captures the CRDs, CRs, Deployments, StatefulSets, Services,
  ConfigMaps, PDBs, NetworkPolicies, Pod/PVC metadata, Leases and RBAC. It does not contain the
  bytes inside the engine PVCs.
- `crd-backup`/`crd-restore` export desired state only (the five CR kinds). They do not include
  status, built-in workloads, Leases, RBAC or PVC data. After a CR restore, let the operator
  re-materialize workloads and recover engines from their PVCs.
- For a full engine restore, use `VolumeGroupSnapshot` over all four PVCs of a shard (or the
  storage-specific equivalent) and restore the CR catalog separately.
- Never restore an old etcd snapshot over a live cluster just to recover one symbol; use selective
  CR restore and the normal operator reconciliation path.

## 9. Quick inspection

Use the API server, not etcd:

```sh
kubectl get crd | grep matching.io
kubectl -n match-eu-1 get cells,symbolshards,tradingsymbols,symboldeployments,symbolroutes
kubectl -n match-eu-1 get symbolshard fx-eu-01 -o yaml
kubectl -n match-control get lease match-engine-operator -o yaml
kubectl -n match-eu-1 get sts,svc,cm,pdb,networkpolicy,pvc
```

For a catalog export:

```sh
cargo run --manifest-path demos/k8s_control_plane/Cargo.toml -- \
  crd-backup --out /tmp/crd-catalog.json
```

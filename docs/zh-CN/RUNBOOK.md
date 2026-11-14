# 运行手册

## 启动集群

### 使用测试脚本快速启动

查看集群运行的最简单方式是运行提供的测试脚本：

```shell
./scripts/test-cluster.sh
```

该脚本会构建一个四节点集群（`127.0.0.1:21001-21004`，gRPC Raft 位于 `22001-22004`），完成
初始化、添加学习者、变更成员、写入挂单，并从每个节点读回订单簿。脚本只用 `curl` 访问 HTTP
API，便于观察请求/应答流程。状态保存在 `/tmp/matchengine-raft-test-cluster`，每次启动前会清空
该目录，因此可以重复运行；如需保留状态，请设置 `RAFT_DATA_DIR` 或 `--data-dir`（显式目录默认
不删除，可用 `--fresh` 强制清空）。

常用选项：

```shell
./scripts/test-cluster.sh --nodes 3              # 更小的集群
./scripts/test-cluster.sh --no-fsync              # 生产 no-fsync 配置（至少 4 个 voter）
./scripts/test-cluster.sh --orders 100            # 写入更多挂单
./scripts/test-cluster.sh --release                # 构建并运行 target/release/matchengine-raft
./scripts/test-cluster.sh --keep                  # 启动后保留集群
./scripts/test-cluster.sh status                  # 查看健康/就绪状态和领导者
./scripts/test-cluster.sh stop                    # 停止保留运行的集群
```

`--no-fsync` 会取消 `RAFT_WAL_FSYNC_MS`，此时二进制要求至少 4 个 voter，脚本会用一次 `/init`
初始化全部节点。未指定时脚本使用 `RAFT_WAL_FSYNC_MS=0`（每次追加都 fsync），因此少于四个节点
的集群仍然有效。`--base-port` 整体移动端口范围，`--wait` 控制每一步的就绪超时。

### 使用 Rust 测试

`cargo test` 运行集群集成测试；测试矩阵见[测试](./TESTING.md)。

### 手动设置集群

#### 1. 启动节点

启动第一个节点：

```shell
./target/debug/matchengine-raft --id 1 --http-addr 127.0.0.1:21001
```

启动其他节点（在不同的终端中）：

```shell
./target/debug/matchengine-raft --id 2 --http-addr 127.0.0.1:21002
./target/debug/matchengine-raft --id 3 --http-addr 127.0.0.1:21003
./target/debug/matchengine-raft --id 4 --http-addr 127.0.0.1:21004
```

每个节点在 `端口 + 1000`（这里是 `22001-22004`）上监听 gRPC Raft RPC；HTTP 监听器只提供应用和
管理 API。

所有状态写入 `<data-dir>/<instance-prefix>-<id>/`，详见[每节点数据目录](./STORAGE.md#每节点数据目录)。

#### 2. 初始化集群

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '[]' \
  http://127.0.0.1:21001/init
```

`/init` 接收 JSON 数组。空数组（`[]`）会把接收请求的节点初始化为单节点集群；非空数组
（元素为 `[id, "host:port"]`）可一次性初始化整个集群。直接 POST 或发送 `{}` 会被反序列化器
拒绝。这一步会将节点 1 初始化为领导者。

#### 3. 添加学习者（Learner）

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

#### 4. 变更成员

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '[1, 2, 3, 4]' \
  http://127.0.0.1:21001/change-membership
```

#### 5. 写入和读取订单

下单：

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '{"Place":{"order":{"live":"Limit","side":"Buy","price":4000.0,"volume":4.4,"id":1,"sequence":0}}}' \
  http://127.0.0.1:21001/write
```

从任意节点读取订单簿：

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '"orderbook_orders"' \
  http://127.0.0.1:21002/read
```

您应该能够从任何节点读取到数据！

### 使用辅助脚本

项目还包含 `scripts/test.sh`，可以更方便地管理集群：

```shell
# 启动节点
./scripts/test.sh start-node 1

# 初始化节点 1，并把另外两个节点添加为学习者
./scripts/test.sh build-cluster 1

# 将学习者提升为投票成员
./scripts/test.sh change-membership

# 查看指标
./scripts/test.sh metrics 1

# 下单
./scripts/test.sh place-order 1

# 读取序列号
./scripts/test.sh get-seq 1

# 清理（数据目录会被重命名，不会被删除）
./scripts/test.sh clean
```

`scripts/test.sh` 把所有状态保存在 `RAFT_DATA_DIR`（默认 `/tmp/matchengine-raft`）下，因此
`place-order` 的结果在 `start-node`/`kill` 循环后仍然存在。

## 成员与领导权

向集群添加节点包含 3 个步骤：

1. **通过 Raft 协议写入节点信息** 到存储
2. **添加为学习者（Learner）** 让其开始从领导者接收复制数据
3. **变更成员** 将学习者提升为正式的投票成员

> **注意**: Raft 本身不存储节点地址。本实现将节点信息存储在存储层，网络层引用存储来查找目标节点地址。

## Kubernetes 控制平面（OrbStack 演示）

Kubernetes 控制平面由 `demos/k8s_control_plane` 中的独立管理器，以及引擎镜像中的 supervisor
组成。它是一个**期望状态写入器**，不是 Kubernetes 控制器：负责校验/放置 symbol、渲染
manifests，并返回运维步骤。ConfigMap/工作负载的 apply 和 PVC 操作仍由运维人员完成。

设计见 [KUBERNETES_CONTROL_PLANE.md](../KUBERNETES_CONTROL_PLANE.md)，测试层级见
[KUBERNETES_LOCAL_TESTING.md](../KUBERNETES_LOCAL_TESTING.md)，参考实现见
[`demos/k8s_control_plane/README.md`](../../demos/k8s_control_plane/README.md)。

> 这里描述的是本地 OrbStack 参考环境。管理器只写期望状态并返回操作步骤，**不是** Kubernetes
> 控制器。`delete-storage` 步骤不可逆。

### 控制平面速查

| 任务 | 管理器操作 | 说明 |
|---|---|---|
| 查看 shard | `GET /api/v1/shards` 或左侧栏 | 返回 `processes`、`disabledProcesses`、`freeSlots`、phase |
| 查看 shard 详情 | `GET /api/v1/shards/{shard}` 或点击卡片 | deployments、routes、控制台链接、模拟观测 |
| 渲染 manifests | `GET /api/v1/shards/{shard}/manifests?profile=local` | 删除前先保存 List，便于 `kubectl delete -f` |
| 添加 symbol | `POST /api/v1/symbols` 或 **+ symbol** | supervisor 零影响；containers 可能返回 `409 restart_required` |
| 停止引擎 | `POST /api/v1/symbols/{symbol}/shutdown` 或 **stop** | 保留 deployment、slot/端口和 PVC |
| 启动引擎 | `POST /api/v1/symbols/{symbol}/start` 或 **start** | 复用记录/slot/PVC；不要重新执行 `/init` |
| 删除 shard | `POST /api/v1/shards/{shard}/drop` 或 **drop shard** | 仅删除目录记录；要求所有引擎已停止；会询问 `retainStorage` |
| 审计 | `GET /api/v1/audit` | 记录 `shutdown-symbol`、`start-symbol`、`drop-shard` |

GUI 位于 `/`（也支持 `/console`）。在工作站上：

```sh
kubectl -n match-control port-forward deploy/match-manager 8099:8099
open http://127.0.0.1:8099/
```

### 安全模型和数据位置

- 一个引擎进程只交易一个 symbol；一个 shard 是共享同一个 StatefulSet 的四个 Pod。
- **Supervisor** shard 启停单个子进程时不修改 Pod 模板。**Containers** shard 增删容器会重建
  所有 Pod，因此默认返回 `409 restart_required`；只有显式传 `allowRestart: true` 才允许逐
  voter 滚动。Containers shard 中最后一个活跃引擎的停止/启动是零影响的。
- 已停止的 deployment 保留为 `Disabled`，slot 和端口继续占用，`freeSlots` 不会增加；这是为了
  防止新 symbol 复用旧数据。
- 引擎数据位于 PVC `data-<shard>-<pod>` 内的 `/data/<symbol>-<pod>`。删除 shard 时
  `retainStorage: true` 会保留这些 PVC；`false` 会删除它们。
- 管理器从不 apply 工作负载，也不删除 PVC。如果应用返回步骤前目录丢失，请按名称检查集群并手动清理。

### 各组件位置

| 组件 | 命名空间 | 地址/镜像 | 说明 |
|---|---|---|---|
| Manager API + GUI | `match-control` | `matchengine-k8s-control-plane:local`, 端口 `8099` | GUI 在 `/`；JSON 演示目录位于 `/data/catalog.json`，生产可使用 CRD 模式 |
| 引擎 shard | `match-eu-1` | `matchengine-raft:k8s-nonroot` | 四个 voter；每个 symbol 一个引擎进程 |
| Supervisor Pod | 引擎 shard Pod | `9090` 上的 `/health`、`/ready`、`/processes` | 每个 Pod 一个容器；子进程是单 symbol 引擎 |
| 引擎控制台 | 各引擎 HTTP 端口 | `/console/` | 管理器反向代理；symbol 是页面标题 |

Raft 端口始终是 `httpPort + 1000`。一个 `SymbolShard` 是四个 Pod。Supervisor shard 每个 Pod
运行多个子进程；containers 模式每个 symbol 一个引擎容器。

2026-11-14 验证时使用的 OrbStack 参考部署：

| Shard | 模式 | Symbol / 用途 |
|---|---|---|
| `fx-eu-01` | packed containers | `eurusd`、`gbpusd` |
| `fx-eu-02` | packed containers | `usdjpy`、`audusd` |
| `fx-eu-03` | reserved packed | 无工作负载；可用于安全测试 reserved shard 删除 |
| `fx-sup-01` | packed supervisor | `eurgbp`、`nzdusd`；两个子进程都在每个 Pod 中运行 |
| `me-btcusdt` | dedicated containers | `btcusdt` |

### 构建镜像

```sh
# 引擎 + supervisor（修改 web/console/ 后必须重建）
docker build -f docker/k8s.Dockerfile -t matchengine-raft:k8s-nonroot .

# Manager API + 内嵌 GUI
docker build -f demos/k8s_control_plane/Dockerfile \
  -t matchengine-k8s-control-plane:local demos/k8s_control_plane

docker images | grep -E 'matchengine-raft|matchengine-k8s-control-plane'
```

OrbStack 与 Kubernetes 节点共享本地 Docker 镜像库。使用 `imagePullPolicy: IfNotPresent`；
重建 `:local` 标签后，重新创建 Pod 即可生效。

### 部署管理器

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

如需目录持久化，给 `/data` 挂载可写卷并传 `--state /data/catalog.json`。当前演示使用
`emptyDir`，因此管理器滚动更新会重新播种 3 个 shard 的演示目录。生产环境应使用下面的 CRD 模式。

#### CRD 模式（生产推荐）

期望状态存放在 `matching.io` CRD 中，由 Kubernetes API server（底层 etcd）持久化；管理器
不会直接连接 etcd。先安装 CRD 和 RBAC：

```sh
cargo run --manifest-path demos/k8s_control_plane/Cargo.toml -- crds \
  | kubectl apply -f -
kubectl apply -f demos/k8s_control_plane/deploy/rbac.yaml
```

管理器 Pod 必须使用 `match-manager` ServiceAccount 并挂载 token。演示 manifest 当前设置
`automountServiceAccountToken: false`，需要 patch：

```sh
kubectl -n match-control patch deploy match-manager --type=merge \
  -p '{"spec":{"template":{"spec":{"serviceAccountName":"match-manager","automountServiceAccountToken":true}}}}'
```

真实集群使用 `--crd --no-seed` 启动管理器。缓存先通过 list 构建，再由每种 CRD 的 informer 刷新：

```sh
kubectl -n match-control set args deploy/match-manager -- \
  serve --addr 0.0.0.0:8099 --crd --crd-namespace match-eu-1 --no-seed
kubectl -n match-control rollout status deploy/match-manager --timeout=120s
```

该模式下 `GET /api/v1/health` 返回 `"store":"crd"`。状态通过 `/status` 子资源写入，并按对象限速；
演示的审计历史仍在内存中，生产应写入外部 append-only 存储。引擎 WAL/snapshot、订单簿、撮合事件
和行情数据永远不进入 CRD/etcd。

管理器需要能访问引擎 Pod 以提供控制台代理。渲染出的 NetworkPolicy 允许来自带
`matching.io/role=control-plane` 标签命名空间的 HTTP；如果 GUI 部署在其他命名空间，请保留该标签。

端口转发到工作站：

```sh
kubectl -n match-control port-forward deploy/match-manager 8099:8099
# http://127.0.0.1:8099/
```

如果管理器运行在工作站而不是集群内，可用
`cargo run --manifest-path demos/k8s_control_plane/Cargo.toml -- serve --engine-console-host 127.0.0.1`
把控制台代理指向回环地址；端口仍来自目录中的 placement。

### 创建目录并 apply shard

内置演示目录包含 `fx-eu-01`（packed containers，`eurusd`/`gbpusd`）、`fx-eu-02`
（reserved packed，4 个 slot）和 `me-btcusdt`（dedicated）。可以通过 GUI **+ shard** 或 API
创建额外的 supervisor/reserved shard。

通过管理器渲染一个 shard（API 返回 `v1/List`，可直接被 `kubectl apply -f -` 接收）：

```sh
curl -s 'http://127.0.0.1:8099/api/v1/shards/fx-eu-01/manifests?profile=local' \
  | kubectl apply -f -
```

对每个 shard 重复。Reserved containers shard 只渲染 ConfigMap、PDB 和 NetworkPolicy；
supervisor shard 即使没有子进程也会保留 supervisor StatefulSet。

等待 Pod 就绪后，每个进程只 bootstrap 一次。管理器 Pod 内有 `curl`，且从控制命名空间可以解析
Pod FQDN：

```sh
for shard in fx-eu-01 fx-eu-02 fx-sup-01 me-btcusdt; do
  kubectl -n match-eu-1 get pods -l matching.io/shard="$shard"
done

# 示例：初始化 fx-eu-01 上端口 21001 的 eurusd（每个进程/端口重复一次）。
kubectl -n match-control exec deploy/match-manager -- \
  curl -s -X POST -H 'content-type: application/json' \
  -d '[[0,"fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001"],[1,"fx-eu-01-1.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001"],[2,"fx-eu-01-2.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001"],[3,"fx-eu-01-3.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001"]]' \
  http://fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001/init
```

每个进程只能执行一次 `/init`。Supervisor shard 的每个子进程有自己的端口，是独立的四 voter
Raft 组。然后通过 leader 写入复制的 symbol 配置：

```sh
# 查看某个进程的 leader。
kubectl -n match-control exec deploy/match-manager -- \
  curl -s http://fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001/metrics

# 向 leader 写入 ConfigureSymbol。
kubectl -n match-control exec deploy/match-manager -- \
  curl -s -X POST -H 'content-type: application/json' \
  -d '{"ConfigureSymbol":{"config":{"symbol_id":1,"base_asset":{"code":"EUR","class":"fiat","decimals":2},"quote_asset":{"code":"USD","class":"fiat","decimals":2},"price_decimals":5,"price_tick":"0.00001","base_lot":"0.01","fee_asset_policy":"received","stp":"off"}}}' \
  http://fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001/write
```

`ConfigureSymbol` 的准确结构就是引擎 `/write` 的 payload；生产工具建议使用
`src/client.rs` 中的 `provision_symbol` 客户端 helper。管理器重启后，
`POST /api/v1/shards/{shard}/simulate?scenario=ready` 只用于让演示 GUI 的合成路由状态更真实，
不会读取真实集群，也不会改变引擎状态。

### 验证

```sh
curl -s http://127.0.0.1:8099/api/v1/health
curl -s http://127.0.0.1:8099/api/v1/shards | python3 -m json.tool | head
kubectl -n match-eu-1 get pods -o wide
kubectl -n match-eu-1 get pvc
kubectl -n match-eu-1 get statefulsets

# Supervisor 子进程（只展示一个 Pod；应检查全部四个）
kubectl -n match-eu-1 exec fx-sup-01-0 -- curl -s http://127.0.0.1:9090/processes

# 通过管理器代理打开引擎控制台（symbol 是醒目标题）
open 'http://127.0.0.1:8099/api/v1/shards/fx-sup-01/processes/eurgbp/pods/0/console/'
```

控制台的 cluster 页必须显示四个可达 peer。已停止的引擎在管理器中不再有控制台链接，其代理返回
`409 process_disabled`。

### 添加 symbol

通过 GUI 或以下命令向 **supervisor** shard 添加 symbol：

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

向非空 **containers** shard 添加会返回 `409 restart_required`；只有能接受逐 voter 滚动时，
才传 `"allowRestart": true`。管理器分配最小空闲 slot 和 `httpBasePort + slot`；如需不同放置，
请重命名或迁移 symbol。

### 停止和启动引擎

```sh
# 优雅停止：保留记录、slot 和 PVC。
curl -s -X POST http://127.0.0.1:8099/api/v1/symbols/chfusd/shutdown \
  -H 'content-type: application/json' -d '{}'

# 从保留数据重新启用；不需要 /init 或 ConfigureSymbol。
curl -s -X POST http://127.0.0.1:8099/api/v1/symbols/chfusd/start \
  -H 'content-type: application/json' -d '{}'
```

管理器把 `SymbolDeployment.spec.desiredState` 和 `SymbolRoute.spec.state` 设置为
`Disabled`/`Active`，并返回运维步骤。Supervisor 模式下只 apply 更新后的 ConfigMap，让
supervisor 启停该子进程而不影响其他 PID：

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

等待 supervisor 读取挂载文件（kubelet 传播可能需要约一分钟），然后确认只有目标子进程变化：

```sh
for pod in 0 1 2 3; do
  echo "fx-sup-01-$pod"
  kubectl -n match-eu-1 exec "fx-sup-01-$pod" -- curl -s http://127.0.0.1:9090/processes
done
```

`desired` 应只列出活跃子进程；未受影响的子进程保持 PID 不变，`restarts` 保持 `0`。启动后等待
`/ready`、`GET /read symbol_id` 和控制台 cluster 页（4/4 peer）再发布路由。如果数据目录已被删除，
引擎必须重新初始化和配置。

Containers 模式下，其他引擎仍活跃时 `shutdown`/`start` 会返回 `409 restart_required`，除非设置
`allowRestart`。Apply 渲染出的 StatefulSet 变更，逐个删除/重建 Pod，并在处理下一个 Pod 前等待
`/ready`。Containers shard 中最后一个活跃引擎的停止/启动是零影响的。

### 删除 shard

删除只会从**管理器目录**移除 shard、其 deployments，以及指向这些 deployments 的 routes。
仍被其他 shard 的 deployment 引用的 symbol 会保留。删除前必须停止 shard 中的每个引擎；否则 API
返回 `409 shard_not_idle` 并列出活跃 symbol。

> **重要：管理器不会删除 Kubernetes 资源。**
> `drop` 成功只删除目录记录。StatefulSet、Pod、Service、ConfigMap、PDB 和 NetworkPolicy 会一直
> 保留，直到运维人员执行 prune。只有选择 `retainStorage: false` 并执行返回的 `delete-storage`
> 步骤后，PVC 才会被删除。**Reserved** shard 从未真正创建过工作负载时，没有任何 Kubernetes
> 对象需要清理；目录返回 404 后即可结束。

删除后的预期状态取决于 shard 内容：

| 删除时的 shard | 运维执行步骤后的状态 |
|---|---|
| Reserved（`0` 个 deployment，从未物化） | 从未存在 StatefulSet/Pod/Service/ConfigMap/PDB/NetworkPolicy/PVC；只有目录记录消失。 |
| 已物化，`retainStorage: true` | 工作负载和 shard 对象消失；四个 `data-<shard>-0..3` PVC 保留为 Bound/Released，以便恢复 shard。 |
| 已物化，`retainStorage: false` | 工作负载和 shard 对象消失；PVC/PV 也消失。此操作不可逆。 |

#### 1. 保存目录和 manifests

在修改目录前保存；删除后管理器无法再渲染该 shard：

```sh
SHARD=fx-eu-03

curl -s http://127.0.0.1:8099/api/v1/catalog > /tmp/catalog-before-drop.json
curl -s "http://127.0.0.1:8099/api/v1/shards/$SHARD" > "/tmp/$SHARD-detail.json"
curl -s "http://127.0.0.1:8099/api/v1/shards/$SHARD/manifests?profile=local" \
  > "/tmp/$SHARD-manifests.json"
```

#### 2. 停止所有 symbol 并 apply ConfigMap

```sh
curl -s "http://127.0.0.1:8099/api/v1/shards/$SHARD" \
  | python3 -c 'import json,sys; print("\n".join(d["spec"]["symbolRef"] for d in json.load(sys.stdin)["deployments"]))' \
  | while read -r symbol; do
      echo "stopping $symbol"
      curl -s -X POST "http://127.0.0.1:8099/api/v1/symbols/$symbol/shutdown" \
        -H 'content-type: application/json' -d '{}'
    done
```

按[停止和启动引擎](#停止和启动引擎)中的方法 apply 更新后的 ConfigMap，然后确认 shard 已空闲：

```sh
curl -s "http://127.0.0.1:8099/api/v1/shards/$SHARD" | python3 -c '
import json, sys
d = json.load(sys.stdin)
print("active", d["activeProcesses"], "disabled", d["disabledProcesses"])
print([(x["spec"]["symbolRef"], x["spec"]["desiredState"]) for x in d["deployments"]])
'
```

#### 3. 选择是否保留存储

GUI 对话框和 API 都会询问；**默认且推荐保留存储**：

```sh
# 安全默认：prune 工作负载但保留 PVC，使 shard 可以再次启动。
curl -s -X POST "http://127.0.0.1:8099/api/v1/shards/$SHARD/drop" \
  -H 'content-type: application/json' \
  -d '{"retainStorage": true}' | tee "/tmp/$SHARD-drop.json"

# 破坏性：同时删除 PVC。集群内将无法恢复。
curl -s -X POST "http://127.0.0.1:8099/api/v1/shards/$SHARD/drop" \
  -H 'content-type: application/json' \
  -d '{"retainStorage": false}'
```

响应包含 `removedDeployments`、`removedRoutes`、`removedSymbols`、预测的 `pvcs`
（`data-<shard>-0..N-1`），以及 `keep-storage`/`delete-storage` 步骤。执行破坏性步骤前先查看：

```sh
python3 -m json.tool "/tmp/$SHARD-drop.json"
```

#### 4. Prune Kubernetes 工作负载

使用删除前保存的 manifests；这会删除 StatefulSet、Service、PDB、NetworkPolicy 和 ConfigMap，
但不会删除 PVC：

```sh
kubectl -n match-eu-1 delete -f "/tmp/$SHARD-manifests.json" --ignore-not-found
kubectl -n match-eu-1 get pods,statefulset,service,pdb,networkpolicy,configmap \
  | grep "$SHARD" || true
```

如果选择 `retainStorage: false`，在确认 shard 名称和备份情况后单独删除 PVC：

```sh
for pod in 0 1 2 3; do
  kubectl -n match-eu-1 delete pvc "data-$SHARD-$pod"
done
```

删除命名空间或所有 PVC 会绕过管理器的保留选择，并且不可逆。

#### 5. 验证

删除并执行运维 prune 后，使用以下检查表：

- [ ] 管理器目录对该 shard 返回 `404`。
- [ ] 没有残留的 StatefulSet、Pod、Service、ConfigMap、PDB 或 NetworkPolicy。
- [ ] `retainStorage: true`：`data-<shard>-0..N-1` PVC 仍然存在（PV 处于 Bound）。
- [ ] `retainStorage: false`：没有该 shard 的 PVC 或 PV。
- [ ] 剩余的 StatefulSet/Pod/PVC 数量与目录中剩余的 shard 一致。

```sh
SHARD=fx-eu-03

# 目录已删除。
curl -s -o /dev/null -w 'catalog HTTP %{http_code}\n' \
  "http://127.0.0.1:8099/api/v1/shards/$SHARD"   # 404
curl -s http://127.0.0.1:8099/api/v1/shards | python3 -m json.tool | head

# 工作负载对象已 prune。
kubectl -n match-eu-1 get statefulsets,deployments,pods,services,configmaps,pdb,networkpolicy \
  -o name | grep "$SHARD" || echo "no workload objects"

# 存储状态与 retainStorage 选择一致。
kubectl -n match-eu-1 get pvc -o name | grep "$SHARD" || echo "no PVCs"
kubectl get pv -o name | grep "$SHARD" || echo "no PVs"

# 剩余资源与目录中剩余的 shard 对齐。
kubectl -n match-eu-1 get statefulsets --no-headers | wc -l
kubectl -n match-eu-1 get pods --no-headers | wc -l
kubectl -n match-eu-1 get pvc --no-headers | wc -l
```

参考示例：删除 reserved `fx-eu-03` 且 `retainStorage: false` 时，审计记录为

```text
drop-shard | fx-eu-03 | removed 0 deployment(s), 0 route(s), 0 symbol(s); retainStorage=false
```

并且没有留下任何 `fx-eu-03` 对象。剩余四个 shard 正好对应 4 个 StatefulSet、16 个 Pod、9 个
Service、4 个 shard ConfigMap、4 个 PDB、4 个 NetworkPolicy 和 16 个 Bound PVC。因为该 shard 是
reserved，本来就没有工作负载需要 prune；目录返回 `404` 就是全部清理工作。对于已物化的 shard，
同样的检查必须显示工作负载对象已消失，且 PVC 状态与存储选择一致。

### 使用保留存储恢复 shard

`retainStorage: true` 会保留 `data-<shard>-<pod>`。要重新启动 shard，必须以**相同的 shard 形状**
重建，使新 Pod 绑定现有 PVC：

1. 确认 PVC 存在，且没有旧 Pod 仍占用它们。
2. 使用相同的名称、命名空间、副本数、storage class 和 HTTP 基础端口重建 shard。最安全的输入是
   创建时使用的原始 `ShardBundle`，或 `/tmp/catalog-before-drop.json` 中的 `shards.<name>` 对象：

   ```sh
   # Bundle 会按精确的 slot/端口恢复 shard 及其 symbol、deployment 和 route。
   curl -s -X POST http://127.0.0.1:8099/api/v1/bundles \
     -H 'content-type: application/json' --data-binary @fx-eu-03.json
   ```

3. 渲染并 apply manifests；StatefulSet 会复用 `data` claim 模板及其 PVC 名称：

   ```sh
   curl -s "http://127.0.0.1:8099/api/v1/shards/$SHARD/manifests?profile=local" \
     | kubectl apply -f -
   ```

4. 等待所有 Pod 就绪。如果保留数据完好，**不要执行 `/init`，也不要重新 apply
   `ConfigureSymbol`**：每个引擎会打开自己的 WAL/snapshot，重新加入同一个四 voter 组，并恢复
   已应用的 symbol 配置。
5. 验证 `GET /ready`、`POST /read` 的 `"symbol_id"`、`POST /read` 的 `"symbol_config"`，以及
   管理器控制台的 cluster 页（4/4 peer）。之后才发布路由。

必须复用原来的 slot、HTTP 端口和副本数。Raft 成员关系保存 peer 地址；用不同端口重建的 shard
无法重新加入旧组。如果某个 PVC 丢失，该引擎的数据就不存在了，必须重新初始化和配置。

### 期望状态持久化和管理器重启

**CRD 模式（推荐）。** 期望状态存放在 `matching.io` CRD 中，因此管理器滚动或 Pod 替换不会
丢失目录：新 Pod 会 list CRD 并从 informer 重建缓存，不需要为期望状态挂载 PVC。升级或破坏性
操作前先备份：

```sh
# 选择性导出目录（JSON，只包含期望状态）。
cargo run --manifest-path demos/k8s_control_plane/Cargo.toml -- \
  crd-backup --out /tmp/crd-catalog.json

# 集群级备份：etcd 快照同样覆盖所有 CR；具体命令取决于托管 Kubernetes 提供商。
```

恢复顺序是：先恢复 CR，再由 operator 物化工作负载，最后恢复引擎 PVC。`crd-restore` 会校验
JSON 并通过 API server 写回 CRD：

```sh
cargo run --manifest-path demos/k8s_control_plane/Cargo.toml -- \
  crd-restore --input /tmp/crd-catalog.json
```

审计/操作历史不属于 CRD，应使用外部审计存储。引擎数据在 PVC 中，使用
`VolumeSnapshot`/`VolumeGroupSnapshot` 单独备份。

**JSON 演示回退。** `serve --state /data/catalog.json` 仍可用于本地演示。参考 manifest 使用
`emptyDir`，所以 Deployment 滚动会重新播种 3 shard 演示目录，并丢失 symbols、routes、
Disabled 状态和审计历史，除非给 `/data` 挂载 PVC：

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
# 先保存当前目录；这个 patch 会滚动管理器。
curl -s http://127.0.0.1:8099/api/v1/catalog > /tmp/manager-catalog.json
kubectl -n match-control patch deploy match-manager --type=json \
  -p='[{"op":"replace","path":"/spec/template/spec/volumes/0","value":{"name":"data","persistentVolumeClaim":{"claimName":"match-manager-state"}}}]'
kubectl -n match-control rollout status deploy/match-manager --timeout=120s
```

JSON 模式下，任何管理器滚动前先导出目录；如果文件丢失，通过 `POST /api/v1/bundles`、
`POST /api/v1/shards` 和 `POST /api/v1/symbols` 重建期望状态。这不会重启引擎，只重建管理器
视图。`POST /api/v1/shards/{shard}/simulate?scenario=ready` 只让演示 GUI 的合成路由状态更真实，
不会改变引擎状态。

### 故障排查

| 现象 | 检查 |
|---|---|
| 管理器 Pod 重启后目录变成 3-shard 演示 | 仅 JSON 模式：`/data` 是 `emptyDir` 或未传 `--state`。改用 CRD 模式，或用保存的目录/bundle 重建；CRD 模式会从 CRD 重建。 |
| CRD 模式返回 `kubernetes_api` 403 | 管理器 ServiceAccount 缺少 RBAC 或 token 未挂载。应用 `deploy/rbac.yaml`，设置 `serviceAccountName: match-manager` 和 `automountServiceAccountToken: true`。 |
| CRD 模式返回 `kubernetes_api` 404 | 五个 `matching.io` CRD 未安装。运行 `cargo run -- crds \| kubectl apply -f -`。 |
| CRD 管理器启动后目录为空 | 所列命名空间中没有 CR，或缺少 list/watch RBAC。用 `crd-restore` 恢复，或通过 API 创建 shard/symbol。 |
| `ImagePullBackOff` / 引擎控制台没有 symbol 标题 | 镜像已重建但 Pod 未重建。使用 `imagePullPolicy: IfNotPresent`，然后 `kubectl -n match-eu-1 rollout restart statefulset/<shard>` 并等待 `/ready`。 |
| 添加/停止/启动返回 `409 restart_required` | shard 是 containers 模式且还有其他活跃引擎；改用 supervisor shard、空 shard，或显式允许 `allowRestart`。 |
| 删除返回 `409 shard_not_idle` | 先停止 shard 中所有 symbol；错误会列出活跃项。Apply 更新后的 ConfigMap 后重试。 |
| `drop` 返回 `200` 但 Pod/StatefulSet 仍存在 | 管理器只写期望状态。用删除前保存的 manifests 执行 `kubectl -n match-eu-1 delete -f /tmp/<shard>-manifests.json --ignore-not-found`。 |
| Reserved shard 删除后没有 PVC 或工作负载 | 正常：reserved shard 从未物化。确认目录返回 `404` 且没有同名对象即可。 |
| 选择 `retainStorage: true` 但 PVC 不见了 | 运维或命名空间清理删除了它们；管理器无法重建数据。从备份恢复或重新初始化 shard。 |
| 恢复后的 shard Pod 一直 `Pending` 或看不到旧数据 | 必须以相同 shard 名称、副本数、slot/端口和 storage class 重建；PVC 名是 `data-<shard>-<pod>`。不同端口或名称不会重新绑定旧 claim，也无法重新加入旧组。 |
| `start` 标记 `Active` 但子进程不存在 | ConfigMap 尚未 apply，或 kubelet 尚未传播。只 apply ConfigMap 并检查 `/processes`；传播可能需要一分钟。 |
| `start` 返回 `409 restart_required` | Containers 模式还有其他活跃引擎。只有在可接受逐 Pod 滚动时才传 `{"allowRestart": true}`。 |
| 管理器显示 symbol `Active` 但引擎已停止 | 管理器重启后重建了目录；用 `/start`（或 GUI **start**）让期望状态与引擎一致，然后检查 `/ready`。 |
| 停止后 `freeSlots` 没有增加 | 符合预期：Disabled deployment 保留 slot，直到 shard 被删除或 symbol 被删除。 |
| 选择 `delete-storage` 后 PVC 仍在 | 管理器不删除 PVC。执行返回的步骤：`kubectl delete pvc data-<shard>-0 ...`。 |
| ConfigMap 变更后 supervisor 没有启停子进程 | kubelet 传播可能需要一分钟；supervisor 每秒轮询。检查 `kubectl exec <pod> -- curl -s :9090/processes` 和挂载的 `processes.json`。 |
| 引擎控制台代理返回 `502` | 管理器无法解析/连接 `<pod>.<shard>-headless.<namespace>.svc:httpPort`；检查 NetworkPolicy、DNS 和 Pod 是否 Ready。 |
| 引擎控制台代理返回 `409 process_disabled` | deployment 是 `Disabled`；先执行 **start**。 |
| `/init` 报 “already initialized” | 不要重复执行 `/init`；重启的引擎会从 PVC 恢复。恢复保留的 shard 时不要重新初始化完好的数据。 |

### 清理

完整删除演示环境（包括所有引擎数据、保留的 PVC 和 CRD）：

```sh
# 如果使用 CRD 模式且以后可能需要恢复，先导出期望状态。
cargo run --manifest-path demos/k8s_control_plane/Cargo.toml -- \
  crd-backup --out /tmp/crd-catalog.json || true
kubectl delete namespace match-control
kubectl delete namespace match-eu-1
kubectl delete -f demos/k8s_control_plane/deploy/crds.yaml --ignore-not-found
```

保留数据的安全清理：

1. 保存 `/api/v1/catalog` 和每个 shard 的 manifests（见[删除 shard](#删除-shard)）。
2. 停止所有 symbol 并 apply 更新后的 ConfigMap。
3. 对每个 shard 执行 `{"retainStorage": true}` 的 drop。
4. 用保存的 manifests 删除工作负载，但保留 `match-eu-1` 命名空间及其
   `data-<shard>-<pod>` PVC。
5. 如果不再需要 GUI，可以删除 manager Deployment/Service；以后用 `--state` 或保存的 bundle
   重建不会影响引擎或 PVC。

如果打算保留存储，**不要**删除 `match-eu-1` 命名空间：命名空间删除会级联删除 PVC。删除命名空间
或 PVC 会绕过管理器的保留选择，并且不可逆。

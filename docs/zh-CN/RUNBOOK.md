# 运行手册

## 启动集群

### 使用测试脚本快速启动

查看集群运行的最简单方式是运行提供的测试脚本：

```shell
./scripts/test-cluster.sh
```

该脚本会构建并启动一个 3 节点集群（`127.0.0.1:21001-21003`，gRPC Raft 位于
`22001-22003`），完成初始化、添加学习者、变更成员以及读写操作。脚本只用 `curl` 访问 HTTP
API，便于观察请求/应答流程。状态保存在 `/tmp/matchengine-raft-test-cluster`，每次启动前会清空
该目录，因此可以重复运行；如需保留状态，请设置 `RAFT_DATA_DIR`。

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
```

每个节点在 `端口 + 1000`（这里是 `22001-22003`）上监听 gRPC Raft RPC；HTTP 监听器只提供应用和
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
```

#### 4. 变更成员

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '[1, 2, 3]' \
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

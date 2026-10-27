# Match Engine Raft

这是一个基于 [OpenRaft](https://github.com/datafuselabs/openraft) 构建的分布式撮合引擎的实践实现。Raft 日志持久化在自研的分段预写日志（WAL）中，状态机通过原子快照落盘，因此节点重启后可以恢复全部已提交状态。

> **注意**: 更详细的中文指南请参考 [GUIDE_cn.md](./GUIDE_cn.md)。

## 目录

- [功能特性](#功能特性)
- [架构概述](#架构概述)
- [快速开始](#快速开始)
- [运行集群](#运行集群)
- [项目结构](#项目结构)
- [存储实现](#存储实现)
- [集群管理](#集群管理)
- [API 参考](#api-参考)
- [测试](#测试)

## 功能特性

- **持久化 Raft 存储**: 带校验和的分段预写日志，每个复制批次一次 fsync；状态机通过原子文件快照持久化
- **崩溃恢复**: 重启时加载快照并重放保留的日志尾部，日志重放与快照两条恢复路径均有集成测试覆盖
- **gRPC 复制**: 基于 [tonic](https://docs.rs/tonic) 的传输层，支持双向 `stream_append`、分块快照传输，并为每个对端维护独立的复制/心跳/快照通道；可用 `--transport http` 切回 HTTP/JSON
- **撮合引擎**: 包含一个简单的订单簿撮合引擎作为示例应用
- **HTTP 服务器**: 基于 [Actix-web](https://docs.rs/actix-web) 构建，同时提供内部 Raft API 和外部应用 API
- **多种读取一致性**: 本地读、经领导者的一致性读，以及线性一致性的 follower read
- **智能客户端**: Rust 客户端自动跟踪并将请求重定向到当前领导者

## 架构概述

本项目扩展了 [example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv)，增加了持久化存储和 gRPC 传输：

```
                    ┌─────────────────────────────────────────┐
                    │           应用层                         │
                    │  (KV 存储 + 订单簿撮合引擎)             │
                    └─────────────────────┬───────────────────┘
                                          │
                    ┌─────────────────────▼───────────────────┐
                    │           OpenRaft 核心                  │
                    │  (共识、复制、领导者选举)                 │
                    └─────────────────────┬───────────────────┘
                                          │
              ┌───────────────────────────┼───────────────────────────┐
              │                           │                           │
    ┌─────────▼─────────┐     ┌─────────▼─────────┐     ┌─────────▼─────────┐
    │  日志 + 状态      │     │   RaftNetwork     │     │   HTTP API        │
    │ (WAL + 快照文件)  │     │  (gRPC / HTTP)    │     │  (Actix-web)      │
    └───────────────────┘     └───────────────────┘     └───────────────────┘
```

节点间 Raft RPC 默认使用 `端口 + 1000` 上的 gRPC。公共 API 与 HTTP Raft 处理器仍由
`--http-addr` 监听器提供，因此无论节点主动使用哪种传输，它都会同时服务两种协议。

### 核心组件

1. **RaftStorage 实现** ([`store/`](./src/store/))
   - Raft 日志存储在分段 WAL（`log/*.wal`）中
   - 投票和 purge 标记存储在原子重写的 `log/state` 文件中
   - 内存状态机，通过原子写入的 `snapshot/current.snap` 持久化

2. **网络层** ([`network/`](./src/network/))
   - gRPC Raft RPC（`src/network/grpc/`），负责复制、投票和快照流式传输
   - 可运行时切换的旧版 HTTP/JSON Raft RPC（`src/network/raft.rs`、`src/network/transport.rs`）
   - 每个对端独立的复制、心跳和快照通道
   - 用于集群管理的管理 API

3. **撮合引擎** ([`matchengine/`](./src/matchengine/))
   - 包含买单和卖单的订单簿
   - 现实世界状态机应用的示例

## 快速开始

### 前置要求

- Rust 1.85 或更高版本（本项目使用 edition 2024）
- Cargo

无需安装 `protoc`：构建脚本通过 `protoc-bin-vendored` 编译 `proto/raft.proto`。

### 构建

```shell
cargo build
```

> **注意**: 生产环境构建可以添加 `--release`，但本项目主要作为示例，不建议在生产环境中直接使用，需要进一步加固。

## 运行集群

### 使用测试脚本快速启动

查看集群运行的最简单方式是运行提供的测试脚本：

```shell
./test-cluster.sh
```

该脚本会构建并启动一个 3 节点集群（`127.0.0.1:21001-21003`，gRPC Raft 位于
`22001-22003`），完成初始化、添加学习者、变更成员以及读写操作。脚本只用 `curl` 访问 HTTP
API，便于观察请求/应答流程。状态保存在 `/tmp/matchengine-raft-test-cluster`，每次启动前会清空
该目录，因此可以重复运行；如需保留状态，请设置 `RAFT_DATA_DIR`。

### 使用 Rust 测试

```shell
cargo test
```

集成测试通过 `MatchEngineClient` 运行集群场景，覆盖 gRPC 快照路径、WAL 与快照文件格式，并重启
节点验证恢复。也可以用旧版传输运行：

```shell
MATCHENGINE_TEST_TRANSPORT=http cargo test --test cluster
```

各测试文件的职责见[测试](#测试)。

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

每个节点在 `端口 + 1000`（这里是 `22001-22003`）上监听 gRPC Raft RPC。使用
`--transport http` 可让节点改用 HTTP/JSON 连接对端；HTTP Raft 端点始终挂载，因此迁移期间可以
混用两种传输。

所有状态写入 `<data-dir>/<instance-prefix>-<id>/`，详见[每节点数据目录](#每节点数据目录)。

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

#### 5. 写入和读取数据

写入数据：

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '{"Set":{"key":"foo","value":"bar"}}' \
  http://127.0.0.1:21001/write
```

从任意节点读取数据：

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '"foo"' \
  http://127.0.0.1:21002/read
```

您应该能够从任何节点读取到数据！

### 使用辅助脚本

项目还包含 `test.sh`，可以更方便地管理集群：

```shell
# 启动节点
./test.sh start-node 1

# 初始化节点 1，并把另外两个节点添加为学习者
./test.sh build-cluster 1

# 将学习者提升为投票成员
./test.sh change-membership

# 查看指标
./test.sh metrics 1

# 下单
./test.sh place-order 1

# 读取序列号
./test.sh get-seq 1

# 清理（数据目录会被重命名，不会被删除）
./test.sh clean
```

`test.sh` 把所有状态保存在 `RAFT_DATA_DIR`（默认 `/tmp/matchengine-raft`）下，因此
`place-order` 的结果在 `start-node`/`kill` 循环后仍然存在。

## 项目结构

```
src/
├── bin/
│   ├── main.rs              # 服务器入口（参数、环境变量、存储配置）
│   └── sled_test.rs         # 游乐场二进制（本地 sled 目录已 gitignore）
├── client.rs                # 支持领导者转发的智能客户端
├── lib.rs                   # 节点组装、服务与关闭
├── app.rs                   # 应用状态
├── matchengine/
│   └── mod.rs               # 订单簿撮合引擎
├── network/
│   ├── api.rs               # 应用 HTTP 端点
│   ├── management.rs        # 管理 HTTP 端点
│   ├── raft.rs              # Raft 内部 HTTP RPC 端点
│   ├── raft_network_impl.rs # HTTP/JSON RaftNetworkV2 实现
│   ├── transport.rs         # 运行时传输选择（gRPC 或 HTTP）
│   ├── grpc/
│   │   ├── client.rs        # gRPC RaftNetworkV2（追加、投票、快照）
│   │   ├── server.rs        # gRPC Raft 服务
│   │   └── mod.rs           # Protobuf 类型与消息大小限制
│   └── mod.rs               # Transport 枚举、Raft 端口推导
├── store/
│   ├── mod.rs               # 存储模块
│   ├── log_store.rs         # 分段 WAL + 投票/purge 状态
│   ├── state_machine.rs     # 内存状态机 + 快照
│   ├── types.rs             # 请求/应答类型
│   └── config.rs            # 存储布局 / 每节点目录
proto/
└── raft.proto               # gRPC Raft 服务定义
tests/
└── cluster/
    ├── main.rs              # 单一集成测试目标
    ├── common.rs            # 唯一临时数据目录
    ├── test_cluster.rs      # 3 节点集群，两种传输
    ├── test_snapshot.rs     # 快照生成/purge + 学习者通过 gRPC 安装
    ├── test_storage.rs      # WAL 与快照格式
    └── test_restart.rs      # 重启恢复：日志重放与快照
```

## 存储实现

### 预写日志（`src/store/log_store.rs`）

Raft 日志追加写入 `log/NNNNNNNNNN.wal` 分段文件：

- 每条记录格式为 `[len: u32 LE][crc32: u32 LE][postcard 负载]`；启动时逐段扫描，遇到撕裂或校验失败的尾部直接截断，而不是当作有效记录解析。
- `append()` 用一次 `write_all` 写入整个 openraft 批次，再执行一次 `sync_data`，因此一次 AppendEntries RPC 中的 N 条记录只付一次 fsync 的代价。
- 读取使用 `pread`（`read_at`）和缓存的文件句柄，复制任务可以在追加进行时并发读取，磁盘 IO 期间不持有全局锁。
- 所有阻塞 IO 都放在 `spawn_blocking` 中执行，异步运行时只负责编解码。
- 分段默认 64 MiB；只有被 purge 完全覆盖的分段才会删除，删除前先轮转活跃分段，保证日志不出现空洞。

投票和最后一次 purge 的日志 id 属于安全关键且低频的状态，单独存放在 `log/state` 文件中，采用原子重写（临时文件 → fsync → rename → 目录 fsync）。committed 日志 id 追加到 WAL 尾部但不单独 fsync，随下一次 append 一起持久化，避免每批次多一次 fsync。

### 快照（`src/store/state_machine.rs`）

状态机（KV 表 + 订单簿 + 序列号）常驻内存以保证 apply 速度。快照写入 `snapshot/current.snap`（magic + 版本 + CRC32 + postcard 元数据 + postcard 状态），同样通过临时文件 + rename 原子替换。`install_snapshot` 会先校验收到的数据再替换磁盘副本，坏快照不会破坏已有的好快照。

### 每节点数据目录

每个节点写入 `<data-dir>/<instance-prefix>-<node-id>/{log,snapshot}`。可通过命令行配置：

```bash
./target/release/matchengine-raft --id 1 --http-addr 127.0.0.1:21001 \
    --data-dir /var/lib/matchengine-raft --instance-prefix prod
```

对应的环境变量为 `RAFT_DATA_DIR` 和 `RAFT_INSTANCE_PREFIX`，默认值分别是 `/tmp/matchengine-raft` 和 `matchengine`。

### 快照配置

`default_raft_config()`（`src/lib.rs`）只设置心跳和选举时间，快照策略沿用 openraft 默认值
（`LogsSinceLast(5000)`，保留 1000 条已覆盖日志）。需要快速生成快照的测试会覆盖它：

```rust
let mut config = default_raft_config();
config.snapshot_policy = SnapshotPolicy::LogsSinceLast(10);
config.max_in_snapshot_log_to_keep = 0; // 清理快照已覆盖的全部日志
```

- 每应用配置数量的日志构建一次快照（默认 5000 条）。
- 快照覆盖的日志加上 `max_in_snapshot_log_to_keep` 之后会被 purge。快照测试同时调小这两个值，
  使后加入的学习者无法只靠日志复制追上，必须安装快照。

### 数据恢复

启动时节点会：
1. 恢复 WAL 索引、投票和 purge 标记（并丢弃撕裂的尾部）。
2. 如果存在 `snapshot/current.snap`，加载它并重建订单簿和 KV 表。
3. 由 openraft 在其上重放保留的日志尾部。

单节点集群因此可以恢复所有已提交的 KV 写入、活动订单和序列号，无论恢复路径是纯日志重放还是快照 + 尾部重放。两条路径都由 `tests/cluster/test_restart.rs` 覆盖。

## 集群管理

向集群添加节点包含 3 个步骤：

1. **通过 Raft 协议写入节点信息** 到存储
2. **添加为学习者（Learner）** 让其开始从领导者接收复制数据
3. **变更成员** 将学习者提升为正式的投票成员

> **注意**: Raft 本身不存储节点地址。本实现将节点信息存储在存储层，网络层引用存储来查找目标节点地址。

## API 参考

所有端点都使用 JSON。每个处理器返回 `Result` 包装，因此成功调用形如 `{"Ok": ...}`，转发或失败
调用形如 `{"Err": ...}`。

### Raft 内部 API

挂载在公共 HTTP 监听器上，供 HTTP/JSON 传输使用：

- `POST /append` - 追加条目 RPC
- `POST /vote` - 请求投票 RPC
- `POST /snapshot` - 安装完整快照 RPC

gRPC 传输在 `--http-addr` 端口 + 1000 上暴露相同操作（`AppendEntries`、`Vote`、
`InstallSnapshot`、`AppendStream`），定义见 `proto/raft.proto`。

### 管理 API

- `POST /init` - 初始化集群；请求体为 `[]` 时只初始化当前节点，也可传
  `[[1, "127.0.0.1:21001"], ...]` 显式指定成员
- `POST /add-learner` - 添加学习者节点；请求体为 `[id, "host:port"]`
- `POST /change-membership` - 变更投票成员集合；请求体为 `[1, 2, 3]`
- `GET /metrics` - 获取 Raft 指标
- `POST /get_linearizer` - 从领导者获取读 linearizer（供 `/follower_read` 使用）

### 应用 API

- `POST /write` - 通过 Raft 提交 `Set`、`Place` 或 `Cancel` 请求
- `POST /read` - 从本地状态机读取（可能读到旧值）
- `POST /linearizable_read` - 线性一致性读；请求必须到达领导者
- `POST /follower_read` - 从领导者获取 linearizer 后在本地完成的线性一致性读

`/read`、`/linearizable_read` 和 `/follower_read` 接收 JSON 字符串键。除普通 KV 键外，状态机
还暴露：

- `orderbook_orders` - 所有活动订单的 JSON 数组
- `orderbook_sequence` - 订单序列号计数器（兼容旧拼写 `orderbook_sequance`）

`/write` 接受的订单示例（注意字段名是 `sequence`）：

```json
{"Place": {"order": {"live": "Limit", "side": "Buy", "price": 4000.0, "volume": 4.4, "id": 1, "sequence": 1}}}
```

撮合引擎在应用订单时会覆盖 `sequence`，因此客户端发送的值会被忽略；约定填 `0`。该字段仍然是
反序列化所必需的。

## 测试

`cargo test` 会构建一个集成测试目标（`tests/cluster`）以及库内单元测试。

| 测试 | 验证内容 |
|---|---|
| `tests/cluster/test_cluster.rs` | 3 节点集群：初始化、学习者、成员变更、读写、线性一致性读、领导者转发、缩容；默认走 gRPC，设置 `MATCHENGINE_TEST_TRANSPORT=http` 后走 HTTP |
| `tests/cluster/test_snapshot.rs` | 快照生成、日志 purge，以及学习者通过 gRPC 快照流安装已 purge 的状态 |
| `tests/cluster/test_storage.rs` | WAL 帧格式、撕裂尾部截断、分段轮转、跨段 truncate/purge、vote/committed 持久化、快照构建/安装/重开、损坏文件拒绝 |
| `tests/cluster/test_restart.rs` | 完整节点重启后通过日志重放恢复，以及通过快照 + 尾部恢复 KV、订单簿和序列号 |

所有集成测试都使用唯一的临时数据目录。集群测试固定使用端口 `21001-21003`（gRPC
`22001-22003`），快照测试使用 `21101-21102`，重启测试使用 `21201-21202`；重跑前请先停止残留进程
（`pkill -f matchengine-raft`）。

## 未来工作

- [x] 优化序列化 —— Raft RPC 在 protobuf 信封内使用 `postcard`（公共 API 仍为 JSON）
- [x] 使用 gRPC 改进网络层 —— tonic 传输，支持流式追加和分块快照
- [x] 添加全面测试 —— 存储、快照、集群与重启覆盖
- [ ] 按 `docs/TRANSPORTS.md` 构建 HTTP/JSON 与 gRPC 的基准测试工具
- [ ] 为 gRPC 监听器配置 TLS/mTLS
- [ ] 通过消息队列添加撮合结果分发
- [ ] 实现更多撮合算法（市价单、整数 tick 价格）
- [ ] 增强客户端库并添加端到端延迟基准

## 致谢

本项目是从 [OpenRaft](https://github.com/datafuselabs/openraft) 项目中的 [example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv) 分叉而来的。

特别感谢 Databend 社区和张延坡的支持。

## 许可证

请参考原始 OpenRaft 项目获取许可信息。

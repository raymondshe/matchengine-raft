```
███╗   ███╗ █████╗ ████████╗ ██████╗██╗  ██╗███████╗███╗   ██╗ ██████╗ ██╗███╗   ██╗███████╗    ██████╗  █████╗ ███████╗████████╗
████╗ ████║██╔══██╗╚══██╔══╝██╔════╝██║  ██║██╔════╝████╗  ██║██╔════╝ ██║████╗  ██║██╔════╝    ██╔══██╗██╔══██╗██╔════╝╚══██╔══╝
██╔████╔██║███████║   ██║   ██║     ███████║█████╗  ██╔██╗ ██║██║  ███╗██║██╔██╗ ██║█████╗      ██████╔╝███████║█████╗     ██║
██║╚██╔╝██║██╔══██║   ██║   ██║     ██╔══██║██╔══╝  ██║╚██╗██║██║   ██║██║██║╚██╗██║██╔══╝      ██╔══██╗██╔══██║██╔══╝     ██║
██║ ╚═╝ ██║██║  ██║   ██║   ╚██████╗██║  ██║███████╗██║ ╚████║╚██████╔╝██║██║ ╚████║███████╗    ██║  ██║██║  ██║██║        ██║
╚═╝     ╚═╝╚═╝  ╚═╝   ╚═╝    ╚═════╝╚═╝  ╚═╝╚══════╝╚═╝  ╚═══╝ ╚═════╝ ╚═╝╚═╝  ╚═══╝╚══════╝    ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝        ╚═╝
```

# Match Engine Raft

[![CI](https://github.com/raymondshe/matchengine-raft/actions/workflows/ci.yml/badge.svg)](https://github.com/raymondshe/matchengine-raft/actions/workflows/ci.yml)

这是一个基于 [OpenRaft](https://github.com/datafuselabs/openraft) 构建的分布式撮合引擎的实践实现。Raft 日志持久化在自研的分段预写日志（WAL）中，状态机通过原子快照落盘，因此节点重启后可以恢复全部已提交状态。

## 目录

- [功能特性](#功能特性)
- [架构概述](#架构概述)
- [快速开始](#快速开始)
- [运行集群](#运行集群)
- [安全与部署边界](#安全与部署边界)
- [项目结构](#项目结构)
- [存储实现](#存储实现)
- [集群管理](#集群管理)
- [API 参考](#api-参考)
- [测试](#测试)
- [性能与当前限制](#性能与当前限制)
- [联系方式](#联系方式)

## 功能特性

- **持久化 Raft 存储**: 带校验和的分段预写日志，每个复制批次一次 fsync；状态机通过原子文件快照持久化
- **崩溃恢复**: 重启时加载快照并重放保留的日志尾部，日志重放与快照两条恢复路径均有集成测试覆盖
- **gRPC 复制**: 基于 [tonic](https://docs.rs/tonic) 的传输层，支持双向 `stream_append`、分块快照传输，并为每个对端维护独立的复制/心跳/快照通道；可用 `--transport http` 切回 HTTP/JSON
- **撮合引擎**: 完整的订单簿实现，支持价格-时间优先、GTC/IOC/FOK/GTD/Day、Post-Only、冰山单、止损单、改单/撤单，以及按成交名义金额计算 maker/taker 费用并支持可配置扣费资产
- **HTTP 服务器**: 基于 [Actix-web](https://docs.rs/actix-web) 构建，同时提供内部 Raft API 和外部应用 API
- **多种读取一致性**: 本地读、经领导者的一致性读，以及线性一致性的 follower read
- **智能客户端**: Rust 客户端自动跟踪并将请求重定向到当前领导者
- **持久化撮合事件 Feed**: 复制且随快照持久化的事件日志，通过 `POST /events` 游标分页读取，支持保留上限和 `client_order_id` 幂等重试

## 架构概述

本项目扩展了 [example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv)，增加了持久化存储和 gRPC 传输：

```
                  ┌─────────────────────────────────────────────────────────┐
                  │                         应用层                          │
                  │                    (订单簿撮合引擎)                     │
                  │          apply ──► 订单簿 + 复制的 market_seq           │
                  │      MatchEvent ──► MatchResultSink（审计/清结算）      │
                  └─────────────────────┬───────────────────────────┬───────┘
                                        │                           │ MatchEvent + 深度日志
                                        │                           │ try_send（有界，不阻塞）
                                        │                           ▼
                                        │             ┌──────────────────────────────────────┐
                                        │             │       行情任务（按领导者发布）       │
                                        │             │    影子订单簿 · OHLC · 线格式 v2     │
                                        │             │      确定性 market_seq/sub_seq       │
                                        │             └──────────────────────────────────────┘
                                        │                                 │ MarketDataSink
                                        │                                 ▼
                                        │             ┌──────────────────────────────────────┐
                                        │             │  当前 JSONL · 后续 Aeron 2001–2007   │
                                        │             │        成交 BBO 深度 生命周期        │
                                        │             │               状态 K线               │
                                        ▼             └──────────────────────────────────────┘
                  ┌─────────────────────────────────────────────────────────┐
                  │                      OpenRaft 核心                      │
                  │                (共识、复制、领导者选举)                 │
                  └────────────────────────────┬────────────────────────────┘
                                               │
                                               │
               ┌───────────────────────────────┴───────────────────────────────┐
               │                               │                               │
               ▼                               ▼                               ▼
    ┌─────────────────────┐         ┌─────────────────────┐         ┌─────────────────────┐
    │     日志 + 状态     │         │     RaftNetwork     │         │      HTTP API       │
    │    (WAL + 快照)     │         │    (gRPC / HTTP)    │         │     (Actix-web)     │
    └─────────────────────┘         └─────────────────────┘         └─────────────────────┘
```

节点间 Raft RPC 默认使用 `端口 + 1000` 上的 gRPC。公共 API 与 HTTP Raft 处理器仍由
`--http-addr` 监听器提供，因此无论节点主动使用哪种传输，它都会同时服务两种协议。

行情输出是节点本地的，且不参与共识：tap 使用有界 `try_send`，因此慢订阅者永远不会阻塞
`apply`。JSONL sink 仅用于调试；同样的线格式 v2 消息就是后续 Aeron 适配器（2001–2007 流）
要发布的内容。

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

3. **撮合引擎** ([`order_book/`](./src/order_book/))
   - 价格-时间优先的订单簿（买单与卖单）
   - 订单类型：GTC、IOC、FOK、GTD、Day、Post-Only、冰山单、止损市价单、止损限价单
   - 确定性过期（`PurgeExpired` 通过日志携带时间戳）与基于复制的最后成交价触发止损
   - 现实世界状态机应用的示例
   - `OrderBook::execute_order` 是完整生命周期，`place_order` 是只返回成交的兼容包装

4. **行情数据出口** ([`market_data/`](./src/market_data/))
   - `apply` 将撮合事件和深度日志交给有界 tap（`try_send`，撮合路径永不等待）
   - 仅领导者发布的任务维护影子订单簿、生成 BBO/OHLC K 线并编码线格式 v2
   - 消息键为 `(symbol_id, stream_id, market_seq, sub_seq)`；每个 taker 一条不设上限的 `TRADE`
   - 当前使用 JSONL sink；下一阶段是 Aeron 2001–2007 流

## 快速开始

### 前置要求

- Rust 1.89 或更高版本（edition 2024；下限由 `actix-web` 和 `tonic` 决定）
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
./scripts/test-cluster.sh
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

## 安全与部署边界

HTTP API 与 gRPC Raft 监听端口（HTTP 端口 + 1000）**没有 TLS、mTLS 或认证**。这是明确的部署契约：
只能绑定回环地址或可信私有网络，并对两个端口设置防火墙；在传输安全和 API 认证实现之前，不得暴露到公网。

节点绑定非回环地址时会记录警告。请求在进入 Raft 之前就在边缘层受限：

| 入口 | 限制 |
|---|---|
| 应用/管理 JSON 请求体 | 1 MiB（`MAX_REQUEST_BODY_BYTES`） |
| `client_order_id` | 1–128 字节，仅限 ASCII 字母/数字及 `. _ - :`；拒绝空字符串 |
| `/read` 键 | 1–128 个可打印 ASCII 字节 |
| `/events` 分页大小 | 1–1000 个事件 |
| HTTP Raft append 请求体 / gRPC 消息 | 64 MiB（`MAX_HTTP_RAFT_BODY_BYTES` / `MAX_MESSAGE_SIZE`） |
| HTTP 快照请求体 | 256 MiB（`MAX_SNAPSHOT_BYTES`） |
| gRPC 快照流 | 总量 256 MiB，在追加每个 chunk 前检查 |

畸形 JSON 和非法字段返回 `400`；超大请求体返回 `413`。复制的 `client_order_id` 去重表最多保留
262,144 条记录，因此键长度限制也限制了该表的内存占用。

## 项目结构

```
src/
├── main.rs                  # 服务器入口（参数、环境变量、存储配置）
├── client.rs                # 支持领导者转发的智能客户端
├── lib.rs                   # 节点组装、服务与关闭
├── app.rs                   # 应用状态
├── order_book/
│   ├── mod.rs               # 订单簿撮合引擎
│   └── fixed.rs             # 精确定点数与标的元数据
├── market_data/
│   ├── mod.rs               # tap、深度日志、影子订单簿、限流发布任务
│   └── wire.rs              # 64 字节二进制头 + 消息体（可直接交给 Aeron）
├── match_result/
│   └── mod.rs               # MatchEvent id + 可替换的 MatchResultSink（noop/memory/file）
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
│   ├── state_machine.rs     # 状态机、事件日志 + 快照
│   ├── types.rs             # 请求/应答类型
│   └── config.rs            # 存储布局 / 每节点目录
proto/
└── raft.proto               # gRPC Raft 服务定义
examples/
└── sled.rs                  # 游乐场示例（本地 sled 目录，不安装）
tests/
└── cluster/
    ├── main.rs              # 单一集成测试目标
    ├── common.rs            # 唯一临时数据目录
    ├── test_cluster.rs      # 3 节点集群，两种传输
    ├── test_snapshot.rs     # 快照生成/purge、学习者安装 + 周期定时器
    ├── test_storage.rs      # WAL 与快照格式
    ├── test_restart.rs      # 重启恢复：日志重放、快照 + WAL 尾部
    ├── test_match_results.rs # 撮合事件写入 JSONL 文件 sink
    ├── test_market_data.rs    # 行情 feed：快照、成交、BBO、深度、K 线
    ├── test_events.rs         # 事件日志：写响应、游标、保留、幂等
    ├── test_membership.rs     # 领导者故障切换 + 学习者从活动日志追赶
    ├── test_late_join.rs      # 过期节点重新加入并从活动成员追赶
    ├── test_exactness.rs      # 18 位小数经 API、事件、行情和快照重启
    ├── test_symbols.rs        # 单标的配置、精度拒绝和配置锁定
    ├── test_fees.rs           # 撮合/吃单费用经 API、事件、行情和重启
    └── test_fee_assets.rs     # QQQ/USD tick/lot、quote 数量和 quote 扣费
```

## 存储实现

### 预写日志（`src/store/log_store.rs`）

Raft 日志追加写入 `log/NNNNNNNNNN.wal` 分段文件：

- 每条记录格式为 `[len: u32 LE][crc32: u32 LE][postcard 负载]`；启动时逐段扫描，遇到撕裂或校验失败的尾部直接截断，而不是当作有效记录解析。
- `append()` 用一次 `write_all` 写入整个 openraft 批次，再执行一次 `sync_data`，因此一次 AppendEntries RPC 中的 N 条记录只付一次 fsync 的代价。
- 读取使用 `pread`（`read_at`）和缓存的文件句柄，复制任务可以在追加进行时并发读取，磁盘 IO 期间不持有全局锁。
- 所有阻塞 IO 都放在 `spawn_blocking` 中执行，异步运行时只负责编解码。
- 分段默认 64 MiB；只有被 purge 完全覆盖的分段才会删除，删除前先轮转活跃分段，保证日志不出现空洞。
- 每个分段以 `MRFT` magic 和 `u32` 格式版本开头。`LogStore` 实现 `RaftLogStorage`
  （`get_log_state`、`read_vote`/`save_vote`、`read_committed`/`save_committed`、`append`、
  `truncate_after`、`purge`、`get_log_reader`）。

投票和最后一次 purge 的日志 id 属于安全关键且低频的状态，单独存放在 `log/state` 文件中，采用原子重写（临时文件 → fsync → rename → 目录 fsync）。committed 日志 id 追加到 WAL 尾部但不单独 fsync，随下一次 append 一起持久化，避免每批次多一次 fsync。

### 快照（`src/store/state_machine.rs`）

状态机（订单簿 + 序列号 + 保留的撮合事件窗口 + 提交去重表）常驻内存以保证 apply 速度。快照 v9 写入 `snapshot/current.snap`，通过临时文件 → fsync → rename → 目录 fsync 原子替换；`install_snapshot` 会先校验收到的数据再替换磁盘副本，坏快照不会破坏已有的好快照。

快照负载是 postcard 编码的 `StateMachineData`：

```rust
pub struct StateMachineData {
    pub orders: Vec<Order>,
    pub sequence: u64,
    pub stops: Vec<Order>,
    pub last_trade_price: Option<Fixed>,
    pub symbol_config: SymbolConfig, // the one symbol this process/cluster trades
    pub symbol_locked: bool,         // true after the first accepted order
    pub events: Vec<StoredMatchEvent>,      // 保留的撮合事件，按 EventId 排序
    pub events_evicted: u64,
    pub submissions: Vec<SubmissionRecord>, // client_order_id 去重记录
    pub submissions_evicted: u64,
}
```

`StoredMatchEvent` 是 `MatchEvent` 的快照镜像，总是写入两个可选字段；`MatchEvent` 在 JSON
中会省略空字段，而 postcard 是位置编码，不能省略。事件身份仍是 `(log_index, ordinal)`，不额外
存计数器。文件 framing 为：

```text
"MRFSNAP" | version u32 | crc32 u32 | postcard(SnapshotPayload)
```

### 每节点数据目录

每个节点写入 `<data-dir>/<instance-prefix>-<node-id>/{log,snapshot}`。可通过命令行配置：

```bash
./target/release/matchengine-raft --id 1 --http-addr 127.0.0.1:21001 \
    --data-dir /var/lib/matchengine-raft --instance-prefix prod
```

对应的环境变量为 `RAFT_DATA_DIR` 和 `RAFT_INSTANCE_PREFIX`，默认值分别是 `/tmp/matchengine-raft` 和 `matchengine`。

### 订单类型

订单是一个扁平的 `Order` 记录，每种可选类型都是它上面的一个字段。新增字段都带 serde 默认值，
因此旧格式的请求负载仍然可以反序列化。语义与
[OrderBook-rs](https://github.com/joaquinbejar/OrderBook-rs) 一致：

| 类型 | 字段 | 行为 |
|---|---|---|
| GTC（默认） | — | 一直挂单，直到成交或被撤销。 |
| IOC | `"tif": "IOC"` | 立即吃掉能成交的部分，剩余量被撤销，并以 `cancelled` 事件报告。 |
| FOK | `"tif": "FOK"` | 预检会汇总限价范围内可达的可见量与冰山隐藏量；不能全部成交时整单被拒绝（`rejected`，原因 `fok_unfillable`），订单簿保持不变。 |
| GTD | `"tif": {"GTD": 1700000000000}`（Unix 毫秒） | 挂单直到任一变更请求的入口时间达到截止时间（自动清除），或被显式 `PurgeExpired` 清除。 |
| Day | `"tif": "DAY"` | 当 `PurgeExpired` 带有 `day_close_ms` 且 `now_ms >= close` 时过期；未提供收盘时间时行为等同 GTC。 |
| Post-Only | `"post_only": true` | 从不吃单：整单挂入订单簿，若会成交则被拒绝（`post_only_would_cross`）。 |
| 冰山单 | `"display_volume": 100.0` | `volume` 是总量，只有一档可见量挂单；其余在 `hidden_volume` 中，对深度不可见，可见量被吃掉后逐档补足。 |
| 止损市价单 | `"live": "Market", "stop_price": 95.0` | 先停在订单簿之外，直到成交价触及止损价（卖单 `<=`，买单 `>=`），然后扫遍对手方所有价位；子单从不挂单，剩余量撤销。 |
| 止损限价单 | `"live": "Limit", "price": 96.0, "stop_price": 95.0` | 触发方式相同，子单受 `price` 限制；剩余量撤销。 |

市价单（`"live": "Market"`）忽略自身价格，扫遍对手方，剩余量撤销，从不挂单。

过期是确定性的：`apply` 从不读取时钟，因此截止时间随复制请求一起传递，每个副本清除完全相同的订单：

```json
{"PurgeExpired": {"now_ms": 1700000000000, "day_close_ms": null}}
```

过期是确定性的：`apply` 不读取时钟。每个可接受的变更请求（`Place`、`Cancel`、`Amend`）在应用前
都会以入口时间执行自动 GTD 清除，因此吃单不会成交已到期的流动性。`Day` 仍需要收盘时间，只能由
显式请求清除。`PurgeExpired` 自身消耗一个市场序列号，响应即该序列号；每个被清除的订单会产生
`expired` 事件。

止损单只会被**新的**成交触发：若最后成交价已经越过止损价，订单在进入时就被拒绝
（`stop_would_trigger`），而不会立刻变成市价单。一次成交可以级联触发多个止损单；被触发的止损按
准入顺序执行。

不支持的组合（止损单同时是 Post-Only 或冰山单、Post-Only 市价单、数量或价格非正、GTD 截止时间
不晚于订单自身的 `timestamp`）会被拒绝，并产生带原因的 `rejected` 事件。

### 订单修改

`Amend` 修改一笔挂单的限价和/或未成交数量。请求与 `Cancel` 一样通过 方向、当前价格和序列号
定位订单，新字段均可选：

```json
{"Amend": {"order": {"live": "Limit", "side": "Buy", "price": 4000.0, "volume": 4.4, "id": 1, "sequence": 7}, "price": 4002.0, "volume": 3.0}}
```

- 同价位减少数量会保留序列号，因此保留时间优先权；
- 修改价格或增加数量会以新序列号重新入簿（失去优先权），`/write` 响应返回之后使用的序列号；
- 新价格会与对手方成交的修改会被拒绝（`amend_would_cross`）：挂单簿永不交叉，吃单仍需先撤后下；
- 冰山单、停在场外的止损单和市价单不可修改（`unsupported_order`），请撤销后重新下单；
- 成功的修改会产生 `amended` 事件，同时携带旧、新订单序列号；`/write` 响应是本次修改的
  `market_seq`（重新入簿时也是新订单序列号）。被拒绝的修改仍会消耗一个序列号，并产生带原因的
  `rejected` 事件。

### 撮合结果处理

`apply` 会把每个 `MatchResult` 转换成 `MatchEvent`（`fill` / `rested` / `cancelled` / `triggered` /
`expired` / `rejected` / `amended`）。事件只由已应用的日志条目推导，因此每个副本产生相同的 id。
有界批次会随 `/write` 响应返回，并追加到复制且随快照持久化的事件日志；清结算与审计消费者通过游标
API 读取：

```
POST /write { Place | Cancel | Amend | PurgeExpired | ConfigureSymbol }
        │
        ▼
  StateMachineStore::apply  （每个副本都执行）
        │
        ├──► 订单簿 + 序列号 + 事件窗口 ──► 快照    [复制状态]
        │
        ├──► MatchEvent 批次 ──► /write 响应        [上限 512]
        │
        └──► POST /events + GET /events/status      [游标 + 保留策略]
                    │
                    ▼
          MatchResultSink                           [节点本地，尽力而为]
            ├── NoopSink      默认：无本地副作用
            └── FileLogSink   --match-event-log -> JSONL
```

`Place` 支持可选的 `client_order_id`；用同一个 id 重试会返回原序列号和事件，而不会重复下单。
事件窗口由 `--max-events`（环境变量 `MAX_EVENTS`，默认 262144）限制；提交去重表另有固定的
262144 条上限，避免重试结果依赖节点本地参数。两者都包含在快照 v9 中。
`GET /events/status` 报告最旧/最新保留 id 和淘汰计数；消费者从 `next_after` 继续，并按 `event_id`
去重。

文件 sink 仍只是调试工具：仅写本地文件，不参与复制，也不 fsync，重启后重放会把同一批事件再写一遍。
需要观察事件时启用：

```bash
./target/release/matchengine-raft --id 1 --http-addr 127.0.0.1:21001 \
    --match-event-log /tmp/node1-events.jsonl
```

每行一个事件，例如
`{"node_id":1,"event_id":{"log_index":42,"ordinal":0},"kind":"fill","order_sequence":7,"counterparty_sequence":3,"price":100.0,"volume":10.0}`。

复制事件日志是可靠的清结算 feed：它能在重启和学习者快照后恢复，`client_order_id` 会折叠重试。
消费者契约见 `docs/MATCH_RESULTS.md`。

游标示例：

```json
POST /events
{"after": {"log_index": 100, "ordinal": 3}, "limit": 500}
```

```json
{"Ok": {
  "events": [ ... ],
  "next_after": {"log_index": 140, "ordinal": 1},
  "oldest_event_id": {"log_index": 1, "ordinal": 0},
  "last_event_id": {"log_index": 140, "ordinal": 1},
  "has_more": true
}}
```

省略 `after` 时从最旧保留事件开始；`limit` 必须在 1–1000 之间。如果游标小于 `oldest_event_id`，
说明事件已被保留策略淘汰，清结算需要带外重建。`GET /events/status` 返回 `retained_events`、
`evictions_total`、`retention_limit`、`submission_retention_limit` 和
`submission_evictions_total`。

### 行情数据 feed（可选）

`--market-data-log <file>`（环境变量 `MARKET_DATA_LOG`）开启 `docs/AERON_SOLUTION.md` 第二部分
设计的确定性行情输出，实现在 `src/market_data/`：

- 撮合路径只追加它本来就知道的可见深度变化（临时日志），并把该条目的 `MatchEvent` 复制进有界队列；
  撮合路径上没有订单簿扫描、编码或 IO；
- 独立 tokio 任务维护影子订单簿并发布**确定性排序**的消息：同一 taker 的所有成交合并为一条 `TRADE`
  消息（不设上限）、每个改变订单簿的 entry 发一次 BBO（最新价/中间价/价差）、深度快照与批后增量、
  生命周期事件、OHLC K 线（默认 1 秒和 1 分钟），以及每 100 个市场序列号一次的 STATUS；
- 每条消息携带 `(symbol_id, stream_id, market_seq, sub_seq)`。`market_seq` 是复制的变更序列号
  （`Place` 使用 taker 订单序列号；`Cancel`、`Amend`、`PurgeExpired` 各消耗一个），因此该键在
  leader 与 follower、主节点切换和重启后都一致；下游按该四元组去重，`(log_index, ordinal)` 仍是
  审计位置；
- 默认只有领导者发布（`--market-data-leader-only=false` 时每个副本都发布）；每次获得领导权先发一次
  重置快照（边界为下一个 `market_seq`）。队列满时丢弃帧并发送 `GAP`，`apply` 永远不会被订阅者阻塞
  或拖慢。

当前 sink 是用于调试的 JSONL。同样的消息在 `src/market_data/wire.rs` 有二进制编码（64 字节头、
版本 4、scale-18 `i128`、小端消息体），`MarketDataSink` 就是后续 Aeron 适配器的接入点。每条消息携带
当前进程配置的 `symbol_id`，便于多个按标的部署共享总线。可调参数：`--market-data-depth`、`--market-data-status-every`、
`--market-data-bar-intervals-ms`、`--market-data-leader-only`。

`/read marketdata_status` 返回 feed 计数器和领导者门控状态。

### 快照配置

`default_raft_config()`（`src/lib.rs`）设置 `snapshot_policy = LogsSinceLast(100_000)`，节点还会运行
30 分钟快照定时器（`StorageConfig::snapshot_interval`，默认 `DEFAULT_SNAPSHOT_INTERVAL`）。只有上次快照后
有新 entry 被应用时才会请求快照，因此空闲节点不会重复写同一个文件。可用 `--snapshot-interval-ms`
（环境变量 `SNAPSHOT_INTERVAL_MS`）覆盖间隔；设为 `0` 会关闭定时器，但保留 100,000 条日志策略。
需要快速生成快照的测试会覆盖该策略：

```rust
let mut config = default_raft_config();
config.snapshot_policy = SnapshotPolicy::LogsSinceLast(10);
config.max_in_snapshot_log_to_keep = 0; // 清理快照已覆盖的全部日志
```

- 达到配置的已应用日志条数（默认 100,000 条）后构建快照；有新 entry 时也会每 30 分钟构建一次。
- 快照覆盖的日志加上 `max_in_snapshot_log_to_keep` 之后会被 purge。快照测试同时调小这两个值，
  使后加入的学习者无法只靠日志复制追上，必须安装快照。

### 数据恢复

启动时节点会：
1. 恢复 WAL 索引、投票和 purge 标记（并丢弃撕裂的尾部）。
2. 选择并加载 `snapshot/current.snap`（如果存在）。该文件原子替换，因此始终是最新的完整快照；
   崩溃只会留下不会被选中的 `.tmp` 文件。
3. 从快照元数据恢复状态机的 `last_applied`，再由 openraft 重放日志索引大于快照最后日志索引的
   保留 WAL 条目。

单节点集群因此可以恢复所有活动订单、序列号、保留事件窗口和幂等去重表，无论恢复路径是纯日志重放
还是快照 + 更新的 WAL 尾部。两条路径都由 `tests/cluster/test_restart.rs` 覆盖。

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
`InstallSnapshot`、`AppendStream`），定义见 `proto/raft.proto`。一元 append/vote 使用
`RPCOption::soft_ttl`，快照按 256 KiB 分块，双向消息上限均为 64 MiB；`Unavailable` 映射为
`RPCError::Unreachable`，超时映射为 `RPCError::Timeout`，其他状态映射为
`RPCError::Network`。

### 管理 API

- `POST /init` - 初始化集群；请求体为 `[]` 时只初始化当前节点，也可传
  `[[1, "127.0.0.1:21001"], ...]` 显式指定成员
- `POST /add-learner` - 添加学习者节点；请求体为 `[id, "host:port"]`
- `POST /change-membership` - 变更投票成员集合；请求体为 `[1, 2, 3]`
- `GET /metrics` - 获取 Raft 指标
- `POST /get_linearizer` - 从领导者获取读 linearizer（供 `/follower_read` 使用）

### 应用 API

- `POST /write` - 通过 Raft 提交 `Place`、`Cancel`、`Amend`、`PurgeExpired` 或 `ConfigureSymbol` 请求。
  订单/变更请求返回分配的 `market_seq`（`Place` 同时是订单序列号），并携带该条目的首批事件（上限 512）。
  `ConfigureSymbol` 返回状态字符串，拒绝时使用 `error` 字段，不消耗市场序列号。`Place` 支持可选的
  `client_order_id`，用于幂等重试。
- `POST /read` - 从本地状态机读取（可能读到旧值）
- `POST /linearizable_read` - 线性一致性读；请求必须到达领导者
- `POST /follower_read` - 从领导者获取 linearizer 后在本地完成的线性一致性读
- `POST /events` - 分页读取复制的撮合事件日志（`after`、`limit`）；线性一致，仅领导者
- `GET /events/status` - 保留事件窗口、淘汰计数和保留上限

`/write` 背后的 Rust 类型：

```rust
pub enum MatchEngineRequest {
    Place { order: Order, client_order_id: Option<String> },
    Cancel { order: Order },
    Amend { order: Order, price: Option<Fixed>, volume: Option<Fixed> },
    PurgeExpired { now_ms: u64, day_close_ms: Option<u64> },
    ConfigureSymbol { config: SymbolConfig },
}

pub struct MatchEngineResponse {
    pub value: Option<String>,        // 兼容旧调用：序列号字符串
    pub sequence: Option<u64>,        // 分配的 market_seq
    pub events: Vec<MatchEvent>,      // 本条目的前 512 个事件
    pub events_truncated: bool,       // 还有更多事件时为 true
    pub error: Option<String>,        // 非致命的应用层错误
}
```

每个订单/变更请求消耗一个 `market_seq`；`Place` 同时把它作为订单序列号，`ConfigureSymbol` 不消耗。
响应最多携带 512 个事件，完整批次从 `/events` 读取。成交事件携带 `quote_volume`、`volume_unit`、
`taker_fee`/`taker_fee_asset` 和 `maker_fee`/`maker_fee_asset`；行情 `TRADE` 与成交 `LIFECYCLE`
记录也包含相同字段（wire v5）。`client_order_id` 可选，只影响 `Place`：同一 id 重试返回原序列号和
事件，不会重复下单。长度必须为 1–128 个可打印 ASCII 字节，字符集为 `[A-Za-z0-9._:-]`；空字符串会被拒绝，
不需要幂等时请省略该字段。

`/read`、`/linearizable_read` 和 `/follower_read` 接收 JSON 字符串键，用于读取订单簿的命名视图：

- `orderbook_orders` - 订单簿中所有挂单的 JSON 数组
- `orderbook_stops` - 停在订单簿之外的止损单（按准入顺序）
- `orderbook_last_trade_price` - 最后成交价（首笔成交前为 `null`），用于触发止损
- `orderbook_depth` - 买卖两侧前 10 档的聚合可见深度（`orderbook_depth:N` 可指定档数，上限 200）
- `orderbook_spread` - 最优卖价减最优买价（任一侧为空时为 `null`）
- `marketdata_status` - 行情 feed 计数器与门控状态（feed 关闭时为 `{}`）
- `orderbook_sequence` - 订单序列号计数器（兼容旧拼写 `orderbook_sequance`）
- `symbol_config` - 当前进程/集群复制的标的与精度配置
- `symbol_id` - 当前进程/集群唯一支持的标的

价格和数量在内部使用精确的 18 位定点整数。JSON 接受十进制字符串或旧版数字；撮合事件、`/read` 和行情输出十进制字符串，以精确往返 18 位小数。一个进程/集群只交易一个标的：`SymbolConfig` 随 Raft 复制并写入快照，包含 base/quote `AssetId`/`AssetClass`、价格 tick、base lot 和扣费资产策略；超过精度或不符合最小变动单位的订单会被拒绝，`symbol_id` 不匹配的订单也会被拒绝。扩展多标的方式是每个标的运行独立的进程/集群，从而隔离资源与故障；内部统一归一化到 18 位。

`/write` 接受的订单示例（注意字段名是 `sequence`）：

```json
{"Place": {"order": {"live": "Limit", "side": "Buy", "volume_unit": "base", "price": "4000.25", "volume": "4.4", "maker_fee_rate": "0.001", "taker_fee_rate": "0.002", "id": 1, "sequence": 1}, "client_order_id": "order-123"}}
```

`client_order_id` 可选；省略时保持旧的非幂等行为。`volume_unit` 默认是 `base`；设为 `quote` 时
`volume` 是毛 quote 金额（限价单按限价归一化为 base，市价单使用 quote 预算/目标扫描）。

交易前设置当前进程的标的、资产和允许精度：

```json
{"ConfigureSymbol": {"config": {
  "symbol_id": 0,
  "base_asset": {"code": "QQQ", "class": "etf", "decimals": 4},
  "quote_asset": {"code": "USD", "class": "fiat", "decimals": 2},
  "price_decimals": 2,
  "price_tick": "0.01",
  "base_lot": "0.0001",
  "fee_asset_policy": "received"
}}}
```

配置会随 Raft 复制，并在首个订单被接受后锁定。有挂单或已经开始交易后修改配置会被拒绝。在正在发布行情的节点上，成功修改还会为新标的排队一个重置快照。该非致命错误通过响应的可选 `error` 字段返回，写入本身仍会提交。

费率字段（`maker_fee_rate`、`taker_fee_rate`）是网关按用户等级带入的有符号 `Fixed` 小数：
`"0.001"` 表示 10 bps，负值表示返佣，缺省为零。每笔成交的名义金额由 maker 价格 × 成交数量计算，
吃单方按吃单费率收费，挂单方按挂单费率收费；费用按 18 位小数向零截断。名义金额或费用超出支持的定点范围时
`fee_overflow` 置位。引擎只记录费用，不维护余额。

法币、稳定币、加密货币、ETF 和股票等标的的 base/quote 数量语义与按资产扣费已实现。
扣费资产可选择收到资产（默认）、quote 或 base；完整模型和未完成扩展（交易时段、公司行为、
固定/监管费用、fee token）见 [`docs/FEE_DEDUCTION.md`](./docs/FEE_DEDUCTION.md)。

其他订单类型字段（`tif`、`post_only`、`stop_price`、`display_volume`、`hidden_volume`、
`timestamp`）都是可选的，默认就是普通的 GTC 限价单：

```json
{"Place": {"order": {"live": "Limit", "side": "Sell", "price": 4000.0, "volume": 100.0, "tif": "IOC", "display_volume": 10.0}}}
```

撮合引擎在应用订单时会覆盖 `sequence`，接收请求的领导者还会在入口处覆盖 `timestamp`，因此客户端
发送的这两个值都会被忽略（约定填 `0`）。`sequence` 仍然是反序列化所必需的。

## 测试

`cargo test` 会构建一个集成测试目标（`tests/cluster`）以及库内单元测试。

| 测试 | 验证内容 |
|---|---|
| `src/order_book/property_tests.rs` | 针对参考模型的随机差分测试，并混合覆盖 GTC/IOC/FOK/GTD/Day、市价、Post-Only、冰山和止损单，以及正费率、零费率和返佣费率下的全部扣费资产策略：可见订单簿、成交、优先级、无交叉盘、数量守恒、序列号/最后成交价状态和确定性重放 |
| `src/order_book/conformance_tests.rs` | 市价 FOK、无流动性市价单、冰山隐藏量与撤单/过期、挂起止损的 GTD/Day 过期，以及市价/IOC/止损子单成交的费用报告 |
| `tests/cluster/test_cluster.rs` | 3 节点集群：初始化、学习者、成员变更、读写、线性一致性读、领导者转发、缩容；默认走 gRPC，设置 `MATCHENGINE_TEST_TRANSPORT=http` 后走 HTTP |
| `tests/cluster/test_snapshot.rs` | 快照生成、日志 purge、学习者通过 gRPC 快照流安装已 purge 的状态，以及关闭日志计数策略时的周期快照定时器 |
| `tests/cluster/test_storage.rs` | WAL 帧格式、撕裂尾部截断、分段轮转、2 万条跨段日志重开、跨段 truncate/purge、vote/committed 持久化、快照构建/安装/重开、损坏文件拒绝 |
| `tests/cluster/test_restart.rs` | 完整节点重启后通过日志重放恢复，以及通过快照 + 更新的 WAL 尾部恢复订单簿、序列号、事件窗口和幂等表；另含 2000 单大日志重放和多分块（约 120 KB）快照启动 |
| `tests/cluster/test_match_results.rs` | 以 `--match-event-log` 启动的节点按日志顺序记录 rest/fill/cancel 事件，同时 `/write` 响应保持不变；第二个节点验证止损触发、GTD 过期和新增的 `/read` 键 |
| `tests/cluster/test_market_data.rs` | 以 `--market-data-log` 启动的节点发布重置快照、每个 taker 一条成交消息、BBO、批后深度总量和一根已收盘 K 线；断言同时检查 `market_seq` 锚点与 `/read marketdata_status` 计数器 |
| `tests/cluster/test_events.rs` | `/write` 返回事件、`/events` 游标分页、`client_order_id` 重试不重复下单、`--max-events` 淘汰与 `/events/status` 计数 |
| `tests/cluster/test_membership.rs` | 杀掉当前领导者：存活的投票节点接管已提交订单簿并接受新写入；写入后加入的学习者通过普通日志复制追赶、晋升成员并从自己的节点写入 |
| `tests/cluster/test_late_join.rs` | 带过期快照/WAL 的节点被移出集群、错过 40 次写入（期间领导者生成快照并 purge），再以学习者身份重新加入并从活动集群追赶 |
| `tests/cluster/test_exactness.rs` | 18 位小数的价格/数量经 `/write`、`/read`、`/events`、JSONL 行情、快照 v9 和重启后仍然精确，全程不经过 `f64` |
| `tests/cluster/test_symbols.rs` | `ConfigureSymbol` 设置进程标的/精度，拒绝超精度和跨标的订单，交易开始后锁定配置，并在快照重启后保留配置 |
| `tests/cluster/test_fees.rs` | 吃单/挂单费率产生精确的成交费用，经 `/write`、`/events`、文件 sink 和行情 feed 输出，并在快照重启后保留 |
| `tests/cluster/test_fee_assets.rs` | QQQ/USD ETF/法币元数据、tick/lot、quote 数量限价单归一化和 quote 资产扣费 |
| `tests/cluster/test_request_limits.rs` | 超大 JSON 请求体、畸形输入、非法/超长 `client_order_id`、超长 read key 和越界事件分页在进入 Raft 前以 `400`/`413` 拒绝 |

所有集成测试都使用唯一的临时数据目录。集群测试固定使用端口 `21001-21003`（gRPC
`22001-22003`），快照测试使用 `21101-21103`，重启测试使用 `21201-21205`，撮合结果测试使用
`21301`/`21302`、行情数据测试 `21303`、事件日志测试 `21304`、故障切换测试
`21401-21403`、学习者追赶测试 `21501-21502`、过期节点重入测试 `21601-21603`、精度往返测试 `21701`（gRPC `22701`）、标的配置测试 `21801`（gRPC `22801`）、费用测试 `21901`（gRPC `22901`）、费用资产测试 `21902`（gRPC `22902`）、请求限制测试 `21903`（gRPC `22903`）；重跑前请先停止残留进程
（`pkill -f matchengine-raft`）。

## 性能与当前限制

已实现的性能设计：

- 每个复制追加批次一次 `write_all` + 一次 `sync_data`，一次 AppendEntries 中的 N 条记录只付一次 fsync。
- `pread` 读取配缓存文件句柄，追加与复制读取互不阻塞。
- 订单簿、事件窗口和去重表常驻内存，`apply` 不做磁盘 IO。
- gRPC 使用双向 `AppendStream` 流水线复制，并为每个对端维护独立的复制、心跳、快照连接。
- 快照按 256 KiB 分块传输；`log/state` 和 `snapshot/current.snap` 都原子替换。
- 事件窗口由 `--max-events` 限制并通过 `/events` 分页；`/read orderbook_orders` 不会复制事件窗口。

仍缺少：

- 没有传输基准工具，`docs/TRANSPORTS.md` 中的数字只是指导值，不是实测结果。
- gRPC 监听器尚未配置 TLS/mTLS。
- 快照安装仍整体缓冲在内存中。
- `Order` 的价格/数量、`last_trade_price`、撮合事件、深度/BBO/spread、JSONL 行情和 wire v5 均使用精确的 18 位定点整数。快照 v9 和 wire v5 是相对旧版本的破坏性变更；每个进程/集群只支持一个标的，`SymbolConfig` 已复制并在交易开始后锁定。多标的通过独立进程/集群隔离。18 位 BTC 分析、微基准和迁移记录见
  [`docs/DECIMAL.md`](./docs/DECIMAL.md)。

已知差距与路线图统一记录在 [`TODO.md`](./TODO.md)。

## 联系方式

- Google Chat: [rshen.sy@gmail.com](https://chat.google.com/u/0/dm/rshen.sy@gmail.com)

## 致谢

本项目是从 [OpenRaft](https://github.com/datafuselabs/openraft) 项目中的 [example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv) 分叉而来的。

特别感谢 Databend 社区和张延坡的支持。

## 许可证

MIT —— 见 [`LICENSE`](./LICENSE)。

本项目最初分叉自 [OpenRaft](https://github.com/datafuselabs/openraft) 项目中的
[example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv)，上游采用
MIT 或 Apache-2.0 双许可。源自上游的代码继续适用其原始版权与许可声明；本仓库以 `LICENSE`
中的 MIT 许可证分发。

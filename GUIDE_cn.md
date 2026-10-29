# OpenRaft 实践分享（当前实现版）

本文最初记录了基于 openraft 早期版本、使用 sled 保存日志和状态机的实现。当前分支已经完成
openraft 0.10 迁移，并改为**自研分段 WAL + 原子快照**的持久化方案，节点间 Raft RPC 默认使用
**gRPC**。本文已按现在的代码重写；如果和代码不一致，以 `src/` 为准。

- 代码仓库：[raymondshe/matchengine-raft](https://github.com/raymondshe/matchengine-raft)
- 依赖版本：`openraft` 0.10.0-alpha.37、`tonic` 0.14、`actix-web` 4.x
- 运行环境：Rust 1.85+（edition 2024）

## 1. 应用场景

撮合引擎对延迟和顺序都很敏感：

- 所有会改变订单簿的命令都必须经过 Raft 复制，并在每个节点按相同顺序应用。
- 状态机本身尽量留在内存里，apply 时不做磁盘随机写。
- 日志必须真正落盘，节点重启后能够从快照 + 日志尾部恢复。
- 节点间复制要能流水线发送，快照传输不能阻塞投票。

因此本项目的分工是：

| 数据 | 存放方式 | 目的 |
|---|---|---|
| Raft 日志 | 分段预写日志 `log/*.wal` | 持久化、顺序追加、一次批次一次 fsync |
| vote / last_purged | `log/state`（原子重写） | 安全关键但低频 |
| 状态机 | 内存（订单簿 + 序列号） | apply 快 |
| 快照 | `snapshot/current.snap`（原子替换） | 启动恢复和追赶落后节点 |

## 2. 整体结构

```
应用层（订单簿撮合 + MatchResultSink）
        │
OpenRaft Core（共识、复制、选举）
        │
┌───────┼────────────────┬──────────────────┐
│       │                │                  │
LogStore  StateMachineStore  RaftNetwork        HTTP API
(WAL)     (内存 + 快照)      (gRPC / HTTP)       (actix-web)
```

节点由 `build_match_engine_node()`（`src/lib.rs`）组装：

1. 打开 `LogStore` 和 `StateMachineStore`。
2. 创建 `Raft` 实例。
3. 绑定 gRPC 监听端口并启动 tonic 服务。
4. 启动 actix-web HTTP 服务，注册 Raft、管理、应用路由。
5. 返回 `MatchEngineNode`；调用 `shutdown()` 可以干净地停止 HTTP、gRPC 和 Raft，之后再重开同一
   存储目录（重启测试依赖这一点）。

## 3. 请求与应答类型

应用命令定义在 `src/store/types.rs`：

```rust
pub enum MatchEngineRequest {
    Place { order: Order },
    Cancel { order: Order },
    // 修改挂单的限价/数量；order 用 方向+当前价格+序列号 定位；消耗一个 market_seq
    Amend { order: Order, price: Option<f64>, volume: Option<f64> },
    // 显式过期清除（Day/管理用）；GTD 会由每次变更请求自动清除
    PurgeExpired { now_ms: u64, day_close_ms: Option<u64> },
}

pub struct MatchEngineResponse {
    pub value: Option<String>,
}
```

`Order`（`src/matchengine/mod.rs`）的核心字段是 `live`（Limit/Market）、`side`（Buy/Sell）、
`price`、`volume`、`id` 和 `sequence`；`sequence` 在反序列化时是必需的，但 `place_order()` 会用
订单簿自己的计数器覆盖它。HTTP 入口的领导者还会覆盖 `timestamp`：它既是 GTD 校验/自动清除的
触发时间，也是行情记录的时间；`Place`/`Cancel`/`Amend`/`PurgeExpired` 的响应都返回分配的
`market_seq`。`OrderBook::amend` 修改普通挂单：同价位减量保留序列号（时间优先权），改价或增量会以新序列号
重新入簿；会穿越对手方的修改被拒绝。冰山单、止损单和市价单请先撤销再下单。订单类型通过带
serde 默认值的附加字段表达：`tif`（GTC/IOC/FOK/
GTD(毫秒)/Day）、`post_only`、`stop_price`（`0.0` 表示非止损单）、`display_volume`（冰山单可见
档位，`0.0` 表示非冰山单）、`hidden_volume`（冰山隐藏量）和 `timestamp`（入口时间，用于 GTD
校验与自动清除）。止损单在触发前停在订单簿之外；成交价触及止损价后作为立即执行的子单（剩余量撤销）。

## 4. 存储实现

### 4.1 分段 WAL（`src/store/log_store.rs`）

日志按段写入 `log/NNNNNNNNNN.wal`：

- 每个段文件开头是 8 字节头：`MRFT` + 版本号（u32 LE）。
- 每条记录是 `[len: u32 LE][crc32: u32 LE][postcard 负载]`。
- `append()` 把 openraft 交来的一整批记录拼进一次 `write_all`，然后只做一次 `sync_data`。
  因此一次 AppendEntries 携带 N 条日志时只付一次 fsync。
- 读取使用 `pread`（`read_at`）和缓存的文件句柄，复制任务可以在追加进行时并发读取；磁盘 IO
  期间不持有全局锁。
- 所有阻塞 IO 都通过 `spawn_blocking` 执行，异步运行时只做编解码。
- 默认段大小 64 MiB。只有被 purge 完全覆盖的段才会删除；删除前先轮转活跃段，保证日志不会出现
  空洞。
- 启动时逐段扫描。遇到长度非法、CRC 不匹配或文件尾部不完整的记录，就把它当作撕裂尾部截断，
  而不是继续解析。

`LogStore` 实现 `RaftLogStorage`，包括 `get_log_state`、`read_vote`/`save_vote`、
`read_committed`/`save_committed`、`append`、`truncate_after`、`purge` 和 `get_log_reader`。
`committed` 作为普通 WAL 记录追加，不单独 fsync，由下一次 append 一并持久化。

### 4.2 vote 与 purge 标记

投票和 `last_purged` 数量少但关系正确性，单独放在 `log/state`：

- 写入方式：临时文件 → `fsync` → `rename` → 目录 `fsync`。
- 这样 crash 后只会看到旧的完整文件或新的完整文件，不会看到半个文件。

### 4.3 状态机与快照（`src/store/state_machine.rs`）

状态机常驻内存：

```rust
pub struct StateMachineData {
    pub orders: Vec<Order>,              // 挂单（先买后卖）
    pub sequence: u64,                   // 订单序列号
    pub stops: Vec<Order>,               // 停在场外的止损单（按准入顺序）
    pub last_trade_price: Option<f64>,   // 最后成交价，用于触发止损
}
```

`StateMachineStore` 另存 `last_applied_log`、`last_membership` 和当前快照，并实现
`RaftStateMachine`（`applied_state`、`apply`、`get_current_snapshot`、`install_snapshot`、
`get_snapshot_builder`）；快照生成通过 `RaftSnapshotBuilder::build_snapshot` 完成。
`begin_receiving_snapshot` 使用 trait 默认实现。

快照文件 `snapshot/current.snap` 的格式：

```
"MRFSNAP" | version u32 | crc32 u32 | postcard(SnapshotPayload)
```

`SnapshotPayload` 包含 `SnapshotMeta` 和 postcard 编码的 `StateMachineData`。快照版本为 4：
除订单簿、序列号、止损单和最后成交价外，还包含保留的撮合事件窗口和 `client_order_id` 去重表。
写盘流程是
临时文件 → `fsync` → `rename` → 目录 `fsync`；`install_snapshot` 会先校验 magic、版本、CRC 和
内容，再替换磁盘上的副本，坏快照不会破坏已有的好快照。启动时如果快照损坏，节点会直接报错，
而不是静默从空状态启动。

### 4.4 数据目录

每个节点使用独立目录：

```
<data_dir>/<instance_prefix>-<node_id>/
  log/
    state
    0000000000.wal
  snapshot/
    current.snap
```

由 `StorageConfig`（`src/store/config.rs`）计算，命令行参数是 `--data-dir` 和
`--instance-prefix`，对应环境变量 `RAFT_DATA_DIR` 和 `RAFT_INSTANCE_PREFIX`。默认值分别是
`/tmp/matchengine-raft` 和 `matchengine`。

### 4.5 恢复流程

节点启动时：

1. 扫描 WAL，重建索引、读取 vote 和 `last_purged`，截断撕裂尾部。
2. 如果存在 `snapshot/current.snap`，加载它并重建订单簿和序列号。
3. openraft 在恢复出的状态之上重放保留的日志尾部。

因此单节点重启后可以恢复所有活动订单和序列号。纯日志重放和“快照 + 尾部”两条
路径都由 `tests/cluster/test_restart.rs` 覆盖。

### 4.6 快照与日志保留

`default_raft_config()` 只覆盖心跳和选举时间，快照策略使用 openraft 默认值
（`LogsSinceLast(5000)`，`max_in_snapshot_log_to_keep = 1000`）。测试会按需调小：

```rust
let mut config = default_raft_config();
config.snapshot_policy = SnapshotPolicy::LogsSinceLast(10);
config.max_in_snapshot_log_to_keep = 0; // 快照覆盖的日志全部 purge
```

`tests/cluster/test_snapshot.rs` 依赖这组配置：学习者必须在日志被 purge 之后才加入，否则它可
通过普通日志复制追上，测试就证明不了快照安装路径。

## 5. 网络层

### 5.1 两种传输

`src/network/transport.rs` 在运行时选择出站 Raft RPC 的传输：

- `Transport::Grpc`（默认）：`src/network/grpc/`，protobuf 信封 + postcard 负载。
- `Transport::Http`：`src/network/raft_network_impl.rs`，HTTP/JSON。

不管选哪种，节点都会同时提供：

- 公共 HTTP API 和 HTTP Raft 端点（`/append`、`/vote`、`/snapshot`）；
- gRPC Raft 服务（`AppendEntries`、`Vote`、`InstallSnapshot`、`AppendStream`）。

### 5.2 端口约定

`BasicNode.addr` 始终保存公共 HTTP 地址，Raft 传输端口由
`raft_addr_from_http(addr)` 推导：同一主机，端口 + `RAFT_PORT_OFFSET`（1000）。

- `127.0.0.1:21001` → gRPC `127.0.0.1:22001`
- 客户端转发仍然使用 `addr`，不需要第二个地址字段。

### 5.3 gRPC 细节

- 普通 append/vote 使用一元 RPC，并遵循 `RPCOption::soft_ttl()`。
- `stream_append` 使用双向流 `AppendStream`， follower 直接把请求喂给 `Raft::stream_append`，
  实现流水线复制而不是一问一答。
- 快照使用客户端流式 `InstallSnapshot`，每块 256 KiB。
- 每个对端分别持有复制、心跳、快照三条连接，快照不会阻塞投票。
- 双向消息上限都是 64 MiB；快照总大小不受单条消息限制。
- 错误映射：`Unavailable` → `RPCError::Unreachable`，超时/`DeadlineExceeded` →
  `RPCError::Timeout`，其他状态 → `RPCError::Network`；Raft 层拒绝以编码后的 `RaftError` 返回。

HTTP 传输保留为兼容和对照实现；它的 `stream_append` 仍然是顺序发送。

## 6. 启动集群

### 6.1 启动节点

```bash
cargo build

./target/debug/matchengine-raft --id 1 --http-addr 127.0.0.1:21001
./target/debug/matchengine-raft --id 2 --http-addr 127.0.0.1:21002
./target/debug/matchengine-raft --id 3 --http-addr 127.0.0.1:21003
```

默认使用 gRPC 连接对端；加 `--transport http` 可切回 HTTP/JSON。所有状态默认写到
`/tmp/matchengine-raft/matchengine-<id>/`。

### 6.2 初始化

```bash
curl -X POST -H "Content-Type: application/json" \
  -d '[]' \
  http://127.0.0.1:21001/init
```

`/init` 接收 JSON 数组：`[]` 表示把当前节点初始化为单节点集群。直接 POST 或 `{}` 会反序列化
失败（"invalid type: map, expected a sequence"）。

### 6.3 添加学习者和变更成员

```bash
curl -X POST -H "Content-Type: application/json" \
  -d '[2, "127.0.0.1:21002"]' \
  http://127.0.0.1:21001/add-learner

curl -X POST -H "Content-Type: application/json" \
  -d '[3, "127.0.0.1:21003"]' \
  http://127.0.0.1:21001/add-learner

curl -X POST -H "Content-Type: application/json" \
  -d '[1, 2, 3]' \
  http://127.0.0.1:21001/change-membership
```

成员关系、vote 和日志都会持久化，所以集群一旦初始化，节点重启后不需要重新 init；直接启动即可
重新加入。

### 6.4 脚本

- `./test-cluster.sh`：启动 3 节点、初始化、添加 learner、变更成员、读写，最后杀掉进程。使用
  `/tmp/matchengine-raft-test-cluster`，未显式设置 `RAFT_DATA_DIR` 时每次启动前清空，便于重复运行。
- `./open-cluster.sh`：同样启动并初始化 3 节点，但保持运行，便于手工调试；使用
  `/tmp/matchengine-raft-open-cluster`。
- `./test.sh <command> [node]`：日常手工操作，支持 `start-node`、`build-cluster`、
  `change-membership`、`metrics`、`place-order`、`get-seq`、`kill-node`、`kill`、`clean`。

`test.sh clean` 会把整个 `RAFT_DATA_DIR` 重命名为带时间戳的目录，再创建空目录；它不会删除数据。

## 7. 读取路径

| 端点 | 一致性 | 说明 |
|---|---|---|
| `POST /read` | 本地读，可能过期 | 直接读本地状态机 |
| `POST /linearizable_read` | 线性一致 | 必须到达领导者，先获取 linearizer 再等待状态机追上 |
| `POST /follower_read` | 线性一致 | follower 向领导者请求 linearizer，本地等待后读取 |
| `POST /events` | 线性一致 | 按 `(log_index, ordinal)` 游标分页读取复制的撮合事件日志 |
| `GET /events/status` | 本地读 | 保留事件窗口、淘汰计数和保留上限 |

三个端点都接收 JSON 字符串键，用于读取订单簿的命名视图：

- `orderbook_orders`：订单簿中所有挂单的 JSON 数组；
- `orderbook_stops`：停在场外的止损单；
- `orderbook_last_trade_price`：最后成交价（首笔成交前为 `null`）；
- `orderbook_depth`：买卖两侧前 10 档聚合深度（`orderbook_depth:N` 指定档数，上限 200）；
- `orderbook_spread`：最优卖价减最优买价；
- `marketdata_status`：行情 feed 计数器与发布门控（未启用行情时为 `{}`）；
- `orderbook_sequence`：订单序列号（兼容旧拼写 `orderbook_sequance`）。

### 行情数据输出（可选）

`--market-data-log <file>`（环境变量 `MARKET_DATA_LOG`）开启 `docs/AERON_SOLUTION.md` 第二部分设计、
`src/market_data/` 实现的确定性行情输出。撮合线程只把本来就知道的可见深度变化写入临时日志，并把
`MatchEvent` 复制进有界队列；独立线程维护影子订单簿并发布按 (symbol, stream, market_seq, sub_seq)
确定性排序的成交、BBO、深度快照/增量、生命周期、OHLC K 线和状态。同一 taker 的全部成交在一条
`TRADE` 中；`market_seq` 是复制的变更序列号（`Cancel`/`Amend`/`PurgeExpired` 也会消耗一个），
因此键在 leader 与 follower、切换和重启后一致。默认只有领导者发布（`--market-data-leader-only=false`
时每个副本都发布），每次获得领导权先发重置快照；队列满时丢弃帧并发送 `GAP`，`apply` 不会被阻塞。当前 sink 为调试用 JSONL，
`src/market_data/wire.rs` 是可直接交给 Aeron 的二进制编码（v2）。

## 8. 测试

`cargo test` 运行库内单元测试和 `tests/cluster` 集成目标：

| 文件 | 覆盖内容 |
|---|---|
| `tests/cluster/test_cluster.rs` | 3 节点集群全流程，默认 gRPC；`MATCHENGINE_TEST_TRANSPORT=http` 时走 HTTP |
| `tests/cluster/test_snapshot.rs` | 快照生成、日志 purge、学习者通过 gRPC 安装快照 |
| `tests/cluster/test_storage.rs` | WAL 帧、撕裂尾部、段轮转、跨段 truncate/purge、vote/committed、快照构建/安装/重开、损坏检测 |
| `tests/cluster/test_restart.rs` | 完整节点重启：日志重放恢复和快照 + 尾部恢复 |
| `tests/cluster/test_events.rs` | `/write` 事件、`/events` 游标、`client_order_id` 重试、保留窗口 |

每个集成测试使用唯一的临时数据目录。测试端口固定：集群测试 `21001-21003`（gRPC
`22001-22003`），快照测试 `21101-21102`，重启测试 `21201-21202`。重跑前先清理残留进程：

```bash
pkill -f matchengine-raft
```

## 9. 性能考虑

已做的优化：

- 一批日志一次 `write_all` + 一次 `sync_data`，避免逐条 fsync。
- `pread` 并发读，追加与复制读取互不阻塞。
- 状态机留在内存，apply 不做磁盘 IO。
- gRPC 双向流复制，减少 RTT 往返。
- 快照分块传输，并与投票/追加使用独立连接。
- 快照和 `log/state` 原子替换，崩溃后不需要额外修复流程。

尚未完成：

- 没有 HTTP/JSON 与 gRPC 的基准测试工具，`docs/TRANSPORTS.md` 中的数字只是预期，不是实测。
- gRPC 监听器尚未配置 TLS/mTLS。
- 快照仍然整体缓冲在内存中；更大的订单簿需要流式安装。

## 10. 未来工作

1. 构建传输基准：L1 RPC 微基准 + L2 三节点提交延迟，记录 HTTP/JSON 与 gRPC 的对比。
2. 为 gRPC 增加 TLS/mTLS。
3. 实现 openraft 的 `pre_vote` / `transfer_leader` RPC（当前使用 trait 默认实现）。
4. 撮合结果分发（消息队列）和更多撮合算法（市价单、整数 tick 价格）。
5. 完善客户端库和端到端压测。

## 11. 结论

当前实现已经具备：

1. 一个可运行的 Raft 撮合引擎示例；
2. 真正持久化的 Raft 日志（分段 WAL）和状态机快照；
3. 节点重启后的完整恢复能力，并有集成测试覆盖；
4. 默认 gRPC、可切换 HTTP 的传输层，以及流式追加和分块快照。

它仍然是一个示例工程，不是生产级交易所；上生产前至少需要补齐 TLS、基准测试、监控和更严格的
撮合/金额表示。

感谢 Databend 社区和 openraft 作者 Zhang Yanpo 的支持。

# 存储

## 预写日志（`src/store/log_store.rs`）

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

## 快照（`src/store/state_machine.rs`）

状态机（订单簿 + 序列号 + 保留的撮合事件窗口 + 提交去重表）常驻内存以保证 apply 速度。快照 v10 写入 `snapshot/current.snap`，通过临时文件 → fsync → rename → 目录 fsync 原子替换；`install_snapshot` 会先校验收到的数据再替换磁盘副本，坏快照不会破坏已有的好快照。

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

## 每节点数据目录

每个节点写入 `<data-dir>/<instance-prefix>-<node-id>/{log,snapshot}`。可通过命令行配置：

```bash
./target/release/matchengine-raft --id 1 --http-addr 127.0.0.1:21001 \
    --data-dir /var/lib/matchengine-raft --instance-prefix prod
```

对应的环境变量为 `RAFT_DATA_DIR` 和 `RAFT_INSTANCE_PREFIX`，默认值分别是 `/tmp/matchengine-raft` 和 `matchengine`。

## 快照配置

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

## 数据恢复

启动时节点会：
1. 恢复 WAL 索引、投票和 purge 标记（并丢弃撕裂的尾部）。
2. 选择并加载 `snapshot/current.snap`（如果存在）。该文件原子替换，因此始终是最新的完整快照；
   崩溃只会留下不会被选中的 `.tmp` 文件。
3. 从快照元数据恢复状态机的 `last_applied`，再由 openraft 重放日志索引大于快照最后日志索引的
   保留 WAL 条目。

单节点集群因此可以恢复所有活动订单、序列号、保留事件窗口和幂等去重表，无论恢复路径是纯日志重放
还是快照 + 更新的 WAL 尾部。两条路径都由 `tests/cluster/test_restart.rs` 覆盖。

## 转储持久化状态

存储示例只读，并校验 WAL/快照 CRC：

```shell
cargo run --example dump_wal -- <节点目录|log 目录|segment.wal>
cargo run --example dump_snapshot -- <节点目录|snapshot 目录|current.snap>
```

`dump_wal` 每条 WAL 记录输出一行 JSON（`entry` 或 `committed`），包含文件和偏移；
`dump_snapshot` 先输出元数据和状态行，再逐行输出 order、stop、event、submission。两者都以
JSONL 写到标准输出，便于配合 `jq` 等按行工具。

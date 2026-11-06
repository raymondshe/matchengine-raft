# 存储

## 预写日志（`src/store/log_store.rs`）

Raft 日志追加写入 `log/NNNNNNNNNN.wal` 分段文件：

- 每条记录格式为 `[len: u32 LE][crc32: u32 LE][postcard 负载]`；启动时逐段扫描，遇到撕裂或校验失败的尾部直接截断，而不是当作有效记录解析。
- `append()` 用一次 `write_all` 写入整个 openraft 批次；fsync 由 `RAFT_WAL_FSYNC_MS` 控制：未设置 = 不 fsync（默认），`0` = 每次追加 fsync，`N > 0` = 每 N ms 周期 fsync。
- 读取使用 `pread`（`read_at`）和缓存的文件句柄，复制任务可以在追加进行时并发读取，磁盘 IO 期间不持有全局锁。
- 所有阻塞 IO 都放在 `spawn_blocking` 中执行，异步运行时只负责编解码。
- 分段默认 64 MiB；只有被 purge 完全覆盖的分段才会删除，删除前先轮转活跃分段，保证日志不出现空洞。
- 每个分段以 `MRFT` magic 和 `u32` 格式版本开头。`LogStore` 实现 `RaftLogStorage`
  （`get_log_state`、`read_vote`/`save_vote`、`read_committed`/`save_committed`、`append`、
  `truncate_after`、`purge`、`get_log_reader`）。

投票和最后一次 purge 的日志 id 属于安全关键且低频的状态，单独存放在 `log/state` 文件中，采用原子重写（临时文件 → fsync → rename → 目录 fsync）。committed 日志 id 追加到 WAL 尾部但不单独 fsync：每次追加模式下随下一次 append 持久化，周期模式在下一次 tick 持久化，不 fsync 模式则不保证持久化；这样避免每批次多一次 fsync。

### WAL 持久性与从 peer 恢复

`RAFT_WAL_FSYNC_MS` 控制 WAL fsync：

- 未设置：不 fsync（默认）。
- `0`：每次追加批次 fsync。
- `N > 0`：专用 `wal-fsync` 线程每 N ms fsync 活跃分段。

只有每次追加 fsync 完全满足 `RaftLogStorage::append` 的持久性契约。不 fsync 或周期 fsync 时，
崩溃可能丢失自上次 fsync 以来的已确认写入（周期模式下最多约 N ms）。

崩溃后本地 WAL 可能短于集群已提交日志。Raft 能处理**合法的较短日志**：leader 补齐缺失条目或
安装快照；选举限制阻止较短日志的节点在多数派拥有已提交条目时当选；`/ready` 会等待
`wait_for_recovery`；投票和 purge 状态始终 fsync，即使 WAL fsync 关闭。

如果崩溃在非最后分段留下撕裂记录，非每次追加模式下 `LogStore::open` 会做尽力恢复：在第一个
损坏记录、索引空洞或分段处截断，删除后续分段，返回最长合法前缀，随后由 Raft 从 peer 追赶。
只有该节点拥有的条目可能丢失；如果没有任何 peer 拥有，则不可恢复。每次追加 fsync 模式下同样的
损坏会直接报错并拒绝启动。

生产环境建议设置 `RAFT_WAL_FSYNC_MS=0` 获得最强持久性，或设置较小的 N 将丢失窗口限制在约
N ms；未设置最快，但崩溃可能丢失已确认写入。

`RAFT_WAL_FSYNC_MS` 未设置时，二进制还要求至少 **4 个投票成员**：`/init` 和
`/change-membership` 会拒绝更小的集合，`/write`/`/write_batch` 在达到 4 个成员前返回 `503`。
4 个投票成员使一条已提交日志至少存在于 3 个节点上，单个节点丢失未 fsync 的尾部后仍至少保留 2 份。
启动 4 个节点后，用包含全部 4 个 `[id, addr]` 的 `/init` 请求初始化；单节点 `[]` 形式会被拒绝。
演示脚本默认设置 `RAFT_WAL_FSYNC_MS=0`，因此三节点和单节点示例仍然可用。

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
1. 恢复 WAL 索引、投票和 purge 标记（并丢弃撕裂的尾部；非每次追加模式下在第一个损坏分段处
   截断，让 Raft 追赶）。
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

# 性能细节

## 生产配置决策（2026-11-08）

测量支持两种生产配置；不要把有损 WAL 模式与单投票者部署混用。无 fsync 模式要求至少 4 个投票者。

### 方案 A：最大吞吐（性能优先，推荐）

| 配置 | 选择 |
|---|---|
| WAL 持久化 | 保持 `RAFT_WAL_FSYNC_MS` 未设置（不 fsync） |
| 投票者 | 至少 4 个；无 fsync 时二进制强制 |
| 读策略 | lease（默认）；NTP/PTP 时钟漂移有界且 `/read-policy/status` 显示 lease 命中时保留，否则设置 `--read-policy read_index` |
| Raft 运行时 | `RAFT_CORE_WORKERS` 未设置或 0（共享运行时） |
| OpenRaft 批处理 | 默认 `max_payload_entries=300`、`purge_batch_size=1`、`api_batch_linger_ms=0` |
| 写入准入 | 每节点 256 个并发 `/write` 或 `/write_batch` 请求；超出返回 `429`；`--max-inflight-writes 0` 关闭 |
| 快照 | 默认每 100,000 条日志或 30 分钟 |
| 异步 WAL | 关闭 |
| 构建 | 默认 release profile，无需 LTO |
| 行情 | 8,192 帧队列、默认 leader-only、生产使用外部 sink |

二进制默认值已经实现该方案，包括默认 lease read；无法保证时钟漂移有界时设置
`--read-policy read_index`。

方案 A 在非正常崩溃时**会丢失数据**。四投票者保护的是单个节点丢失未 fsync 尾部的情况，不保护
同时多节点故障或整机断电。已确认写入必须在这种情况下存活时，使用方案 B。

实测范围：四节点无 fsync 集群 **5,039 串行 writes/s**、**256 在途 90,569 writes/s**（p99 6.47ms）、
串行 128 单批次 **227,319 单/s**、100 x 32 并发批次 **216,163 单/s**。单节点无 fsync 串行更高
（9,648/s），因为没有 3/4 quorum 往返；容量估算用集群行。

客户端：`/write_batch` 每批 32–128 单。128 单吞吐最高；32–64 延迟更低。服务端默认每节点最多
**256** 个并发 `/write` 或 `/write_batch` 请求；超出返回 `429`，客户端应退避重试。设置
`--max-inflight-writes 0` 可关闭拒绝，或降低上限进一步限制准入工作量，并针对部署突发形态测量 p99。批量实测形状为 100 个并发
32 单批次，低于该上限。`apply` 在整批期间持有状态机锁，不要超过 128 单上限。

### 方案 B：持久写入

设置 `RAFT_WAL_FSYNC_MS=0`，其余运行时、OpenRaft、快照、行情和构建选择不变，并用 `/write_batch`
摊薄 fsync。单节点实测：串行 262 writes/s、并发 17.9k writes/s、100 x 32 并发批次 180.2k 单/s，
串行 p50/p99 3.9/5.0ms。四节点每次追加模式尚未测量；quorum 往返和 follower fsync 会进一步降低。

### 不采用的选项

| 选项 | 测量 | 决策 |
|---|---|---|
| `max_payload_entries=1,024` | 256 在途 66.0k vs 90.6k | 保留 300 |
| `purge_batch_size=1,024` + 每 1,000 条快照 | 58.3k；快照密集对照 60.3k | 保留 1 和 100k 快照策略 |
| `api_batch_linger_ms=1` | 37.4k，p99 19.75ms | 保留 0 |
| `RAFT_CORE_WORKERS=2` | 65.7k vs 90.6k；配对 A/B 在噪声内；单 worker 再低约 7% | 共享；仅用于隔离 |
| Thin LTO + `codegen-units=1` | 77.8k vs 74.1k 对照，在主机波动内 | 保留默认 profile |
| 异步 WAL | 存储 6.6x，但 e2e 18.3k vs 17.3k（噪声） | 关闭；请求路径无收益 |
| 10ms fsync | 7,374 writes/s，p99 2.2-3.0ms | 不采用：被两种方案同时压制 |
| 100ms fsync | 9,495 vs 9,648 writes/s；仍有损且无四投票者保护 | 不选；仅在接受 100ms 丢失窗口且有 4 投票者时使用 |
| 每价位 bucket | sweep 1.84x/cancel 3.31x，但稀疏 amend 0.75x/BBO advance 0.87x | 保留当前订单簿；仅原型 |
| Arena 分配器（`bumpalo`） | 外部评审中单操作延迟差 79-88% | 保留内联缓冲；不采用 |
| 高负载全量 `/read orderbook_orders` | p99 134ms，峰值 RSS 1.49GiB | 用 `orderbook_depth:N`，全量读不要放热路径 |
| 无界行情队列 | 2M fills/s 过载测试依赖丢弃 + GAP | 保留 8,192 有界队列 |

### 经验规则

- 本地 `/read` 是快路径但可能过期；`/linearizable_read` 和 `/events` 使用读策略。lease 在 1/64
  读者下把 `/linearizable_read` 提升 2.17x/1.71x，`/events` 提升 1.46x/1.18x；依赖前确认 fallback 计数。
- 不要把 `--match-event-log` 或 `--market-data-log` 当生产 feed；它们是节点本地调试 sink。使用
  复制 `/events` 游标或 Aeron 行情服务。
- 除非刻意调整事件窗口，保持 `--max-events` 默认 262,144；更大窗口增加内存和快照成本。
- 所有集群数字来自四节点同进程 loopback；上生产前在部署硬件重跑 `cluster_e2e`、`read_e2e` 和 `e2e`。

## 已实现的性能设计

- 每节点写入准入默认限制 256 个并发 `/write` 和 `/write_batch` 请求，超出返回 `429`，并在 `/admission/status` 暴露上限、在途、峰值、准入和拒绝计数。
- 每个复制追加批次一次 `write_all`；fsync 由 `RAFT_WAL_FSYNC_MS` 控制（未设置 = 不 fsync，`0` = 每次追加，`N` = 每 N ms 周期）。
- `pread` 读取配缓存文件句柄，追加与复制读取互不阻塞。
- 订单簿、事件窗口和去重表常驻内存，`apply` 不做磁盘 IO。
- gRPC 使用双向 `AppendStream` 流水线复制，并为每个对端维护独立的复制、心跳、快照连接和调优的
  HTTP/2 窗口/keepalive。
- 快照按 OpenRaft 配置的分块大小传输（默认 3 MiB，上限 8 MiB）；`log/state` 和
  `snapshot/current.snap` 都原子替换。
- 事件窗口由 `--max-events` 限制并通过 `/events` 分页；`/read orderbook_orders` 不会复制事件窗口。
- WAL 追加不再逐条 clone entry；恢复时顺序扫描 segment；连续日志读取合并为一次 `pread`；快照编码/写盘在状态机锁之外执行，快照 body 通过增量 CRC writer 直接流式写盘。
- gRPC 服务故障对 `run()` 是致命的：先停止 HTTP、回收后台任务并关闭 Raft，再返回错误；HTTP 请求只记录一次日志。

## 四节点无 fsync 基线（2026-11-07）

`cargo bench --bench cluster_e2e -- --rounds 3` 在 loopback 上启动四个同进程投票节点
（HTTP 25101–25104，gRPC 26101–26104），一次 `/init` 初始化全部四个成员，关闭 WAL fsync，
每轮使用全新集群并排除 50 次预热写入。三轮中位数：

| 工作负载 | 结果 |
|---|---:|
| 串行 `/write` | 5,039 writes/s（范围 4,637–5,378） |
| 串行 `/write` p50 / p95 / p99 | 186.75 µs / 288.33 µs / 368.88 µs |
| 并发 `/write`（1,000 在途） | 35,152 writes/s（范围 32,851–36,055） |
| 串行 `/write_batch`（每批 128 单） | 227,319 单/s（p50 563.42 µs / p99 622.46 µs） |
| 并发 `/write_batch`（100 × 32 单） | 216,163 单/s |

同机单节点无 fsync 对照（`cargo bench --bench e2e` 三轮）：串行 10,165 writes/s（p50
87.46 µs / p99 166.96 µs）、并发 33,831 writes/s、128 单批次 310,338 单/s、并发批次
223,406 单/s。四节点串行路径每个写入多一次 3/4 quorum 往返，吞吐约为单节点一半、p50/p99
约为 2.2 倍；并发写入和批次通过流水线把差异缩小到几个百分点。四个节点在同一进程、
loopback 上运行，覆盖 Raft quorum 与 gRPC 传输成本，但不模拟跨主机网络、独立机器故障或
节点间磁盘竞争。

## 写并发与批处理 sweep（2026-11-08）

`cargo bench --bench cluster_e2e -- --rounds 2 --concurrency-only` 在服务端运行时启动四节点
无 fsync 集群，客户端在独立运行时按 1/8/64/256/1024/4096 个在途请求扫描。该 sweep 设置
`--max-inflight-writes 0`，测的是无上限路径；默认准入见[写入准入控制](#写入准入控制2026-11-09)。
两轮中位数：

| 在途 clients | writes/s | p50 | p95 | p99 |
|---:|---:|---:|---:|---:|
| 1 | 5,778 | 169.3 µs | 216.8 µs | 293.9 µs |
| 8 | 26,958 | 309.8 µs | 413.1 µs | 455.7 µs |
| 64 | 79,191 | 792.5 µs | 1.064 ms | 1.662 ms |
| 256 | **90,569** | 2.256 ms | 5.487 ms | 6.467 ms |
| 1,024 | 44,829 | 14.199 ms | 20.040 ms | 21.789 ms |
| 4,096 | 50,996 | 19.707 ms | 64.566 ms | 76.465 ms |

吞吐在 **256 在途**达到峰值，之后 1,024/4,096 的 p99 升到 21.8/76.5 ms。对 p99 敏感的部署应把
在途请求限制在 256-512，或改用 `/write_batch`（128 单批次在四节点集群上为 227,319 单/s）；
继续增加并发不会增加服务端吞吐。服务端在途上限或队列深度指标是自然后续项。

单因素 sweep（同一拓扑）结论：`max_payload_entries=1024`、`purge_batch_size=1024`（配合
`LogsSinceLast(1000)`）、`RAFT_CORE_WORKERS=2` 和薄 LTO + `codegen-units=1` 都没有稳定收益；
`api_batch_linger_ms=1` 明显退化。`api_batch_linger_ms` 字段单位是毫秒，100/500µs 无法表示；
最小的非零值 1ms 已经伤害吞吐和尾延迟，100ms 还会耗尽客户端临时端口。因此保留
`300 / 1 / 0 / shared` 和默认构建 profile。快照密集策略本身在 256 客户端下损失约三分之一吞吐，
单纯增大 purge batch 没有恢复。所有节点仍在同一进程/主机，1,024/4,096 是突发尾延迟，不是容量保证。

## 异步 WAL 原型（2026-11-08）

异步 WAL 原型（feature-gated）写入 WAL 批次并更新可见日志，把 `sync_data` 交给 Tokio
`async-wal` worker；只有 fsync 后才触发 `IOFlushed`。存储基准会先入队全部 200 个批次再等待
回调，模拟 Raft 流水线。每种模式 5 轮，20,000 条真实订单、每批 100 条、512 KiB 分段：

| 模式 | 追加吞吐 | 范围 |
|---|---:|---:|
| 每次追加，同步 | 26,043 条/s | 25,839-26,314 |
| 每次追加，异步原型 | **172,374 条/s** | 65,176-192,160 |

异步中位数约为同步的 **6.6x**，但波动很大：一轮只有 65.2k，其余四轮为 168k-192k；worker 的
fsync 批处理和文件系统时序影响明显。读/重开数字接近。这是存储层结果；Raft 级 A/B 见下。

单节点 Raft 级 A/B 使用同一 `e2e` 套件、多线程运行时、每次追加模式，每种配置 3 轮。串行写入
两种模式都约 258-260 writes/s，因为每个请求仍需等待 `IOFlushed`。1,000 并发写入中位数为同步
18,344/s、异步 17,277/s；100 x 32 并发批次为同步 152,833、异步 157,414 orders/s。差异都在约
10% 的轮间噪声内：在这台单节点 macOS 主机上，异步路径**没有稳定的请求路径收益**。存储层收益
来自在等待回调前排队 200 个追加批次；而 Raft 并发请求本身已经重叠同步 fsync，操作系统级
group commit 已经获得了大部分可用批处理。

原型默认关闭，只支持每次追加模式。`tests/cluster/test_storage.rs` 证明回调等待 fsync、
flush 前可读、丢失尾部恢复和日志可继续追加；`tests/cluster/test_async_wal.rs` 证明 Raft 节点
可以提交并重开。生产化仍需 Linux 多节点 A/B、周期模式支持、更广的崩溃/故障转移覆盖，以及
比当前请求路径暴露更多追加流水线的设计。

## 写入准入控制（2026-11-09）

每个节点为 `/write` 和 `/write_batch` 增加了有界在途准入：`--max-inflight-writes`（环境变量
`MATCHENGINE_MAX_INFLIGHT_WRITES`）默认 **256**，`0` 关闭拒绝但仍统计计数。超出上限的请求立即
返回 `429 Too Many Requests`，不会堆积在 Raft/apply 之后。`GET /admission/status` 报告上限、
当前在途、峰值、准入和拒绝总数。上限按请求计数，不按订单计数：一个 128 单 `/write_batch`
只占一个名额。

四节点无 fsync 单轮，256/512/1024/4096 个在途客户端，对比无上限（`0`）和默认准入（`256`）。
吞吐为实际准入的 writes/s；被拒请求快速返回 `429`。这是不重试的开环突发；生产客户端应退避重试。

| 在途客户端 | 上限 | 准入 | 拒绝 | 准入 writes/s | p50 | p99 |
|---:|---:|---:|---:|---:|---:|---:|
| 256 | 0 | 1,792 | 0 | 90,312 | 2.28 ms | 5.01 ms |
| 256 | 256 | 1,792 | 0 | 90,091 | 2.19 ms | 5.59 ms |
| 512 | 0 | 1,536 | 0 | 50,053 | 8.66 ms | 21.30 ms |
| 512 | 256 | 1,202 | 334 | 42,769 | 8.18 ms | 22.07 ms |
| 1,024 | 0 | 1,024 | 0 | 43,267 | 13.39 ms | 21.02 ms |
| 1,024 | 256 | 795 | 229 | 34,028 | 10.35 ms | 20.28 ms |
| 4,096 | 0 | 4,096 | 0 | 45,912 | 16.07 ms | 72.77 ms |
| 4,096 | 256 | 2,118 | 1,978 | 42,837 | 6.47 ms | **15.37 ms** |

在 256 在途的拐点上准入控制是中性的；4,096 在途时把 p99 从 **72.77 ms** 降到 **15.37 ms**
（4.7x），准入吞吐只下降 6.7%。512/1,024 的 p99 没有改善，因为 256 个在途请求本身已高于低尾
拐点；准入控制阻止无界排队，但不能让该并发变便宜。64/128 的更低上限会降低准入吞吐，且在这个
突发模型中没有稳定的 p99 收益，因此默认保留 256。需要更紧尾延迟时，降低客户端并发或使用更大的
`/write_batch`。

`/write_batch` 在 100 个并发 32 单批次（3,200 单）下低于上限：无上限 **309,180 单/s**、
p99 **10.31 ms**，上限 256 时 **303,609 单/s**、p99 **10.49 ms**。这是单轮结果，部署硬件需重跑。
被拒请求是快速 `429`，不是排队请求；客户端应退避重试（Rust 客户端把 429 视为可重试）。

## 读路径基准：ReadIndex vs lease（2026-11-08）

`cargo bench --bench read_e2e -- --policy both` 在服务端运行时启动同样的四节点无 fsync 集群，
客户端运行在独立运行时。集群预置 2,000 个挂单；本地读为 `orderbook_depth:20`，`/events` 每页
100 条；混合场景以 2,000 writes/s 施加写入。每个场景每读者 2,000 次读，预热 100 次；下表为
两轮中位数（只读结果稳定，混合结果受实际写入节奏影响）。

| 读路径 | readers | ReadIndex reads/s | lease reads/s | 吞吐提升 | ReadIndex p50 / p99 | lease p50 / p99 |
|---|---:|---:|---:|---:|---:|---:|
| `/linearizable_read` | 1 | 8,092 | 17,559 | 2.17x | 119.7 / 243.6 µs | 54.9 / 83.9 µs |
| `/linearizable_read` | 64 | 30,232 | 51,777 | 1.71x | 317.2 µs / 3.847 ms | 91.5 µs / 1.155 ms |
| `/events` | 1 | 5,100 | 7,461 | 1.46x | 191.7 / 352.1 µs | 131.7 / 194.3 µs |
| `/events` | 64 | 32,979 | 38,856 | 1.18x | 1.768 / 4.366 ms | 1.584 / 3.552 ms |

本地 `/read` 基本不变（1 读者约 20k reads/s，64 读者约 50-59k），符合预期。混合场景方向一致但
波动更大：单读者 `/linearizable_read` 吞吐 7,469 → 17,973 reads/s（2.41x），p50 122.1 → 48.0 µs；
64 读者 p50 338.6 → 91.1 µs（3.72x），吞吐提升 1.55-2.28x。`/events` 混合场景分别提升 1.52x
（1 读者）和 1.17x（64 读者）。

两轮 lease 命中率均为 **100%**：1 读者每路径 2,000/2,000，64 读者 128,000/128,000，无回退。
四个节点在同一主机，lease 一直新鲜，回退分支未被触发；计数器已实现并有单元测试，真实跨主机
时钟漂移部署应观察 `/read-policy/status` 的 fallback 计数。收益小于上游 90.4x/3.9x 的本地
lease 结果，因为 alpha.37 的 `ReadPolicy::LeaseRead` 仍经过 RaftCore；上游的
`enable_local_lease_read` 本地缓存路径更新。lease 现在是默认；设置 `--read-policy read_index`
可强制新的 quorum 往返。

## 上游 OpenRaft minimal 基准对比（2026-11-08）

上游 `openraft/benchmarks/minimal` 用单进程三节点、内存日志/空状态机、函数调用网络和空请求
隔离 OpenRaft：1 客户端 33k put/s，4,096 客户端 3.5M put/s，batch 4 5.6M put/s；lease read
单读者 90.4x、64 读者 3.9x。我们的 4 节点无 fsync 集群为 5,039 串行 / 90,569（256 在途峰值）
writes/s。Raft 不是当前瓶颈，HTTP/JSON、WAL、apply 和基准运行时竞争占主导。

执行结果与后续项（详见英文 [PERFORMANCE.md](../PERFORMANCE.md)）：

- 读取路径：`/linearizable_read`、`/events` 和 gRPC `StreamEvents` 默认使用
  `ReadPolicy::LeaseRead`，并回退到 `ReadIndex`；alpha.37 没有 `enable_local_lease_read`，
  完整的本地缓存路径需要升级 OpenRaft。基准已完成：lease 在 1/64 读者下提升 2.17x/1.71x；
  时钟漂移无界时用 `--read-policy read_index` 强制新的 quorum 往返。
- 写入/批处理：已完成 1/8/64/256/1024/4096 客户端 sweep；256 在途峰值 90,569 writes/s，
  1,024/4,096 降至 44,829/50,996 writes/s。`max_payload_entries=1024`、`purge_batch_size=1024`、
  `api_batch_linger_ms=1`、`RAFT_CORE_WORKERS=2` 和薄 LTO 均无稳定收益；保留
  300/1/0/shared 和默认构建 profile。
- 观测/存储：启用 `runtime-stats`、暴露 data/server metrics、加入 flamegraph/tokio-console；
  异步 WAL 原型已完成（存储 6.6x，但 Raft 请求路径无稳定收益，默认关闭）。
  薄 LTO + `codegen-units=1` A/B 无稳定收益，保留默认 profile。

## 当前限制与已知差距

- TLS/mTLS 与 API 认证由前置网关负责；本进程是内部服务。
- 快照安装需要一块连续缓冲区，单个快照上限为 256 MiB。
- 一个进程/集群只交易一个标的；多标的需要横向部署多个实例。

## 数据格式`Order` 的价格/数量、`last_trade_price`、撮合事件、深度/BBO/spread、JSONL 行情和 wire v6 均使用精确的 18 位定点整数。快照 **v10** 和 wire **v6** 是相对旧版本的破坏性变更，升级时请重建数据目录。每个进程/集群只支持一个标的，`SymbolConfig` 已复制并在交易开始后锁定。18 位 BTC 分析、微基准和迁移记录见
[`docs/DECIMAL.md`](../DECIMAL.md)。

已知差距与路线图统一记录在 [`TODO.md`](../../TODO.md)。

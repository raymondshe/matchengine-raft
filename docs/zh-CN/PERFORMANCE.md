# 性能细节

## 已实现的性能设计

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
无 fsync 集群，客户端在独立运行时按 1/8/64/256/1024/4096 个在途请求扫描。两轮中位数：

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
fsync 批处理和文件系统时序影响明显。读/重开数字接近。这是存储层结果，尚未做 Raft 级 A/B。

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
`enable_local_lease_read` 本地缓存路径更新。默认仍为 `ReadIndex`，lease 为可选。

## 上游 OpenRaft minimal 基准对比（2026-11-08）

上游 `openraft/benchmarks/minimal` 用单进程三节点、内存日志/空状态机、函数调用网络和空请求
隔离 OpenRaft：1 客户端 33k put/s，4,096 客户端 3.5M put/s，batch 4 5.6M put/s；lease read
单读者 90.4x、64 读者 3.9x。我们的 4 节点无 fsync 集群为 5,039 串行 / 35,152 并发
writes/s。Raft 不是当前瓶颈，HTTP/JSON、WAL、apply 和基准运行时竞争占主导。

执行结果与后续项（详见英文 [PERFORMANCE.md](../PERFORMANCE.md)）：

- 读取路径：为 `/linearizable_read`、`/events` 和 gRPC `StreamEvents` 增加可选
  `ReadPolicy::LeaseRead` 与 `ReadIndex` 回退；alpha.37 没有 `enable_local_lease_read`，
  完整的本地缓存路径需要升级 OpenRaft。先加读取基准再改默认值。
- 写入/批处理：已完成 1/8/64/256/1024/4096 客户端 sweep；256 在途峰值 90,569 writes/s，
  1,024/4,096 降至 44,829/50,996 writes/s。`max_payload_entries=1024`、`purge_batch_size=1024`、
  `api_batch_linger_ms=1`、`RAFT_CORE_WORKERS=2` 和薄 LTO 均无稳定收益；保留
  300/1/0/shared 和默认构建 profile。
- 观测/存储：启用 `runtime-stats`、暴露 data/server metrics、加入 flamegraph/tokio-console；
  异步 WAL append（pending cache + `IOFlushed`）是每次追加 fsync 模式下最大的存储机会。
  薄 LTO + `codegen-units=1` 做 A/B。

## 当前限制与已知差距

- TLS/mTLS 与 API 认证由前置网关负责；本进程是内部服务。
- 快照安装需要一块连续缓冲区，单个快照上限为 256 MiB。
- 一个进程/集群只交易一个标的；多标的需要横向部署多个实例。

## 数据格式`Order` 的价格/数量、`last_trade_price`、撮合事件、深度/BBO/spread、JSONL 行情和 wire v6 均使用精确的 18 位定点整数。快照 **v10** 和 wire **v6** 是相对旧版本的破坏性变更，升级时请重建数据目录。每个进程/集群只支持一个标的，`SymbolConfig` 已复制并在交易开始后锁定。18 位 BTC 分析、微基准和迁移记录见
[`docs/DECIMAL.md`](../DECIMAL.md)。

已知差距与路线图统一记录在 [`TODO.md`](../../TODO.md)。

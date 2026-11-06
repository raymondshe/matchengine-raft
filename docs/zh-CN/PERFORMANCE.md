# 性能细节

## 已实现的性能设计

- 每个复制追加批次一次 `write_all` + 一次 `sync_data`，一次 AppendEntries 中的 N 条记录只付一次 fsync。
- `pread` 读取配缓存文件句柄，追加与复制读取互不阻塞。
- 订单簿、事件窗口和去重表常驻内存，`apply` 不做磁盘 IO。
- gRPC 使用双向 `AppendStream` 流水线复制，并为每个对端维护独立的复制、心跳、快照连接和调优的
  HTTP/2 窗口/keepalive。
- 快照按 OpenRaft 配置的分块大小传输（默认 3 MiB，上限 8 MiB）；`log/state` 和
  `snapshot/current.snap` 都原子替换。
- 事件窗口由 `--max-events` 限制并通过 `/events` 分页；`/read orderbook_orders` 不会复制事件窗口。
- WAL 追加不再逐条 clone entry；恢复时顺序扫描 segment；连续日志读取合并为一次 `pread`；快照编码/写盘在状态机锁之外执行，快照 body 通过增量 CRC writer 直接流式写盘。
- gRPC 服务故障对 `run()` 是致命的：先停止 HTTP、回收后台任务并关闭 Raft，再返回错误；HTTP 请求只记录一次日志。

## 当前限制与已知差距

- TLS/mTLS 与 API 认证由前置网关负责；本进程是内部服务。
- 快照安装需要一块连续缓冲区，单个快照上限为 256 MiB。
- 一个进程/集群只交易一个标的；多标的需要横向部署多个实例。

## 数据格式`Order` 的价格/数量、`last_trade_price`、撮合事件、深度/BBO/spread、JSONL 行情和 wire v6 均使用精确的 18 位定点整数。快照 **v10** 和 wire **v6** 是相对旧版本的破坏性变更，升级时请重建数据目录。每个进程/集群只支持一个标的，`SymbolConfig` 已复制并在交易开始后锁定。18 位 BTC 分析、微基准和迁移记录见
[`docs/DECIMAL.md`](../DECIMAL.md)。

已知差距与路线图统一记录在 [`TODO.md`](../../TODO.md)。

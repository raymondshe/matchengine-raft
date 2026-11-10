# 行情数据 feed

## 行情数据 feed（可选）

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
  审计位置；头部还携带当前 Raft term（`epoch`）和已知 leader id，重置快照标记领导权代次；
- 默认只有领导者发布（`--market-data-leader-only=false` 时每个副本都发布）；每次获得领导权先发一次
  重置快照（边界为下一个 `market_seq`）。队列满时丢弃帧并发送 `GAP`，`apply` 永远不会被订阅者阻塞
  或拖慢。

JSONL sink 继续用于调试。同样的消息在 `src/market_data/wire.rs` 有二进制编码（64 字节头、版本 7、
scale-18 `i128`、小端消息体）。启用 `market-data-aeron` feature 后，`AeronSink` 由专用线程把它们发布到
2001–2007 流：每个流一个独占 publication，支持嵌入式或外部 media driver，并周期重发重置深度快照
（`--market-data-aeron-snapshot-every-ms`，默认 30 秒），让服务可以在流中途加入。空闲时按
`--market-data-aeron-heartbeat-ms`（默认 1 秒）发布带保留 `sub_seq` 的 STATUS 心跳；2010 流是
subscriber→engine 的固定 24 字节快照请求（`MSRQ` v1），服务在深度缓存为空、过期或出现 GAP 时发送，
引擎在状态机锁内抓取并排队一次重置快照。每条消息携带当前进程配置的
`symbol_id`，便于多个按标的部署共享总线。可调参数：`--market-data-aeron-channel`、
`--market-data-aeron-stream-base`、`--market-data-aeron-dir`、`--market-data-aeron-embedded-driver`、
`--market-data-aeron-heartbeat-ms`、`--market-data-depth`、`--market-data-status-every`、
`--market-data-bar-intervals-ms`、`--market-data-leader-only`。

`/read marketdata_status` 返回 feed 计数器和领导者门控状态。

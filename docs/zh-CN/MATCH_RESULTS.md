# 撮合结果

## 撮合结果处理

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
        └──► POST /events (long-poll) + GET /events/status   [游标 + 保留策略]
                    │
                    ▼
          MatchResultSink                           [节点本地，尽力而为]
            ├── NoopSink      默认：无本地副作用
            └── FileLogSink   --match-event-log -> JSONL
```

`Place` 支持可选的 `client_order_id`；用同一个 id 重试会返回原序列号和事件，而不会重复下单。
事件窗口由 `--max-events`（环境变量 `MAX_EVENTS`，默认 262144）限制；提交去重表另有固定的
262144 条上限，避免重试结果依赖节点本地参数。两者都包含在快照 v10 中。
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
{"after": {"log_index": 100, "ordinal": 3}, "limit": 500, "wait_ms": 1000}
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

`wait_ms`（0–1000，默认 0）把请求变成长轮询：游标位于尾部时最多等待该时长，有新事件就立即返回，
否则在超时后返回空页（`next_after` 保持不变）。等待由节点本地 `watch` 通道唤醒，不持有状态机锁。

集群内消费者也可以使用 gRPC 监听器（HTTP 端口 + 1000）上的 `EventService.StreamEvents`：它按同一
游标流式推送与 `POST /events` 相同的 JSON 页，空闲时每 `wait_ms` 发一个空心跳页，并支持
`max_pages`。参考 MQ bridge sidecar 消费 `POST /events`，在目标确认每一页后提交游标：

```bash
cargo run --bin events_bridge -- \
    --engine 127.0.0.1:21001 --engine-id 1 \
    --out /tmp/events.jsonl --cursor /tmp/events.cursor
```

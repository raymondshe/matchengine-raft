# API 参考

## HTTP 与 gRPC 接口

HTTP 应用与管理端点使用 JSON。每个处理器返回 `Result` 包装，因此成功调用形如 `{"Ok": ...}`，
转发或失败调用形如 `{"Err": ...}`。Raft 节点间 RPC 使用 gRPC，不经过 HTTP JSON API。

### Raft gRPC API

Raft 节点间 RPC 仅由 `--http-addr` 端口 + 1000 上的 gRPC 提供；HTTP JSON 监听器上不挂载
任何 Raft RPC：

- `AppendEntries`、`Vote`（一元）
- `InstallSnapshot`（客户端流式分块）
- `AppendStream`（双向流水线追加）
- `TransferLeader`（控制台使用的优雅领导者交接）

定义见 `proto/raft.proto`。一元 append/vote 使用 `RPCOption::soft_ttl`，快照按 OpenRaft
配置的分块大小传输（默认 3 MiB，上限 8 MiB），双向消息上限均为 64 MiB；`Unavailable` 映射为
`RPCError::Unreachable`，超时映射为
`RPCError::Timeout`，其他状态映射为 `RPCError::Network`。

### 管理 API

- `POST /init` - 初始化集群；请求体为 `[]` 时只初始化当前节点，也可传
  `[[1, "127.0.0.1:21001"], ...]` 显式指定成员
- `POST /add-learner` - 添加学习者节点；请求体为 `[id, "host:port"]`
- `POST /change-membership` - 变更投票成员集合；请求体为 `[1, 2, 3]`
- `RAFT_WAL_FSYNC_MS` 未设置时，二进制要求至少 4 个投票成员：`/init` 和
  `/change-membership` 会拒绝更小的集合，`/write`/`/write_batch` 在达到 4 个成员前返回 `503`。
- `GET /metrics` - 获取 Raft 指标
- `POST /get_linearizer` - 从领导者获取读 linearizer（供 `/follower_read` 使用）

### 运维 API

- `GET /health` - 存活检查：报告 gRPC 监听器、Raft 角色/领导者、WAL 与状态机前沿。监听器、Raft
  核心和存储可用时返回 `200`，否则返回 `503`。
- `GET /ready` - 就绪检查：仅在节点健康且已知集群领导者时返回 `200`；启动、选举或分区期间返回
  `503`。知道领导者的 follower 可服务读请求，写请求仍需发送给领导者。
- `GET /read-policy/status` - 当前 leader 读策略及各读路径的 lease/ReadIndex 计数
- `GET /admission/status` - 每节点写入准入上限及当前/峰值/准入/拒绝计数
- `GET /console/` - 内置运维控制台（HTML/CSS/JS），无需构建步骤或外部资源；当前 `BASE/QUOTE` symbol 作为醒目标题
- `GET /console/api/node` - 稳定的节点快照：健康、Raft 角色、存储前沿、事件、feed 和当前 symbol（`symbol_id`、base/quote、tick/lot）
- `GET /console/api/cluster` - 当前成员关系中的本节点与对端快照
- `POST /console/api/actions/transfer-leader` - 优雅领导者切换（仅领导者）
- `POST /console/api/actions/elect` - 尽力而为的本地 Pre-Vote 选举请求
- `POST /console/api/actions/snapshot` - 触发本地快照
- `GET /console/api/actions` - 最近的内存操作记录

控制台操作要求 `X-Console-Action: 1` 请求头，并在进程内串行执行。控制台继承内部 HTTP
契约；TLS 和运维认证仍由网关负责。

### 应用 API

- `POST /write` - 通过 Raft 提交 `Place`、`Cancel`、`Amend`、`PurgeExpired` 或 `ConfigureSymbol` 请求。
  订单/变更请求返回分配的 `market_seq`（`Place` 同时是订单序列号），并携带该条目的首批事件（上限 512）。
  `ConfigureSymbol` 返回状态字符串，拒绝时使用 `error` 字段，不消耗市场序列号。`Place` 支持可选的
  `client_order_id`，用于幂等重试。`/write` 也接受 `Batch { requests }` 信封；`/write_batch` 则以裸
  JSON 数组接收同样的请求。
- `POST /write_batch` - 将 1–128 个非嵌套请求放入一个 Raft entry；响应中的 `batch` 数组逐项返回结果，
  事件序号在整个 entry 内连续。
- `/write` 和 `/write_batch` 共享每节点在途准入上限（默认 **1,024** 个请求）。达到上限时返回
  `429 Too Many Requests`，客户端应退避重试。上限按请求计数，不按订单计数，因此一个 128 单批次
  只占一个名额。约 1,024 个并发客户端且需要更紧尾延迟时可降到 `512`；`--max-inflight-writes 0`
  可关闭拒绝；`GET /admission/status` 报告上限、当前在途、峰值、准入和拒绝总数。
- `POST /read` - 从本地状态机读取（可能读到旧值）
- `POST /linearizable_read` - 线性一致性读；请求必须到达领导者
- `POST /follower_read` - 从领导者获取 fresh linearizer 后在本地完成的线性一致性读；不在 follower 缓存 linearizer（旧值可能漏掉已完成的写），但复用单个进程级 HTTP client
- `POST /events` - 分页读取复制的撮合事件日志（`after`、`limit`、`wait_ms`）；线性一致，仅领导者，支持长轮询
- `GET /events/status` - 保留事件窗口、淘汰计数和保留上限

#### 读一致性与 lease read

`POST /read` 是本地读，可能过期，不受读策略影响。需要 leader 一致性的路径默认使用
`local_lease`：`/linearizable_read`、`/events`、gRPC `StreamEvents` 锚点，以及
`/follower_read` 使用的 `/get_linearizer` 先使用 OpenRaft 的调用侧 lease 缓存，再走核心
`ReadPolicy::LeaseRead`。在 leader lease 有效时复用最近的 quorum 确认，省去新的往返；lease
过期时自动回退到 `ReadIndex`，quorum 可用时承担回退延迟后成功，否则返回 Raft 读错误。

启动参数 `--read-policy read_index`（环境变量 `MATCHENGINE_READ_POLICY=read_index`）可强制每次
读都使用新的 quorum 往返；`--read-policy lease` 保留不带调用侧缓存的核心路径。本地缓存路径在
4 节点基准中读吞吐提升 11-36%，64 读者 p50/p99 降低约 40%。

Lease 与 local-lease read 假设节点间**时钟漂移有界**。使用 NTP 或 PTP；除非部署能保证 leader
lease 不会超过实际领导权，否则设置 `--read-policy read_index`。local-lease 缓存命中可能越过尚未
完成的写请求；需要读到某次写结果的客户端应先等待写响应。客户端重试是 **at-least-once**：需要
去重的写请求应设置 `client_order_id`，复制的提交映射会返回原始 sequence 和事件。下游事件消费者
应保证幂等：从 `(log_index, ordinal)` 恢复并按 `event_id` 去重；local-lease read 不改变这两个
契约。状态端点按读路径报告 `lease_attempts`、`lease_hits`、`lease_fallbacks`、
`lease_failures`、`read_index_attempts` 和 `read_index_failures`。
- gRPC `EventService.StreamEvents` - 在 gRPC 监听器上按同一游标流式推送 JSON 事件页，支持 `wait_ms` 和 `max_pages`
- `GET /events/status` - 保留事件窗口、淘汰计数和保留上限

`/write` 背后的 Rust 类型：

```rust
pub enum MatchEngineRequest {
    Place { order: Order, client_order_id: Option<String> },
    Cancel { order: Order },
    Amend { order: Order, price: Option<Fixed>, volume: Option<Fixed> },
    PurgeExpired { now_ms: u64, day_close_ms: Option<u64> },
    ConfigureSymbol { config: SymbolConfig },
    Batch { requests: Vec<MatchEngineRequest> },
}

pub struct MatchEngineResponse {
    pub value: Option<String>,        // 兼容旧调用：序列号字符串
    pub sequence: Option<u64>,        // 分配的 market_seq
    pub events: SmallVec<[MatchEvent; 1]>, // 本条目的前 512 个事件；一个内联，序列化为数组
    pub events_truncated: bool,       // 还有更多事件时为 true
    pub error: Option<String>,        // 非致命的应用层错误
    pub batch: Vec<MatchEngineResponse>, // /write_batch 的逐项响应
}
```

每个订单/变更请求消耗一个 `market_seq`；`Place` 同时把它作为订单序列号，`ConfigureSymbol` 不消耗。
响应最多携带 512 个事件，完整批次从 `/events` 读取。成交事件携带 `quote_volume`、`volume_unit`、
`taker_fee`/`taker_fee_asset` 和 `maker_fee`/`maker_fee_asset`；行情 `TRADE` 与成交 `LIFECYCLE`
记录也包含相同字段（wire v6）。`client_order_id` 可选，只影响 `Place`：同一 id 重试返回原序列号和
事件，不会重复下单。长度必须为 1–128 字节，字符集为 `[A-Za-z0-9._:-]`；空字符串会被拒绝，
不需要幂等时请省略该字段。

`/write_batch` 在一个 `apply` 调用中按顺序执行子请求，后一项能看到前一项的结果；每项保留自己的
序列号和事件，所有事件共享该 entry 的日志索引且序号连续。单项被拒绝不会中止整个批次。空批次、
嵌套批次、超大批次和批次内重复 `client_order_id` 会在进入 Raft 前被拒绝。

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

价格和数量在内部使用精确的 18 位定点整数。JSON 接受十进制字符串或旧版数字；撮合事件、`/read` 和行情输出十进制字符串，以精确往返 18 位小数。一个进程/集群只交易一个标的：`SymbolConfig` 随 Raft 复制并写入快照，包含 base/quote `AssetId`/`AssetClass`、价格 tick、base lot 和扣费资产策略；超过精度或不符合最小变动单位的订单会被拒绝。在 leader 上，`/write` 和 `/write_batch` 会在进入 Raft 前以 `400` 拒绝 `symbol_id` 与已配置标的不匹配的订单；状态机仍会再次校验作为纵深防御。扩展多标的方式是每个标的运行独立的进程/集群，从而隔离资源与故障；内部统一归一化到 18 位。

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
  "fee_asset_policy": "quote"
}}}
```

配置会随 Raft 复制，并在首个订单被接受后锁定。有挂单或已经开始交易后修改配置会被拒绝。在正在发布行情的节点上，成功修改还会为新标的排队一个重置快照。该非致命错误通过响应的可选 `error` 字段返回，写入本身仍会提交。

费率字段（`maker_fee_rate`、`taker_fee_rate`）是网关按用户等级带入的有符号 `Fixed` 小数：
`"0.001"` 表示 10 bps，负值表示返佣，缺省为零。每笔成交的名义金额由 maker 价格 × 成交数量计算，
吃单方按吃单费率收费，挂单方按挂单费率收费；费用按 18 位小数向零截断。名义金额或费用超出支持的定点范围时
会通过 `error` 返回 `notional_overflow`、`taker_fee_overflow`、`maker_fee_overflow` 等错误码。
存在错误码时对应数值字段为 `0`，因此只有错误码缺失时零才是真实值；Rust 消费方可使用
`MatchResult`/`MatchEvent` 的 `exact_*` 访问器。为保持 snapshot v10/wire v6 契约，未改为可空字段。
引擎只记录费用，不维护余额：订单系统冻结交易资产，清算系统从冻结资产中扣费。

法币、稳定币、加密货币、ETF 和股票等标的的 base/quote 数量语义与按资产扣费已实现。
ETF/股票 base 的费用始终以 quote 资产结算；加密货币和法币对法币（FX）默认使用收到资产，
加密货币也可显式选择 quote 或 base。quote 数量仅支持普通限价单和市价单；quote iceberg 和 FOK
在准入时被拒绝。完整模型见 [`docs/FEE_DEDUCTION.md`](../FEE_DEDUCTION.md)；第三资产 fee token
和衍生品不在引擎范围内，交易时段、结算、公司行为以及固定/监管/最低/税费由外部订单系统负责。

其他订单类型字段（`tif`、`post_only`、`stop_price`、`display_volume`、`hidden_volume`、
`timestamp`）都是可选的，默认就是普通的 GTC 限价单：

```json
{"Place": {"order": {"live": "Limit", "side": "Sell", "price": 4000.0, "volume": 100.0, "tif": "IOC", "display_volume": 10.0}}}
```

撮合引擎在应用订单时会覆盖 `sequence`，接收请求的领导者还会在入口处覆盖 `timestamp`，因此客户端
发送的这两个值都会被忽略（约定填 `0`）。`sequence` 可选，默认值为 `0`。

订单身份分为三个独立字段：

- `id` 是客户端元数据，不是订单簿键，也不校验唯一性；允许重复 `id`。
- `sequence` 由引擎在应用时分配；后续撤单/改单必须使用返回的序列号。簿上订单键为
  `(方向, 当前价格, sequence)`，停在场外的止损单为 `(方向, sequence)`。`Place` 中客户端发送的
  `sequence` 会被忽略。
- `client_order_id` 仅用于 `Place` 幂等，保留最近 262,144 次提交；窗口内重试返回原序列号和事件，
  被淘汰后重试会变成新订单。它从不用于定位挂单。

`Cancel` 或 `Amend` 的键找不到挂单时，仍会消耗一个 `market_seq`，并产生原因为 `order_not_found`
的 `rejected` 事件。`/write` 仍返回 `200`，因为请求已经提交；权威结果是事件而不是 HTTP 状态码。

### Schema v11 订单类型

- `market_to_limit: true`：市价扫单后，剩余量以最后成交价挂为普通限价单；无成交则整单撤销。
  与 IOC/FOK、Post-Only、止损、冰山或 quote 数量组合会被拒绝。
- `trail_offset`：移动止损距离。`stop_price` 入库时必须为 `0`，`trail_reference` 由引擎维护并随
  快照持久化，当前触发价写入 `stop_price`。
- `peg`（`primary`/`midpoint`/`last_trade`）+ `peg_offset`：以非 Peg 流动性为参考挂单；参考变化时
  重新定价并产生 `amended` 事件，新价格会失去时间优先；会穿越时以 `peg_would_cross` 撤销。
- `owner_id`（可选）：自成交防护身份；`SymbolConfig.stp` 为 `off`（默认）、`cancel_resting`、
  `cancel_aggressor` 或 `cancel_both`。只有两个非空且相同的 `owner_id` 才会触发 STP。

Peg 订单的 `Amend` 不接受 `price`；非零 `order.peg_offset` 表示新偏移，`0` 表示保持当前偏移
（要归零请撤单后重下）。移动止损的 `Amend` 用 `order.trail_offset` 表示新偏移；回传当前派生
`stop_price` 可接受，传入不同的非零触发价会被拒绝。

# 撮合

## 订单类型

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
| 市价转限价 | `"live": "Market", "market_to_limit": true` | 先按市价扫单，剩余量以最后成交价挂为普通限价单；无成交则整单撤销。 |
| 移动止损 | `"trail_offset": 1.0`（止损限价或止损市价） | 触发价随卖出最高价/买入最低价移动，由后续成交触发；入库时 `stop_price` 必须为 `0`。 |
| Peg 订单 | `"peg": "primary" \| "midpoint" \| "last_trade"`，`"peg_offset": "0"` | 以非 Peg 流动性为参考的限价单；参考变化时重新定价，`price` 必须为 `0`。 |

市价单（`"live": "Market"`）忽略自身价格，扫遍对手方，剩余量撤销，从不挂单；`market_to_limit`
是唯一例外，剩余量会以最后成交价挂单。

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

## 撤单

`Cancel` 通过方向、当前价格和序列号定位挂单；停在场外的止损单通过方向和序列号定位。`Order.id`
是客户端元数据，不是键。撤销不存在的挂单不会静默确认：会消耗一个市场序列号并产生 `rejected`
事件，原因 `order_not_found`。成功撤销产生 `cancelled` 事件，携带当时仍在簿上的数量。

## 订单修改

`Amend` 修改一笔挂单的限价和/或未成交数量。请求与 `Cancel` 一样通过 方向、当前价格和序列号
定位订单，新字段均可选：

```json
{"Amend": {"order": {"live": "Limit", "side": "Buy", "price": 4000.0, "volume": 4.4, "id": 1, "sequence": 7}, "price": 4002.0, "volume": 3.0}}
```

- 同价位减少数量会保留序列号，因此保留时间优先权；
- 修改价格或增加数量会以新序列号重新入簿（失去优先权），`/write` 响应返回之后使用的序列号；
- 新价格会与对手方成交的修改会被拒绝（`amend_would_cross`）：挂单簿永不交叉，吃单仍需先撤后下；
- 冰山单的 `volume` 是新的**总未成交量**，显示档位保留；停在场外的止损单保留序列号并继续停放；
- 移动止损用 `order.trail_offset` 表示新偏移；回传当前派生 `stop_price` 可接受，不同的非零触发价
  会被拒绝；Peg 订单不接受 `price`，非零 `order.peg_offset` 表示新偏移（`0` 保持当前值）；
- 新价格会与对手方成交的修改会被拒绝（`amend_would_cross`）：挂单簿永不交叉，吃单仍需先撤后下；
- 市价单不可修改（`unsupported_order`），请撤销后重新下单；
- 成功的修改会产生 `amended` 事件，同时携带旧、新订单序列号；`/write` 响应是本次修改的
  `market_seq`（重新入簿时也是新订单序列号）。被拒绝的修改仍会消耗一个序列号，并产生带原因的
  `rejected` 事件。

## 自成交防护

`SymbolConfig.stp` 选择策略：`off`（默认）、`cancel_resting`、`cancel_aggressor` 或
`cancel_both`。只有吃单方和挂单方都带非空且相同的 `owner_id` 时才会触发；匿名订单永不触发。

- `cancel_resting`：撤销同 owner 的挂单并继续吃单；
- `cancel_aggressor`：遇到第一个同 owner 挂单时撤销吃单剩余量；
- `cancel_both`：两者都撤销并停止。

FOK 预检使用相同策略；止损子单继承父单的 `owner_id`。STP 撤销产生 `cancelled` 事件，原因
`self_trade_prevented`；已经部分成交的吃单不应报告为 `rejected`。

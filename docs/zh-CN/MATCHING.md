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

市价单（`"live": "Market"`）忽略自身价格，扫遍对手方，剩余量撤销，从不挂单。

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

## 订单修改

`Amend` 修改一笔挂单的限价和/或未成交数量。请求与 `Cancel` 一样通过 方向、当前价格和序列号
定位订单，新字段均可选：

```json
{"Amend": {"order": {"live": "Limit", "side": "Buy", "price": 4000.0, "volume": 4.4, "id": 1, "sequence": 7}, "price": 4002.0, "volume": 3.0}}
```

- 同价位减少数量会保留序列号，因此保留时间优先权；
- 修改价格或增加数量会以新序列号重新入簿（失去优先权），`/write` 响应返回之后使用的序列号；
- 新价格会与对手方成交的修改会被拒绝（`amend_would_cross`）：挂单簿永不交叉，吃单仍需先撤后下；
- 冰山单、停在场外的止损单和市价单不可修改（`unsupported_order`），请撤销后重新下单；
- 成功的修改会产生 `amended` 事件，同时携带旧、新订单序列号；`/write` 响应是本次修改的
  `market_seq`（重新入簿时也是新订单序列号）。被拒绝的修改仍会消耗一个序列号，并产生带原因的
  `rejected` 事件。

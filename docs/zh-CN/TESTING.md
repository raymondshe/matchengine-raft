# 测试

## 测试套件

`cargo test` 会构建一个集成测试目标（`tests/cluster`）以及库内单元测试。

| 测试 | 验证内容 |
|---|---|
| `src/order_book/property_tests.rs` | 针对参考模型的随机差分测试，并混合覆盖 GTC/IOC/FOK/GTD/Day、市价、Post-Only、冰山和止损单，以及正费率、零费率和返佣费率下的全部扣费资产策略：可见订单簿、成交、优先级、无交叉盘、数量守恒、序列号/最后成交价状态和确定性重放 |
| `src/order_book/conformance_tests.rs` | 市价 FOK、无流动性市价单、冰山隐藏量与撤单/过期、挂起止损的 GTD/Day 过期，以及市价/IOC/止损子单成交的费用报告 |
| `tests/cluster/test_cluster.rs` | 3 节点 gRPC 集群：初始化、学习者、成员变更、读写、线性一致性读、领导者转发、缩容；测试持有并在成功或 panic 时关闭所有节点 |
| `tests/cluster/test_snapshot.rs` | 快照生成、日志 purge、学习者通过 gRPC 快照流安装已 purge 的状态，以及关闭日志计数策略时的周期快照定时器 |
| `tests/cluster/test_storage.rs` | WAL 帧格式、撕裂尾部截断、分段轮转、按字节切分的追加与批次内轮转及失败回滚、暂存快照校验/清理、2 万条跨段日志重开、跨段 truncate/purge、vote/committed 持久化、快照构建/安装/重开、损坏文件拒绝 |
| `tests/cluster/test_restart.rs` | 完整节点重启后通过日志重放恢复，以及通过快照 + 更新的 WAL 尾部恢复订单簿、序列号、事件窗口和幂等表；另含 2000 单大日志重放和多分块（约 120 KB）快照启动 |
| `tests/cluster/test_match_results.rs` | 以 `--match-event-log` 启动的节点按日志顺序记录 rest/fill/cancel 事件，同时 `/write` 响应保持不变；第二个节点验证止损触发、GTD 过期和新增的 `/read` 键 |
| `tests/cluster/test_market_data.rs` | 以 `--market-data-log` 启动的节点发布重置快照、每个 taker 一条成交消息、BBO、批后深度总量和一根已收盘 K 线；断言同时检查 `market_seq` 锚点与 `/read marketdata_status` 计数器 |
| `demos/market_data_system` | 流名称解析与载荷、WebSocket 快照/合并/组合流/控制错误、单连接承载多个 symbol 并用实时 `UNSUBSCRIBE`/`SUBSCRIBE` 切换，以及嵌入式 Aeron 引擎→服务→WebSocket 测试（`aeron` feature） |
| `tests/cluster/test_events.rs` | `/write` 返回事件、`/events` 游标分页、`client_order_id` 重试不重复下单、`--max-events` 淘汰与 `/events/status` 计数、长轮询在并发写入时唤醒或在超时后返回空页、gRPC `StreamEvents` 推送成交与空闲心跳、bridge sidecar 发布事件并记录游标 |
| `tests/cluster/test_membership.rs` | 杀掉当前领导者：存活的投票节点接管已提交订单簿并接受新写入；写入后加入的学习者通过普通日志复制追赶、晋升成员并从自己的节点写入 |
| `tests/cluster/test_late_join.rs` | 带过期快照/WAL 的节点被移出集群、错过 40 次写入（期间领导者生成快照并 purge），再以学习者身份重新加入并从活动集群追赶 |
| `tests/cluster/test_exactness.rs` | 18 位小数的价格/数量经 `/write`、`/read`、`/events`、JSONL 行情、快照 v11 和重启后仍然精确，全程不经过 `f64` |
| `tests/cluster/test_symbols.rs` | HTTP 入口在进入 Raft 前以 `400` 拒绝错误标的的 `Place`/`Cancel`/`Amend` 和整个批次，并校验序列号/订单簿未变化；同一 entry 中先 `ConfigureSymbol` 再下单的批次仍可工作。另覆盖精度拒绝、配置锁定和快照重启 |
| `tests/cluster/test_fees.rs` | 吃单/挂单费率产生精确的成交费用，经 `/write`、`/events`、文件 sink 和行情 feed 输出，并在快照重启后保留 |
| `tests/cluster/test_fee_assets.rs` | QQQ/USD ETF/法币与 BTC/USDT 加密/稳定币元数据、tick/lot、quote 数量限价单归一化，以及 quote 资产和 received 资产扣费 |
| `tests/cluster/test_request_limits.rs` | 超大 JSON 请求体、畸形输入、非法/超长 `client_order_id`、超长 read key 和越界事件分页在进入 Raft 前以 `400`/`413` 拒绝 |
| `tests/cluster/test_health.rs` | `/health` 报告 gRPC 监听器、领导者和 WAL/状态机状态；`/ready` 在初始化前返回 `503`，选出领导者后返回 `200` |
| `tests/cluster/test_console.rs` | 三节点集群：内置控制台资源、节点/集群 API、危险操作拒绝，以及通过 `/console/api/actions/transfer-leader` 切换领导者 |
| `tests/cluster/test_batch_write.rs` | 多个请求在单个 Raft entry 中应用，逐项序列号、连续事件序号，以及空/嵌套/超大/重复 id 批次校验 |
| `tests/cluster/test_feature_matrix.rs` | 通过 `/write` 发送每种已宣传的订单类型并断言返回的事件类型；覆盖挂单 → 改单 → 部分成交 → 撤单以及到期/止损生命周期，证明正常操作使用显式事件而非拒绝；同时校验畸形请求体和未知 read key |
| `tests/cluster/test_client.rs` | 类型化的无领导者/HTTP 边缘错误、连接被丢弃后的重试/退避，以及领导者故障切换后向新领导者的转发 |
| `tests/cluster/test_failure.rs` | Raft 核心停止、注入的 WAL `ENOSPC`/`EIO`、写流期间 SIGKILL 和学习者快照安装期间 SIGKILL 时 fail-stop；两个 SIGKILL 用例启动真实二进制 |
| `tests/replay/*/run.sh` | Shell 驱动的回放矩阵，覆盖 GTC、IOC、FOK、GTD、Day、市价、Post-Only、冰山单、止损市价单、止损限价单、改单、撤单、撮合/吃单扣费、quote 扣费和费用溢出场景；每个用例对比生成的事件序列与已检入的 `expected-events.jsonl` |
| `fuzz/` | `cargo-fuzz` 目标：`OrderBook::execute_order`、WAL 字节解码、快照字节/内容解码和行情 wire 解码；每夜的定时工作流构建四个目标并做一次冒烟运行 |

回放用例不属于 `cargo test`；用 `./tests/replay/run-all.sh` 运行全部，或用
`./tests/replay/<case>/run.sh` 运行单个用例。它们使用 HTTP 端口 `24211-24226`（gRPC
`25211-25226`），每个用例一个端口，并把生成的撮合事件 JSONL 与仓库中的参考文件对比。
用例矩阵和重新生成步骤见 `tests/replay/README.md`。

模糊测试目标是独立的 nightly crate，见 `fuzz/README.md` 和 `.github/workflows/fuzz.yml`。

所有集成测试都使用唯一的临时数据目录。集群测试固定使用端口 `21001-21003`（gRPC
`22001-22003`），快照测试使用 `21101-21103`，重启测试使用 `21201-21205`，撮合结果测试使用
`21301`/`21302`、行情数据测试 `21303`、事件日志测试 `21304`、故障切换测试
`21401-21403`、学习者追赶测试 `21501-21502`、过期节点重入测试 `21601-21603`、精度往返测试 `21701`（gRPC `22701`）、标的配置测试 `21801-21802`（gRPC `22801-22802`）、费用测试 `21901`（gRPC `22901`）、费用资产测试 `21902` 和 `21924`（gRPC `22902`/`22924`）、请求限制测试 `21903`（gRPC `22903`）、批量写入测试 `21904`（gRPC `22904`）、健康检查测试 `21905`（gRPC `22905`）、控制台测试 `21906-21908`（gRPC `22906-22908`）、功能矩阵/生命周期测试 `21909-21911`（gRPC `22909-22911`）、客户端测试 `21912-21917`（gRPC `22912-22917`）和故障测试 `21918-21923`（gRPC `22918-22923`）；重跑前请先停止残留进程
（`pkill -f matchengine-raft`）。

## 回放订单

`replay_orders` 会启动一个启用 `--match-event-log` 的进程内节点，按 `/write_batch` 批次回放
订单 JSONL，然后把生成的撮合事件与参考文件对比：

```shell
cargo run --release --example replay_orders -- \
  --orders orders.jsonl \
  --expected expected-events.jsonl
```

订单文件每行是一个 `MatchEngineRequest` 或裸 `Order`（自动包装为 `Place`）；空行和 `#` 注释会被
忽略。`Batch` 行作为一个 entry 提交，普通请求按 `--batch-size`（默认 128）分组。默认比较会忽略
`node_id` 和 `event_id.log_index`；需要精确事件 id 时使用 `--strict-log-index`。`--actual` 可保留
生成的 JSONL，`--keep` 保留临时节点数据，其余选项见 `--help`。

`replay_orders_events` 是读取 `/events` 的版本：不启用本地事件文件，回放后分页读取复制事件日志
再与参考文件比较，用于验证下游消费者实际拿到的事件、游标和 id；`--events-limit` 控制每页大小。

```shell
cargo run --release --example replay_orders_events -- \
  --orders orders.jsonl \
  --expected expected-events.jsonl \
  --events-limit 100
```

运行全部用例的 `/events` 版本：`./tests/replay/run-all.sh events`。

仓库在 `tests/replay/` 下为每种订单类型或生命周期操作提供一个用例目录，每个目录包含
`orders.jsonl`、`expected-events.jsonl`、`run.sh` 和场景 README。矩阵覆盖 GTC、IOC、FOK、
GTD、Day、市价、Post-Only、冰山单、止损市价单、止损限价单、改单、撤单、撮合/吃单扣费、
quote 资产扣费和费用溢出。用 `./tests/replay/run-all.sh` 运行全部，或用
`./tests/replay/<case>/run.sh` 运行单个用例。

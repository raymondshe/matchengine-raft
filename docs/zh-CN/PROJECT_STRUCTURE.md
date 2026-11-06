# 项目结构

```
src/
├── main.rs                  # 服务器入口（参数、环境变量、存储配置）
├── client.rs                # 支持领导者转发、重试和超时的智能客户端
├── lib.rs                   # 节点组装、服务与关闭
├── app.rs                   # 应用状态
├── order_book/
│   ├── mod.rs               # 订单簿撮合引擎
│   └── fixed.rs             # 精确定点数与标的元数据
├── market_data/
│   ├── mod.rs               # tap、深度日志、影子订单簿、限流发布任务
│   └── wire.rs              # 64 字节二进制头 + 消息体（可直接交给 Aeron）
├── match_result/
│   └── mod.rs               # MatchEvent id + 可替换的 MatchResultSink（noop/memory/file）
├── network/
│   ├── api.rs               # 应用 HTTP 端点
│   ├── management.rs        # 管理 HTTP 端点
│   ├── health.rs            # /health 与 /ready 探针
│   ├── console.rs           # 内置控制台与受保护操作
│   ├── grpc/
│   │   ├── client.rs        # gRPC RaftNetworkV2（追加、投票、快照）
│   │   ├── server.rs        # gRPC Raft 服务
│   │   └── mod.rs           # Protobuf 类型与消息大小限制
│   └── mod.rs               # Raft 端口推导与传输限制
├── store/
│   ├── mod.rs               # 存储模块
│   ├── log_store.rs         # 分段 WAL + 投票/purge 状态
│   ├── state_machine.rs     # 状态机、事件日志 + 快照
│   ├── types.rs             # 请求/应答类型
│   └── config.rs            # 存储布局 / 每节点目录
proto/
└── raft.proto               # gRPC Raft 服务定义
examples/
├── sled.rs                  # 游乐场示例（本地 sled 目录，不安装）
├── dump_wal.rs              # WAL 分段的 JSONL 转储
├── dump_snapshot.rs         # 当前快照的 JSONL 转储
├── replay_orders.rs         # 回放订单并对比撮合结果 JSONL
└── replay_orders_events.rs  # 回放订单并从 POST /events 读取事件对比
demos/
├── order_system/            # 独立下单系统
└── market_data_system/      # 独立 Binance 风格 WebSocket 行情系统
tests/
├── cluster/
    ├── main.rs              # 单一集成测试目标
    ├── common.rs            # 唯一临时数据目录
    ├── test_cluster.rs      # 3 节点 gRPC 集群
    ├── test_snapshot.rs     # 快照生成/purge、学习者安装 + 周期定时器
    ├── test_storage.rs      # WAL 与快照格式
    ├── test_restart.rs      # 重启恢复：日志重放、快照 + WAL 尾部
    ├── test_match_results.rs # 撮合事件写入 JSONL 文件 sink
    ├── test_market_data.rs    # 行情 feed：快照、成交、BBO、深度、K 线
    ├── test_events.rs         # 事件日志：写响应、游标、保留、幂等
    ├── test_membership.rs     # 领导者故障切换 + 学习者从活动日志追赶
    ├── test_late_join.rs      # 过期节点重新加入并从活动成员追赶
    ├── test_exactness.rs      # 18 位小数经 API、事件、行情和快照重启
    ├── test_symbols.rs        # 单标的配置、网关标的守卫和配置锁定
    ├── test_fees.rs           # 撮合/吃单费用经 API、事件、行情和重启
    ├── test_fee_assets.rs     # QQQ/USD 与 BTC/USDT tick/lot、quote 数量和扣费
    ├── test_request_limits.rs # 恶意输入和请求大小限制
    ├── test_batch_write.rs    # 单 Raft entry 批量写入
    ├── test_health.rs         # /health 与 /ready 探针
    ├── test_console.rs        # 内置控制台与受保护的领导者操作
    ├── test_feature_matrix.rs # 公共 API 订单类型矩阵与生命周期事件
    ├── test_client.rs         # 客户端重试、类型化错误与故障切换转发
    └── test_failure.rs        # Raft/WAL 错误 fail-stop 与 SIGKILL 恢复
└── replay/
    ├── README.md              # 场景矩阵、端口和重新生成说明
    ├── run-all.sh             # 运行全部已检入回放用例
    ├── limit_orders/          # GTC 价格-时间优先与部分挂单
    ├── ioc_orders/            # IOC 部分成交与撤销
    ├── fok_orders/            # FOK 拒绝与全量成交
    ├── gtd_orders/            # GTD 到期与已过期拒绝
    ├── day_orders/            # Day 收盘清除与无收盘行为
    ├── market_orders/         # 市价扫描与剩余撤销
    ├── post_only_orders/      # Post-Only 挂单与穿价拒绝
    ├── iceberg_orders/        # 可见档与隐藏储备
    ├── stop_market_orders/    # 止损市价触发与子单
    ├── stop_limit_orders/     # 止损限价触发与子单
    ├── amend_orders/          # 改单优先级规则与拒绝原因
    ├── cancel_orders/         # 撤单生命周期与停靠止损单
    ├── fee_orders/            # 撮合/吃单费用、返佣和两种费用资产
    ├── fee_quote_orders/      # ETF/法币 quote 扣费与 quote 数量归一化
    ├── fee_overflow_orders/   # 名义金额溢出与结构化错误码
    └── fee_overflow_base_orders/ # 溢出发送时基础费用仍精确
benches/
├── matchengine.rs           # 订单簿微基准
├── book_replay.rs           # 纯订单簿扫描/撤单/BBO 与 p50/p99
├── level_book.rs            # 按价格档桶原型对比
├── apply_path.rs            # 每条 apply 记录的堆分配
├── e2e.rs                   # 请求 -> Raft 提交 -> apply -> 响应
├── feed_load.rs             # 行情 feed 负载测试
├── storage.rs               # WAL 追加/读取/恢复
├── p0_bench.rs              # 锁/扫描 A/B 基准
└── bench_support/           # 共享 warm/cold p50/p99 计时工具
```

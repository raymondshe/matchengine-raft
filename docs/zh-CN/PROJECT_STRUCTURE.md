# 项目结构

仓库由四个同级的 Rust 项目组成。每个项目都有自己的 `Cargo.toml`、锁文件和 README；跨项目的文档、脚本和 Dockerfile 保留在仓库根目录。

```text
.
├── match_engine/                 # Raft 复制撮合引擎
│   ├── Cargo.toml
│   ├── src/                      # 引擎、订单簿、存储、网络、行情
│   ├── proto/raft.proto          # gRPC Raft 服务
│   ├── examples/                 # 转储与回放工具
│   ├── benches/                  # 订单簿、apply、存储和 e2e 基准
│   ├── tests/cluster/            # 集群、恢复、快照和故障测试
│   ├── tests/replay/             # 已检入的订单类型回放矩阵
│   ├── fuzz/                     # cargo-fuzz crate（独立于主 package）
│   ├── docker/                   # 引擎镜像定义
│   └── web/console/              # 内置运维控制台资源
├── k8s_control_plane/            # CRD 管理器、operator、渲染器和 GUI
│   ├── Cargo.toml
│   ├── src/
│   ├── static/
│   ├── deploy/                   # CRD、RBAC 和 operator manifest
│   └── examples/                 # 示例 shard 目录
├── market_data_system/           # Aeron/JSONL 消费端与 WebSocket 行情服务
│   ├── Cargo.toml
│   ├── src/
│   ├── tests/
│   ├── benches/
│   └── web/
├── order_system/                 # 独立演示下单系统
│   ├── Cargo.toml
│   ├── src/
│   └── web/
├── docs/                         # 跨项目设计、API、runbook 和性能文档
├── scripts/                      # 集群、实验环境和 demo 编排
├── .github/workflows/            # CI、fuzz 和 benchmark workflow
├── README.md
├── README_CN.md
├── AGENTS.md
├── TODO.md
└── LICENSE
```

## 引擎目录（`match_engine/`）

```text
src/
├── main.rs                  # 服务器入口（参数、环境变量、存储配置）
├── client.rs                # 领导者感知客户端（重试、超时）
├── lib.rs                   # 节点组装、服务与关闭
├── app.rs                   # 应用状态
├── order_book/              # 撮合引擎、定点数和一致性测试
├── market_data/             # tap、影子订单簿、wire 格式和 Aeron 出口
├── match_result/            # 事件 id、sink 和复制事件日志
├── network/                 # HTTP API、管理接口、控制台和 gRPC Raft
└── store/                   # 分段 WAL、状态机、快照和配置
```

引擎命令在 `match_engine/` 下执行（或在仓库根目录使用 `--manifest-path match_engine/Cargo.toml`）。其他独立项目同理。

# ⚡ match_engine

[Matchengine Raft](../README_CN.md) 项目的 Rust 引擎 crate，负责复制订单簿、WAL/快照、gRPC Raft
传输、事件日志和行情 feed。五项目总览、演示、Kubernetes 控制平面、安全边界和性能总表见
[根 README](../README_CN.md)。

## 🚀 构建与运行

需要 Rust 1.89+（edition 2024）。构建脚本自带 `protoc`，无需系统安装。

```shell
cargo build --release
cargo run --release --bin matchengine-raft -- --id 1 --http-addr 127.0.0.1:21001
```

HTTP API 监听 `--http-addr`；Raft 节点间 RPC 使用 gRPC，端口为 HTTP 端口 + 1000。没有
`--transport` 开关。二进制处理 SIGTERM/SIGINT，并通过 `MatchEngineNode::run_until` 优雅退出；
在容器中作为 PID 1 时这一点很重要。

单节点必须用空 JSON 数组初始化：

```shell
curl -s -X POST http://127.0.0.1:21001/init -H 'content-type: application/json' -d '[]'
```

启动四节点本地集群并保留以便检查：

```shell
../scripts/open-cluster.sh
../scripts/test-cluster.sh status
../scripts/test-cluster.sh stop
```

手动启动、成员变更、故障演练和恢复见 [`docs/zh-CN/RUNBOOK.md`](../docs/zh-CN/RUNBOOK.md)。
OrbStack Linux 实验环境见 [`docs/ORBSTACK_LAB.md`](../docs/ORBSTACK_LAB.md)。

## ⚙️ 引擎配置

| 设置 | 作用 |
|---|---|
| `--id`、`--http-addr` | Raft 节点 ID 和 HTTP 监听地址；gRPC 为 HTTP 端口 + 1000 |
| `--data-dir`、`--instance-prefix` | 持久化根目录和节点目录前缀（默认 `/tmp/matchengine-raft`、`matchengine`） |
| `--id-offset` | 同时加到 Raft 节点 ID 和数据目录后缀；迁移 shadow 使用不重叠范围 |
| `--read-policy local_lease\|lease\|read_index` | leader 一致读路径；默认 `local_lease`，`read_index` 强制新 quorum 往返 |
| `--max-inflight-writes N` | 每节点 `/write` 和 `/write_batch` 准入上限（默认 1024，`0` 关闭） |
| `--max-wal-append-bytes N` | 把一次 apply 批次切成字节有界 WAL 写入（默认 8 MiB，`0` 关闭） |
| `--snapshot-interval-ms N` | 快照定时器；`0` 只关闭定时器，不关闭日志条数策略 |
| `--match-event-log FILE` | 节点本地 JSONL 调试 sink，不复制 |
| `--market-data-log FILE` | 确定性行情 JSONL sink |
| `--async-wal` | 原型异步 WAL；需要 `--features async-wal` 和 `RAFT_WAL_FSYNC_MS=0` |

关键环境变量：

- `RAFT_WAL_FSYNC_MS` — 未设置 = 不 fsync（默认；要求至少 4 个投票成员），`0` = 每次追加，
  `N > 0` = 每 N ms 周期。
- `RAFT_CORE_WORKERS` — 正数时 OpenRaft 运行在独立 `raft-worker` runtime。
- `MATCHENGINE_READ_POLICY`、`MATCHENGINE_MAX_INFLIGHT_WRITES` — 对应 flag 的默认值。
- `MATCHENGINE_ASYNC_WAL` — 启用原型异步 WAL。

Cargo features：`async-wal`、`market-data-aeron`（Aeron 出口）和 `profiling`（OpenRaft 运行时直方图）。

## 🧱 引擎内部

1. **Raft 存储** ([`src/store/`](./src/store/))
   - `log/*.wal` 分段校验 WAL，支持撕裂尾部恢复。
   - `log/state` 原子重写 vote/purge 状态。
   - 内存状态机由原子 `snapshot/current.snap` 持久化。

2. **网络层** ([`src/network/`](./src/network/))
   - gRPC Raft RPC：append、vote、pre-vote、快照安装和 stream append。
   - 每个对端独立的复制、心跳和快照通道。
   - 应用/管理 HTTP API、内置控制台、健康与就绪探针。

3. **撮合引擎** ([`src/order_book/`](./src/order_book/))
   - 价格-时间优先订单簿，支持 GTC/IOC/FOK/GTD/Day、市价、市价转限价、Post-Only、冰山单、
     止损市价/限价、移动止损、Peg 和 STP。
   - 确定性 `apply`；撮合路径不读墙上时钟。
   - `OrderBook::execute_order` 是完整生命周期；`place_order` 只返回成交。

4. **行情出口** ([`src/market_data/`](./src/market_data/))
   - `apply` 把撮合事件和深度日志交给有界 `try_send` tap。
   - leader-gated tokio 任务维护影子订单簿、BBO/OHLC 和 wire v7 编码。
   - JSONL 调试 sink 和 feature 门控的 Aeron publisher（2001–2007 流）。

## 🧪 测试与文档

```shell
cargo test --locked
cargo test --features async-wal --test cluster async_wal --locked
cargo test --features market-data-aeron --lib market_data::aeron --locked
```

- [API 参考](../docs/zh-CN/API.md)
- [存储、WAL 与恢复](../docs/zh-CN/STORAGE.md)
- [撮合与改单](../docs/MATCHING.md)
- [事件交付](../docs/MATCH_RESULTS.md)
- [行情计划](../docs/MARKET_DATA_PLAN.md)
- [测试](../docs/zh-CN/TESTING.md)
- [性能方法与基线](../docs/PERFORMANCE.md)
- [项目结构](../docs/zh-CN/PROJECT_STRUCTURE.md)
- [路线图](../TODO.md)

引擎 crate 是自包含的；独立演示和控制平面位于上一级，各自有 manifest 和 README。

## 📄 License

MIT，见 [`LICENSE`](./LICENSE)。项目最初 fork 自
[example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv)
（[OpenRaft](https://github.com/datafuselabs/openraft) 项目），原始声明继续适用于派生代码。

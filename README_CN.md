# Match Engine Raft

这是一个基于 [OpenRaft](https://github.com/datafuselabs/openraft) 构建的分布式键值存储和撮合引擎的实践实现。本项目演示了如何使用 [Sled](https://github.com/spacejam/sled) 作为底层存储引擎，在磁盘上实现 Raft 日志和快照的持久化存储。

> **注意**: 更详细的中文指南请参考 [GUIDE_cn.md](./GUIDE_cn.md)。

## 目录

- [功能特性](#功能特性)
- [架构概述](#架构概述)
- [快速开始](#快速开始)
- [运行集群](#运行集群)
- [项目结构](#项目结构)
- [存储实现](#存储实现)
- [集群管理](#集群管理)
- [API 参考](#api-参考)

## 功能特性

- **持久化 Raft 存储**: 使用 Sled（高性能嵌入式键值存储）持久化 Raft 日志和状态
- **快照管理**: 状态机快照以文件形式存储在磁盘上，便于高效恢复
- **撮合引擎**: 包含一个简单的订单簿撮合引擎作为示例应用
- **HTTP 服务器**: 基于 [Actix-web](https://docs.rs/actix-web) 构建，同时提供内部 Raft API 和外部应用 API
- **智能客户端**: Rust 客户端自动跟踪并将请求重定向到当前领导者

## 架构概述

本项目扩展了 [example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv)，增加了持久化存储能力：

```
                    ┌─────────────────────────────────────────┐
                    │           应用层                         │
                    │  (KV 存储 + 订单簿撮合引擎)             │
                    └─────────────────────┬───────────────────┘
                                          │
                    ┌─────────────────────▼───────────────────┐
                    │           OpenRaft 核心                  │
                    │  (共识、复制、领导者选举)                 │
                    └─────────────────────┬───────────────────┘
                                          │
              ┌───────────────────────────┼───────────────────────────┐
              │                           │                           │
    ┌─────────▼─────────┐     ┌─────────▼─────────┐     ┌─────────▼─────────┐
    │   RaftStorage     │     │   RaftNetwork     │     │   HTTP Server      │
    │  (Sled + 文件)    │     │  (Reqwest 客户端) │     │  (Actix-web)       │
    └───────────────────┘     └───────────────────┘     └───────────────────┘
```

### 核心组件

1. **RaftStorage 实现** ([`store/`](./src/store/))
   - Raft 日志和投票存储在 Sled 中
   - 内存状态机，配合基于文件的快照
   - 每 500 条日志自动创建快照

2. **网络层** ([`network/`](./src/network/))
   - 用于复制和投票的内部 Raft RPC
   - 连接池，实现高效的节点间通信
   - 用于集群管理的管理 API

3. **撮合引擎** ([`matchengine/`](./src/matchengine/))
   - 包含买单和卖单的订单簿
   - 现实世界状态机应用的示例

## 快速开始

### 前置要求

- Rust 1.60 或更高版本
- Cargo

### 构建

```shell
cargo build
```

> **注意**: 生产环境构建可以添加 `--release`，但本项目主要作为示例，不建议在生产环境中直接使用，需要进一步加固。

## 运行集群

### 使用测试脚本快速启动

查看集群运行的最简单方式是运行提供的测试脚本：

```shell
./test-cluster.sh
```

该脚本使用 `curl` 命令演示了一个 3 节点集群，展示了客户端与集群之间的 HTTP 通信。

### 使用 Rust 测试

```shell
cargo test
```

这会运行与 `test-cluster.sh` 相同的场景，但使用 Rust 的 `ExampleClient`。

### 手动设置集群

#### 1. 启动节点

启动第一个节点：

```shell
./target/debug/raft-key-value --id 1 --http-addr 127.0.0.1:21001
```

启动其他节点（在不同的终端中）：

```shell
./target/debug/raft-key-value --id 2 --http-addr 127.0.0.1:21002
./target/debug/raft-key-value --id 3 --http-addr 127.0.0.1:21003
```

#### 2. 初始化集群

```shell
curl -X POST http://127.0.0.1:21001/init
```

这会将节点 1 初始化为领导者。

#### 3. 添加学习者（Learner）

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '[2, "127.0.0.1:21002"]' \
  http://127.0.0.1:21001/add-learner

curl -X POST -H "Content-Type: application/json" \
  -d '[3, "127.0.0.1:21003"]' \
  http://127.0.0.1:21001/add-learner
```

#### 4. 变更成员

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '[1, 2, 3]' \
  http://127.0.0.1:21001/change-membership
```

#### 5. 写入和读取数据

写入数据：

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '{"Set":{"key":"foo","value":"bar"}}' \
  http://127.0.0.1:21001/write
```

从任意节点读取数据：

```shell
curl -X POST -H "Content-Type: application/json" \
  -d '"foo"' \
  http://127.0.0.1:21002/read
```

您应该能够从任何节点读取到数据！

### 使用辅助脚本

项目还包含 `test.sh`，可以更方便地管理集群：

```shell
# 启动节点
./test.sh start-node 1

# 构建集群
./test.sh build-cluster

# 查看指标
./test.sh metrics 1

# 清理
./test.sh clean
```

## 项目结构

```
src/
├── bin/
│   └── main.rs              # 服务器入口点
├── client.rs                # 示例 Raft 客户端
├── lib.rs                   # 包含服务器设置的库
├── app.rs                   # 应用状态
├── matchengine/
│   └── mod.rs               # 订单簿撮合引擎
├── network/
│   ├── api.rs               # 应用 HTTP 端点
│   ├── management.rs        # 管理 HTTP 端点
│   ├── raft.rs              # Raft 内部 RPC 端点
│   ├── raft_network_impl.rs # RaftNetwork 实现
│   └── mod.rs
└── store/
    ├── mod.rs               # 状态机定义
    ├── store.rs             # RaftStorage 实现
    └── config.rs            # 存储配置
```

## 存储实现

### RaftStorage 特征

`RaftStorage` 特征实现是本项目的核心。它负责：

1. **Raft 状态**: 使用专用键存储在 Sled 中
2. **日志**: 以日志索引为键存储在 Sled 中
3. **状态机**: 内存状态机，配合基于文件的快照
4. **投票**: 持久化在 Sled 中以确保选举安全

### 快照配置

```rust
let mut config = Config::default().validate().unwrap();
config.snapshot_policy = SnapshotPolicy::LogsSinceLast(500);
config.max_applied_log_to_keep = 20000;
config.install_snapshot_timeout = 400;
```

- 每 500 条日志创建一个快照
- 保留最多 20,000 条日志后开始清理
- 快照文件存储在磁盘上，文件名中包含元数据

### 数据恢复

启动时，存储会：
1. 从磁盘加载最新的快照
2. 从 Sled 重放剩余的日志
3. 将状态机恢复到最新状态

## 集群管理

向集群添加节点包含 3 个步骤：

1. **通过 Raft 协议写入节点信息** 到存储
2. **添加为学习者（Learner）** 让其开始从领导者接收复制数据
3. **变更成员** 将学习者提升为正式的投票成员

> **注意**: Raft 本身不存储节点地址。本实现将节点信息存储在存储层，网络层引用存储来查找目标节点地址。

## API 参考

### Raft 内部 API

- `POST /raft/append` - 追加条目 RPC
- `POST /raft/snapshot` - 安装快照 RPC
- `POST /raft/vote` - 请求投票 RPC

### 管理 API

- `POST /init` - 初始化单节点集群
- `POST /add-learner` - 添加学习者节点
- `POST /change-membership` - 变更集群成员
- `GET /metrics` - 获取 Raft 指标

### 应用 API

- `POST /write` - 向状态机写入数据
- `POST /read` - 从状态机读取数据（本地读取）
- `POST /consistent_read` - 一致性读取（通过领导者）

## 未来工作

- [ ] 优化序列化（用 Protobuf/Avro 替换 JSON）
- [ ] 使用 gRPC 改进网络层
- [ ] 通过消息队列添加撮合结果分发
- [ ] 实现更多撮合算法
- [ ] 添加全面的测试和基准测试
- [ ] 增强客户端库

## 致谢

本项目是从 [OpenRaft](https://github.com/datafuselabs/openraft) 项目中的 [example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv) 分叉而来的。

特别感谢 Databend 社区和张延坡的支持。

## 许可证

请参考原始 OpenRaft 项目获取许可信息。

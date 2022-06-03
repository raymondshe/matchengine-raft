# OpenRaft实操分享

由于工作需要，一直对原子多播应用有非常浓厚的兴趣。通过一段时间的技术选型。我们非常幸运的得到了databOpenRaft实操分享end社群的热心支持。我也想通过我们的实际工作，对Openraft的未来应用尽一些微薄之力。

我的实践的上一篇文章反应了我们的选型过程，有兴趣的人可以看一下。[Raft in Rust (原子多播+撮合引擎）](http://t.csdn.cn/jcOnv )这篇文章更多的是想说明我们在使用OpenRaft的实际问题，并且通过我们的实现，揭秘OpenRaft的一些机制。

-------

## 代码仓库

大家在使用OpenRaft的时候，我相信很多人都查看了手册：
[Getting Started - openraftThe openraft user guide.](https://datafuselabs.github.io/openraft/getting-started.html)

当然，这是一个非常优秀的手册。我们从这个手册里，会学习到如何使用OpenRaft实现自己的应用。而且，[openraft/example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv)这个例子确实能够很好的说明如何实现一个简单的应用。但是，这个例子是使用的内存来做持久化实现。当然内存不会真正做持久化，所以很容易在节点退出后，丢失状态。而我们真正需要的示例是一个能够持久化的例子。


另外一个实例就是 [databend/metasrv](https://github.com/datafuselabs/databend/tree/main/metasrv)而这个示例里面，我们可以看到一个完整的 metadata 存储服务的实现。实际上，metasrv本身是一个类似于etcd的kv存储。存储整个databend集群的metadata。这个示例里面，metasrv使用了sled作为底层存储。sled寄存储log，有存储 statemachine 的状态。这个例子，statemachine所有的更新都直接在sled存储里通过sled的事物完成了。所以，对于如何存储snapshot这个问题，我们并不太容易看清楚。所以snapshot的产生和传递主要是在节点间同步的时候使用。

这里，大家可以看到我们开放的源代码。虽然这个示例是基于 example-raft-kv 示例，没有达到 metasrv的生产强度。但是我们还是非常全面的表现出了 openraft 对 log, snapshot 处理的行为和能力。

[GitHub - raymondshe/matchengine-raft](https://github.com/raymondshe/matchengine-raft)

-------
## 应用场景

和metasrv的场景不同。我们需要我们的statemachine尽量在内存里面更新，尽量少落盘。虽然sled本地落盘的速度也很快，但是内存操作的速度会更快。所以，我们基本上就是这样进行操作的。

![image info](./match_engine.png)

 *[总体设计图](https://excalidraw.com/#json=kSzwFGNGr_WNjytPO65RN,CjvsM4m_3efHnSIGK37Sow)*

所以在这个图里面，大家可以看到日志是通过sled进行存储的。而这些日志由于通过Raft协议，实际上他们在每台机器上的顺序是一致的。所以，不同的matchengine-raft实例，在相同的日志流情况下，对状态机的操作就是一致的。所以，不管我们从哪一个日志开始写snapshot，通过加载snapshot并且回放后续的日志，我们都可以恢复到最新状态。

按照设计图中显示，当前StateMachine的状态是处理了第9个日志里的消息。这时候，系统保存了所有的消息到Sled。并且在第3个消息的时候落盘了一次snapshot，并且在低6个消息的时候落盘了一次snapshot。如果这台机器当机，我们是可以从编号为3的snapshot恢复状态机，并且继续处理3,4,5,6,7,8,9这6条消息来恢复当前状态。当然，我们也可以从编号为6的snapshot恢复状态机，并且继续处理7,8,9这3条消息来恢复当前状态。

当然我们可以选择多少个消息进行一次落盘。当然落盘的次数越多越可靠，但是性能影响比较大。好在snapshot的生成和落盘是异步的方式做的。

有兴趣的朋友可以看一下akka的 [EventSroucing](https://doc.akka.io/docs/akka/current/typed/index-persistence.html "EventSroucing") 模式。这种模式和Raft单节点非常相像。不同的是OpenRaft强调多实例一致性，而Akka 则提供了非常多的方式来存储 Log( Journal )和 Snapshot.

-------

## 实现细节

 谈到实现细节。我们还是回到官方文档 [geting-started](https://datafuselabs.github.io/openraft/getting-started.html "geting-started") 来。我们也按照这个文档的顺序进行说明。

Raft对于从应用开发着的角度，我们可以简化到下面的这张图里。Raft的分布式共识就是要保证驱动状态机的指令能够在 Log 里被一致的复制到各个节点里。

![image info](https://datafuselabs.github.io/openraft/images/raft-overview.png)​


Raft有两个重要的组成部分：

* 如何一致的在节点之间复制日志
* 并且在状态机里面如何消费这些日志

基于OpenRaft 实现我们自己的 Raft 应用其实并不复杂，只需要一下三部分：

* 定义好客户端的请求和应答
* 实现好存储 RaftStore 来持久化状态
* 实现一个网络层，来保证 Raft 节点之间能相互传递消息。

好，那我们就开始吧：

### 1. 定义好客户端的请求和应答
--------------------

请求有可能是客户端发出的驱动 Raft 状态机的数据，而应答则是 Raft 状态机会打给客户端的数据。

请求和应答需要实现 AppData 和 AppDataResponse 这里，我们的实现如下：

```rust
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ExampleRequest {
    Set { key: String, value: String },
    Place { order: Order },
    Cancel { order: Order}
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExampleResponse {
    pub value: Option<String>,
}
```

这两个类型完全是应用相关的，而且和RaftStrage的状态机实现相关。

1. 这里，Set是 example-raft-kv 原示例就有的命令。
2. 大家也注意到了，命令都是对状态机有驱动的命令。也就是会对状态机状态造成改变的命令。如果我们需要取状态机内部数据的值返回给客户端。我们大可不必定义到这里。

--------
### 2. 实现 RaftStorage

这是整个项目非常关键的一部分。

只要实现好 trait RaftStorage，我们就把数据存储和消费的行为定义好。RaftStoreage可以包装一个像 [RocksDB](https://docs.rs/rocksdb/latest/rocksdb/ "RocksDB"), [Sled](https://github.com/spacejam/sled "Sled") 的本地 KV 存储或者远端的 SQL DB。

RaftStorage 定义了下面一些API


* 读写Raft状态，比方说 term，vote (term：任期，vote：投票结果）

```rust
fn save_vote(vote:&Vote) 
fn read_vote() -> Result<Option<Vote>>
```

* 读写日志

```rust 
fn get_log_state() -> Result<LogState> fn try_get_log_entries(range) -> Result<Vec<Entry>> 
fn append_to_log(entries) 
fn delete_conflict_logs_since(since:LogId) 
fn purge_logs_upto(upto:LogId)
```

* 将日志的内容应用到状态机

``` rust
fn last_applied_state() -> Result<(Option<LogId>,Option<EffectiveMembership>)> 
fn apply_to_state_machine(entries) -> Result<Vec<AppResponse>>
```

* 创建和安装快照（snapshot） 

```rust
fn build_snapshot() -> Result<Snapshot> fn get_current_snapshot() -> Result<Option<Snapshot>> 
fn begin_receiving_snapshot() -> Result<Box<SnapshotData>> 
fn install_snapshot(meta, snapshot)
```

在 [ExampleStore](https://github.com/datafuselabs/openraft/blob/main/example-raft-kv/src/store/mod.rs "ExampleStore")，这些内存化存储行为是非常明确简单的。而我们不是要真正落盘了吗？那我们就看一下 matchengine-rust 是怎么实现的。

这里是 [matchengine-raft/src/store](https://github.com/raymondshe/matchengine-raft/tree/master/src/store "matchengine-raft/src/store")

| 接口 | 实现方式 |
| ------------- |-------------| 
|Raft状态 | sled存储，使用专门的key来读写raft状态。|
|日志 | sled存储，使用 log_index 来唯一标识一个 Log Entity |
|应用状态机 | 状态机里面一部分是业务数据，但是一部分是raft的数据。业务数据主要是订单薄。 |   
|快照 | 快照完全是通过文件进行存储的，而且文件的名字就保留了快照的全部meta信息。|

我们说明一些设计要点

### ExampleStore的数据

ExchangeStore 里面主要是包含下面的成员变量。

```rust
#[derive(Debug)]
pub struct ExampleStore {

    last_purged_log_id: RwLock<Option<LogId<ExampleNodeId>>>,

    /// The Raft log.

    pub log: sled::Tree,
    
    /// The Raft state machine.
    pub state_machine: RwLock<ExampleStateMachine>,

    /// The current granted vote.
    vote: sled::Tree,

    snapshot_idx: Arc<Mutex<u64>>,

    current_snapshot: RwLock<Option<ExampleSnapshot>>,

    config : Config,

    pub node_id: ExampleNodeId,

}
```

帮助我们落盘的成员主要是 log, vote。而需要产生snapshot进行落盘的所有内容都在 state_machine.

1. last_purged_log_id： 这是最后删除的日志ID。删除日志本身可以节约存储，但是，对我们来讲，我了保证数据存储的安全。在删除日志之前，我们必须有这条日志index大的snapshot产生。否则，我们就没有办法通过snapshot来恢复数据。

2. log: 这是一个 sled::Tree，也就是一个map。如果看着部分代码的话，我们就可以清楚的明白log对象的结构。key是一个log_id_index的 Big Endian 的字节片段。value是通过serd_json进行序列化的内容。

```rust
    #[tracing::instrument(level = "trace", skip(self, entries))]
    async fn append_to_log(
        &mut self,
        entries: &[&Entry<ExampleTypeConfig>],
    ) -> Result<(), StorageError<ExampleNodeId>> {

        let log = &self.log;
        for entry in entries {
            log.insert(entry.log_id.index.to_be_bytes(), IVec::from(serde_json::to_vec(&*entry).unwrap())).unwrap();
        }
        Ok(())
    }

```

3. state_machine: 这里就是通过日志驱动的所有状态的集合。
```rust
#[derive(Serialize, Deserialize, Debug, Default, Clone)]pub struct ExampleStateMachine {

    pub last_applied_log: Option<LogId<ExampleNodeId>>,

    // TODO: it should not be Option.
    pub last_membership: EffectiveMembership<ExampleNodeId>,

    /// Application data.
    pub data: BTreeMap<String, String>,

    // ** Orderbook
    pub orderbook: OrderBook,

}
```

StateMachine里面最重要的数据就是 orderbook这部分就是撮合引擎里面重要的订单表。存放买方和卖方的未成交订单信息。这是主要的业务逻辑。 data 这部分是原来例子中的kv存储。我们还在这里没有删除。

这里last_applied_log, last_menbership 这些状态和业务逻辑没有太大关系。所以，如果您要实现自己的StateMachine。还是尽量和例子保持一致。主要是因为这两个状态是通过apply_to_state_machine这个接口更新。也正好需要持久化。如果需要进一步隐藏raft的细节，我们还是建议openraft能将这两个状态进一步进行隐藏封装。

对state_machine的落盘操作主要集中在这里：[store/store.rs](https://github.com/raymondshe/matchengine-raft/blob/master/src/store/store.rs "store/store.rs")。有兴趣的可以看一下。这里面比较有意思的问题是orderbook本身无法被默认的serde_json序列化/反序列化。所以我们才在 [matchengine/mod.rs](https://github.com/raymondshe/matchengine-raft/blob/master/src/matchengine/mod.rs "matchengine/mod.rs") 加了这段代码：

```rust
pub mod vectorize {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::iter::FromIterator;
 
    pub fn serialize<'a, T, K, V, S>(target: T, ser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: IntoIterator<Item = (&'a K, &'a V)>,
        K: Serialize + 'a,
        V: Serialize + 'a,
    {
        let container: Vec<_> = target.into_iter().collect();
        serde::Serialize::serialize(&container, ser)
    }
 
    pub fn deserialize<'de, T, K, V, D>(des: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: FromIterator<(K, V)>,
        K: Deserialize<'de>,
        V: Deserialize<'de>,
    {
        let container: Vec<_> = serde::Deserialize::deserialize(des)?;
        Ok(T::from_iter(container.into_iter()))
    }
}
 
/// main OrderBook structure
#[derive(Clone, Default, Serialize, Deserialize, Debug)]
pub struct OrderBook {
    #[serde(with = "vectorize")]
    pub bids: BTreeMap<BidKey, Order>,
    #[serde(with = "vectorize")]
    pub asks: BTreeMap<AskKey, Order>,
    pub sequance: u64,
}
```

4. vote: 就是对最后一次vote的存储。具体请看, 铁这段代码倒不是因为这段代码有多重要，只是由于代码比较简单，看可以少些一些说明：

```rust
#[tracing::instrument(level = "trace", skip(self))]
    async fn save_vote(&mut self, vote: &Vote<ExampleNodeId>) -> Result<(), StorageError<ExampleNodeId>> {
        self.vote.insert(b"vote", IVec::from(serde_json::to_vec(vote).unwrap())).unwrap();
        Ok(())
    }
 
    async fn read_vote(&mut self) -> Result<Option<Vote<ExampleNodeId>>, StorageError<ExampleNodeId>> {
        let value = self.vote.get(b"vote").unwrap();
        match value {
            None => {Ok(None)},
            Some(val) =>  {Ok(Some(serde_json::from_slice::<Vote<ExampleNodeId>>(&*val).unwrap()))}
        }
    }
```
但是这儿确实有个小坑，之前我没有注意到vote需要持久化，开始调试的时候产生了很多问题。知道找到openraft作者 Zhang Yanpo才解决。也是出发我想开源这个 openraft 文件持久化实现的诱因吧。感谢 Zhang Yanpo, 好样的。

5. 其他的成员变量其实没什么太好说的了。和原例子一样。

### 对日志和快照的控制

日志，快照相互配合，我们可以很好的持久化状态，并且恢复最新状态。多久写一次快照，保存多少日志。在这里我们使用了下面的代码。
```rust
let mut config = Config::default().validate().unwrap();
    config.snapshot_policy = SnapshotPolicy::LogsSinceLast(500);
    config.max_applied_log_to_keep = 20000;
    config.install_snapshot_timeout = 400;
```  

强烈建议大家看一下 [Config in openraft::config - Rust](https://docs.rs/openraft/latest/openraft/config/struct.Config.html "Config in openraft::config - Rust")

重点看 snapshot_policy, 代码里可以清楚的标识，我们需要500次log写一次快照。也就是 openraft会调用 build_snapshot 函数创建snapshot。原示例里，snapshot只是在内存里保存在 current_snapshot 变量里。而我们需要真实的落盘。请注意这段代码的 self.write_snapshot()

```rust
#[async_trait]
impl RaftSnapshotBuilder<ExampleTypeConfig, Cursor<Vec<u8>>> for Arc<ExampleStore> {
    #[tracing::instrument(level = "trace", skip(self))]
    async fn build_snapshot(
        &mut self,
    ) -> Result<Snapshot<ExampleTypeConfig, Cursor<Vec<u8>>>, StorageError<ExampleNodeId>> {
        let (data, last_applied_log);
 
        {
            // Serialize the data of the state machine.
            let state_machine = self.state_machine.read().await;
            data = serde_json::to_vec(&*state_machine)
                .map_err(|e| StorageIOError::new(ErrorSubject::StateMachine, ErrorVerb::Read, AnyError::new(&e)))?;
 
            last_applied_log = state_machine.last_applied_log;
        }
 
        let last_applied_log = match last_applied_log {
            None => {
                panic!("can not compact empty state machine");
            }
            Some(x) => x,
        };
 
        let snapshot_idx = {
            let mut l = self.snapshot_idx.lock().unwrap();
            *l += 1;
            *l
        };
 
        let snapshot_id = format!(
            "{}-{}-{}",
            last_applied_log.leader_id, last_applied_log.index, snapshot_idx
        );
 
        let meta = SnapshotMeta {
            last_log_id: last_applied_log,
            snapshot_id,
        };
 
        let snapshot = ExampleSnapshot {
            meta: meta.clone(),
            data: data.clone(),
        };
 
        {
            let mut current_snapshot = self.current_snapshot.write().await;
            *current_snapshot = Some(snapshot);
        }
 
        self.write_snapshot().await.unwrap();
 
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}
```

这下我们有了snapshot，当然snapshot一方面可以用来在节点之间同步状态。另一方面就是在启动的时候恢复状态。而 openraft 的实现非常好。实际上恢复状态只需要回复到最新的snapshot就行。只要本地日志完备，openraft 会帮助你调用 apply_to_statemachine 来恢复到最新状态。所以我们就有了restore函数。

```rust
#[async_trait]
impl Restore for Arc<ExampleStore> {
    #[tracing::instrument(level = "trace", skip(self))]
    async fn restore(&mut self) {
        tracing::debug!("restore");
        let log = &self.log;
 
        let first = log.iter().rev()
        .next()
        .map(|res| res.unwrap()).map(|(_, val)|
            serde_json::from_slice::<Entry<ExampleTypeConfig>>(&*val).unwrap().log_id);
 
        match first {
            Some(x) => {
                tracing::debug!("restore: first log id = {:?}", x);
                let mut ld = self.last_purged_log_id.write().await;
                *ld = Some(x);
            },
            None => {}
        }
 
        let snapshot = self.get_current_snapshot().await.unwrap();
 
        match snapshot {
            Some (ss) => {self.install_snapshot(&ss.meta, ss.snapshot).await.unwrap();},
            None => {}
        }
    }
}
```

大家注意一下snapshot的操作。当然，在这里，我们也恢复了last_purged_log_id。

当然store这个函数会在ExampleStore刚刚构建的时候调用。

 ```rust
    // Create a instance of where the Raft data will be stored.
    let es = ExampleStore::open_create(node_id);
    
    //es.load_latest_snapshot().await.unwrap();
 
    let mut store = Arc::new(es);
    
    store.restore().await;
```

### 如何确定RaftStorage是对的

请查阅 [Test suite for RaftStorage](https://github.com/datafuselabs/openraft/blob/main/memstore/src/test.rs "Test suite for RaftStorage"), 如果通过这个测试，一般来讲，OpenRaft就可以使用他了。

```rust
#[test] 
pub fn test_mem_store() -> anyhow::Result<()> { openraft::testing::Suite::test_all(MemStore::new) }
```

### RaftStorage的竞争状态

在我们的设计里，在一个时刻，最多有一个线程会写状态，但是，会有多个线程来进行读取。比方说，可能有多个复制任务在同时度日志和存储。

### 实现必须保证数据持久性

调用者会假设所有的写操作都被持久化了。而且Raft的纠错机制也是依赖于可靠的存储。

----------------------
## 3. 实现 RaftNetwork


为了节点之间对日志能够有共识，我们需要能够让节点之间进行通讯。trait RaftNetwork就定义了数据传输的需求。RaftNetwork的实现可以是考虑调用远端的Raft节点的服务

```rust
pub trait RaftNetwork<D>: Send + Sync + 'static where D: AppData { 
    async fn send_append_entries(&self, target: NodeId, node:Option<Node>, rpc: AppendEntriesRequest<D>) -> Result<AppendEntriesResponse>; 
    async fn send_install_snapshot( &self, target: NodeId, node:Option<Node>, rpc: InstallSnapshotRequest,) -> Result<InstallSnapshotResponse>; 
    async fn send_vote(&self, target: NodeId, node:Option<Node>, rpc: VoteRequest) -> Result<VoteResponse>; 
}
```

[ExampleNetwork](https://github.com/datafuselabs/openraft/blob/main/example-raft-kv/src/network/raft_network_impl.rs "ExampleNetwork") 显示了如何调用传输消息。每一个Raft节点都应该提供有这样一个RPC服务。当节点收到raft rpc，服务会吧请求传递给raft实例，并且通过[raft-server-endpoint](https://github.com/datafuselabs/openraft/blob/main/example-raft-kv/src/network/raft.rs "raft-server-endpoint")返回应答。

在实际情况下可能使用 [Tonic gRPC](https://github.com/hyperium/tonic "Tonic gRPC")是一个更好的选择。 [databend-meta](#L89 "databend-meta") 里有一个非常好的参考实现。

在我们的 matchengen-raft 实现里，我们解决了原示例中大量重连的问题。

1. 维护一个可服用量的client

这段代码在：[network/raft_network_impl.rs](https://github.com/raymondshe/matchengine-raft/blob/master/src/network/raft_network_impl.rs "network/raft_network_impl.rs")

```rust
     let clients = Arc::get_mut(&mut self.clients).unwrap();
     let client = clients.entry(url.clone()).or_insert(reqwest::Client::new());
```
2. 在服务器端引入keep_alive

这段代码在：[lib.rs](https://github.com/raymondshe/matchengine-raft/blob/master/src/lib.rs "lib.rs")
```rust
    // Start the actix-web server.
    let server = HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .wrap(Logger::new("%a %{User-Agent}i"))
            .wrap(middleware::Compress::default())
            .app_data(app.clone())
            // raft internal RPC
            .service(raft::append)
            .service(raft::snapshot)
            .service(raft::vote)
            // admin API
            .service(management::init)
            .service(management::add_learner)
            .service(management::change_membership)
            .service(management::metrics)
            // application API
            .service(api::write)
            .service(api::read)
            .service(api::consistent_read)
    }).keep_alive(Duration::from_secs(5));
```

这样的改动确实是对性能有一些提升。但是真的需要更快的话，我们使用grpc，甚至使用 reliable multicast，比方说 [pgm](https://datatracker.ietf.org/doc/html/rfc3208 "pgm")。

------------
## 4. 启动集群

由于我们保留了之前的 key/value 实现。所以之前的脚本应该还是能够工作的。而且之前的key/value 有了真正的存储。

为了能够运行集群：

* 启动三个没有初始化的raft节点;
* 初始化其中一台raft节点；
* 把其他的raft节点加入到这个集群里;
* 更新raft成员配置。
[example-raft-kv](https://github.com/datafuselabs/openraft/tree/main/example-raft-kv "example-raft-kv") 的readme文档里面把这些步骤都介绍的比较清楚了。

* 下面两个测试脚本是非常有用的：
[test-cluster.sh](https://github.com/datafuselabs/openraft/blob/main/example-raft-kv/test-cluster.sh "test-cluster.sh") 这个脚本可以简练的掩饰如何用curl和raft集群进行交互。在脚本里，您可以看到所有http请求和应答。



[test\_cluster.rs](https://github.com/datafuselabs/openraft/blob/main/example-raft-kv/tests/cluster/test_cluster.rs "test_cluster.rs") 这个rust程序显示了怎么使用ExampleClient操作集群，发送请求和接受应答。
 

这里我们要强调的是，在初始化raft集群的时候。我们需要上述的过程。如果集群已经被初始化，并且我们已经持久化了相应的状态 （menbership, vote, log) 以后，再某些节点退出并且重新加入，我们就不需要再过多干预了。

在使用metasrv启动meta service的时候，我也遇到了相同的情况。所以还是要先启动一个single node以保证这个节点作为种子节点被合理初始化了。

[Deploy a Databend Meta Service Cluster | Databend](https://databend.rs/doc/manage/metasrv/metasrv-deploy "Deploy a Databend Meta Service Cluster | Databend")

为了更好的启动管理集群，我们在项目里添加了 [test.sh](https://github.com/raymondshe/matchengine-raft/blob/master/test.sh "test.sh")。用法如下：


```bash
./test.sh <command> <node_id>
```

我们可以在不同阶段调用不同的命令。大家有兴趣的话可以看一下代码。这部分是主程序部分，包含了我们实现的所有命令。

```bash
echo "Run command $1"
case $1 in
"place-order")
    place_order $2
    ;;
"metrics")
    get_metrics $2
    ;;
 
"kill-node")
    kill_node $2
    ;;
"kill")
    kill
    ;;
"start-node")
    start_node $2
    ;;
"get-seq")
    rpc 2100$2/read  '"orderbook_sequance"'
    ;;
"build-cluster")
    build_cluster $2
    ;;
"change-membership")
	change_membership $2
    ;;
 
"clean")
    clean
    ;;
  *)
    "Nothing is done!"
    ;;
esac
```
# 未来的工作

当前我们实现的matchengine-raft只是为了示例怎么通过raft应用到撮合引擎这样一个对性能，稳定性，高可用要求都非常苛刻的应用场景。通过 raft 集群来完成撮合引擎的分布式管理。我们相信真正把这个玩具撮合引擎推向产品环境，我们还是需要进行很多工作：

1. 优化序列化方案，serd_json固然好，但是通过字符串进行编解码还是差点儿意思。至少用到bson或者更好的用 protobuf, avro等，提高编解码速度，传输和存储的开销。
2. 优化RaftNetwork, 在可以使用multi-cast的情况下使用pgm，如果不行，可以使用grpc。
3. 撮合结果的分发。这部分在很多情况下依赖消息队列中间件比较好。
4. 增加更多的撮合算法。这部分完全是业务需求，和openraft无关。我们就不在这个文章里讨论了。
5. 完善测试和客户端的调用。
6. 完善压测程序，准备进一步调优。

# 结论

通过这个简单的小项目，我们：

1. 实现了一个简单的玩具撮合引擎。
2. 验证了OpenRaft在功能上对撮合引擎场景的支持能力。
3. 给OpenRaft提供了一个基于 sled KV存储的日志存储的参考实现。
4. 给OpenRaft提供了一个基于本地文件的快照存储的参考实现。

给大家透露一个小秘密，SAP也在使用OpenRaft来构建关键应用。大家想想，都用到Raft协议了，一定是非常重要的应用。

对于 databend 社群的帮助，我表示由衷的感谢。作为一个长期工作在软件行业一线的老程序猿，看到中国开源软件开始在基础构建发力，由衷的感到欣慰。也希望中国开源社群越来越好，越来越强大，走向软件行业的顶端。
​
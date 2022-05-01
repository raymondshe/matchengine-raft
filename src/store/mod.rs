use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::sync::Arc;
use std::sync::Mutex;

use openraft::async_trait::async_trait;
use openraft::storage::LogState;
use openraft::storage::Snapshot;
use openraft::AnyError;
use openraft::EffectiveMembership;
use openraft::Entry;
use openraft::EntryPayload;
use openraft::ErrorVerb;
use openraft::LogId;
use openraft::RaftLogReader;
use openraft::RaftSnapshotBuilder;
use openraft::RaftStorage;
use openraft::SnapshotMeta;
use openraft::StateMachineChanges;
use openraft::StorageError;
use openraft::StorageIOError;
use openraft::ErrorSubject;
use openraft::Vote;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::RwLock;

use crate::ExampleNodeId;
use crate::ExampleTypeConfig;
use crate::matchengine::OrderBook;
use crate::matchengine::Order;
pub mod store;
pub mod log;
pub mod config;
use crate::store::config::Config;

#[derive(Debug)]
pub struct ExampleSnapshot {
    pub meta: SnapshotMeta<ExampleNodeId>,

    /// The data of the state machine at the time of this snapshot.
    pub data: Vec<u8>,
}

/**
 * Here you will set the types of request that will interact with the raft nodes.
 * For example the `Set` will be used to write data (key and value) to the raft database.
 * The `AddNode` will append a new node to the current existing shared list of nodes.
 * You will want to add any request that can write data in all nodes here.
 */
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ExampleRequest {
    Set { key: String, value: String },
    Place { order: Order },
    Cancel { order: Order}
}

/**
 * Here you will defined what type of answer you expect from reading the data of a node.
 * In this example it will return a optional value from a given key in
 * the `ExampleRequest.Set`.
 *
 * TODO: SHould we explain how to create multiple `AppDataResponse`?
 *
 */
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExampleResponse {
    pub value: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct StateMachineContent {
    pub last_applied_log: Option<LogId<ExampleNodeId>>,

    // TODO: it should not be Option.
    pub last_membership: EffectiveMembership<ExampleNodeId>,

    /// Application data.
    pub data: BTreeMap<String, String>,

    // ** Orderbook
    pub orders: Vec<Order>,

    pub sequance: u64
}

/**
 * Here defines a state machine of the raft, this state represents a copy of the data
 * between each node. Note that we are using `serde` to serialize the `data`, which has
 * a implementation to be serialized. Note that for this test we set both the key and
 * value as String, but you could set any type of value that has the serialization impl.
 */
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct ExampleStateMachine {
    pub last_applied_log: Option<LogId<ExampleNodeId>>,

    // TODO: it should not be Option.
    pub last_membership: EffectiveMembership<ExampleNodeId>,

    /// Application data.
    pub data: BTreeMap<String, String>,

    // ** Orderbook
    pub orderbook: OrderBook,
}

impl ExampleStateMachine {
    pub fn to_content(&self) -> StateMachineContent {
        let mut content = StateMachineContent {
            last_applied_log: self.last_applied_log,
            last_membership: self.last_membership.clone(),
            data: self.data.clone(),
            orders: vec!(),
            sequance: self.orderbook.sequance
        };

        let mut bids: Vec<Order> = self.orderbook.bids.values().cloned().collect();
        let mut asks: Vec<Order> = self.orderbook.asks.values().cloned().collect();

        content.orders.append(&mut bids);
        content.orders.append(&mut asks);
        return content;
    }

    pub fn from_content(&mut self, content: &StateMachineContent) {
            // ** Build from content **
                self.last_applied_log = content.last_applied_log;
                self.last_membership = content.last_membership.clone();
                self.data = content.data.clone();
                self.orderbook.asks.clear();
                self.orderbook.bids.clear();
                self.orderbook.sequance = content.sequance;
            
            for order in content.orders.clone()  {
                self.orderbook.insert_order(&order);
            }
    }
}


#[derive(Debug, Default)]
pub struct ExampleStore {
    last_purged_log_id: RwLock<Option<LogId<ExampleNodeId>>>,

    /// The Raft log.
    pub log: RwLock<BTreeMap<u64, Entry<ExampleTypeConfig>>>,

    pub sled : RwLock<Option<sled::Db>>,

    /// The Raft state machine.
    pub state_machine: RwLock<ExampleStateMachine>,

    /// The current granted vote.
    vote: RwLock<Option<Vote<ExampleNodeId>>>,

    snapshot_idx: Arc<Mutex<u64>>,

    current_snapshot: RwLock<Option<ExampleSnapshot>>,

    config : Config,

    pub node_id: ExampleNodeId,
}

#[async_trait]
impl RaftLogReader<ExampleTypeConfig> for Arc<ExampleStore> {
    async fn get_log_state(&mut self) -> Result<LogState<ExampleTypeConfig>, StorageError<ExampleNodeId>> {
        let log = self.log.read().await;
        let last = log.iter().rev().next().map(|(_, ent)| ent.log_id);

        let last_purged = *self.last_purged_log_id.read().await;

        let last = match last {
            None => last_purged,
            Some(x) => Some(x),
        };

        Ok(LogState {
            last_purged_log_id: last_purged,
            last_log_id: last,
        })
    }

    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send + Sync>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<ExampleTypeConfig>>, StorageError<ExampleNodeId>> {
        let log = self.log.read().await;
        let response = log.range(range.clone()).map(|(_, val)| val.clone()).collect::<Vec<_>>();
        Ok(response)
    }
}

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

            // ** Content for snapshot
            let content = state_machine.to_content();
            
            data = serde_json::to_vec(&content)
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

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

#[async_trait]
impl RaftStorage<ExampleTypeConfig> for Arc<ExampleStore> {
    type SnapshotData = Cursor<Vec<u8>>;
    type LogReader = Self;
    type SnapshotBuilder = Self;

    #[tracing::instrument(level = "trace", skip(self))]
    async fn save_vote(&mut self, vote: &Vote<ExampleNodeId>) -> Result<(), StorageError<ExampleNodeId>> {
        let mut v = self.vote.write().await;
        *v = Some(*vote);
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<ExampleNodeId>>, StorageError<ExampleNodeId>> {
        Ok(*self.vote.read().await)
    }

    #[tracing::instrument(level = "trace", skip(self, entries))]
    async fn append_to_log(
        &mut self,
        entries: &[&Entry<ExampleTypeConfig>],
    ) -> Result<(), StorageError<ExampleNodeId>> {

        self.write_log(entries).await;

        let mut log = self.log.write().await;
        for entry in entries {
            log.insert(entry.log_id.index, (*entry).clone());
        }
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn delete_conflict_logs_since(
        &mut self,
        log_id: LogId<ExampleNodeId>,
    ) -> Result<(), StorageError<ExampleNodeId>> {
        tracing::debug!("delete_log: [{:?}, +oo)", log_id);

        let mut log = self.log.write().await;
        let keys = log.range(log_id.index..).map(|(k, _v)| *k).collect::<Vec<_>>();
        for key in keys {
            log.remove(&key);
        }

        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn purge_logs_upto(&mut self, log_id: LogId<ExampleNodeId>) -> Result<(), StorageError<ExampleNodeId>> {
        tracing::debug!("delete_log: [{:?}, +oo)", log_id);
        {
            let mut ld = self.last_purged_log_id.write().await;
            assert!(*ld <= Some(log_id));
            *ld = Some(log_id);
        }

        if log_id.index % self.config.snapshot_per_events as u64 == 1 {
            self.build_snapshot().await?;
            self.write_file().await.unwrap();
            self.purge_logfile_upto(log_id).await?;
        }

        {
            let mut log = self.log.write().await;

            let keys = log.range(..=log_id.index).map(|(k, _v)| *k).collect::<Vec<_>>();
            for key in keys {
                log.remove(&key);
            }
        }

        Ok(())
    }

    async fn last_applied_state(
        &mut self,
    ) -> Result<(Option<LogId<ExampleNodeId>>, EffectiveMembership<ExampleNodeId>), StorageError<ExampleNodeId>> {
        let state_machine = self.state_machine.read().await;
        Ok((state_machine.last_applied_log, state_machine.last_membership.clone()))
    }

    #[tracing::instrument(level = "trace", skip(self, entries))]
    async fn apply_to_state_machine(
        &mut self,
        entries: &[&Entry<ExampleTypeConfig>],
    ) -> Result<Vec<ExampleResponse>, StorageError<ExampleNodeId>> {
        let mut res = Vec::with_capacity(entries.len());

        let mut sm = self.state_machine.write().await;

        for entry in entries {
            tracing::debug!(%entry.log_id, "replicate to sm");

            sm.last_applied_log = Some(entry.log_id);

            match entry.payload {
                EntryPayload::Blank => res.push(ExampleResponse { value: None }),
                EntryPayload::Normal(ref req) => match req {
                    ExampleRequest::Set { key, value } => {
                        sm.data.insert(key.clone(), value.clone());
                        res.push(ExampleResponse {
                            value: Some(value.clone()),
                        })
                    }
                    ExampleRequest::Place { order } => {
                        let mut o = order.clone();
                        let mr = sm.orderbook.place_order(&mut o);
                        res.push(ExampleResponse {
                            value: Some(o.sequance.to_string()),
                        })
                    }
                    
                    ExampleRequest::Cancel { order } => {
                        sm.orderbook.cancle(order);
                        res.push(ExampleResponse {
                            value: Some("OK".to_string()),
                        })
                    }                    
                },
                EntryPayload::Membership(ref mem) => {
                    sm.last_membership = EffectiveMembership::new(Some(entry.log_id), mem.clone());
                    res.push(ExampleResponse { value: None })
                }
            };
        }
        Ok(res)
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn begin_receiving_snapshot(&mut self) -> Result<Box<Self::SnapshotData>, StorageError<ExampleNodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    #[tracing::instrument(level = "trace", skip(self, snapshot))]
    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<ExampleNodeId>,
        snapshot: Box<Self::SnapshotData>,
    ) -> Result<StateMachineChanges<ExampleTypeConfig>, StorageError<ExampleNodeId>> {
        tracing::info!(
            { snapshot_size = snapshot.get_ref().len() },
            "decoding snapshot for installation"
        );

        let new_snapshot = ExampleSnapshot {
            meta: meta.clone(),
            data: snapshot.into_inner(),
        };

        // Update the state machine.
        {
            let content : StateMachineContent = 
            serde_json::from_slice(&new_snapshot.data).map_err(|e| {
                StorageIOError::new(
                    ErrorSubject::Snapshot(new_snapshot.meta.clone()),
                    ErrorVerb::Read,
                    AnyError::new(&e),
                )
            })?;

            // ** Build from content **
            let mut updated_state_machine = ExampleStateMachine::default();
            updated_state_machine.from_content(&content);
            
            let mut state_machine = self.state_machine.write().await;
            *state_machine = updated_state_machine;
        }

        // Update current snapshot.
        {
            let mut current_snapshot = self.current_snapshot.write().await;
            *current_snapshot = Some(new_snapshot);
        }
        self.write_file().await.unwrap();
        Ok(StateMachineChanges {
            last_applied: meta.last_log_id,
            is_snapshot: true,
        })
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<ExampleTypeConfig, Self::SnapshotData>>, StorageError<ExampleNodeId>> {
        tracing::debug!("get_current_snapshot: start");

        match &*self.current_snapshot.read().await {
            Some(snapshot) => {
                let data = snapshot.data.clone();
                Ok(Some(Snapshot {
                    meta: snapshot.meta.clone(),
                    snapshot: Box::new(Cursor::new(data)),
                }))
            }
            None => {
                let data = self.read_file().await;
                tracing::debug!("get_current_snapshot: data = {:?}",data);

                let data = match data {
                    Ok(c) => c,
                    Err(e) => return Ok(None),
                };
                
                let content : StateMachineContent = 
                serde_json::from_slice(&data).unwrap();

                let last_applied_log = content.last_applied_log.unwrap();
                tracing::debug!("get_current_snapshot: last_applied_log = {:?}",last_applied_log);

                let snapshot_idx = {
                    let mut l = self.snapshot_idx.lock().unwrap();
                    *l += 1;
                    *l
                };
        
                let snapshot_id = format!(
                    "{}-{}-{}",
                    last_applied_log.leader_id, last_applied_log.index, snapshot_idx
                );
                tracing::debug!("get_current_snapshot: {}",snapshot_id);

                let meta = SnapshotMeta {
                    last_log_id: last_applied_log,
                    snapshot_id: snapshot_id,
                };

                //self.install_snapshot(&meta, Box::new(Cursor::new(data.clone()))).await?;

                Ok(Some(Snapshot {
                    meta: meta,
                    snapshot: Box::new(Cursor::new(data)),
                }))
            }
        }
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }
}

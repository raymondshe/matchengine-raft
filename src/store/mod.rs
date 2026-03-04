use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::{Bound, RangeBounds};
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
use sled::{Db, IVec};

use crate::ExampleNodeId;
use crate::ExampleTypeConfig;
use crate::matchengine::OrderBook;
use crate::matchengine::Order;
pub mod config;
pub mod store;

use crate::store::config::Config;

/// A snapshot of the Raft state machine.
///
/// Contains both the metadata about when the snapshot was taken and
/// the serialized state machine data itself.
#[derive(Debug, Clone)]
pub struct ExampleSnapshot {
    /// Metadata about this snapshot, including the last log ID applied.
    pub meta: SnapshotMeta<ExampleNodeId>,

    /// The serialized data of the state machine at the time of this snapshot.
    pub data: Vec<u8>,
}

/// Application requests that can be applied to the Raft state machine.
///
/// These requests are submitted through the `/write` API endpoint and
/// go through the Raft consensus protocol before being applied to the
/// state machine on all nodes.
///
/// # Variants
///
/// * `Set` - Stores a key-value pair in the KV store
/// * `Place` - Adds an order to the order book
/// * `Cancel` - Removes an order from the order book
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ExampleRequest {
    /// Store a key-value pair in the distributed KV store.
    Set { key: String, value: String },
    /// Place a new order in the matching engine.
    Place { order: Order },
    /// Cancel an existing order from the matching engine.
    Cancel { order: Order}
}

/// Response returned from applying a request to the state machine.
///
/// This is the result of a successful write operation, containing
/// any output from applying the request.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExampleResponse {
    /// The value returned from the state machine application, if any.
    pub value: Option<String>,
}

/// Serialized form of the state machine for snapshot storage.
///
/// This structure captures the entire state of the application in a
/// format that can be easily serialized to JSON and written to disk.
/// It is used for creating and restoring from snapshots.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct StateMachineContent {
    /// The last log ID that was applied to the state machine.
    pub last_applied_log: Option<LogId<ExampleNodeId>>,

    /// The last known cluster membership configuration.
    // TODO: it should not be Option.
    pub last_membership: EffectiveMembership<ExampleNodeId>,

    /// Application KV store data.
    pub data: BTreeMap<String, String>,

    /// All active orders in the order book (both bids and asks).
    // ** Orderbook
    pub orders: Vec<Order>,

    /// The current sequence number for order placement.
    pub sequance: u64
}

/// The in-memory state machine of the Raft system.
///
/// This struct holds the complete state of the application, including:
/// - The Raft protocol state (last applied log, membership)
/// - The application KV store
/// - The order book matching engine
///
/// This state is replicated across all nodes in the cluster and
/// can be snapshotted to disk for efficient recovery.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct ExampleStateMachine {
    /// The ID of the last log entry applied to this state machine.
    pub last_applied_log: Option<LogId<ExampleNodeId>>,

    /// The last known cluster membership configuration.
    // TODO: it should not be Option.
    pub last_membership: EffectiveMembership<ExampleNodeId>,

    /// Application KV store data - arbitrary string key-value pairs.
    pub data: BTreeMap<String, String>,

    /// The order book for the matching engine.
    // ** Orderbook
    pub orderbook: OrderBook,
}

impl ExampleStateMachine {
    /// Converts the state machine to a serializable content format.
    ///
    /// This method extracts the data from the state machine and
    /// arranges it into a format suitable for JSON serialization.
    /// The order book is flattened into a single vector of orders.
    ///
    /// # Returns
    ///
    /// A `StateMachineContent` containing all the state machine data.
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

    /// Rebuilds the state machine from serialized content.
    ///
    /// This method restores the state machine from data that was
    /// previously serialized. It reconstructs the order book by
    /// re-inserting all the orders.
    ///
    /// # Arguments
    ///
    /// * `content` - The serialized state machine content to restore from
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


/// The persistent storage implementation for Raft.
///
/// This struct implements the `RaftStorage` trait and provides durable
/// storage for all Raft state:
/// - Log entries (stored in Sled)
/// - Vote information (stored in Sled)
/// - State machine (in-memory with file-based snapshots)
///
/// The storage uses Sled for fast, durable key-value storage of logs
/// and votes, and the filesystem for storing complete state machine snapshots.
#[derive(Debug)]
pub struct ExampleStore {
    /// The ID of the last log entry that was purged from storage.
    last_purged_log_id: RwLock<Option<LogId<ExampleNodeId>>>,

    /// The Raft log entries stored in Sled.
    ///
    /// Logs are keyed by log index (as big-endian bytes) for efficient
    /// range scans. Values are serialized Entry objects.
    pub log: sled::Tree,//RwLock<BTreeMap<u64, Entry<StorageRaftTypeConfig>>>,

    /// The in-memory Raft state machine.
    ///
    /// This is kept in memory for fast access, but is periodically
    /// snapshotted to disk for durability.
    pub state_machine: RwLock<ExampleStateMachine>,

    /// The current granted vote, stored in Sled.
    ///
    /// This persists vote information across restarts to ensure
    /// election safety (a node won't vote twice in the same term).
    vote: sled::Tree,

    /// Counter for generating unique snapshot IDs.
    snapshot_idx: Arc<Mutex<u64>>,

    /// The most recently built or installed snapshot, if any.
    current_snapshot: RwLock<Option<ExampleSnapshot>>,

    /// Configuration for paths and snapshot behavior.
    config : Config,

    /// The ID of the node this storage belongs to.
    pub node_id: ExampleNodeId,
}

/// Opens or creates a Sled database for the given node.
///
/// This helper function ensures the journal directory exists and
/// opens (or creates) the Sled database at the configured path.
///
/// # Arguments
///
/// * `config` - Storage configuration with paths
/// * `node_id` - ID of the node this database is for
///
/// # Returns
///
/// An open Sled database handle
fn get_sled_db(config: Config, node_id: ExampleNodeId) -> Db {
    let db_path = format!(
        "{}/{}-{}.binlog",
        config.journal_path, config.instance_prefix, node_id
    );
    // Ensure journal directory exists
    let _ = std::fs::create_dir_all(&config.journal_path);
    let db = sled::open(db_path.clone()).expect("failed to open sled database");
    tracing::debug!("get_sled_db: created log at: {:?}", db_path);
    db
}

impl ExampleStore {
    /// Opens or creates storage for a given node.
    ///
    /// This method:
    /// 1. Creates or opens the Sled database
    /// 2. Opens or creates the log and vote trees
    /// 3. Initializes the in-memory state machine
    ///
    /// # Arguments
    ///
    /// * `node_id` - The ID of the node this storage is for
    ///
    /// # Returns
    ///
    /// A new ExampleStore instance ready for use
    pub fn open_create(
        node_id: ExampleNodeId
    ) -> ExampleStore {
        tracing::info!("open_create, node_id: {}", node_id);

        let config = Config::default();

        let db = get_sled_db(config.clone(), node_id);

        let log = db.open_tree(format!("journal_entities_{}", node_id)).unwrap();

        let vote = db.open_tree(format!("votes_{}", node_id)).unwrap();

        let current_snapshot = RwLock::new(None);

        ExampleStore {
            last_purged_log_id: Default::default(),
            config: config,
            node_id: node_id,
            log,
            state_machine: Default::default(),
            vote: vote,
            snapshot_idx: Arc::new(Mutex::new(0)),
            current_snapshot,
        }
    }
}

/// Trait for restoring state from persistent storage.
///
/// This trait provides a method to load existing state from disk
/// when a node starts up.
//Store trait for restore things from snapshot and log
#[async_trait]
pub trait Restore {
    /// Restores the storage state from disk.
    ///
    /// This method should be called once at node startup to:
    /// 1. Load the latest snapshot (if any)
    /// 2. Replay any log entries after the snapshot
    /// 3. Bring the state machine up to date
    async fn restore(&mut self);
}

#[async_trait]
impl Restore for Arc<ExampleStore> {
    /// Restores the storage state when a node starts up.
    ///
    /// Currently this:
    /// 1. Finds the last log entry to determine the purged log state
    /// 2. Loads and installs the latest snapshot (if any)
    #[tracing::instrument(level = "trace", skip(self))]
    async fn restore(&mut self) {
        tracing::debug!("restore");
        let log = &self.log;

        let first = log.iter().rev()
            .next()
            .and_then(|res| res.ok())
            .and_then(|(_, val)| {
                serde_json::from_slice::<Entry<ExampleTypeConfig>>(&*val).ok()
            })
            .map(|entry| entry.log_id);

        match first {
            Some(x) => {
                tracing::debug!("restore: first log id = {:?}", x);
                let mut ld = self.last_purged_log_id.write().await;
                *ld = Some(x);
            },
            None => {}
        }

        match self.get_current_snapshot().await {
            Ok(Some(ss)) => {
                let _ = self.install_snapshot(&ss.meta, ss.snapshot).await;
            },
            _ => {}
        }
    }
}

#[async_trait]
impl RaftLogReader<ExampleTypeConfig> for Arc<ExampleStore> {
    #[tracing::instrument(level = "trace", skip(self))]
    async fn get_log_state(&mut self) -> Result<LogState<ExampleTypeConfig>, StorageError<ExampleNodeId>> {
        let log = &self.log;
        let last = log.iter()
            .rev()
            .next()
            .and_then(|res| res.ok())
            .and_then(|(_, val)| {
                serde_json::from_slice::<Entry<ExampleTypeConfig>>(&*val).ok()
            })
            .map(|entry| entry.log_id);

        let last_purged = *self.last_purged_log_id.read().await;

        let last = match last {
            None => last_purged,
            Some(x) => Some(x),
        };
        tracing::debug!("get_log_state: last_purged = {:?}, last = {:?}", last_purged, last);
        Ok(LogState {
            last_purged_log_id: last_purged,
            last_log_id: last,
        })
    }

    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send + Sync>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<ExampleTypeConfig>>, StorageError<ExampleNodeId>> {
        let log = &self.log;
        let response = log.range(transform_range_bound(range))
            .filter_map(|res| res.ok())
            .filter_map(|(_, val)| {
                serde_json::from_slice::<Entry<ExampleTypeConfig>>(&*val).ok()
            })
            .collect();

        Ok(response)
    }
}

fn transform_range_bound<RB: RangeBounds<u64> + Clone + Debug + Send + Sync>(range: RB) -> (Bound<IVec>, Bound<IVec>) {
    (serialize_bound(&range.start_bound()), serialize_bound(&range.end_bound()))
}


fn serialize_bound(
    v: &Bound<&u64>,
) -> Bound<IVec> {
    match v {
        Bound::Included(v) => Bound::Included(IVec::from(&v.to_be_bytes())),
        Bound::Excluded(v) => Bound::Excluded(IVec::from(&v.to_be_bytes())),
        Bound::Unbounded => Bound::Unbounded
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
            data = serde_json::to_vec(&*state_machine)
                .map_err(|e| StorageIOError::new(ErrorSubject::StateMachine, ErrorVerb::Read, AnyError::new(&e)))?;

            last_applied_log = state_machine.last_applied_log;
        }

        let last_applied_log = match last_applied_log {
            None => {
                return Err(StorageIOError::new(
                    ErrorSubject::StateMachine,
                    ErrorVerb::Read,
                    AnyError::new(&std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "can not compact empty state machine"
                    )),
                ).into());
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

        // Store snapshot first (no clone)
        {
            let mut current_snapshot = self.current_snapshot.write().await;
            *current_snapshot = Some(ExampleSnapshot {
                meta: meta.clone(),
                data: data.clone(),
            });
        }

        self.write_snapshot().await.map_err(|e| {
            StorageIOError::new(
                ErrorSubject::Snapshot(meta.clone()),
                ErrorVerb::Write,
                AnyError::new(&e),
            )
        })?;

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
        let vote_bytes = serde_json::to_vec(vote).map_err(|e| {
            StorageIOError::new(
                ErrorSubject::Vote,
                ErrorVerb::Write,
                AnyError::new(&e),
            )
        })?;
        self.vote.insert(b"vote", IVec::from(vote_bytes)).map_err(|e| {
            StorageIOError::new(
                ErrorSubject::Vote,
                ErrorVerb::Write,
                AnyError::new(&e),
            )
        })?;
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<ExampleNodeId>>, StorageError<ExampleNodeId>> {
        let value = self.vote.get(b"vote").map_err(|e| {
            StorageIOError::new(
                ErrorSubject::Vote,
                ErrorVerb::Read,
                AnyError::new(&e),
            )
        })?;
        match value {
            None => Ok(None),
            Some(val) => {
                let vote = serde_json::from_slice::<Vote<ExampleNodeId>>(&*val).map_err(|e| {
                    StorageIOError::new(
                        ErrorSubject::Vote,
                        ErrorVerb::Read,
                        AnyError::new(&e),
                    )
                })?;
                Ok(Some(vote))
            }
        }
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    #[tracing::instrument(level = "trace", skip(self, entries))]
    async fn append_to_log(
        &mut self,
        entries: &[&Entry<ExampleTypeConfig>],
    ) -> Result<(), StorageError<ExampleNodeId>> {
        let log = &self.log;
        for entry in entries {
            let entry_bytes = serde_json::to_vec(&*entry).map_err(|e| {
                StorageIOError::new(
                    ErrorSubject::Log(entry.log_id),
                    ErrorVerb::Write,
                    AnyError::new(&e),
                )
            })?;
            log.insert(entry.log_id.index.to_be_bytes(), IVec::from(entry_bytes)).map_err(|e| {
                StorageIOError::new(
                    ErrorSubject::Log(entry.log_id),
                    ErrorVerb::Write,
                    AnyError::new(&e),
                )
            })?;
        }
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn delete_conflict_logs_since(
        &mut self,
        log_id: LogId<ExampleNodeId>,
    ) -> Result<(), StorageError<ExampleNodeId>> {
        tracing::debug!("delete_log: [{:?}, +oo)", log_id);

        let log = &self.log;
        let keys: Vec<_> = log.range(transform_range_bound(log_id.index..))
            .filter_map(|res| res.ok())
            .map(|(k, _v)| k)
            .collect();

        for key in keys {
            log.remove(&key).map_err(|e| {
                StorageIOError::new(
                    ErrorSubject::Log(log_id),
                    ErrorVerb::Delete,
                    AnyError::new(&e),
                )
            })?;
        }

        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn purge_logs_upto(&mut self, log_id: LogId<ExampleNodeId>) -> Result<(), StorageError<ExampleNodeId>> {
        tracing::debug!("delete_log: [{:?}, +oo)", log_id);

        {
            let mut ld = self.last_purged_log_id.write().await;
            if let Some(last) = *ld {
                debug_assert!(last <= log_id, "cannot purge logs before last purged");
            }
            *ld = Some(log_id);
        }

        {
            let log = &self.log;
            let keys: Vec<_> = log.range(transform_range_bound(..=log_id.index))
                .filter_map(|res| res.ok())
                .map(|(k, _)| k)
                .collect();

            for key in keys {
                log.remove(&key).map_err(|e| {
                    StorageIOError::new(
                        ErrorSubject::Log(log_id),
                        ErrorVerb::Delete,
                        AnyError::new(&e),
                    )
                })?;
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
                        let _mr = sm.orderbook.place_order(&mut o);
                        res.push(ExampleResponse {
                            value: Some(o.sequance.to_string()),
                        })
                    }

                    ExampleRequest::Cancel { order } => {
                        sm.orderbook.cancel(order);
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

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
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
            let updated_state_machine: ExampleStateMachine =
                serde_json::from_slice(&new_snapshot.data).map_err(|e| {
                    StorageIOError::new(
                        ErrorSubject::Snapshot(new_snapshot.meta.clone()),
                        ErrorVerb::Read,
                        AnyError::new(&e),
                    )
                })?;
            let mut state_machine = self.state_machine.write().await;
            *state_machine = updated_state_machine;
        }

        // Update current snapshot.
        let mut current_snapshot = self.current_snapshot.write().await;
        *current_snapshot = Some(new_snapshot);
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
                let data = match self.read_snapshot_file().await {
                    Ok(c) => c,
                    Err(_e) => return Ok(None)
                };

                let content: ExampleStateMachine = match serde_json::from_slice(&data) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("get_current_snapshot: failed to deserialize snapshot: {}", e);
                        return Ok(None);
                    }
                };

                let last_applied_log = match content.last_applied_log {
                    Some(log) => log,
                    None => {
                        tracing::error!("get_current_snapshot: snapshot missing last_applied_log");
                        return Ok(None);
                    }
                };

                tracing::debug!("get_current_snapshot: last_applied_log = {:?}", last_applied_log);

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

                tracing::debug!("get_current_snapshot: meta {:?}", meta);

                Ok(Some(Snapshot {
                    meta,
                    snapshot: Box::new(Cursor::new(data)),
                }))
            }
        }
    }
}

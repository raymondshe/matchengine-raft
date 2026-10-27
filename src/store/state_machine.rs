//! `StateMachineStore`: an in-memory KV store + matching engine, made durable by snapshots.
//!
//! The state machine itself is intentionally kept in memory: applying a Raft entry must be fast
//! and the log already provides durability. Every applied batch is recoverable by replaying the
//! retained log tail from the latest snapshot (see [`crate::store::log_store`]).
//!
//! Snapshots are written to `<dir>/current.snap` with a small framing header, then fsynced and
//! renamed into place, so a crash never leaves a half-written snapshot visible.

use std::collections::BTreeMap;
use std::io;
use std::io::Cursor;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use futures::Stream;
use futures::TryStreamExt;
use futures::lock::Mutex;
use openraft::Entry;
use openraft::EntryPayload;
use openraft::OptionalSend;
use openraft::RaftSnapshotBuilder;
use openraft::RaftTypeConfig;
use openraft::StoredMembership;
use openraft::alias::CommittedLeaderIdOf;
use openraft::alias::EntryPayloadOf;
use openraft::alias::LogIdOf;
use openraft::alias::SnapshotMetaOf;
use openraft::alias::SnapshotOf;
use openraft::alias::StoredMembershipOf;
use openraft::storage::EntryResponder;
use openraft::storage::RaftStateMachine;
use serde::Deserialize;
use serde::Serialize;

use crate::matchengine::Order;
use crate::matchengine::OrderBook;
use crate::store::config::sync_dir;
use crate::store::types::MatchEngineRequest;
use crate::store::types::MatchEngineResponse;

/// Name of the durable snapshot file inside the snapshot directory.
const SNAPSHOT_FILE: &str = "current.snap";
/// Magic prefix of a snapshot file.
const SNAPSHOT_MAGIC: &[u8; 7] = b"MRFSNAP";
/// Snapshot file format version.
const SNAPSHOT_VERSION: u32 = 1;
/// `magic + version + crc32 of the body`.
const SNAPSHOT_HEADER_LEN: usize = SNAPSHOT_MAGIC.len() + 4 + 4;

/// A snapshot as stored in memory and on disk.
#[derive(Debug)]
pub struct StoredSnapshot<C: RaftTypeConfig> {
    pub meta: SnapshotMetaOf<C>,

    /// The postcard-encoded [`StateMachineData`] at the time of this snapshot.
    pub data: Vec<u8>,
}

/// On-disk snapshot body: metadata plus the encoded state machine content.
#[derive(Serialize, Deserialize)]
#[serde(bound = "")]
struct SnapshotPayload<C: RaftTypeConfig> {
    meta: SnapshotMetaOf<C>,
    data: Vec<u8>,
}

/// Data contained in the Raft state machine.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct StateMachineData {
    /// Application KV store data.
    pub data: BTreeMap<String, String>,

    /// All active orders in the order book (both bids and asks).
    pub orders: Vec<Order>,

    /// The current sequence number for order placement.
    pub sequence: u64,
}

/// Inner storage for the state machine.
#[derive(Debug)]
pub struct StateMachineStoreInner<C: RaftTypeConfig> {
    pub last_applied_log: Option<LogIdOf<C>>,

    pub last_membership: StoredMembershipOf<C>,

    /// The Raft state machine.
    pub state_machine_data: StateMachineData,

    /// The order book for the matching engine.
    pub orderbook: OrderBook,

    /// The last built or installed snapshot, kept so it can be shipped to lagging peers.
    pub current_snapshot: Option<StoredSnapshot<C>>,
}

impl<C: RaftTypeConfig> Default for StateMachineStoreInner<C> {
    fn default() -> Self {
        Self {
            last_applied_log: None,
            last_membership: StoredMembership::default(),
            state_machine_data: StateMachineData::default(),
            orderbook: OrderBook::new(),
            current_snapshot: None,
        }
    }
}

impl<C: RaftTypeConfig> StateMachineStoreInner<C> {
    /// Replaces the state with serialized content, rebuilding the order book indices.
    fn install_content(&mut self, content: StateMachineData) {
        self.state_machine_data.data = content.data;
        self.orderbook.asks.clear();
        self.orderbook.bids.clear();
        self.orderbook.sequence = content.sequence;

        for order in &content.orders {
            self.orderbook.insert_order(order);
        }
    }

    /// Converts the state machine to a serializable content format.
    pub fn to_content(&self) -> StateMachineData {
        let mut content = StateMachineData {
            data: self.state_machine_data.data.clone(),
            orders: Vec::new(),
            sequence: self.orderbook.sequence,
        };

        let mut bids: Vec<Order> = self.orderbook.bids.values().cloned().collect();
        let mut asks: Vec<Order> = self.orderbook.asks.values().cloned().collect();

        content.orders.append(&mut bids);
        content.orders.append(&mut asks);
        content
    }
}

/// Defines a state machine for the Raft cluster.
///
/// This is a newtype wrapper around `Arc<Mutex<StateMachineStoreInner<C>>>` to satisfy
/// Rust's orphan rules when implementing foreign traits. Clones share the same state.
#[derive(Debug)]
pub struct StateMachineStore<C: RaftTypeConfig> {
    inner: Arc<Mutex<StateMachineStoreInner<C>>>,
    dir: Arc<PathBuf>,
}

impl<C: RaftTypeConfig> Clone for StateMachineStore<C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            dir: self.dir.clone(),
        }
    }
}

impl<C: RaftTypeConfig> StateMachineStore<C> {
    /// Opens the state machine in `dir`, restoring the latest durable snapshot if one exists.
    ///
    /// A corrupt snapshot is reported as an error instead of silently starting empty: silently
    /// dropping committed data would be worse than refusing to start.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;

        let mut inner = StateMachineStoreInner::<C>::default();

        if let Some((meta, data)) = read_snapshot_file::<C>(&dir)? {
            let content = decode_content(&data)?;
            inner.install_content(content);
            inner.last_applied_log = meta.last_log_id.clone();
            inner.last_membership = meta.last_membership.clone();
            inner.current_snapshot = Some(StoredSnapshot {
                meta,
                data,
            });
        }

        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
            dir: Arc::new(dir),
        })
    }

    /// Directory that holds this state machine's snapshot.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn inner(&self) -> &Arc<Mutex<StateMachineStoreInner<C>>> {
        &self.inner
    }
}

/// Public access to the state machine internals for our application
impl<C> StateMachineStore<C>
where C: RaftTypeConfig<D = MatchEngineRequest, R = MatchEngineResponse> {
    pub async fn orderbook_sequence(&self) -> u64 {
        let inner = self.inner.lock().await;
        inner.orderbook.sequence
    }

    pub async fn to_content(&self) -> StateMachineData {
        let inner = self.inner.lock().await;
        inner.to_content()
    }

    /// Writes the current in-memory content as a durable snapshot.
    ///
    /// This is what [`RaftSnapshotBuilder::build_snapshot`] uses; it is also handy in tests that
    /// want a snapshot without waiting for openraft's snapshot policy to trigger.
    pub async fn persist_snapshot(&self) -> io::Result<SnapshotMetaOf<C>> {
        let inner = self.inner.lock().await;
        let meta = SnapshotMetaOf::<C> {
            last_log_id: inner.last_applied_log.clone(),
            last_membership: inner.last_membership.clone(),
        };
        let content = inner.to_content();
        let data = encode_content(&content)?;
        let dir = self.dir.clone();
        let write_meta = meta.clone();
        let write_data = data.clone();
        blocking(move || write_snapshot_file::<C>(&dir, &write_meta, &write_data)).await?;
        Ok(meta)
    }
}

impl<C> RaftSnapshotBuilder<C> for StateMachineStore<C>
where C: RaftTypeConfig<D = MatchEngineRequest, R = MatchEngineResponse> {
    type SnapshotData = Cursor<Vec<u8>>;

    #[tracing::instrument(level = "trace", skip(self))]
    async fn build_snapshot(&mut self) -> Result<SnapshotOf<C, Cursor<Vec<u8>>>, io::Error> {
        // Hold the lock while encoding/writing: the snapshot must correspond to exactly one
        // applied log id, and the blocking work runs off the async runtime anyway.
        let mut inner = self.inner.lock().await;

        let meta = SnapshotMetaOf::<C> {
            last_log_id: inner.last_applied_log.clone(),
            last_membership: inner.last_membership.clone(),
        };
        let content = inner.to_content();
        let data = encode_content(&content)?;

        let dir = self.dir.clone();
        let write_data = data.clone();
        let write_meta = meta.clone();
        blocking(move || write_snapshot_file::<C>(&dir, &write_meta, &write_data)).await?;

        inner.current_snapshot = Some(StoredSnapshot {
            meta: meta.clone(),
            data: data.clone(),
        });

        Ok(SnapshotOf::<C, Cursor<Vec<u8>>> {
            meta,
            snapshot: Cursor::new(data),
        })
    }
}

impl<C> RaftStateMachine<C> for StateMachineStore<C>
where
    C: RaftTypeConfig<
        D = MatchEngineRequest,
        R = MatchEngineResponse,
        Entry = Entry<CommittedLeaderIdOf<C>, EntryPayloadOf<C>>,
    >,
{
    type SnapshotData = Cursor<Vec<u8>>;
    type SnapshotBuilder = Self;

    async fn applied_state(&mut self) -> Result<(Option<LogIdOf<C>>, StoredMembershipOf<C>), io::Error> {
        let inner = self.inner.lock().await;
        Ok((inner.last_applied_log.clone(), inner.last_membership.clone()))
    }

    #[tracing::instrument(level = "trace", skip(self, entries))]
    async fn apply<Strm>(&mut self, mut entries: Strm) -> Result<(), io::Error>
    where Strm: Stream<Item = Result<EntryResponder<C>, io::Error>> + Unpin + OptionalSend {
        let mut inner = self.inner.lock().await;

        while let Some((entry, responder)) = entries.try_next().await? {
            tracing::debug!(log_id = %entry.log_id, "replicate to sm");

            inner.last_applied_log = Some(entry.log_id.clone());

            let response = match &entry.payload {
                EntryPayload::Blank => MatchEngineResponse::none(),
                EntryPayload::Normal(req) => match req {
                    MatchEngineRequest::Set {
                        key,
                        value,
                    } => {
                        inner.state_machine_data.data.insert(key.clone(), value.clone());
                        MatchEngineResponse::new(value.clone())
                    }
                    MatchEngineRequest::Place {
                        order,
                    } => {
                        let mut o = *order;
                        let _mr = inner.orderbook.place_order(&mut o);
                        MatchEngineResponse {
                            value: Some(o.sequence.to_string()),
                        }
                    }
                    MatchEngineRequest::Cancel {
                        order,
                    } => {
                        let _o = inner.orderbook.cancel(order);
                        MatchEngineResponse::none()
                    }
                },
                EntryPayload::Membership(mem) => {
                    inner.last_membership = StoredMembership::new(Some(entry.log_id.clone()), mem.clone());
                    MatchEngineResponse::none()
                }
            };

            if let Some(responder) = responder {
                responder.send(response);
            }
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self, snapshot))]
    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMetaOf<C>,
        snapshot: Self::SnapshotData,
    ) -> Result<(), io::Error> {
        let data = snapshot.into_inner();
        tracing::info!({ snapshot_size = data.len() }, "installing snapshot");

        // Validate before touching the durable copy, so a bad snapshot cannot destroy a good one.
        let content = decode_content(&data)?;

        let dir = self.dir.clone();
        let write_data = data.clone();
        let write_meta = meta.clone();
        blocking(move || write_snapshot_file::<C>(&dir, &write_meta, &write_data)).await?;

        let mut inner = self.inner.lock().await;
        inner.last_applied_log = meta.last_log_id.clone();
        inner.last_membership = meta.last_membership.clone();
        inner.install_content(content);
        inner.current_snapshot = Some(StoredSnapshot {
            meta: meta.clone(),
            data,
        });

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    #[allow(clippy::type_complexity)]
    async fn get_current_snapshot(&mut self) -> Result<Option<SnapshotOf<C, Cursor<Vec<u8>>>>, io::Error> {
        let inner = self.inner.lock().await;
        match &inner.current_snapshot {
            Some(snapshot) => Ok(Some(SnapshotOf::<C, Cursor<Vec<u8>>> {
                meta: snapshot.meta.clone(),
                snapshot: Cursor::new(snapshot.data.clone()),
            })),
            None => Ok(None),
        }
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }
}

// --- File format helpers

fn encode_content(content: &StateMachineData) -> io::Result<Vec<u8>> {
    postcard::to_allocvec(content).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn decode_content(data: &[u8]) -> io::Result<StateMachineData> {
    postcard::from_bytes(data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("corrupt snapshot: {e}")))
}

/// Reads and validates the snapshot file, returning metadata plus the encoded content.
fn read_snapshot_file<C: RaftTypeConfig>(dir: &Path) -> io::Result<Option<(SnapshotMetaOf<C>, Vec<u8>)>> {
    let path = dir.join(SNAPSHOT_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };

    if bytes.len() < SNAPSHOT_HEADER_LEN || &bytes[0..SNAPSHOT_MAGIC.len()] != SNAPSHOT_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a snapshot file", path.display()),
        ));
    }
    let version = u32::from_le_bytes(bytes[SNAPSHOT_MAGIC.len()..SNAPSHOT_MAGIC.len() + 4].try_into().unwrap());
    if version != SNAPSHOT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} has unsupported snapshot version {version}", path.display()),
        ));
    }

    let expected_crc =
        u32::from_le_bytes(bytes[SNAPSHOT_MAGIC.len() + 4..SNAPSHOT_HEADER_LEN].try_into().unwrap());
    if crc32fast::hash(&bytes[SNAPSHOT_HEADER_LEN..]) != expected_crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} has a snapshot checksum mismatch", path.display()),
        ));
    }

    let payload: SnapshotPayload<C> = postcard::from_bytes(&bytes[SNAPSHOT_HEADER_LEN..])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("corrupt snapshot: {e}")))?;

    Ok(Some((payload.meta, payload.data)))
}

/// Writes a snapshot atomically: temp file, fsync, rename, fsync directory.
fn write_snapshot_file<C: RaftTypeConfig>(dir: &Path, meta: &SnapshotMetaOf<C>, data: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let payload = SnapshotPayload::<C> {
        meta: meta.clone(),
        data: data.to_vec(),
    };
    let body = postcard::to_allocvec(&payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let tmp = dir.join(format!("{SNAPSHOT_FILE}.tmp"));
    let path = dir.join(SNAPSHOT_FILE);

    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(SNAPSHOT_MAGIC)?;
    file.write_all(&SNAPSHOT_VERSION.to_le_bytes())?;
    file.write_all(&crc32fast::hash(&body).to_le_bytes())?;
    file.write_all(&body)?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(&tmp, &path)?;
    sync_dir(dir)
}

/// Runs blocking storage work without stalling the async runtime.
async fn blocking<F, T>(f: F) -> io::Result<T>
where
    F: FnOnce() -> io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| io::Error::other(format!("storage task panicked: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MatchEngineTypeConfig;
    use openraft::LogId;
    use openraft::alias::CommittedLeaderIdOf;
    use openraft::vote::RaftLeaderId;

    fn log_id(term: u64, index: u64) -> LogIdOf<MatchEngineTypeConfig> {
        LogId::new(CommittedLeaderIdOf::<MatchEngineTypeConfig>::new(term, 1), index)
    }

    #[tokio::test]
    async fn snapshot_survives_reopen() {
        let dir = tempdir();

        let store = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
        {
            let mut inner = store.inner().lock().await;
            inner.state_machine_data.data.insert("k".to_string(), "v".to_string());
            inner.orderbook.sequence = 7;
            inner.last_applied_log = Some(log_id(1, 3));
            inner.last_membership = StoredMembership::new(Some(log_id(1, 1)), Default::default());
        }
        store.persist_snapshot().await.unwrap();

        let reopened = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
        let content = reopened.to_content().await;
        assert_eq!(Some(&"v".to_string()), content.data.get("k"));
        assert_eq!(7, content.sequence);
    }

    #[tokio::test]
    async fn corrupt_snapshot_is_an_error() {
        let dir = tempdir();
        std::fs::write(dir.path().join(SNAPSHOT_FILE), b"not a snapshot").unwrap();

        let err = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap_err();
        assert_eq!(io::ErrorKind::InvalidData, err.kind());
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir() -> TempDir {
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;

        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let unique = format!(
            "matchengine-sm-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

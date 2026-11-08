//! Append-only, segmented write-ahead log for Raft entries.
//!
//! # Design
//!
//! - Records are framed as `[len: u32 LE][crc32: u32 LE][postcard payload]`, so a torn or corrupt
//!   record is detected and dropped instead of being parsed as garbage.
//! - `append()` writes the whole openraft batch with **one** `write_all` and **one** `sync_data`.
//!   openraft batches entries per AppendEntries RPC, which makes that group commit at the Raft
//!   layer: N client requests replicated in one RPC cost one fsync.
//! - `open_with_fsync(..., None)` (the default, and `RAFT_WAL_FSYNC_MS` unset) skips append and
//!   rotation fsyncs. `Some(Duration::ZERO)` fsyncs every append; `Some(interval)` fsyncs the
//!   active segment every `interval` on a Tokio task. Without fsync a crash can lose
//!   acknowledged writes.
//! - Reads use `pread` (`read_at`), so replication can read while a write is in flight and no
//!   global lock is held during disk IO.
//! - All blocking IO runs on `spawn_blocking`; the async runtime only encodes.
//! - Vote and the last purged log id are kept in a small `state` file that is rewritten atomically.
//!   They are rare, safety-critical updates and must survive segment deletion. `committed` rides
//!   at the tail of the WAL; every-append mode makes it durable with the next append, periodic
//!   mode on the next tick, and no-fsync mode not at all. This avoids a second fsync per batch.
//! - Segments are deleted only when fully covered by a purge, and the active segment is rotated
//!   first, so a purge never leaves a hole.
//! - With the `async-wal` feature and `--async-wal`, append writes the batch and defers
//!   `sync_data` to a Tokio worker; `IOFlushed` fires only after the fsync. Pending entries are
//!   readable from memory, and `truncate_after`/`purge` flush them first. Off by default.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::marker::PhantomData;
use std::ops::RangeBounds;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
#[cfg(debug_assertions)]
use std::sync::atomic::AtomicU8;
#[cfg(feature = "async-wal")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use openraft::LogState;
use openraft::RaftTypeConfig;
use openraft::alias::LogIdOf;
use openraft::alias::VoteOf;
use openraft::entry::RaftEntry;
use openraft::storage::IOFlushed;
use openraft::storage::RaftLogReader;
use openraft::storage::RaftLogStorage;
use serde::Deserialize;
use serde::Serialize;

use crate::store::config::sync_dir;

#[cfg(feature = "async-wal")]
type AsyncWalSender<C> = tokio::sync::mpsc::UnboundedSender<(u64, IOFlushed<C>)>;

/// Target size of one WAL segment before a new one is started.
pub const DEFAULT_SEGMENT_SIZE: u64 = 64 * 1024 * 1024;

/// Segment file header: magic + format version.
const SEGMENT_MAGIC: &[u8; 4] = b"MRFT";
const SEGMENT_VERSION: u32 = 1;
const SEGMENT_HEADER_LEN: u64 = 8;

/// Record frame header: payload length + CRC32.
const RECORD_HEADER_LEN: u64 = 8;

/// Guard against decoding a corrupt length into a huge allocation.
const MAX_RECORD_LEN: u32 = 256 * 1024 * 1024;

/// Name of the atomically rewritten vote/purge file.
const STATE_FILE: &str = "state";

/// A record in the WAL. Entries dominate; `Committed` is appended without its own fsync.
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub enum WalRecord<C: RaftTypeConfig> {
    /// One replicated Raft entry.
    Entry(C::Entry),
    /// The latest committed log id, written without its own fsync.
    Committed(Option<LogIdOf<C>>),
}

/// Borrowed form used for encoding append batches.
///
/// The variant order and payload layout are identical to [`WalRecord`], so the
/// on-disk format is unchanged while large entries avoid a clone per append.
#[derive(Serialize)]
#[serde(bound = "")]
enum WalRecordRef<'a, C: RaftTypeConfig> {
    Entry(&'a C::Entry),
    Committed(&'a Option<LogIdOf<C>>),
}

/// Vote and purge marker, persisted in the `state` file.
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound = "")]
struct PersistedState<C: RaftTypeConfig> {
    vote: Option<VoteOf<C>>,
    last_purged: Option<LogIdOf<C>>,
}

/// Where one entry lives on disk.
#[derive(Clone, Copy, Debug)]
struct RecordLoc {
    seq: u32,
    offset: u64,
    /// Payload length, excluding the frame header.
    len: u32,
}

#[derive(Clone, Debug)]
struct SegmentMeta {
    seq: u32,
    path: PathBuf,
    /// Index of the first entry stored in this segment.
    first_index: Option<u64>,
    /// Index of the last entry stored in this segment.
    last_index: Option<u64>,
}

impl SegmentMeta {
    fn new(seq: u32, path: PathBuf) -> Self {
        Self {
            seq,
            path,
            first_index: None,
            last_index: None,
        }
    }
}

/// In-memory view of the durable hard state.
struct RuntimeState<C: RaftTypeConfig> {
    vote: Option<VoteOf<C>>,
    committed: Option<LogIdOf<C>>,
    last_purged: Option<LogIdOf<C>>,
    last_log_id: Option<LogIdOf<C>>,
}

impl<C: RaftTypeConfig> Default for RuntimeState<C> {
    fn default() -> Self {
        Self {
            vote: None,
            committed: None,
            last_purged: None,
            last_log_id: None,
        }
    }
}

/// How the WAL makes appended data durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FsyncPolicy {
    /// Do not fsync. Fastest; a crash can lose acknowledged writes.
    None,
    /// Fsync every append batch.
    EveryAppend,
    /// Fsync the active segment periodically on a Tokio task.
    Interval(Duration),
}

impl FsyncPolicy {
    fn from_option(fsync: Option<Duration>) -> Self {
        match fsync {
            None => Self::None,
            Some(interval) if interval.is_zero() => Self::EveryAppend,
            Some(interval) => Self::Interval(interval),
        }
    }

    fn enabled(self) -> bool {
        !matches!(self, Self::None)
    }
}

struct Disk<C: RaftTypeConfig> {
    /// Active segment, opened in append mode.
    writer: File,
    writer_seq: u32,
    writer_len: u64,
    segments: Vec<SegmentMeta>,
    /// Cached read handles per segment, so replication does not reopen files.
    readers: HashMap<u32, Arc<File>>,
    _marker: PhantomData<C>,
}

impl<C: RaftTypeConfig> Disk<C> {
    fn frame(payload: &[u8]) -> [u8; 8] {
        let mut header = [0u8; 8];
        header[0..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        header[4..8].copy_from_slice(&crc32fast::hash(payload).to_le_bytes());
        header
    }

    fn write_frame(&mut self, payload: &[u8]) -> io::Result<()> {
        let header = Self::frame(payload);
        self.writer.write_all(&header)?;
        self.writer.write_all(payload)?;
        self.writer_len += RECORD_HEADER_LEN + payload.len() as u64;
        Ok(())
    }

    fn segment(&mut self, seq: u32) -> Option<&mut SegmentMeta> {
        self.segments.iter_mut().find(|s| s.seq == seq)
    }

    /// Starts a new segment. When fsync is enabled, the previous one is
    /// fsynced first so nothing is lost.
    fn rotate(&mut self, dir: &Path, fsync: FsyncPolicy) -> io::Result<()> {
        if fsync.enabled() {
            self.writer.sync_data()?;
        }

        let seq = self.writer_seq + 1;
        let path = segment_path(dir, seq);
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .append(true)
            .open(&path)?;
        file.write_all(&segment_header())?;
        if fsync.enabled() {
            file.sync_data()?;
            sync_dir(dir)?;
        }

        self.writer = file;
        self.writer_seq = seq;
        self.writer_len = SEGMENT_HEADER_LEN;
        self.segments.push(SegmentMeta::new(seq, path));
        Ok(())
    }

    /// Reopens an existing segment for appending at `len`.
    fn reopen(&mut self, seq: u32, len: u64, fsync: FsyncPolicy) -> io::Result<()> {
        let path = self
            .segments
            .iter()
            .find(|s| s.seq == seq)
            .map(|s| s.path.clone())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("segment {seq} is gone"))
            })?;

        let file = OpenOptions::new().read(true).append(true).open(&path)?;
        file.set_len(len)?;
        if fsync.enabled() {
            file.sync_data()?;
        }

        self.writer = file;
        self.writer_seq = seq;
        self.writer_len = len;
        self.readers.remove(&seq);
        Ok(())
    }
}

struct Inner<C: RaftTypeConfig> {
    dir: PathBuf,
    segment_size: u64,
    /// How appended data is fsynced.
    fsync: FsyncPolicy,
    /// Set after a write that is not yet known to be durable.
    dirty: AtomicBool,
    /// Whether the periodic fsync task has been started.
    fsync_started: AtomicBool,
    /// Wakes the periodic fsync task on drop.
    fsync_stop: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    index: RwLock<BTreeMap<u64, RecordLoc>>,
    state: RwLock<RuntimeState<C>>,
    disk: RwLock<Disk<C>>,
    /// Serializes mutations so concurrent appends cannot interleave on disk.
    write_order: tokio::sync::Mutex<()>,
    /// Prototype asynchronous append is enabled.
    async_wal: bool,
    /// Entries written but not yet confirmed durable by the async worker.
    pending: RwLock<BTreeMap<u64, C::Entry>>,
    #[cfg(feature = "async-wal")]
    async_tx: Mutex<Option<AsyncWalSender<C>>>,
    #[cfg(feature = "async-wal")]
    async_started: AtomicBool,
    #[cfg(feature = "async-wal")]
    async_pending: AtomicUsize,
    #[cfg(feature = "async-wal")]
    async_idle: tokio::sync::Notify,
    #[cfg(feature = "async-wal")]
    async_stop: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// One-shot fault injection for tests: 0 = off, 1 = ENOSPC, 2 = EIO.
    #[cfg(debug_assertions)]
    append_fault: AtomicU8,
}

impl<C: RaftTypeConfig> Drop for Inner<C> {
    fn drop(&mut self) {
        let mut stop = self
            .fsync_stop
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(stop) = stop.take() {
            let _ = stop.send(());
        }
        #[cfg(feature = "async-wal")]
        {
            let mut stop = self
                .async_stop
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(stop) = stop.take() {
                let _ = stop.send(());
            }
        }
    }
}

impl<C: RaftTypeConfig> Inner<C> {
    /// Consumes a one-shot injected append failure, if armed.
    #[cfg(debug_assertions)]
    fn take_injected_append_error(&self) -> io::Result<()> {
        match self.append_fault.swap(0, Ordering::SeqCst) {
            1 => Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected WAL append failure (ENOSPC)",
            )),
            2 => Err(io::Error::other("injected WAL append failure (EIO)")),
            _ => Ok(()),
        }
    }

    fn append_blocking(&self, entries: Vec<C::Entry>) -> io::Result<()>
    where
        C::Entry: Clone,
    {
        self.append_blocking_inner(entries, false)
    }

    #[cfg(feature = "async-wal")]
    fn append_blocking_deferred(&self, entries: Vec<C::Entry>) -> io::Result<()>
    where
        C::Entry: Clone,
    {
        self.append_blocking_inner(entries, true)
    }

    fn append_blocking_inner(&self, entries: Vec<C::Entry>, defer_fsync: bool) -> io::Result<()>
    where
        C::Entry: Clone,
    {
        if entries.is_empty() {
            return Ok(());
        }

        #[cfg(debug_assertions)]
        self.take_injected_append_error()?;

        let last_log_id = entries.last().map(|e| e.log_id().clone());
        let first_index = entries.first().map(|e| e.log_id().index);
        let (buf, locs) = encode_entries::<C>(&entries)?;

        let mut disk = self.disk.write().unwrap();

        if disk.writer_len > SEGMENT_HEADER_LEN
            && disk.writer_len + buf.len() as u64 > self.segment_size
        {
            disk.rotate(&self.dir, self.fsync)?;
        }

        let seq = disk.writer_seq;
        let base = disk.writer_len;

        if let Err(e) = disk.writer.write_all(&buf) {
            // Undo a partial write: a hole in the log is much worse than losing the batch.
            let _ = disk.writer.set_len(base);
            if self.fsync.enabled() {
                let _ = disk.writer.sync_data();
            }
            disk.writer_len = base;
            return Err(e);
        }
        self.dirty.store(true, Ordering::Release);
        if self.fsync == FsyncPolicy::EveryAppend
            && !defer_fsync
            && let Err(e) = disk.writer.sync_data()
        {
            // The bytes may or may not be durable; drop them so the retry starts at `base`.
            let _ = disk.writer.set_len(base);
            let _ = disk.writer.sync_data();
            disk.writer_len = base;
            return Err(e);
        }
        if self.fsync == FsyncPolicy::EveryAppend && !defer_fsync {
            self.dirty.store(false, Ordering::Release);
        }
        disk.writer_len = base + buf.len() as u64;

        if let Some(segment) = disk.segment(seq) {
            if segment.first_index.is_none() {
                segment.first_index = first_index;
            }
            segment.last_index = last_log_id.as_ref().map(|l| l.index);
        }
        drop(disk);

        {
            let mut index = self.index.write().unwrap();
            for (index_value, offset, len) in locs {
                // `offset` is relative to the batch buffer; shift it onto the segment.
                index.insert(
                    index_value,
                    RecordLoc {
                        seq,
                        offset: base + offset,
                        len,
                    },
                );
            }
        }

        self.state.write().unwrap().last_log_id = last_log_id;
        Ok(())
    }

    /// Appends a commit marker without fsync; the next append batch makes it durable.
    fn save_committed_blocking(&self, committed: Option<LogIdOf<C>>) -> io::Result<()> {
        let payload = postcard::to_allocvec(&WalRecordRef::<C>::Committed(&committed))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let mut disk = self.disk.write().unwrap();
        if disk.writer_len > SEGMENT_HEADER_LEN
            && disk.writer_len + payload.len() as u64 > self.segment_size
        {
            disk.rotate(&self.dir, self.fsync)?;
        }
        let base = disk.writer_len;
        if let Err(e) = disk.write_frame(&payload) {
            // Undo the partial frame so the next append starts at a record boundary.
            let _ = disk.writer.set_len(base);
            disk.writer_len = base;
            return Err(e);
        }
        self.dirty.store(true, Ordering::Release);
        drop(disk);

        self.state.write().unwrap().committed = committed;
        Ok(())
    }

    fn save_vote_blocking(&self, vote: &VoteOf<C>) -> io::Result<()> {
        let state = self.state.read().unwrap();
        let persisted = PersistedState::<C> {
            vote: Some(vote.clone()),
            last_purged: state.last_purged.clone(),
        };
        drop(state);

        write_state_file(&self.dir, &persisted)?;
        self.state.write().unwrap().vote = Some(vote.clone());
        Ok(())
    }

    fn purge_blocking(&self, log_id: LogIdOf<C>) -> io::Result<()> {
        {
            let state = self.state.read().unwrap();
            if let Some(prev) = &state.last_purged
                && prev.index > log_id.index
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("purge index regressed: {:?} -> {:?}", prev, log_id),
                ));
            }
        }

        // Persist the marker first: if we crash after deleting segments, recovery re-reads the
        // marker and knows the logs below it are gone.
        let persisted = {
            let state = self.state.read().unwrap();
            PersistedState::<C> {
                vote: state.vote.clone(),
                last_purged: Some(log_id.clone()),
            }
        };
        write_state_file(&self.dir, &persisted)?;

        {
            let mut disk = self.disk.write().unwrap();

            // A purge never truncates the front of a segment, so if the active segment is fully
            // covered, start a new one before deleting it.
            let writer_seq = disk.writer_seq;
            let active_covered = disk
                .segment(writer_seq)
                .and_then(|s| s.last_index)
                .is_some_and(|last| last <= log_id.index);
            if active_covered {
                disk.rotate(&self.dir, self.fsync)?;
            }

            // Never drop the active segment: it is where the next append goes.
            let writer_seq = disk.writer_seq;
            let covered: Vec<u32> = disk
                .segments
                .iter()
                .filter(|s| s.seq != writer_seq)
                .filter(|s| s.last_index.is_none_or(|last| last <= log_id.index))
                .map(|s| s.seq)
                .collect();

            for seq in &covered {
                if let Some(segment) = disk.segments.iter().find(|s| s.seq == *seq) {
                    let _ = std::fs::remove_file(&segment.path);
                }
                disk.readers.remove(seq);
            }
            disk.segments.retain(|s| !covered.contains(&s.seq));
        }

        {
            let mut index = self.index.write().unwrap();
            index.retain(|index, _| *index > log_id.index);
        }

        let mut state = self.state.write().unwrap();
        state.last_purged = Some(log_id.clone());
        if state
            .last_log_id
            .as_ref()
            .is_some_and(|last| last.index <= log_id.index)
        {
            state.last_log_id = None;
        }
        drop(state);

        sync_dir(&self.dir)
    }

    fn truncate_after_blocking(&self, last_log_id: Option<LogIdOf<C>>) -> io::Result<()> {
        let keep_upto = last_log_id.as_ref().map(|l| l.index);

        let mut disk = self.disk.write().unwrap();

        let cut = {
            let index = self.index.read().unwrap();
            let located = match keep_upto {
                Some(upto) => index.range(..=upto).next_back().map(|(_, loc)| *loc),
                None => None,
            };
            match located {
                Some(loc) => (loc.seq, loc.offset + RECORD_HEADER_LEN + loc.len as u64),
                None => {
                    let seq = disk.segments.first().map(|s| s.seq).unwrap_or(0);
                    (seq, SEGMENT_HEADER_LEN)
                }
            }
        };

        // Drop whole segments after the cut, then truncate the segment that contains it.
        let stale: Vec<u32> = disk
            .segments
            .iter()
            .filter(|s| s.seq > cut.0)
            .map(|s| s.seq)
            .collect();
        for seq in &stale {
            if let Some(segment) = disk.segments.iter().find(|s| s.seq == *seq) {
                let _ = std::fs::remove_file(&segment.path);
            }
            disk.readers.remove(seq);
        }
        disk.segments.retain(|s| !stale.contains(&s.seq));

        disk.reopen(cut.0, cut.1, self.fsync)?;
        drop(disk);

        // Update the index first, then derive the surviving bounds from it.
        let new_last_index = {
            let mut index = self.index.write().unwrap();
            match keep_upto {
                Some(upto) => index.retain(|index, _| *index <= upto),
                None => index.clear(),
            }
            index.keys().next_back().copied()
        };

        {
            let mut disk = self.disk.write().unwrap();
            if let Some(segment) = disk.segment(cut.0) {
                let index = self.index.read().unwrap();
                segment.first_index = index
                    .iter()
                    .find(|(_, loc)| loc.seq == cut.0)
                    .map(|(index, _)| *index);
                segment.last_index = index
                    .iter()
                    .next_back()
                    .filter(|(_, loc)| loc.seq == cut.0)
                    .map(|(index, _)| *index);
            }
        }

        let mut state = self.state.write().unwrap();
        state.last_log_id = match (new_last_index, last_log_id) {
            (Some(last_index), Some(log_id)) if last_index == log_id.index => Some(log_id),
            _ => None,
        };
        // A commit marker that pointed past the truncated suffix is no longer meaningful.
        match &state.last_log_id {
            Some(last) => {
                if state
                    .committed
                    .as_ref()
                    .is_some_and(|committed| committed.index > last.index)
                {
                    state.committed = state.last_log_id.clone();
                }
            }
            None => state.committed = None,
        }
        drop(state);

        sync_dir(&self.dir)
    }

    fn read_entries_blocking(&self, locs: &[RecordLoc]) -> io::Result<Vec<C::Entry>> {
        let mut out = Vec::with_capacity(locs.len());
        let mut start = 0usize;

        while start < locs.len() {
            let first = locs[start];
            let mut end = first.offset + RECORD_HEADER_LEN + first.len as u64;
            let mut next = start + 1;

            // Records in one append batch are contiguous unless a commit marker
            // sits between them. Group the contiguous run into one pread.
            while next < locs.len() && locs[next].seq == first.seq && locs[next].offset == end {
                end += RECORD_HEADER_LEN + locs[next].len as u64;
                next += 1;
            }

            let file = self.reader(first.seq)?;
            let mut buf = vec![0u8; (end - first.offset) as usize];
            read_exact_at(&file, first.offset, &mut buf)?;

            let mut cursor = 0usize;
            for loc in &locs[start..next] {
                let header_end = cursor + RECORD_HEADER_LEN as usize;
                let len = u32::from_le_bytes(buf[cursor..cursor + 4].try_into().unwrap());
                let crc = u32::from_le_bytes(buf[cursor + 4..header_end].try_into().unwrap());
                if len != loc.len {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("log record length changed: {len} != {}", loc.len),
                    ));
                }

                let payload_end = header_end + len as usize;
                let payload = &buf[header_end..payload_end];
                if crc32fast::hash(payload) != crc {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "log record checksum mismatch",
                    ));
                }

                match decode_record::<C>(payload)? {
                    WalRecord::Entry(entry) => out.push(entry),
                    WalRecord::Committed(_) => {}
                }
                cursor = payload_end;
            }

            start = next;
        }

        Ok(out)
    }

    fn reader(&self, seq: u32) -> io::Result<Arc<File>> {
        if let Some(file) = self.disk.read().unwrap().readers.get(&seq) {
            return Ok(file.clone());
        }

        let path = self
            .disk
            .read()
            .unwrap()
            .segments
            .iter()
            .find(|s| s.seq == seq)
            .map(|s| s.path.clone())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("segment {seq} is gone"))
            })?;

        let file = Arc::new(File::open(path)?);
        self.disk.write().unwrap().readers.insert(seq, file.clone());
        Ok(file)
    }
}

/// Persistent Raft log backed by segmented WAL files.
///
/// Cloning is cheap: every clone shares the same on-disk log. openraft hands clones to replication
/// tasks as log readers.
pub struct LogStore<C: RaftTypeConfig> {
    inner: Arc<Inner<C>>,
}

impl<C: RaftTypeConfig> Clone for LogStore<C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<C: RaftTypeConfig> std::fmt::Debug for LogStore<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogStore")
            .field("dir", &self.inner.dir)
            .finish()
    }
}

impl<C: RaftTypeConfig> LogStore<C> {
    /// Opens (or creates) the log in `dir`, recovering any segments already there.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        Self::open_with_segment_size(dir, DEFAULT_SEGMENT_SIZE)
    }

    /// Same as [`Self::open`] with an explicit segment size. Small values are useful in tests.
    pub fn open_with_segment_size(dir: impl Into<PathBuf>, segment_size: u64) -> io::Result<Self> {
        Self::open_with_segment_size_and_fsync(dir, segment_size, None)
    }

    /// Opens the log with the default segment size and an explicit fsync policy.
    ///
    /// `None` disables fsync; `Some(Duration::ZERO)` fsyncs every append;
    /// `Some(interval)` fsyncs the active segment every `interval` after
    /// [`Self::start_fsync_worker`] is called from a Tokio runtime.
    pub fn open_with_fsync(dir: impl Into<PathBuf>, fsync: Option<Duration>) -> io::Result<Self> {
        Self::open_with_segment_size_and_fsync(dir, DEFAULT_SEGMENT_SIZE, fsync)
    }

    /// Opens the log with an explicit fsync policy and async-append switch.
    ///
    /// The async path is only active with the `async-wal` feature and
    /// `Some(Duration::ZERO)`; otherwise it is rejected by
    /// [`Self::start_async_wal_worker`].
    pub fn open_with_fsync_and_async_wal(
        dir: impl Into<PathBuf>,
        fsync: Option<Duration>,
        async_wal: bool,
    ) -> io::Result<Self> {
        Self::open_with_segment_size_fsync_and_async_wal(
            dir,
            DEFAULT_SEGMENT_SIZE,
            fsync,
            async_wal,
        )
    }

    /// Compatibility wrapper: `true` means fsync every append, `false` means no fsync.
    pub fn open_with_sync(dir: impl Into<PathBuf>, sync: bool) -> io::Result<Self> {
        let fsync = if sync { Some(Duration::ZERO) } else { None };
        Self::open_with_fsync(dir, fsync)
    }

    /// Same as [`Self::open_with_segment_size`] with an explicit fsync policy.
    pub fn open_with_segment_size_and_fsync(
        dir: impl Into<PathBuf>,
        segment_size: u64,
        fsync: Option<Duration>,
    ) -> io::Result<Self> {
        Self::open_with_segment_size_fsync_and_async_wal(dir, segment_size, fsync, false)
    }

    /// Same as [`Self::open_with_segment_size_and_fsync`] with the async-append switch.
    pub fn open_with_segment_size_fsync_and_async_wal(
        dir: impl Into<PathBuf>,
        segment_size: u64,
        fsync: Option<Duration>,
        async_wal: bool,
    ) -> io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;

        let fsync = FsyncPolicy::from_option(fsync);
        // Strict recovery only when every append is fsynced; otherwise a crash
        // can leave a torn non-last segment, so keep the longest valid prefix
        // and let Raft catch up from peers.
        let recover_corrupt = fsync != FsyncPolicy::EveryAppend;

        let mut index = BTreeMap::new();
        let mut state = RuntimeState::<C>::default();
        let mut segments = recover_segments::<C>(&dir, &mut index, &mut state, recover_corrupt)?;

        // The state file is written atomically, so a decode failure is real corruption.
        if let Some(bytes) = read_state_file(&dir)? {
            let persisted: PersistedState<C> = postcard::from_bytes(&bytes).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("corrupt log state file: {e}"),
                )
            })?;
            state.vote = persisted.vote;
            state.last_purged = persisted.last_purged;
        }

        // Self-heal a crash between writing the purge marker and deleting the covered segments.
        if let Some(purged) = state.last_purged.clone() {
            index.retain(|index, _| *index > purged.index);
            segments.retain(|segment| {
                if segment.last_index.is_none_or(|last| last <= purged.index) {
                    let _ = std::fs::remove_file(&segment.path);
                    false
                } else {
                    true
                }
            });

            // No surviving entry can be at or below the purge marker, so do not report one.
            if state
                .last_log_id
                .as_ref()
                .is_some_and(|last| last.index <= purged.index)
            {
                state.last_log_id = None;
            }
        }

        if segments.is_empty() {
            let seq = segments.last().map(|s| s.seq).unwrap_or(0);
            let path = segment_path(&dir, seq);
            write_segment_header(&path)?;
            sync_dir(&dir)?;
            segments.push(SegmentMeta::new(seq, path));
        }

        let active = segments.last().expect("a segment exists");
        let writer = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&active.path)?;
        let writer_len = writer.metadata()?.len();
        let writer_seq = active.seq;

        let inner = Arc::new(Inner {
            dir,
            segment_size,
            fsync,
            // Recovered data may still be unsynced; the periodic task should
            // flush it on its first tick.
            dirty: AtomicBool::new(true),
            fsync_started: AtomicBool::new(false),
            fsync_stop: Mutex::new(None),
            index: RwLock::new(index),
            state: RwLock::new(state),
            async_wal,
            pending: RwLock::new(BTreeMap::new()),
            #[cfg(feature = "async-wal")]
            async_tx: Mutex::new(None),
            #[cfg(feature = "async-wal")]
            async_started: AtomicBool::new(false),
            #[cfg(feature = "async-wal")]
            async_pending: AtomicUsize::new(0),
            #[cfg(feature = "async-wal")]
            async_idle: tokio::sync::Notify::new(),
            #[cfg(feature = "async-wal")]
            async_stop: Mutex::new(None),
            disk: RwLock::new(Disk {
                writer,
                writer_seq,
                writer_len,
                segments,
                readers: HashMap::new(),
                _marker: PhantomData,
            }),
            write_order: tokio::sync::Mutex::new(()),
            #[cfg(debug_assertions)]
            append_fault: AtomicU8::new(0),
        });

        Ok(Self { inner })
    }

    /// Compatibility wrapper: `true` means fsync every append, `false` means no fsync.
    pub fn open_with_segment_size_and_sync(
        dir: impl Into<PathBuf>,
        segment_size: u64,
        sync: bool,
    ) -> io::Result<Self> {
        let fsync = if sync { Some(Duration::ZERO) } else { None };
        Self::open_with_segment_size_and_fsync(dir, segment_size, fsync)
    }

    /// Starts the periodic fsync task for [`FsyncPolicy::Interval`].
    ///
    /// Must be called from a Tokio runtime. The node calls this during startup;
    /// direct [`LogStore`] users that configure a periodic policy must call it
    /// themselves. Calling it twice is a no-op.
    pub fn start_fsync_worker(&self) -> io::Result<()> {
        let interval = match self.inner.fsync {
            FsyncPolicy::Interval(interval) => interval,
            _ => return Ok(()),
        };
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| io::Error::other("periodic WAL fsync requires a Tokio runtime"))?;
        if self
            .inner
            .fsync_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        *self
            .inner
            .fsync_stop
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(stop_tx);
        let weak = Arc::downgrade(&self.inner);
        handle.spawn(fsync_loop(weak, interval, stop_rx));
        Ok(())
    }

    /// Starts the prototype async WAL append worker.
    ///
    /// Requires the `async-wal` Cargo feature and `RAFT_WAL_FSYNC_MS=0`.
    /// Calling it when async append is disabled is a no-op. Must be called from
    /// a Tokio runtime; the node calls it during startup.
    pub fn start_async_wal_worker(&self) -> io::Result<()> {
        if !self.inner.async_wal {
            return Ok(());
        }
        #[cfg(not(feature = "async-wal"))]
        {
            Err(io::Error::other(
                "async WAL requested but the binary was built without the `async-wal` feature",
            ))
        }
        #[cfg(feature = "async-wal")]
        {
            self.start_async_wal_worker_with_delay(Duration::ZERO)
        }
    }

    /// Test hook: start the async WAL worker with a delay before the fsync.
    #[cfg(feature = "async-wal")]
    #[doc(hidden)]
    pub fn start_async_wal_worker_with_delay(&self, delay: Duration) -> io::Result<()> {
        if !self.inner.async_wal {
            return Ok(());
        }
        if self.inner.fsync != FsyncPolicy::EveryAppend {
            return Err(io::Error::other(
                "async WAL prototype requires every-append fsync (RAFT_WAL_FSYNC_MS=0)",
            ));
        }
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| io::Error::other("async WAL worker requires a Tokio runtime"))?;
        if self
            .inner
            .async_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(u64, IOFlushed<C>)>();
        *self
            .inner
            .async_tx
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(tx);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        *self
            .inner
            .async_stop
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(stop_tx);
        let weak = Arc::downgrade(&self.inner);
        handle.spawn(async_wal_loop(weak, rx, stop_rx, delay));
        Ok(())
    }

    /// Directory holding this log's segments.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Whether the WAL has writes that have not been fsynced yet.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn wal_dirty(&self) -> bool {
        self.inner.dirty.load(Ordering::Acquire)
    }

    /// Arms a one-shot WAL append failure for fault-injection tests.
    ///
    /// Debug builds only: release builds have no hook and no per-append check.
    /// The error is returned before anything is written, so openraft treats it
    /// as a fatal storage error and stops the Raft core.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn inject_append_error(&self, storage_full: bool) {
        self.inner
            .append_fault
            .store(if storage_full { 1 } else { 2 }, Ordering::SeqCst);
    }

    #[cfg(feature = "async-wal")]
    async fn append_async(&self, entries: Vec<C::Entry>, callback: IOFlushed<C>) -> io::Result<()>
    where
        C::Entry: Clone + Send + 'static,
    {
        if entries.is_empty() {
            callback.io_completed(Ok(()));
            return Ok(());
        }

        let last_index = entries
            .last()
            .map(|entry| entry.log_id().index)
            .expect("non-empty entries");
        let pending_entries = entries.clone();
        let inner = self.inner.clone();
        // Hold the mutation lock until the batch is visible in `pending` and its
        // callback is queued. Otherwise `truncate_after`/`purge` could acquire the
        // lock, observe no pending work, and cut the log before this batch
        // registers itself.
        let _order = self.inner.write_order.lock().await;
        let tx = self
            .inner
            .async_tx
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let Some(tx) = tx else {
            let error = io::Error::other("async WAL worker is not started");
            callback.io_completed(Err(io::Error::new(error.kind(), error.to_string())));
            return Err(error);
        };
        let write_result = blocking(move || inner.append_blocking_deferred(entries)).await;

        if let Err(error) = write_result {
            callback.io_completed(Err(io::Error::new(error.kind(), error.to_string())));
            return Err(error);
        }

        {
            let mut pending = self.inner.pending.write().unwrap();
            for entry in &pending_entries {
                pending.insert(entry.log_id().index, entry.clone());
            }
        }
        self.inner.async_pending.fetch_add(1, Ordering::AcqRel);

        if let Err(error) = tx.send((last_index, callback)) {
            let (_, callback) = error.0;
            self.rollback_pending(&pending_entries);
            let error = io::Error::other("async WAL worker stopped");
            callback.io_completed(Err(io::Error::new(error.kind(), error.to_string())));
            return Err(error);
        }
        Ok(())
    }

    #[cfg(feature = "async-wal")]
    fn rollback_pending(&self, entries: &[C::Entry]) {
        let mut pending = self.inner.pending.write().unwrap();
        for entry in entries {
            pending.remove(&entry.log_id().index);
        }
        drop(pending);
        if self.inner.async_pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.async_idle.notify_one();
        }
    }

    #[cfg(feature = "async-wal")]
    async fn flush_async_wal(&self) -> io::Result<()> {
        if !self.inner.async_wal {
            return Ok(());
        }
        loop {
            if self.inner.async_pending.load(Ordering::Acquire) == 0 {
                return Ok(());
            }
            self.inner.async_idle.notified().await;
            if self.inner.async_pending.load(Ordering::Acquire) == 0 {
                return Ok(());
            }
        }
    }

    #[cfg(not(feature = "async-wal"))]
    async fn flush_async_wal(&self) -> io::Result<()> {
        Ok(())
    }

    async fn append_entries(&self, entries: Vec<C::Entry>) -> io::Result<()>
    where
        C::Entry: Clone + Send + 'static,
    {
        if entries.is_empty() {
            return Ok(());
        }

        let _order = self.inner.write_order.lock().await;
        let inner = self.inner.clone();
        blocking(move || inner.append_blocking(entries)).await
    }
}

impl<C> RaftLogReader<C> for LogStore<C>
where
    C: RaftTypeConfig,
    C::Entry: Clone + Send + 'static,
{
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug>(
        &mut self,
        range: RB,
    ) -> Result<Vec<C::Entry>, io::Error> {
        let pending_entries: Vec<C::Entry> = if self.inner.async_wal {
            let pending = self.inner.pending.read().unwrap();
            pending
                .range(range.clone())
                .map(|(_, entry)| entry.clone())
                .collect()
        } else {
            Vec::new()
        };
        if pending_entries.is_empty() {
            let locs: Vec<RecordLoc> = {
                let index = self.inner.index.read().unwrap();
                index.range(range).map(|(_, loc)| *loc).collect()
            };
            if locs.is_empty() {
                return Ok(Vec::new());
            }
            let inner = self.inner.clone();
            return blocking(move || inner.read_entries_blocking(&locs)).await;
        }

        let pending_indexes: BTreeSet<u64> = pending_entries
            .iter()
            .map(|entry| entry.log_id().index)
            .collect();
        let locs: Vec<RecordLoc> = {
            let index = self.inner.index.read().unwrap();
            index
                .range(range)
                .filter(|(index, _)| !pending_indexes.contains(index))
                .map(|(_, loc)| *loc)
                .collect()
        };

        if locs.is_empty() {
            let mut entries = pending_entries;
            entries.sort_by_key(|entry| entry.log_id().index);
            return Ok(entries);
        }

        let inner = self.inner.clone();
        let mut entries = blocking(move || inner.read_entries_blocking(&locs)).await?;
        entries.extend(pending_entries);
        entries.sort_by_key(|entry| entry.log_id().index);
        Ok(entries)
    }

    async fn read_vote(&mut self) -> Result<Option<VoteOf<C>>, io::Error> {
        Ok(self.inner.state.read().unwrap().vote.clone())
    }
}

impl<C> RaftLogStorage<C> for LogStore<C>
where
    C: RaftTypeConfig,
    C::Entry: Clone + Send + 'static,
{
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<C>, io::Error> {
        let state = self.inner.state.read().unwrap();
        let last_log_id = state
            .last_log_id
            .clone()
            .or_else(|| state.last_purged.clone());

        Ok(LogState {
            last_purged_log_id: state.last_purged.clone(),
            last_log_id,
        })
    }

    async fn save_committed(&mut self, committed: Option<LogIdOf<C>>) -> Result<(), io::Error> {
        let _order = self.inner.write_order.lock().await;
        let inner = self.inner.clone();
        blocking(move || inner.save_committed_blocking(committed)).await
    }

    async fn read_committed(&mut self) -> Result<Option<LogIdOf<C>>, io::Error> {
        Ok(self.inner.state.read().unwrap().committed.clone())
    }

    async fn save_vote(&mut self, vote: &VoteOf<C>) -> Result<(), io::Error> {
        let _order = self.inner.write_order.lock().await;
        let inner = self.inner.clone();
        let vote = vote.clone();
        blocking(move || inner.save_vote_blocking(&vote)).await
    }

    async fn append<I>(&mut self, entries: I, callback: IOFlushed<C>) -> Result<(), io::Error>
    where
        I: IntoIterator<Item = C::Entry> + openraft::OptionalSend,
    {
        let entries: Vec<C::Entry> = entries.into_iter().collect();
        #[cfg(feature = "async-wal")]
        if self.inner.async_wal {
            return self.append_async(entries, callback).await;
        }
        #[cfg(not(feature = "async-wal"))]
        if self.inner.async_wal {
            let error = io::Error::other(
                "async WAL requested but the binary was built without the `async-wal` feature",
            );
            callback.io_completed(Err(io::Error::new(error.kind(), error.to_string())));
            return Err(error);
        }
        self.append_entries(entries).await?;
        callback.io_completed(Ok(()));
        Ok(())
    }

    async fn truncate_after(&mut self, last_log_id: Option<LogIdOf<C>>) -> Result<(), io::Error> {
        let _order = self.inner.write_order.lock().await;
        self.flush_async_wal().await?;
        let inner = self.inner.clone();
        blocking(move || inner.truncate_after_blocking(last_log_id)).await
    }

    async fn purge(&mut self, log_id: LogIdOf<C>) -> Result<(), io::Error> {
        let _order = self.inner.write_order.lock().await;
        self.flush_async_wal().await?;
        let inner = self.inner.clone();
        blocking(move || inner.purge_blocking(log_id)).await
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }
}

// --- File format helpers

fn segment_header() -> [u8; SEGMENT_HEADER_LEN as usize] {
    let mut header = [0u8; SEGMENT_HEADER_LEN as usize];
    header[0..4].copy_from_slice(SEGMENT_MAGIC);
    header[4..8].copy_from_slice(&SEGMENT_VERSION.to_le_bytes());
    header
}

fn segment_path(dir: &Path, seq: u32) -> PathBuf {
    dir.join(format!("{seq:010}.wal"))
}

fn write_segment_header(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(&segment_header())?;
    file.sync_all()
}

fn decode_record<C: RaftTypeConfig>(payload: &[u8]) -> io::Result<WalRecord<C>> {
    postcard::from_bytes(payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Reads one WAL segment from memory, validating the header and every record
/// CRC.
///
/// This is the pure decoder behind [`read_wal_file`]; it is public so fuzz
/// targets and diagnostic tools can exercise the framing without touching the
/// filesystem. A truncated final frame is ignored, matching the recovery rule
/// that only the last segment may have a torn tail. A CRC mismatch or an
/// unsupported format returns an error.
pub fn read_wal_bytes<C: RaftTypeConfig>(bytes: &[u8]) -> io::Result<Vec<(u64, WalRecord<C>)>> {
    decode_wal_bytes::<C>(bytes, "WAL bytes")
}

fn decode_wal_bytes<C: RaftTypeConfig>(
    bytes: &[u8],
    source: &str,
) -> io::Result<Vec<(u64, WalRecord<C>)>> {
    let file_len = bytes.len() as u64;
    if file_len < SEGMENT_HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{source} has a truncated segment header"),
        ));
    }
    if &bytes[0..SEGMENT_MAGIC.len()] != SEGMENT_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{source} has a bad segment magic"),
        ));
    }
    let version = u32::from_le_bytes(
        bytes[SEGMENT_MAGIC.len()..SEGMENT_HEADER_LEN as usize]
            .try_into()
            .unwrap(),
    );
    if version != SEGMENT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{source} has unsupported segment version {version}"),
        ));
    }

    let mut records = Vec::new();
    let mut pos = SEGMENT_HEADER_LEN;
    while pos < file_len {
        if pos + RECORD_HEADER_LEN > file_len {
            break;
        }
        let frame = &bytes[pos as usize..(pos + RECORD_HEADER_LEN) as usize];
        let len = u32::from_le_bytes(frame[0..4].try_into().unwrap());
        let crc = u32::from_le_bytes(frame[4..8].try_into().unwrap());
        if len > MAX_RECORD_LEN || pos + RECORD_HEADER_LEN + len as u64 > file_len {
            break;
        }
        let payload_start = (pos + RECORD_HEADER_LEN) as usize;
        let payload = &bytes[payload_start..payload_start + len as usize];
        if crc32fast::hash(payload) != crc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{source} has a CRC mismatch at offset {pos}"),
            ));
        }
        records.push((pos, decode_record::<C>(payload)?));
        pos += RECORD_HEADER_LEN + len as u64;
    }
    Ok(records)
}

/// Reads one WAL segment without modifying it, validating the header and every
/// record CRC.
///
/// Returns `(offset, record)` pairs in file order. A truncated final frame is
/// ignored, matching the recovery rule that only the last segment may have a
/// torn tail. A CRC mismatch or an unsupported format returns an error.
pub fn read_wal_file<C: RaftTypeConfig>(path: &Path) -> io::Result<Vec<(u64, WalRecord<C>)>> {
    let bytes = std::fs::read(path)?;
    decode_wal_bytes::<C>(&bytes, &path.display().to_string())
}

/// `(log index, offset within the batch buffer, payload length)`.
type EncodedLocs = Vec<(u64, u64, u32)>;

/// Encodes a batch into one buffer, returning each entry's offset and payload length.
fn encode_entries<C: RaftTypeConfig>(entries: &[C::Entry]) -> io::Result<(Vec<u8>, EncodedLocs)>
where
    C::Entry: Clone,
{
    let mut buf = Vec::with_capacity(entries.len() * 128);
    let mut locs = Vec::with_capacity(entries.len());

    for entry in entries {
        let payload = postcard::to_allocvec(&WalRecordRef::<C>::Entry(entry))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let offset = buf.len() as u64;
        buf.extend_from_slice(&Disk::<C>::frame(&payload));
        buf.extend_from_slice(&payload);
        locs.push((entry.log_id().index, offset, payload.len() as u32));
    }

    Ok((buf, locs))
}

/// Rebuilds the index and hard state from every segment, truncating a torn tail in the last one.
struct RecoveredSegment {
    /// The segment metadata when the segment remains part of the log.
    meta: Option<SegmentMeta>,
    /// This segment is unusable; the caller must remove it and every later segment.
    stop: bool,
}

fn recover_segments<C: RaftTypeConfig>(
    dir: &Path,
    index: &mut BTreeMap<u64, RecordLoc>,
    state: &mut RuntimeState<C>,
    recover_corrupt: bool,
) -> io::Result<Vec<SegmentMeta>> {
    let mut found: Vec<(u32, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let seq = name.strip_suffix(".wal")?.parse::<u32>().ok()?;
            Some((seq, entry.path()))
        })
        .collect();
    found.sort_by_key(|(seq, _)| *seq);

    let mut segments = Vec::with_capacity(found.len());
    let mut expected_next = None;
    for position in 0..found.len() {
        let (seq, path) = &found[position];
        let is_last = position + 1 == found.len();
        let recovered = recover_segment::<C>(
            *seq,
            path,
            is_last,
            index,
            state,
            recover_corrupt,
            &mut expected_next,
        )?;
        let stop = recovered.stop;
        if let Some(meta) = recovered.meta {
            segments.push(meta);
        } else if stop {
            let _ = std::fs::remove_file(path);
        }
        if stop {
            for (_, later) in &found[position + 1..] {
                let _ = std::fs::remove_file(later);
            }
            break;
        }
    }

    Ok(segments)
}

fn recover_segment<C: RaftTypeConfig>(
    seq: u32,
    path: &Path,
    is_last: bool,
    index: &mut BTreeMap<u64, RecordLoc>,
    state: &mut RuntimeState<C>,
    recover_corrupt: bool,
    expected_next: &mut Option<u64>,
) -> io::Result<RecoveredSegment> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let bytes = std::fs::read(path)?;
    let file_len = bytes.len() as u64;
    let mut meta = SegmentMeta::new(seq, path.to_path_buf());

    if file_len < SEGMENT_HEADER_LEN {
        // A crash while creating a segment: only the last one may be repaired.
        if !is_last {
            if recover_corrupt {
                tracing::warn!(
                    path = %path.display(),
                    "discarding a non-last WAL segment with a truncated header"
                );
                return Ok(RecoveredSegment {
                    meta: None,
                    stop: true,
                });
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("segment {} has a truncated header", path.display()),
            ));
        }
        file.set_len(0)?;
        let mut file = OpenOptions::new().write(true).open(path)?;
        file.write_all(&segment_header())?;
        file.sync_all()?;
        return Ok(RecoveredSegment {
            meta: Some(meta),
            stop: false,
        });
    }

    let header = &bytes[..SEGMENT_HEADER_LEN as usize];
    if &header[0..4] != SEGMENT_MAGIC {
        if recover_corrupt {
            tracing::warn!(
                path = %path.display(),
                "discarding a WAL segment with a bad magic"
            );
            return Ok(RecoveredSegment {
                meta: None,
                stop: true,
            });
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("segment {} has a bad magic", path.display()),
        ));
    }
    let version = u32::from_le_bytes(header[4..8].try_into().unwrap());
    if version != SEGMENT_VERSION {
        if recover_corrupt {
            tracing::warn!(
                path = %path.display(),
                version,
                "discarding a WAL segment with an unsupported version"
            );
            return Ok(RecoveredSegment {
                meta: None,
                stop: true,
            });
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "segment {} has unsupported version {version}",
                path.display()
            ),
        ));
    }

    let mut pos = SEGMENT_HEADER_LEN;
    let mut valid_tail = pos;
    let mut torn = false;

    while pos < file_len {
        if pos + RECORD_HEADER_LEN > file_len {
            torn = true;
            break;
        }

        let frame = &bytes[pos as usize..(pos + RECORD_HEADER_LEN) as usize];
        let len = u32::from_le_bytes(frame[0..4].try_into().unwrap());
        let crc = u32::from_le_bytes(frame[4..8].try_into().unwrap());

        if len > MAX_RECORD_LEN || pos + RECORD_HEADER_LEN + len as u64 > file_len {
            torn = true;
            break;
        }

        let payload_start = (pos + RECORD_HEADER_LEN) as usize;
        let payload_end = payload_start + len as usize;
        let payload = &bytes[payload_start..payload_end];
        if crc32fast::hash(payload) != crc {
            torn = true;
            break;
        }

        let record = match decode_record::<C>(payload) {
            Ok(record) => record,
            Err(_) => {
                torn = true;
                break;
            }
        };

        match record {
            WalRecord::Entry(entry) => {
                let log_id = entry.log_id();
                let index_value = log_id.index;
                if let Some(expected) = *expected_next
                    && index_value != expected
                {
                    // A gap violates openraft's no-hole contract. Treat it like
                    // a torn record: keep the prefix before it.
                    torn = true;
                    break;
                }
                *expected_next = Some(index_value + 1);
                index.insert(
                    index_value,
                    RecordLoc {
                        seq,
                        offset: pos,
                        len,
                    },
                );
                if meta.first_index.is_none() {
                    meta.first_index = Some(index_value);
                }
                meta.last_index = Some(index_value);
                state.last_log_id = Some(log_id.clone());
            }
            WalRecord::Committed(committed) => {
                state.committed = committed;
            }
        }

        pos += RECORD_HEADER_LEN + len as u64;
        valid_tail = pos;
    }

    if torn {
        if !is_last && !recover_corrupt {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "segment {} has a torn record at offset {pos}; only the last segment may be truncated",
                    path.display()
                ),
            ));
        }
        tracing::warn!(
            path = %path.display(),
            offset = valid_tail,
            "discarding a torn WAL tail"
        );
        file.set_len(valid_tail)?;
        file.sync_all()?;
        return Ok(RecoveredSegment {
            meta: Some(meta),
            stop: !is_last,
        });
    }

    Ok(RecoveredSegment {
        meta: Some(meta),
        stop: false,
    })
}

fn read_state_file(dir: &Path) -> io::Result<Option<Vec<u8>>> {
    match std::fs::read(dir.join(STATE_FILE)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Rewrites the state file atomically: temp file, fsync, rename, fsync directory.
fn write_state_file<C: RaftTypeConfig>(dir: &Path, state: &PersistedState<C>) -> io::Result<()> {
    let tmp = dir.join(format!("{STATE_FILE}.tmp"));
    let path = dir.join(STATE_FILE);
    let bytes =
        postcard::to_allocvec(state).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let mut file = File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(&tmp, &path)?;
    sync_dir(dir)
}

#[cfg(unix)]
fn read_exact_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;

    while !buf.is_empty() {
        let read = file.read_at(buf, offset)?;
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
        }
        buf = &mut buf[read..];
        offset += read as u64;
    }
    Ok(())
}

/// Periodic fsync task. The actual `sync_data` call stays on the blocking pool.
async fn fsync_loop<C: RaftTypeConfig>(
    inner: Weak<Inner<C>>,
    interval: Duration,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first interval tick is immediate; recovered data should get a full
    // interval like the old thread did.
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                if !inner.dirty.swap(false, Ordering::AcqRel) {
                    continue;
                }
                // Clone the file descriptor under a short read lock, then fsync
                // the clone without holding the disk lock. Appends can continue
                // while the filesystem flushes the previous writes.
                let file = {
                    let disk = inner.disk.read().unwrap();
                    disk.writer.try_clone()
                };
                let file = match file {
                    Ok(file) => file,
                    Err(error) => {
                        inner.dirty.store(true, Ordering::Release);
                        tracing::warn!(%error, "periodic WAL fsync clone failed");
                        continue;
                    }
                };
                let result = tokio::task::spawn_blocking(move || file.sync_data()).await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        inner.dirty.store(true, Ordering::Release);
                        tracing::warn!(%error, "periodic WAL fsync failed");
                    }
                    Err(error) => {
                        inner.dirty.store(true, Ordering::Release);
                        tracing::warn!(%error, "periodic WAL fsync task failed");
                    }
                }
            }
            _ = &mut stop => return,
        }
    }
}

#[cfg(feature = "async-wal")]
async fn async_wal_loop<C: RaftTypeConfig>(
    inner: Weak<Inner<C>>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<(u64, IOFlushed<C>)>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
    delay: Duration,
) {
    loop {
        let Some((mut max_index, first)) = (tokio::select! {
            item = rx.recv() => item,
            _ = &mut stop => return,
        }) else {
            return;
        };
        let mut callbacks = vec![first];
        while let Ok((index, callback)) = rx.try_recv() {
            max_index = max_index.max(index);
            callbacks.push(callback);
        }

        if !delay.is_zero() {
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = &mut stop => return,
            }
        }

        let completed = callbacks.len();
        let Some(inner) = inner.upgrade() else {
            for callback in callbacks {
                callback.io_completed(Err(io::Error::other("async WAL store dropped")));
            }
            return;
        };

        // Appends are serialized and segment rotation fsyncs the previous
        // segment when fsync is enabled, so syncing the current writer covers
        // every queued batch, including one written before a rotation.
        let file = {
            let disk = inner.disk.read().unwrap();
            disk.writer.try_clone()
        };
        let result = match file {
            Ok(file) => match tokio::task::spawn_blocking(move || file.sync_data()).await {
                Ok(result) => result,
                Err(error) => Err(io::Error::other(format!(
                    "async WAL fsync task failed: {error}"
                ))),
            },
            Err(error) => Err(error),
        };

        match result {
            Ok(()) => {
                inner
                    .pending
                    .write()
                    .unwrap()
                    .retain(|index, _| *index > max_index);
                for callback in callbacks {
                    callback.io_completed(Ok(()));
                }
            }
            Err(error) => {
                // The batch is not durable and its callbacks now carry an error;
                // do not leave it readable in the pending cache.
                inner
                    .pending
                    .write()
                    .unwrap()
                    .retain(|index, _| *index > max_index);
                let kind = error.kind();
                let message = error.to_string();
                for callback in callbacks {
                    callback.io_completed(Err(io::Error::new(kind, message.clone())));
                }
            }
        }

        if inner.async_pending.fetch_sub(completed, Ordering::AcqRel) == completed {
            inner.async_idle.notify_waiters();
            inner.async_idle.notify_one();
        }
    }
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

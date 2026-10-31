//! Append-only, segmented write-ahead log for Raft entries.
//!
//! # Design
//!
//! - Records are framed as `[len: u32 LE][crc32: u32 LE][postcard payload]`, so a torn or corrupt
//!   record is detected and dropped instead of being parsed as garbage.
//! - `append()` writes the whole openraft batch with **one** `write_all` and **one** `sync_data`.
//!   openraft batches entries per AppendEntries RPC, which makes that group commit at the Raft
//!   layer: N client requests replicated in one RPC cost one fsync.
//! - Reads use `pread` (`read_at`), so replication can read while a write is in flight and no
//!   global lock is held during disk IO.
//! - All blocking IO runs on `spawn_blocking`; the async runtime only encodes.
//! - Vote and the last purged log id are kept in a small `state` file that is rewritten atomically.
//!   They are rare, safety-critical updates and must survive segment deletion. `committed` rides
//!   at the tail of the WAL and becomes durable with the next append fsync, which avoids a second
//!   fsync per batch.
//! - Segments are deleted only when fully covered by a purge, and the active segment is rotated
//!   first, so a purge never leaves a hole.

use std::collections::BTreeMap;
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
use std::sync::RwLock;

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

    /// Starts a new segment. The previous one is fsynced first so nothing is lost.
    fn rotate(&mut self, dir: &Path) -> io::Result<()> {
        self.writer.sync_data()?;

        let seq = self.writer_seq + 1;
        let path = segment_path(dir, seq);
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .append(true)
            .open(&path)?;
        file.write_all(&segment_header())?;
        file.sync_data()?;
        sync_dir(dir)?;

        self.writer = file;
        self.writer_seq = seq;
        self.writer_len = SEGMENT_HEADER_LEN;
        self.segments.push(SegmentMeta::new(seq, path));
        Ok(())
    }

    /// Reopens an existing segment for appending at `len`.
    fn reopen(&mut self, seq: u32, len: u64) -> io::Result<()> {
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
        file.sync_data()?;

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
    index: RwLock<BTreeMap<u64, RecordLoc>>,
    state: RwLock<RuntimeState<C>>,
    disk: RwLock<Disk<C>>,
    /// Serializes mutations so concurrent appends cannot interleave on disk.
    write_order: tokio::sync::Mutex<()>,
}

impl<C: RaftTypeConfig> Inner<C> {
    fn append_blocking(&self, entries: Vec<C::Entry>) -> io::Result<()>
    where
        C::Entry: Clone,
    {
        if entries.is_empty() {
            return Ok(());
        }

        let last_log_id = entries.last().map(|e| e.log_id().clone());
        let first_index = entries.first().map(|e| e.log_id().index);
        let (buf, locs) = encode_entries::<C>(&entries)?;

        let mut disk = self.disk.write().unwrap();

        if disk.writer_len > SEGMENT_HEADER_LEN
            && disk.writer_len + buf.len() as u64 > self.segment_size
        {
            disk.rotate(&self.dir)?;
        }

        let seq = disk.writer_seq;
        let base = disk.writer_len;

        if let Err(e) = disk.writer.write_all(&buf) {
            // Undo a partial write: a hole in the log is much worse than losing the batch.
            let _ = disk.writer.set_len(base);
            let _ = disk.writer.sync_data();
            disk.writer_len = base;
            return Err(e);
        }
        if let Err(e) = disk.writer.sync_data() {
            // The bytes may or may not be durable; drop them so the retry starts at `base`.
            let _ = disk.writer.set_len(base);
            let _ = disk.writer.sync_data();
            disk.writer_len = base;
            return Err(e);
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
            disk.rotate(&self.dir)?;
        }
        let base = disk.writer_len;
        if let Err(e) = disk.write_frame(&payload) {
            // Undo the partial frame so the next append starts at a record boundary.
            let _ = disk.writer.set_len(base);
            disk.writer_len = base;
            return Err(e);
        }
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
                disk.rotate(&self.dir)?;
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

        disk.reopen(cut.0, cut.1)?;
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
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;

        let mut index = BTreeMap::new();
        let mut state = RuntimeState::<C>::default();
        let mut segments = recover_segments::<C>(&dir, &mut index, &mut state)?;

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

        let inner = Inner {
            dir,
            segment_size,
            index: RwLock::new(index),
            state: RwLock::new(state),
            disk: RwLock::new(Disk {
                writer,
                writer_seq,
                writer_len,
                segments,
                readers: HashMap::new(),
                _marker: PhantomData,
            }),
            write_order: tokio::sync::Mutex::new(()),
        };

        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Directory holding this log's segments.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
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
        let locs: Vec<RecordLoc> = {
            let index = self.inner.index.read().unwrap();
            index.range(range).map(|(_, loc)| *loc).collect()
        };

        if locs.is_empty() {
            return Ok(Vec::new());
        }

        let inner = self.inner.clone();
        blocking(move || inner.read_entries_blocking(&locs)).await
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
        self.append_entries(entries).await?;
        callback.io_completed(Ok(()));
        Ok(())
    }

    async fn truncate_after(&mut self, last_log_id: Option<LogIdOf<C>>) -> Result<(), io::Error> {
        let _order = self.inner.write_order.lock().await;
        let inner = self.inner.clone();
        blocking(move || inner.truncate_after_blocking(last_log_id)).await
    }

    async fn purge(&mut self, log_id: LogIdOf<C>) -> Result<(), io::Error> {
        let _order = self.inner.write_order.lock().await;
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

/// Reads one WAL segment without modifying it, validating the header and every
/// record CRC.
///
/// Returns `(offset, record)` pairs in file order. A truncated final frame is
/// ignored, matching the recovery rule that only the last segment may have a
/// torn tail. A CRC mismatch or an unsupported format returns an error.
pub fn read_wal_file<C: RaftTypeConfig>(path: &Path) -> io::Result<Vec<(u64, WalRecord<C>)>> {
    let bytes = std::fs::read(path)?;
    let file_len = bytes.len() as u64;
    if file_len < SEGMENT_HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} has a truncated segment header", path.display()),
        ));
    }
    if &bytes[0..SEGMENT_MAGIC.len()] != SEGMENT_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} has a bad segment magic", path.display()),
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
            format!(
                "{} has unsupported segment version {version}",
                path.display()
            ),
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
                format!("{} has a CRC mismatch at offset {pos}", path.display()),
            ));
        }
        records.push((pos, decode_record::<C>(payload)?));
        pos += RECORD_HEADER_LEN + len as u64;
    }
    Ok(records)
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
fn recover_segments<C: RaftTypeConfig>(
    dir: &Path,
    index: &mut BTreeMap<u64, RecordLoc>,
    state: &mut RuntimeState<C>,
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

    let count = found.len();
    let mut segments = Vec::with_capacity(count);

    for (position, (seq, path)) in found.into_iter().enumerate() {
        let is_last = position + 1 == count;
        let meta = recover_segment::<C>(seq, &path, is_last, index, state)?;
        segments.push(meta);
    }

    Ok(segments)
}

fn recover_segment<C: RaftTypeConfig>(
    seq: u32,
    path: &Path,
    is_last: bool,
    index: &mut BTreeMap<u64, RecordLoc>,
    state: &mut RuntimeState<C>,
) -> io::Result<SegmentMeta> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let bytes = std::fs::read(path)?;
    let file_len = bytes.len() as u64;
    let mut meta = SegmentMeta::new(seq, path.to_path_buf());

    if file_len < SEGMENT_HEADER_LEN {
        // A crash while creating a segment: only the last one may be repaired.
        if !is_last {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("segment {} has a truncated header", path.display()),
            ));
        }
        file.set_len(0)?;
        let mut file = OpenOptions::new().write(true).open(path)?;
        file.write_all(&segment_header())?;
        file.sync_all()?;
        return Ok(meta);
    }

    let header = &bytes[..SEGMENT_HEADER_LEN as usize];
    if &header[0..4] != SEGMENT_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("segment {} has a bad magic", path.display()),
        ));
    }
    let version = u32::from_le_bytes(header[4..8].try_into().unwrap());
    if version != SEGMENT_VERSION {
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
        if !is_last {
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
    }

    Ok(meta)
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

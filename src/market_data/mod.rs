//! Market-data egress: a tap on `apply`, a shadow book and a publisher task.
//!
//! This is the deterministic feed pipeline from `docs/MARKET_DATA_PLAN.md` and
//! `docs/AERON_SOLUTION.md` Part II. The matching path only records depth
//! deltas it already knows ([`crate::order_book::OrderBook::take_depth_journal`])
//! and hands them, with the entry's [`MatchEvent`]s, to a bounded channel. A
//! dedicated tokio task owns the derived state and all encoding/IO:
//!
//! ```text
//! apply ──► MarketDataTap::publish (try_send, never blocks)
//!                │
//!                ▼
//!        market-data task
//!          shadow book · entry-driven batching · OHLC bars · wire encoding
//!                │
//!                ▼
//!        MarketDataSink (JSONL today, Aeron next)
//! ```
//!
//! Timestamps are the intake timestamps carried in the replicated requests
//! (§13); nothing here reads a clock to label data. Message boundaries are a
//! pure function of the applied entry, so every replica derives the same keys.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use serde::Serialize;

use crate::match_result::MatchEvent;
use crate::match_result::MatchEventKind;
use crate::order_book::DepthChange;
use crate::order_book::DepthLevel;
use crate::order_book::FeeAsset;
use crate::order_book::Fixed;
use crate::order_book::OrderSide;
use crate::order_book::RejectReason;
use crate::order_book::VolumeUnit;

pub mod wire;

/// Aeron stream IDs reserved by the design (§15).
pub const STREAM_TRADE: u16 = 2001;
/// Latest-value top of book.
pub const STREAM_BBO: u16 = 2002;
/// Full top-N depth snapshot (reset/recovery path).
pub const STREAM_DEPTH_SNAPSHOT: u16 = 2003;
/// Coalesced depth changes (post-batch totals).
pub const STREAM_DEPTH_DELTA: u16 = 2004;
/// Order-lifecycle events, one message per entry.
pub const STREAM_LIFECYCLE: u16 = 2005;
/// Deterministic status cadence and diagnostic gaps.
pub const STREAM_STATUS: u16 = 2006;
/// OHLCV bars.
pub const STREAM_BAR: u16 = 2007;

/// `sub_seq` reserved for the local `GAP` diagnostic, so it can never collide
/// with a deterministic message in the same `(stream, market_seq)` anchor.
pub const GAP_SUB_SEQ: u16 = u16::MAX;
/// `sub_seq` of the reset snapshot's BBO: the first post-snapshot frame starts
/// at the same `market_seq`, so the two must not share a key.
const SNAPSHOT_BBO_SUB_SEQ: u16 = 1;

/// Header flags (§16).
pub mod flags {
    /// The message carries a full snapshot.
    pub const SNAPSHOT: u16 = 1 << 0;
    /// First message of a publication session / leader generation.
    pub const RESET: u16 = 1 << 1;
    /// Records were dropped before this message.
    pub const GAP: u16 = 1 << 2;
    /// Last message of an entry's batch.
    pub const END_OF_BATCH: u16 = 1 << 3;
    /// Reserved for a future transport-level throttle marker.
    pub const RESERVED_THROTTLE: u16 = 1 << 4;
    /// A live (still open) OHLC bar.
    pub const PARTIAL_BAR: u16 = 1 << 5;
    /// A final OHLC bar.
    pub const BAR_CLOSE: u16 = 1 << 6;
}

/// Configuration for the feed pipeline.
#[derive(Clone, Debug)]
pub struct MarketDataConfig {
    /// Optional JSONL debug sink. `None` runs the pipeline with a null sink
    /// (useful in tests and when the transport is injected separately).
    pub log_path: Option<PathBuf>,
    /// Depth levels per side in snapshots (default 10).
    pub depth_levels: usize,
    /// Emit a deterministic STATUS message when `market_seq % status_every == 0`
    /// (default 100; 0 disables).
    pub status_every: u64,
    /// Maximum lifecycle records per entry (default 512); `rested` is shed first
    /// when the entry overflows. Trades are never capped.
    pub lifecycle_batch_max: usize,
    /// OHLC intervals in milliseconds (default `1s` and `1m`).
    pub bar_intervals_ms: Vec<u64>,
    /// Bounded tap queue; overflow drops the frame and requests a `GAP`.
    pub queue_capacity: usize,
    /// Publish only on the leader (default true).
    pub leader_only: bool,
}

impl Default for MarketDataConfig {
    fn default() -> Self {
        Self {
            log_path: None,
            depth_levels: 10,
            status_every: 100,
            lifecycle_batch_max: 512,
            bar_intervals_ms: vec![1_000, 60_000],
            queue_capacity: 8_192,
            leader_only: true,
        }
    }
}

impl MarketDataConfig {
    /// Enables the feed with a JSONL debug sink at `path`.
    pub fn jsonl(path: impl Into<PathBuf>) -> Self {
        Self {
            log_path: Some(path.into()),
            ..Self::default()
        }
    }
}

/// Feed counters, shared by the tap, the processor and `/read`.
#[derive(Debug, Default)]
pub struct MarketDataStats {
    /// Frames handed to the tap (including frames dropped on a full queue).
    pub frames: AtomicU64,
    /// Fill records published across trade messages.
    pub trades: AtomicU64,
    /// Trade messages published.
    pub trade_messages: AtomicU64,
    /// BBO updates published.
    pub bbo: AtomicU64,
    /// Depth snapshots published.
    pub depth_snapshots: AtomicU64,
    /// Depth delta messages published.
    pub depth_deltas: AtomicU64,
    /// Lifecycle messages published.
    pub lifecycle_messages: AtomicU64,
    /// Lifecycle records shed because the batch budget was exceeded.
    pub lifecycle_shed: AtomicU64,
    /// Bars published (closed + live).
    pub bars: AtomicU64,
    /// Status messages published.
    pub status: AtomicU64,
    /// Gap markers published.
    pub gaps: AtomicU64,
    /// Frames dropped on a full queue.
    pub dropped_frames: AtomicU64,
    /// Snapshots installed.
    pub snapshots: AtomicU64,
    /// `market_seq` of the last processed frame.
    pub last_market_seq: AtomicU64,
    /// `log_index` of the last processed frame.
    pub last_log_index: AtomicU64,
    /// `ts_ms` of the last processed frame.
    pub last_ts_ms: AtomicU64,
    /// Whether this node currently publishes.
    pub publishing: AtomicBool,
    /// Sink name, set once when the feed starts.
    sink: OnceLock<&'static str>,
}

impl MarketDataStats {
    /// Records the sink name for `/read marketdata_status`.
    pub fn set_sink(&self, sink: &'static str) {
        let _ = self.sink.set(sink);
    }

    /// Renders the counters as the JSON served by `/read marketdata_status`.
    pub fn status_json(&self) -> String {
        serde_json::json!({
            "sink": self.sink.get().copied().unwrap_or("none"),
            "publishing": self.publishing.load(Ordering::Relaxed),
            "frames": self.frames.load(Ordering::Relaxed),
            "trades": self.trades.load(Ordering::Relaxed),
            "trade_messages": self.trade_messages.load(Ordering::Relaxed),
            "bbo": self.bbo.load(Ordering::Relaxed),
            "depth_snapshots": self.depth_snapshots.load(Ordering::Relaxed),
            "depth_deltas": self.depth_deltas.load(Ordering::Relaxed),
            "lifecycle_messages": self.lifecycle_messages.load(Ordering::Relaxed),
            "lifecycle_shed": self.lifecycle_shed.load(Ordering::Relaxed),
            "bars": self.bars.load(Ordering::Relaxed),
            "status": self.status.load(Ordering::Relaxed),
            "gaps": self.gaps.load(Ordering::Relaxed),
            "dropped_frames": self.dropped_frames.load(Ordering::Relaxed),
            "snapshots": self.snapshots.load(Ordering::Relaxed),
            "last_market_seq": self.last_market_seq.load(Ordering::Relaxed),
            "last_log_index": self.last_log_index.load(Ordering::Relaxed),
            "last_ts_ms": self.last_ts_ms.load(Ordering::Relaxed),
        })
        .to_string()
    }
}

// ---------------------------------------------------------------------------
// Wire records
// ---------------------------------------------------------------------------

/// One executed fill in a `TRADE` batch.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TradeRecord {
    /// Raft log index of the entry that produced the fill.
    pub log_index: u64,
    /// Ordinal of the fill inside that entry.
    pub ordinal: u32,
    /// Taker order's intake timestamp (Unix ms).
    pub ts_ms: u64,
    /// Execution price (maker price), scale 18.
    pub price: Fixed,
    /// Executed volume, scale 18.
    pub volume: Fixed,
    /// Gross quote notional, scale 18.
    pub quote_volume: Fixed,
    /// Taker fee for this fill; positive = charge, negative = rebate.
    pub taker_fee: Fixed,
    /// Asset of `taker_fee`.
    pub taker_fee_asset: Option<FeeAsset>,
    /// Maker fee for this fill; positive = charge, negative = rebate.
    pub maker_fee: Fixed,
    /// Asset of `maker_fee`.
    pub maker_fee_asset: Option<FeeAsset>,
    /// True when the fee calculation overflowed the supported range.
    pub fee_overflow: bool,
    /// Taker side (the maker's side is the opposite).
    pub taker_side: OrderSide,
    /// Taker order sequence.
    pub taker_sequence: u64,
    /// Maker order sequence.
    pub maker_sequence: u64,
}

/// Latest-value top of book.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BboRecord {
    /// Last trade price, if any.
    pub last_trade: Option<Fixed>,
    /// Best bid price; zero when there is no bid.
    pub bid: Fixed,
    /// Visible volume at the best bid.
    pub bid_volume: Fixed,
    /// Best ask price; zero when there is no ask.
    pub ask: Fixed,
    /// Visible volume at the best ask.
    pub ask_volume: Fixed,
    /// `(bid + ask) / 2` when both sides exist (truncated to 18 places).
    pub mid: Option<Fixed>,
    /// `ask - bid` when both sides exist.
    pub spread: Option<Fixed>,
}

/// Full top-N depth for one instrument.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DepthSnapshotRecord {
    /// Bid levels, best first.
    pub bids: Vec<DepthLevel>,
    /// Ask levels, best first.
    pub asks: Vec<DepthLevel>,
}

/// One touched price level with its post-batch totals.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LevelUpdate {
    /// Side of the level.
    pub side: OrderSide,
    /// Level after the batch.
    pub level: DepthLevel,
}

/// One order-lifecycle event.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LifecycleRecord {
    /// Raft log index.
    pub log_index: u64,
    /// Ordinal inside the entry.
    pub ordinal: u32,
    /// Intake timestamp (Unix ms).
    pub ts_ms: u64,
    /// What happened.
    pub kind: MatchEventKind,
    /// Sequence of the order the event is about.
    pub order_sequence: u64,
    /// Side of that order.
    pub side: OrderSide,
    /// Maker sequence on a fill.
    pub counterparty_sequence: Option<u64>,
    /// Price associated with the event, scale 18.
    pub price: Fixed,
    /// Volume associated with the event, scale 18.
    pub volume: Fixed,
    /// Gross quote notional for a fill; zero otherwise.
    pub quote_volume: Fixed,
    /// Unit of `volume`.
    pub volume_unit: VolumeUnit,
    /// Taker fee for a fill; zero for non-fill events.
    pub taker_fee: Fixed,
    /// Asset of `taker_fee`, when applicable.
    pub taker_fee_asset: Option<FeeAsset>,
    /// Maker fee for a fill; zero for non-fill events.
    pub maker_fee: Fixed,
    /// Asset of `maker_fee`, when applicable.
    pub maker_fee_asset: Option<FeeAsset>,
    /// True when a fill's fee calculation overflowed.
    pub fee_overflow: bool,
    /// Rejection/expiry reason when present.
    pub reason: Option<RejectReason>,
}

/// One OHLCV bar.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BarRecord {
    /// Bar width.
    pub interval_ms: u64,
    /// Start of the window (Unix ms, aligned to `interval_ms`).
    pub window_start_ms: u64,
    /// Last trade timestamp included.
    pub last_ts_ms: u64,
    /// First trade price in the window.
    pub open: Fixed,
    /// Highest trade price.
    pub high: Fixed,
    /// Lowest trade price.
    pub low: Fixed,
    /// Last trade price.
    pub close: Fixed,
    /// Summed trade volume.
    pub volume: Fixed,
    /// Number of fills aggregated.
    pub trade_count: u32,
    /// Monotonic per `(symbol, interval)`.
    pub bar_seq: u64,
}

/// Deterministic status cadence record.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusRecord {
    /// Last processed log index.
    pub last_log_index: u64,
    /// Last processed ordinal.
    pub last_ordinal: u32,
    /// Last processed market sequence.
    pub last_market_seq: u64,
    /// Sub-sequence of the last message in that anchor.
    pub last_sub_seq: u16,
    /// Last applied log index (same as `last_log_index` for a live feed).
    pub applied_index: u64,
    /// Replicated book sequence counter.
    pub book_sequence: u64,
    /// Parked stop orders at the snapshot.
    pub pending_stops: u32,
}

/// A hard-failure marker: frames were dropped before this message.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GapRecord {
    /// Last position published before the gap.
    pub last_good_log_index: u64,
    /// Last ordinal published before the gap.
    pub last_good_ordinal: u32,
    /// Frames dropped since the previous gap marker.
    pub dropped: u32,
}

/// A decoded market-data message: one wire frame.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MarketMessage {
    /// Destination stream (2001..2007).
    pub stream: u16,
    /// Header flags.
    pub flags: u16,
    /// Replicated market/mutation sequence (the taker order sequence for Place).
    pub market_seq: u64,
    /// Deterministic message index within `(stream, market_seq)`; `GAP` uses
    /// [`GAP_SUB_SEQ`] and the reset snapshot's BBO uses 1.
    pub sub_seq: u16,
    /// Raft log index the message refers to.
    pub log_index: u64,
    /// Ordinal inside that entry.
    pub ordinal: u32,
    /// Leader generation (Raft term; 0 until wired).
    pub epoch: u64,
    /// Leader node id (0 until wired).
    pub leader_id: u64,
    /// Intake timestamp (Unix ms).
    pub ts_ms: u64,
    /// Instrument id of this process/cluster.
    pub symbol_id: u32,
    /// Message payload.
    pub body: MessageBody,
}

/// Payload variants; one per stream.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageBody {
    /// Executed fills.
    Trade(Vec<TradeRecord>),
    /// Latest top of book.
    Bbo(BboRecord),
    /// Full depth snapshot.
    DepthSnapshot(DepthSnapshotRecord),
    /// Coalesced post-batch level totals.
    DepthDelta(Vec<LevelUpdate>),
    /// Order lifecycle events.
    Lifecycle(Vec<LifecycleRecord>),
    /// Deterministic status cadence.
    Status(StatusRecord),
    /// Dropped-frame marker.
    Gap(GapRecord),
    /// OHLCV bar.
    Bar(BarRecord),
}

/// A frame handed from `apply` to the market-data task.
#[derive(Debug, Clone)]
pub struct TapFrame {
    /// Raft log index of the entry.
    pub log_index: u64,
    /// Replicated market/mutation sequence of the entry (the taker order
    /// sequence for a `Place`).
    pub market_seq: u64,
    /// Intake timestamp of the request (Unix ms).
    pub ts_ms: u64,
    /// Instrument id of this process/cluster.
    pub symbol_id: u32,
    /// Events the entry produced.
    pub events: Vec<MatchEvent>,
    /// Visible-depth changes recorded by the book.
    pub depth: Vec<DepthChange>,
}

/// A full state capture used to reset the feed at start-up / leadership gain.
#[derive(Debug, Clone)]
pub struct SnapshotFrame {
    /// Applied log index the snapshot reflects.
    pub log_index: u64,
    /// Next replicated market/mutation sequence: the exclusive reset boundary.
    /// Frames below it are already in the snapshot; the first post-capture
    /// frame starts exactly here.
    pub market_seq: u64,
    /// Capture timestamp (Unix ms).
    pub ts_ms: u64,
    /// Instrument id of this process/cluster.
    pub symbol_id: u32,
    /// Bid levels, best first.
    pub bids: Vec<DepthLevel>,
    /// Ask levels, best first.
    pub asks: Vec<DepthLevel>,
    /// Last trade price, if any.
    pub last_trade: Option<Fixed>,
    /// Replicated book sequence counter.
    pub book_sequence: u64,
    /// Parked stop orders.
    pub pending_stops: u32,
}

/// Messages consumed by the market-data task.
#[derive(Debug)]
pub enum Ingest {
    /// One applied entry.
    Frame(TapFrame),
}

// ---------------------------------------------------------------------------
// Sink
// ---------------------------------------------------------------------------

/// Consumer of decoded market-data messages.
///
/// The JSONL implementation is the debugging aid shipped today; the Aeron
/// adapter will implement this trait and send [`wire::encode`] bytes.
pub trait MarketDataSink: Send + 'static {
    /// Short name used in logs and in `/read marketdata_status`.
    fn name(&self) -> &'static str;

    /// Publishes one message. A failure is logged and never reaches `apply`.
    fn publish(&mut self, message: &MarketMessage) -> io::Result<()>;
}

/// Discards every message (used when the transport is injected separately).
#[derive(Debug, Default)]
pub struct NullSink;

impl MarketDataSink for NullSink {
    fn name(&self) -> &'static str {
        "null"
    }

    fn publish(&mut self, _message: &MarketMessage) -> io::Result<()> {
        Ok(())
    }
}

/// Appends one JSON object per message to a local file.
///
/// A debugging aid, like [`crate::match_result::FileLogSink`]: node-local, not
/// replicated, not fsynced, and replayed after a restart if the log is replayed.
pub struct JsonlSink {
    writer: BufWriter<File>,
    node_id: u64,
}

impl fmt::Debug for JsonlSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonlSink")
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

impl JsonlSink {
    /// Opens `path` in append mode, creating parent directories.
    pub fn open(path: impl AsRef<Path>, node_id: u64) -> io::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            node_id,
        })
    }
}

impl MarketDataSink for JsonlSink {
    fn name(&self) -> &'static str {
        "jsonl"
    }

    fn publish(&mut self, message: &MarketMessage) -> io::Result<()> {
        #[derive(Serialize)]
        struct Record<'a> {
            node_id: u64,
            #[serde(flatten)]
            message: &'a MarketMessage,
        }
        let line = serde_json::to_string(&Record {
            node_id: self.node_id,
            message,
        })
        .map_err(io::Error::other)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }
}

// ---------------------------------------------------------------------------
// Tap and handle
// ---------------------------------------------------------------------------

/// Receivers and shared counters handed out with a [`MarketDataTap`].
pub struct TapChannels {
    /// The processor's input queue.
    pub receiver: Receiver<Ingest>,
    /// Counters shared by the tap, processor and `/read`.
    pub stats: Arc<MarketDataStats>,
    /// Frames dropped because the queue was full.
    pub dropped: Arc<AtomicU64>,
    /// Set when a frame was dropped; the processor emits a `GAP`.
    pub gap_pending: Arc<AtomicBool>,
    /// The publishing gate shared with the processor and the leader watcher.
    pub enabled: Arc<AtomicBool>,
}

/// The matching path's handle on the feed.
///
/// `publish` is a `try_send`: it never blocks and never fails `apply`. The tap
/// is cheap to clone; every clone feeds the same queue.
#[derive(Clone)]
pub struct MarketDataTap {
    sender: Sender<Ingest>,
    stats: Arc<MarketDataStats>,
    dropped: Arc<AtomicU64>,
    gap_pending: Arc<AtomicBool>,
    /// Publishing gate. While closed the tap drops frames at the source, so no
    /// stale frame can be queued ahead of the takeover snapshot.
    enabled: Arc<AtomicBool>,
}

impl fmt::Debug for MarketDataTap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MarketDataTap").finish_non_exhaustive()
    }
}

impl MarketDataTap {
    /// Creates a tap and the receiving side of its bounded queue.
    pub fn channel(capacity: usize) -> (Self, TapChannels) {
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity.max(1));
        let stats = Arc::new(MarketDataStats::default());
        let dropped = Arc::new(AtomicU64::new(0));
        let gap_pending = Arc::new(AtomicBool::new(false));
        let enabled = Arc::new(AtomicBool::new(true));
        (
            Self {
                sender,
                stats: stats.clone(),
                dropped: dropped.clone(),
                gap_pending: gap_pending.clone(),
                enabled: enabled.clone(),
            },
            TapChannels {
                receiver,
                stats,
                dropped,
                gap_pending,
                enabled,
            },
        )
    }

    /// Hands one applied entry to the feed. Never blocks.
    ///
    /// While the gate is closed the frame is dropped at the source: followers
    /// and deposed leaders do not fill the queue with data that a takeover
    /// snapshot would supersede.
    pub fn publish(&self, frame: TapFrame) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        match self.sender.try_send(Ingest::Frame(frame)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Closed(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.gap_pending.store(true, Ordering::Relaxed);
                self.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Counters as JSON for `/read marketdata_status`.
    pub fn status_json(&self) -> String {
        self.stats.status_json()
    }

    /// Shared counters.
    pub fn stats(&self) -> &Arc<MarketDataStats> {
        &self.stats
    }
}

/// A cheap, cloneable control handle for the leader watcher.
#[derive(Clone, Debug)]
pub struct MarketDataControl {
    leader: Arc<AtomicBool>,
    leader_only: bool,
    resetting: Arc<AtomicBool>,
    pending: Arc<Mutex<Option<SnapshotFrame>>>,
    wake: Arc<Notify>,
}

impl MarketDataControl {
    /// Builds a control handle from its parts (used by tests and the node wiring).
    pub(crate) fn new(
        leader: Arc<AtomicBool>,
        leader_only: bool,
        resetting: Arc<AtomicBool>,
        pending: Arc<Mutex<Option<SnapshotFrame>>>,
        wake: Arc<Notify>,
    ) -> Self {
        Self {
            leader,
            leader_only,
            resetting,
            pending,
            wake,
        }
    }

    /// Suppresses emission until the next reset snapshot is applied.
    pub fn begin_reset(&self) {
        self.resetting.store(true, Ordering::Relaxed);
    }

    /// Whether this node is currently allowed to publish (leader-only check).
    pub fn is_publishing(&self) -> bool {
        !self.leader_only || self.leader.load(Ordering::Relaxed)
    }

    /// Opens or closes the publish gate (the tap and processor share it).
    ///
    /// A leader-only feed closes on loss and waits for the next takeover
    /// snapshot; a feed that publishes from every replica stays open.
    pub fn set_leader(&self, leader: bool) {
        if self.leader_only {
            self.leader.store(leader, Ordering::Relaxed);
            if !leader {
                self.resetting.store(true, Ordering::Relaxed);
            }
        } else {
            self.leader.store(true, Ordering::Relaxed);
        }
    }

    /// Queues a reset snapshot. The processor applies it before any later frame.
    pub fn push_snapshot(&self, snapshot: SnapshotFrame) {
        *self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(snapshot);
        self.resetting.store(true, Ordering::Relaxed);
        self.wake.notify_one();
    }

    /// Takes the pending reset snapshot, if any (tests).
    #[cfg(test)]
    pub(crate) fn take_pending(&self) -> Option<SnapshotFrame> {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

/// Owns the processor task; created by [`crate::build_match_engine_node`].
pub struct MarketDataHandle {
    tap: MarketDataTap,
    leader: Arc<AtomicBool>,
    leader_only: bool,
    resetting: Arc<AtomicBool>,
    pending: Arc<Mutex<Option<SnapshotFrame>>>,
    wake: Arc<Notify>,
    stop: watch::Sender<bool>,
    join: Option<JoinHandle<()>>,
}

impl fmt::Debug for MarketDataHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MarketDataHandle").finish_non_exhaustive()
    }
}

impl MarketDataHandle {
    /// Starts the tokio processor task with the configured sink.
    pub async fn start(config: MarketDataConfig, node_id: u64) -> io::Result<Self> {
        let sink: Box<dyn MarketDataSink> = match &config.log_path {
            Some(path) => Box::new(JsonlSink::open(path, node_id)?),
            None => Box::new(NullSink),
        };
        let leader_only = config.leader_only;
        let (tap, channels) = MarketDataTap::channel(config.queue_capacity);
        channels.stats.set_sink(sink.name());
        // A leader-only node starts closed and waits for its first snapshot;
        // an all-replica feed starts open but still waits for the startup reset.
        channels.enabled.store(!leader_only, Ordering::Relaxed);
        let leader = channels.enabled.clone();
        let resetting = Arc::new(AtomicBool::new(true));
        let pending = Arc::new(Mutex::new(None));
        let wake = Arc::new(Notify::new());
        let (stop, stop_rx) = watch::channel(false);

        let mut processor = MarketDataProcessor::new(
            config,
            sink,
            channels.stats,
            leader.clone(),
            resetting.clone(),
            pending.clone(),
            channels.dropped,
            channels.gap_pending,
        );
        let processor_wake = wake.clone();
        let join = tokio::spawn(async move {
            processor
                .run(channels.receiver, stop_rx, processor_wake)
                .await;
        });

        Ok(Self {
            tap,
            leader,
            leader_only,
            resetting,
            pending,
            wake,
            stop,
            join: Some(join),
        })
    }

    /// A clone of the tap to install on the state machine.
    pub fn tap(&self) -> MarketDataTap {
        self.tap.clone()
    }

    /// A cloneable control handle for the leadership watcher.
    pub fn control(&self) -> MarketDataControl {
        MarketDataControl::new(
            self.leader.clone(),
            self.leader_only,
            self.resetting.clone(),
            self.pending.clone(),
            self.wake.clone(),
        )
    }

    /// Updates the publish gate (driven by `raft.metrics()`).
    pub fn set_leader(&self, leader: bool) {
        self.control().set_leader(leader);
    }

    /// Queues a state capture; the processor emits it before later deltas.
    pub fn push_snapshot(&self, snapshot: SnapshotFrame) {
        self.control().push_snapshot(snapshot);
    }

    /// Stops and awaits the processor task. Safe to call once.
    pub async fn shutdown(&mut self) {
        let _ = self.stop.send(true);
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

// ---------------------------------------------------------------------------
// Shadow book
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct LevelState {
    price: Fixed,
    volume: Fixed,
    orders: u32,
}

/// Visible book per instrument, fed by the depth journal.
#[derive(Debug, Default)]
struct ShadowBook {
    bids: BTreeMap<Fixed, LevelState>,
    asks: BTreeMap<Fixed, LevelState>,
    last_trade: Option<Fixed>,
}

impl ShadowBook {
    fn apply(&mut self, change: DepthChange) {
        let map = match change.side {
            OrderSide::Buy => &mut self.bids,
            OrderSide::Sell => &mut self.asks,
        };
        let key = change.price;
        let level = map.entry(key).or_insert(LevelState {
            price: change.price,
            volume: Fixed::ZERO,
            orders: 0,
        });
        level.volume += change.delta_volume;
        let orders = i64::from(level.orders) + i64::from(change.delta_orders);
        level.orders = orders.max(0) as u32;
        if level.volume <= Fixed::ZERO && level.orders == 0 {
            map.remove(&key);
        }
    }

    fn reset(&mut self, bids: &[DepthLevel], asks: &[DepthLevel], last_trade: Option<Fixed>) {
        self.bids = bids
            .iter()
            .map(|level| {
                (
                    level.price,
                    LevelState {
                        price: level.price,
                        volume: level.volume,
                        orders: level.orders,
                    },
                )
            })
            .collect();
        self.asks = asks
            .iter()
            .map(|level| {
                (
                    level.price,
                    LevelState {
                        price: level.price,
                        volume: level.volume,
                        orders: level.orders,
                    },
                )
            })
            .collect();
        self.last_trade = last_trade;
    }

    fn best_bid(&self) -> Option<LevelState> {
        self.bids.values().next_back().copied()
    }

    fn best_ask(&self) -> Option<LevelState> {
        self.asks.values().next().copied()
    }

    fn level(&self, side: OrderSide, price: Fixed) -> Option<LevelState> {
        match side {
            OrderSide::Buy => self.bids.get(&price).copied(),
            OrderSide::Sell => self.asks.get(&price).copied(),
        }
    }

    fn depth(&self, side: OrderSide, levels: usize) -> Vec<DepthLevel> {
        let iter: Box<dyn Iterator<Item = &LevelState>> = match side {
            OrderSide::Buy => Box::new(self.bids.values().rev()),
            OrderSide::Sell => Box::new(self.asks.values()),
        };
        iter.take(levels)
            .map(|level| DepthLevel {
                price: level.price,
                volume: level.volume,
                orders: level.orders,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Bars
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct BarAgg {
    interval_ms: u64,
    window_start_ms: u64,
    last_ts_ms: u64,
    open: Fixed,
    high: Fixed,
    low: Fixed,
    close: Fixed,
    volume: Fixed,
    trade_count: u32,
    last_live_ms: u64,
}

impl BarAgg {
    fn new(interval_ms: u64, window_start_ms: u64, ts_ms: u64, first_price: Fixed) -> Self {
        Self {
            interval_ms,
            window_start_ms,
            last_ts_ms: ts_ms,
            open: first_price,
            high: first_price,
            low: first_price,
            close: first_price,
            volume: Fixed::ZERO,
            trade_count: 0,
            last_live_ms: ts_ms,
        }
    }

    fn update(&mut self, ts_ms: u64, price: Fixed, volume: Fixed) {
        self.high = self.high.max(price);
        self.low = self.low.min(price);
        self.close = price;
        self.volume += volume;
        self.trade_count += 1;
        self.last_ts_ms = ts_ms;
    }

    fn record(&self, bar_seq: u64) -> BarRecord {
        BarRecord {
            interval_ms: self.interval_ms,
            window_start_ms: self.window_start_ms,
            last_ts_ms: self.last_ts_ms,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
            trade_count: self.trade_count,
            bar_seq,
        }
    }
}

// ---------------------------------------------------------------------------
// Processor
// ---------------------------------------------------------------------------

struct MarketDataProcessor {
    config: MarketDataConfig,
    sink: Box<dyn MarketDataSink>,
    stats: Arc<MarketDataStats>,
    leader: Arc<AtomicBool>,
    resetting: Arc<AtomicBool>,
    pending: Arc<Mutex<Option<SnapshotFrame>>>,
    shadow: BTreeMap<u32, ShadowBook>,
    bars: BTreeMap<(u32, u64), BarAgg>,
    bar_seq: BTreeMap<(u32, u64), u64>,
    active_market_seq: Option<u64>,
    book_sequence: u64,
    pending_stops: u32,
    dropped: Arc<AtomicU64>,
    dropped_seen: u64,
    gap_pending: Arc<AtomicBool>,
    last_position: (u64, u32),
    /// Frames applied after a takeover snapshot was captured but before it was
    /// installed; they are replayed once the snapshot is the active boundary.
    pre_snapshot: Vec<TapFrame>,
}

impl MarketDataProcessor {
    // One argument per shared handle keeps ownership obvious at the single
    // construction site; this is not a hot-path call.
    #[allow(clippy::too_many_arguments)]
    fn new(
        config: MarketDataConfig,
        sink: Box<dyn MarketDataSink>,
        stats: Arc<MarketDataStats>,
        leader: Arc<AtomicBool>,
        resetting: Arc<AtomicBool>,
        pending: Arc<Mutex<Option<SnapshotFrame>>>,
        dropped: Arc<AtomicU64>,
        gap_pending: Arc<AtomicBool>,
    ) -> Self {
        Self {
            config,
            sink,
            stats,
            leader,
            resetting,
            pending,
            shadow: BTreeMap::new(),
            bars: BTreeMap::new(),
            bar_seq: BTreeMap::new(),
            active_market_seq: None,
            book_sequence: 0,
            pending_stops: 0,
            dropped,
            dropped_seen: 0,
            gap_pending,
            last_position: (0, 0),
            pre_snapshot: Vec::new(),
        }
    }

    /// Runs the processor loop until `stop` is set.
    async fn run(
        &mut self,
        mut receiver: Receiver<Ingest>,
        mut stop: watch::Receiver<bool>,
        wake: Arc<Notify>,
    ) {
        loop {
            if *stop.borrow() {
                break;
            }
            if let Some(snapshot) = self.take_pending() {
                self.apply_snapshot(snapshot);
            }
            tokio::select! {
                message = receiver.recv() => match message {
                    Some(Ingest::Frame(frame)) => self.ingest(frame),
                    None => break,
                },
                _ = wake.notified() => {}
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
            }
        }
    }

    fn take_pending(&self) -> Option<SnapshotFrame> {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    fn publishing(&self) -> bool {
        let gate_open = !self.config.leader_only || self.leader.load(Ordering::Relaxed);
        gate_open && !self.resetting.load(Ordering::Relaxed)
    }

    /// Installs a reset point: new shadow, cleared bars, new active sequence.
    fn apply_snapshot(&mut self, snapshot: SnapshotFrame) {
        self.stats.snapshots.fetch_add(1, Ordering::Relaxed);
        let symbol = snapshot.symbol_id;
        self.shadow.entry(symbol).or_default().reset(
            &snapshot.bids,
            &snapshot.asks,
            snapshot.last_trade,
        );
        self.bars.clear();
        self.bar_seq.clear();
        self.active_market_seq = Some(snapshot.market_seq);
        self.book_sequence = snapshot.book_sequence;
        self.pending_stops = snapshot.pending_stops;
        self.dropped_seen = self.dropped.load(Ordering::Relaxed);
        self.last_position = (snapshot.log_index, 0);
        self.resetting.store(false, Ordering::Relaxed);
        self.stats
            .publishing
            .store(self.publishing(), Ordering::Relaxed);

        let body = DepthSnapshotRecord {
            bids: self.shadow.get(&symbol).map_or_else(Vec::new, |book| {
                book.depth(OrderSide::Buy, self.config.depth_levels)
            }),
            asks: self.shadow.get(&symbol).map_or_else(Vec::new, |book| {
                book.depth(OrderSide::Sell, self.config.depth_levels)
            }),
        };
        self.emit(
            STREAM_DEPTH_SNAPSHOT,
            flags::SNAPSHOT | flags::RESET,
            snapshot.market_seq,
            0,
            snapshot.log_index,
            0,
            snapshot.ts_ms,
            symbol,
            MessageBody::DepthSnapshot(body),
        );
        self.stats.depth_snapshots.fetch_add(1, Ordering::Relaxed);
        self.emit_bbo(
            symbol,
            snapshot.market_seq,
            snapshot.log_index,
            0,
            snapshot.ts_ms,
            SNAPSHOT_BBO_SUB_SEQ,
        );

        // Replay frames applied between the capture and the snapshot install.
        // Anything below the boundary is already in the snapshot.
        let buffered = std::mem::take(&mut self.pre_snapshot);
        for frame in buffered {
            self.ingest(frame);
        }
    }

    // `is_multiple_of` would raise the crate's MSRV above 1.85.
    #[allow(clippy::manual_is_multiple_of)]
    fn ingest(&mut self, frame: TapFrame) {
        // A takeover snapshot was captured at an earlier log position; frames
        // applied after the capture but before the snapshot is installed carry
        // a sequence at or above its boundary and must not be lost.
        if self.resetting.load(Ordering::Relaxed) {
            if self.pre_snapshot.len() < 4_096 {
                self.pre_snapshot.push(frame);
            } else {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.gap_pending.store(true, Ordering::Relaxed);
                self.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        self.stats
            .last_market_seq
            .store(frame.market_seq, Ordering::Relaxed);
        self.stats
            .last_log_index
            .store(frame.log_index, Ordering::Relaxed);
        self.stats.last_ts_ms.store(frame.ts_ms, Ordering::Relaxed);
        let publishing = self.publishing();
        self.stats.publishing.store(publishing, Ordering::Relaxed);
        if !publishing {
            return;
        }
        if self
            .active_market_seq
            .is_some_and(|active| frame.market_seq < active)
        {
            return;
        }

        if self.gap_pending.swap(false, Ordering::Relaxed) {
            let dropped = self.dropped.load(Ordering::Relaxed);
            let delta = dropped.saturating_sub(self.dropped_seen);
            self.dropped_seen = dropped;
            self.emit(
                STREAM_STATUS,
                flags::GAP,
                frame.market_seq,
                GAP_SUB_SEQ,
                frame.log_index,
                0,
                frame.ts_ms,
                frame.symbol_id,
                MessageBody::Gap(GapRecord {
                    last_good_log_index: self.last_position.0,
                    last_good_ordinal: self.last_position.1,
                    dropped: delta.min(u64::from(u32::MAX)) as u32,
                }),
            );
            self.stats.gaps.fetch_add(1, Ordering::Relaxed);
        }

        let symbol = frame.symbol_id;
        let ordinal = frame
            .events
            .last()
            .map_or(0, |event| event.event_id.ordinal);

        // Visible depth: apply the journal, then publish post-batch totals.
        let mut touched: BTreeSet<(u8, Fixed)> = BTreeSet::new();
        for change in &frame.depth {
            let side = match change.side {
                OrderSide::Buy => 0u8,
                OrderSide::Sell => 1u8,
            };
            self.shadow.entry(symbol).or_default().apply(*change);
            touched.insert((side, change.price));
        }

        // Fills: all of them go into one TRADE message (no chunking).
        let mut trades = Vec::new();
        for event in &frame.events {
            if event.kind != MatchEventKind::Fill {
                continue;
            }
            let Some(maker_sequence) = event.counterparty_sequence else {
                continue;
            };
            self.shadow.entry(symbol).or_default().last_trade = Some(event.price);
            trades.push(TradeRecord {
                log_index: event.event_id.log_index,
                ordinal: event.event_id.ordinal,
                ts_ms: frame.ts_ms,
                price: event.price,
                volume: event.volume,
                quote_volume: event.quote_volume,
                taker_side: event.side,
                taker_sequence: event.order_sequence,
                maker_sequence,
                taker_fee: event.taker_fee,
                taker_fee_asset: event.taker_fee_asset,
                maker_fee: event.maker_fee,
                maker_fee_asset: event.maker_fee_asset,
                fee_overflow: event.fee_overflow,
            });
        }

        if !touched.is_empty() {
            let mut updates = Vec::new();
            if let Some(book) = self.shadow.get(&symbol) {
                for (side, price) in &touched {
                    let side = if *side == 0 {
                        OrderSide::Buy
                    } else {
                        OrderSide::Sell
                    };
                    if let Some(level) = book.level(side, *price) {
                        updates.push(LevelUpdate {
                            side,
                            level: DepthLevel {
                                price: level.price,
                                volume: level.volume,
                                orders: level.orders,
                            },
                        });
                    }
                }
            }
            if !updates.is_empty() {
                self.emit(
                    STREAM_DEPTH_DELTA,
                    flags::END_OF_BATCH,
                    frame.market_seq,
                    0,
                    frame.log_index,
                    ordinal,
                    frame.ts_ms,
                    symbol,
                    MessageBody::DepthDelta(updates),
                );
                self.stats.depth_deltas.fetch_add(1, Ordering::Relaxed);
            }
        }

        if !touched.is_empty() || !trades.is_empty() {
            self.emit_bbo(
                symbol,
                frame.market_seq,
                frame.log_index,
                ordinal,
                frame.ts_ms,
                0,
            );
        }

        if !trades.is_empty() {
            self.stats
                .trades
                .fetch_add(trades.len() as u64, Ordering::Relaxed);
            self.emit(
                STREAM_TRADE,
                flags::END_OF_BATCH,
                frame.market_seq,
                0,
                frame.log_index,
                ordinal,
                frame.ts_ms,
                symbol,
                MessageBody::Trade(trades),
            );
            self.stats.trade_messages.fetch_add(1, Ordering::Relaxed);
        }

        if !frame.events.is_empty() {
            let mut records: Vec<LifecycleRecord> = frame
                .events
                .iter()
                .map(|event| LifecycleRecord {
                    log_index: event.event_id.log_index,
                    ordinal: event.event_id.ordinal,
                    ts_ms: frame.ts_ms,
                    kind: event.kind,
                    order_sequence: event.order_sequence,
                    side: event.side,
                    counterparty_sequence: event.counterparty_sequence,
                    price: event.price,
                    volume: event.volume,
                    quote_volume: event.quote_volume,
                    volume_unit: event.volume_unit,
                    taker_fee: event.taker_fee,
                    taker_fee_asset: event.taker_fee_asset,
                    maker_fee: event.maker_fee,
                    maker_fee_asset: event.maker_fee_asset,
                    fee_overflow: event.fee_overflow,
                    reason: event.reason,
                })
                .collect();
            let max = self.config.lifecycle_batch_max.max(1);
            if records.len() > max {
                let overflow = records.len() - max;
                let mut shed = 0usize;
                records.retain(|record| {
                    if shed < overflow && record.kind == MatchEventKind::Rested {
                        shed += 1;
                        false
                    } else {
                        true
                    }
                });
                self.stats
                    .lifecycle_shed
                    .fetch_add(shed as u64, Ordering::Relaxed);
            }
            self.emit(
                STREAM_LIFECYCLE,
                flags::END_OF_BATCH,
                frame.market_seq,
                0,
                frame.log_index,
                ordinal,
                frame.ts_ms,
                symbol,
                MessageBody::Lifecycle(records),
            );
            self.stats
                .lifecycle_messages
                .fetch_add(1, Ordering::Relaxed);
        }

        self.update_bars(
            symbol,
            frame.market_seq,
            frame.log_index,
            ordinal,
            frame.ts_ms,
            &frame.events,
        );

        if self.config.status_every > 0 && frame.market_seq % self.config.status_every == 0 {
            self.emit(
                STREAM_STATUS,
                0,
                frame.market_seq,
                0,
                frame.log_index,
                ordinal,
                frame.ts_ms,
                symbol,
                MessageBody::Status(StatusRecord {
                    last_market_seq: frame.market_seq,
                    last_sub_seq: 0,
                    last_log_index: frame.log_index,
                    last_ordinal: ordinal,
                    applied_index: self.stats.last_log_index.load(Ordering::Relaxed),
                    book_sequence: self.book_sequence,
                    pending_stops: self.pending_stops,
                }),
            );
            self.stats.status.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn emit_bbo(
        &mut self,
        symbol: u32,
        market_seq: u64,
        log_index: u64,
        ordinal: u32,
        ts_ms: u64,
        sub_seq: u16,
    ) {
        let Some(book) = self.shadow.get(&symbol) else {
            return;
        };
        let bid = book.best_bid();
        let ask = book.best_ask();
        if bid.is_none() && ask.is_none() && book.last_trade.is_none() {
            return;
        }
        let (bid_price, bid_volume) = bid.map_or((Fixed::ZERO, Fixed::ZERO), |level| {
            (level.price, level.volume)
        });
        let (ask_price, ask_volume) = ask.map_or((Fixed::ZERO, Fixed::ZERO), |level| {
            (level.price, level.volume)
        });
        let mid = if bid.is_some() && ask.is_some() {
            bid_price.midpoint(ask_price)
        } else {
            None
        };
        let spread = if bid.is_some() && ask.is_some() {
            Some(ask_price - bid_price)
        } else {
            None
        };
        self.emit(
            STREAM_BBO,
            flags::END_OF_BATCH,
            market_seq,
            sub_seq,
            log_index,
            ordinal,
            ts_ms,
            symbol,
            MessageBody::Bbo(BboRecord {
                last_trade: book.last_trade,
                bid: bid_price,
                bid_volume,
                ask: ask_price,
                ask_volume,
                mid,
                spread,
            }),
        );
        self.stats.bbo.fetch_add(1, Ordering::Relaxed);
    }

    fn update_bars(
        &mut self,
        symbol: u32,
        market_seq: u64,
        log_index: u64,
        ordinal: u32,
        ts_ms: u64,
        events: &[MatchEvent],
    ) {
        let fills: Vec<(Fixed, Fixed)> = events
            .iter()
            .filter(|event| event.kind == MatchEventKind::Fill)
            .map(|event| (event.price, event.volume))
            .collect();
        if fills.is_empty() {
            return;
        }

        let mut outgoing: Vec<(BarAgg, bool)> = Vec::new();
        let intervals = self.config.bar_intervals_ms.clone();
        for interval in &intervals {
            let interval = *interval;
            let key = (symbol, interval);
            let window = ts_ms - (ts_ms % interval);

            match self.bars.get_mut(&key) {
                Some(bar) if bar.window_start_ms == window => {
                    for (price, volume) in &fills {
                        bar.update(ts_ms, *price, *volume);
                    }
                }
                Some(bar) => {
                    let old =
                        std::mem::replace(bar, BarAgg::new(interval, window, ts_ms, fills[0].0));
                    for (price, volume) in &fills {
                        bar.update(ts_ms, *price, *volume);
                    }
                    if old.trade_count > 0 {
                        outgoing.push((old, true));
                    }
                }
                None => {
                    let mut bar = BarAgg::new(interval, window, ts_ms, fills[0].0);
                    for (price, volume) in &fills {
                        bar.update(ts_ms, *price, *volume);
                    }
                    self.bars.insert(key, bar);
                }
            }
        }

        for interval in &intervals {
            let key = (symbol, *interval);
            if let Some(bar) = self.bars.get_mut(&key)
                && bar.last_ts_ms > bar.last_live_ms
                && bar.last_ts_ms.saturating_sub(bar.last_live_ms) >= 1_000
            {
                bar.last_live_ms = bar.last_ts_ms;
                outgoing.push((bar.clone(), false));
            }
        }

        for (sub_seq, (bar, closed)) in outgoing.into_iter().enumerate() {
            let seq = {
                let entry = self.bar_seq.entry((symbol, bar.interval_ms)).or_insert(0);
                *entry += 1;
                *entry
            };
            let ts = if closed {
                bar.window_start_ms + bar.interval_ms
            } else {
                bar.last_ts_ms
            };
            self.emit(
                STREAM_BAR,
                if closed {
                    flags::BAR_CLOSE
                } else {
                    flags::PARTIAL_BAR
                },
                market_seq,
                sub_seq.min(usize::from(u16::MAX)) as u16,
                log_index,
                ordinal,
                ts,
                symbol,
                MessageBody::Bar(bar.record(seq)),
            );
            self.stats.bars.fetch_add(1, Ordering::Relaxed);
        }
    }

    // One argument per wire-header field keeps every call site explicit.
    #[allow(clippy::too_many_arguments)]
    fn emit(
        &mut self,
        stream: u16,
        message_flags: u16,
        market_seq: u64,
        sub_seq: u16,
        log_index: u64,
        ordinal: u32,
        ts_ms: u64,
        symbol_id: u32,
        body: MessageBody,
    ) {
        let message = MarketMessage {
            stream,
            flags: message_flags,
            market_seq,
            sub_seq,
            log_index,
            ordinal,
            epoch: 0,
            leader_id: 0,
            ts_ms,
            symbol_id,
            body,
        };
        if let Err(error) = self.sink.publish(&message) {
            tracing::warn!(sink = self.sink.name(), %error, "market-data sink failed");
        }
        self.last_position = (log_index, ordinal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order_book::MatchResult;
    use crate::order_book::Order;
    use crate::order_book::fixed::SCALE;

    #[derive(Clone, Default)]
    struct CollectSink(Arc<Mutex<Vec<MarketMessage>>>);

    impl MarketDataSink for CollectSink {
        fn name(&self) -> &'static str {
            "collect"
        }

        fn publish(&mut self, message: &MarketMessage) -> io::Result<()> {
            self.0.lock().unwrap().push(message.clone());
            Ok(())
        }
    }

    struct Harness {
        processor: MarketDataProcessor,
        messages: Arc<Mutex<Vec<MarketMessage>>>,
        stats: Arc<MarketDataStats>,
        leader: Arc<AtomicBool>,
        resetting: Arc<AtomicBool>,
        gap_pending: Arc<AtomicBool>,
        dropped: Arc<AtomicU64>,
    }

    fn harness(config: MarketDataConfig) -> Harness {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let stats = Arc::new(MarketDataStats::default());
        let leader = Arc::new(AtomicBool::new(true));
        let resetting = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(Mutex::new(None));
        let dropped = Arc::new(AtomicU64::new(0));
        let gap_pending = Arc::new(AtomicBool::new(false));
        let processor = MarketDataProcessor::new(
            config,
            Box::new(CollectSink(messages.clone())),
            stats.clone(),
            leader.clone(),
            resetting.clone(),
            pending,
            dropped.clone(),
            gap_pending.clone(),
        );
        Harness {
            processor,
            messages,
            stats,
            leader,
            resetting,
            gap_pending,
            dropped,
        }
    }

    impl Harness {
        fn messages(&self) -> Vec<MarketMessage> {
            self.messages.lock().unwrap().clone()
        }

        fn bodies(&self) -> Vec<MessageBody> {
            self.messages()
                .into_iter()
                .map(|message| message.body)
                .collect()
        }

        fn clear(&self) {
            self.messages.lock().unwrap().clear();
        }

        fn assert_unique_keys(&self) {
            let messages = self.messages();
            let mut keys: Vec<(u16, u64, u16)> = messages
                .iter()
                .map(|message| (message.stream, message.market_seq, message.sub_seq))
                .collect();
            let total = keys.len();
            keys.sort_unstable();
            keys.dedup();
            assert_eq!(total, keys.len(), "duplicate message keys: {messages:?}");
        }
    }

    fn frame(
        market_seq: u64,
        ts_ms: u64,
        depth: Vec<DepthChange>,
        events: Vec<MatchEvent>,
    ) -> TapFrame {
        TapFrame {
            log_index: market_seq + 1,
            market_seq,
            ts_ms,
            symbol_id: 0,
            events,
            depth,
        }
    }

    fn fill_event(
        market_seq: u64,
        side: OrderSide,
        price: f64,
        volume: f64,
        taker: u64,
        maker: u64,
    ) -> MatchEvent {
        let taker_order = Order::new(side, price, volume, taker);
        let maker_order = Order::new(
            match side {
                OrderSide::Buy => OrderSide::Sell,
                OrderSide::Sell => OrderSide::Buy,
            },
            price,
            volume,
            maker,
        );
        MatchEvent::for_fills(
            market_seq + 1,
            0,
            side,
            taker,
            &[MatchResult::new(&taker_order, &maker_order, volume)],
        )
        .remove(0)
    }

    fn depth(side: OrderSide, price: f64, delta_volume: f64, delta_orders: i32) -> DepthChange {
        DepthChange {
            side,
            price: price.into(),
            delta_volume: delta_volume.into(),
            delta_orders,
        }
    }

    fn snapshot(market_seq: u64) -> SnapshotFrame {
        SnapshotFrame {
            log_index: market_seq + 1,
            market_seq,
            ts_ms: 5_000,
            symbol_id: 0,
            bids: vec![DepthLevel {
                price: 100.0.into(),
                volume: 5.0.into(),
                orders: 1,
            }],
            asks: vec![DepthLevel {
                price: 101.0.into(),
                volume: 4.0.into(),
                orders: 1,
            }],
            last_trade: Some(100.5.into()),
            book_sequence: market_seq + 1,
            pending_stops: 1,
        }
    }

    #[test]
    fn publishes_all_products_for_one_frame() {
        let mut h = harness(MarketDataConfig::default());
        h.processor.ingest(frame(
            7,
            1_000,
            vec![
                depth(OrderSide::Sell, 100.5, 5.0, 1),
                depth(OrderSide::Buy, 99.5, 3.0, 1),
            ],
            vec![fill_event(7, OrderSide::Buy, 100.5, 2.0, 10, 1)],
        ));

        let messages = h.messages();
        assert!(
            messages
                .iter()
                .all(|message| message.market_seq == 7 && message.sub_seq == 0),
            "every message is anchored to the frame: {messages:?}"
        );

        let trade = messages
            .iter()
            .find_map(|message| match &message.body {
                MessageBody::Trade(records) => Some(records),
                _ => None,
            })
            .expect("trade message");
        assert_eq!(1, trade.len());
        assert_eq!(OrderSide::Buy, trade[0].taker_side);
        assert_eq!(100.5, trade[0].price);
        assert_eq!(2.0, trade[0].volume);

        let bbo = messages
            .iter()
            .find_map(|message| match &message.body {
                MessageBody::Bbo(bbo) => Some(bbo),
                _ => None,
            })
            .expect("bbo message");
        assert_eq!(Some(100.5.into()), bbo.last_trade);
        assert_eq!((99.5.into(), 3.0.into()), (bbo.bid, bbo.bid_volume));
        assert_eq!((100.5.into(), 5.0.into()), (bbo.ask, bbo.ask_volume));

        let depth = messages
            .iter()
            .find_map(|message| match &message.body {
                MessageBody::DepthDelta(updates) => Some(updates),
                _ => None,
            })
            .expect("depth message");
        assert_eq!(2, depth.len());

        assert!(
            messages
                .iter()
                .any(|message| matches!(message.body, MessageBody::Lifecycle(_))),
            "lifecycle message"
        );
        h.assert_unique_keys();
    }

    #[test]
    fn one_trade_message_carries_every_fill_of_the_taker() {
        let mut h = harness(MarketDataConfig::default());
        let events: Vec<MatchEvent> = (0..500u64)
            .map(|i| fill_event(9, OrderSide::Sell, 100.0, 1.0, 100, i))
            .collect();
        h.processor.ingest(frame(9, 1_000, vec![], events));

        let trades: Vec<_> = h
            .bodies()
            .into_iter()
            .filter_map(|body| match body {
                MessageBody::Trade(records) => Some(records),
                _ => None,
            })
            .collect();
        assert_eq!(1, trades.len(), "one message per taker");
        assert_eq!(500, trades[0].len(), "no chunking");
        assert_eq!(500, h.stats.trades.load(Ordering::Relaxed));
    }

    #[test]
    fn each_frame_emits_its_own_post_batch_depth_total() {
        let mut h = harness(MarketDataConfig::default());
        h.processor.ingest(frame(
            1,
            1_000,
            vec![depth(OrderSide::Sell, 100.0, 5.0, 1)],
            vec![],
        ));
        h.processor.ingest(frame(
            2,
            1_001,
            vec![depth(OrderSide::Sell, 100.0, -2.0, 0)],
            vec![],
        ));

        let levels: Vec<u64> = h
            .messages()
            .iter()
            .filter_map(|message| match &message.body {
                MessageBody::DepthDelta(updates) => {
                    Some(updates[0].level.volume.raw() as u64 / (SCALE / 10) as u64)
                }
                _ => None,
            })
            .collect();
        assert_eq!(vec![50, 30], levels);
        assert_eq!(
            vec![1u64, 2u64],
            h.messages()
                .iter()
                .filter(|m| matches!(m.body, MessageBody::DepthDelta(_)))
                .map(|m| m.market_seq)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn bars_close_from_event_time_and_are_anchored() {
        let config = MarketDataConfig {
            bar_intervals_ms: vec![1_000],
            ..MarketDataConfig::default()
        };
        let mut h = harness(config);

        h.processor.ingest(frame(
            1,
            1_000,
            vec![],
            vec![fill_event(1, OrderSide::Buy, 100.0, 1.0, 1, 2)],
        ));
        h.processor.ingest(frame(
            2,
            1_100,
            vec![],
            vec![fill_event(2, OrderSide::Buy, 102.0, 2.0, 3, 4)],
        ));
        h.processor.ingest(frame(
            3,
            2_100,
            vec![],
            vec![fill_event(3, OrderSide::Buy, 99.0, 1.0, 5, 6)],
        ));

        let bars: Vec<(u64, BarRecord)> = h
            .messages()
            .into_iter()
            .filter_map(|message| match message.body {
                MessageBody::Bar(bar) => Some((message.market_seq, bar)),
                _ => None,
            })
            .collect();
        let (anchor, closed) = bars
            .iter()
            .find(|(_, bar)| bar.window_start_ms == 1_000 && bar.trade_count > 0)
            .expect("closed bar");
        assert_eq!(3, *anchor, "the frame that rolled the window");
        assert_eq!(100.0, closed.open);
        assert_eq!(102.0, closed.high);
        assert_eq!(100.0, closed.low);
        assert_eq!(102.0, closed.close);
        assert_eq!(3.0, closed.volume);
        assert_eq!(2, closed.trade_count);
    }

    #[test]
    fn lifecycle_sheds_rested_before_critical_events() {
        let config = MarketDataConfig {
            lifecycle_batch_max: 2,
            ..MarketDataConfig::default()
        };
        let mut h = harness(config);

        let mut events = Vec::new();
        for _ in 0..3 {
            let order = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
            events.extend(MatchEvent::for_place_at(
                1,
                0,
                &order,
                &[],
                Some(1.0.into()),
            ));
        }
        for sequence in 0..2 {
            let mut order = Order::new(OrderSide::Buy, 100.0, 1.0, sequence);
            order.sequence = sequence;
            events.push(MatchEvent::for_cancel_at(2, 0, &order));
        }
        h.processor.ingest(frame(3, 3_000, vec![], events));

        let lifecycle = h
            .bodies()
            .into_iter()
            .find_map(|body| match body {
                MessageBody::Lifecycle(records) => Some(records),
                _ => None,
            })
            .expect("lifecycle batch");
        assert_eq!(2, lifecycle.len());
        assert!(
            lifecycle
                .iter()
                .all(|record| record.kind == MatchEventKind::Cancelled)
        );
        assert_eq!(3, h.stats.lifecycle_shed.load(Ordering::Relaxed));
    }

    #[test]
    fn snapshot_resets_and_suppresses_older_frames() {
        let mut h = harness(MarketDataConfig::default());
        h.processor.apply_snapshot(snapshot(10));

        let messages = h.messages();
        let reset = messages
            .iter()
            .find(|message| matches!(message.body, MessageBody::DepthSnapshot(_)))
            .expect("snapshot message");
        assert_eq!(10, reset.market_seq);
        assert_eq!(flags::SNAPSHOT | flags::RESET, reset.flags);
        assert!(
            messages
                .iter()
                .any(|message| matches!(message.body, MessageBody::Bbo(_)))
        );

        h.clear();
        h.processor.ingest(frame(
            9,
            4_000,
            vec![depth(OrderSide::Buy, 99.0, 1.0, 1)],
            vec![fill_event(9, OrderSide::Buy, 99.0, 1.0, 7, 8)],
        ));
        assert!(h.messages().is_empty(), "pre-snapshot frame suppressed");

        h.processor.ingest(frame(
            11,
            6_000,
            vec![depth(OrderSide::Buy, 99.0, 1.0, 1)],
            vec![fill_event(11, OrderSide::Buy, 99.0, 1.0, 9, 10)],
        ));
        assert!(h.messages().iter().all(|message| message.market_seq == 11));
    }

    #[test]
    fn leader_and_reset_gates_suppress_emission() {
        let mut h = harness(MarketDataConfig::default());
        h.leader.store(false, Ordering::Relaxed);
        h.processor.ingest(frame(
            1,
            1_000,
            vec![depth(OrderSide::Buy, 100.0, 1.0, 1)],
            vec![fill_event(1, OrderSide::Buy, 100.0, 1.0, 1, 2)],
        ));
        assert!(h.messages().is_empty());

        h.leader.store(true, Ordering::Relaxed);
        h.resetting.store(true, Ordering::Relaxed);
        h.processor.ingest(frame(
            2,
            1_100,
            vec![],
            vec![fill_event(2, OrderSide::Buy, 100.0, 1.0, 3, 4)],
        ));
        assert!(h.messages().is_empty(), "resetting suppresses");

        h.processor.apply_snapshot(snapshot(2));
        h.clear();
        h.processor.ingest(frame(
            3,
            1_200,
            vec![],
            vec![fill_event(3, OrderSide::Buy, 100.0, 1.0, 5, 6)],
        ));
        assert!(!h.messages().is_empty());

        h.leader.store(false, Ordering::Relaxed);
        h.clear();
        h.processor.ingest(frame(
            4,
            1_300,
            vec![],
            vec![fill_event(4, OrderSide::Buy, 100.0, 1.0, 7, 8)],
        ));
        assert!(h.messages().is_empty(), "loss closes the gate");
    }

    #[test]
    fn gap_marker_is_anchored_to_the_next_frame() {
        let mut h = harness(MarketDataConfig::default());
        h.dropped.store(3, Ordering::Relaxed);
        h.gap_pending.store(true, Ordering::Relaxed);
        h.processor.ingest(frame(5, 1_000, vec![], vec![]));

        let gap = h
            .bodies()
            .into_iter()
            .find_map(|body| match body {
                MessageBody::Gap(gap) => Some(gap),
                _ => None,
            })
            .expect("gap marker");
        assert_eq!(3, gap.dropped);
        assert_eq!(1, h.stats.gaps.load(Ordering::Relaxed));
    }

    #[test]
    fn status_uses_a_deterministic_cadence() {
        let config = MarketDataConfig {
            status_every: 100,
            ..MarketDataConfig::default()
        };
        let mut h = harness(config);

        h.processor.ingest(frame(100, 1_000, vec![], vec![]));
        h.processor.ingest(frame(101, 1_001, vec![], vec![]));

        let statuses: Vec<_> = h
            .messages()
            .into_iter()
            .filter_map(|message| match message.body {
                MessageBody::Status(status) => Some((message.market_seq, status)),
                _ => None,
            })
            .collect();
        assert_eq!(1, statuses.len());
        assert_eq!(100, statuses[0].0);
        assert_eq!(100, statuses[0].1.last_market_seq);
        assert_eq!(0, statuses[0].1.last_sub_seq);
    }

    #[test]
    fn tap_drops_and_requests_a_gap_when_the_queue_is_full() {
        let (tap, channels) = MarketDataTap::channel(1);
        tap.publish(frame(1, 1_000, vec![], vec![]));
        tap.publish(frame(2, 1_001, vec![], vec![]));

        assert_eq!(1, channels.dropped.load(Ordering::Relaxed));
        assert!(channels.gap_pending.load(Ordering::Relaxed));
        assert_eq!(1, channels.stats.dropped_frames.load(Ordering::Relaxed));
        assert_eq!(2, channels.stats.frames.load(Ordering::Relaxed));
    }

    #[test]
    fn two_replicas_fed_the_same_frames_emit_identical_messages() {
        let frames = vec![
            frame(
                1,
                1_000,
                vec![depth(OrderSide::Sell, 100.5, 5.0, 1)],
                vec![fill_event(1, OrderSide::Buy, 100.5, 2.0, 1, 2)],
            ),
            frame(2, 1_100, vec![depth(OrderSide::Buy, 99.5, 3.0, 1)], vec![]),
            frame(
                3,
                2_100,
                vec![depth(OrderSide::Sell, 100.5, -2.0, 0)],
                vec![fill_event(3, OrderSide::Buy, 100.5, 1.0, 3, 2)],
            ),
        ];
        let mut a = harness(MarketDataConfig::default());
        let mut b = harness(MarketDataConfig::default());
        for frame in &frames {
            a.processor.ingest(frame.clone());
            b.processor.ingest(frame.clone());
        }

        let json = |h: &Harness| serde_json::to_string(&h.messages()).unwrap();
        assert_eq!(json(&a), json(&b), "same frames, same messages");
        assert!(a.messages().iter().all(|message| message.market_seq > 0));
    }

    #[test]
    fn a_takeover_snapshot_continues_the_same_keys() {
        let frames: Vec<TapFrame> = (1..=6)
            .map(|sequence| {
                frame(
                    sequence,
                    1_000 * sequence,
                    vec![depth(OrderSide::Sell, 100.5, 1.0, 1)],
                    vec![fill_event(sequence, OrderSide::Buy, 100.5, 1.0, 10, 20)],
                )
            })
            .collect();

        let mut a = harness(MarketDataConfig::default());
        for frame in &frames {
            a.processor.ingest(frame.clone());
        }

        let mut b = harness(MarketDataConfig::default());
        b.processor.apply_snapshot(snapshot(4));
        for frame in frames.iter().filter(|frame| frame.market_seq >= 4) {
            b.processor.ingest(frame.clone());
        }

        let trades = |h: &Harness, from: u64| -> Vec<(u64, Vec<TradeRecord>)> {
            h.messages()
                .into_iter()
                .filter(|message| message.market_seq >= from)
                .filter_map(|message| match message.body {
                    MessageBody::Trade(records) => Some((message.market_seq, records)),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            trades(&a, 4),
            trades(&b, 4),
            "the takeover continues the same keys"
        );
        assert!(trades(&b, 4).iter().all(|(sequence, _)| *sequence >= 4));
    }

    #[test]
    fn frames_after_the_snapshot_capture_are_replayed() {
        let mut h = harness(MarketDataConfig::default());
        h.resetting.store(true, Ordering::Relaxed);
        h.processor.ingest(frame(
            5,
            5_000,
            vec![depth(OrderSide::Buy, 99.0, 1.0, 1)],
            vec![fill_event(5, OrderSide::Buy, 99.0, 1.0, 7, 8)],
        ));
        assert!(h.messages().is_empty(), "buffered while resetting");

        h.processor.apply_snapshot(snapshot(5));

        let messages = h.messages();
        assert!(
            messages
                .iter()
                .any(|message| matches!(message.body, MessageBody::DepthSnapshot(_)))
        );
        assert!(
            messages
                .iter()
                .any(|message| message.market_seq == 5
                    && matches!(message.body, MessageBody::Trade(_))),
            "the post-capture frame is replayed after the snapshot"
        );
        h.assert_unique_keys();
    }

    #[test]
    fn gap_and_status_share_the_stream_without_key_collisions() {
        let config = MarketDataConfig {
            status_every: 100,
            ..MarketDataConfig::default()
        };
        let mut h = harness(config);
        h.dropped.store(2, Ordering::Relaxed);
        h.gap_pending.store(true, Ordering::Relaxed);

        h.processor.ingest(frame(100, 1_000, vec![], vec![]));

        let statuses: Vec<_> = h
            .messages()
            .into_iter()
            .filter(|message| message.stream == STREAM_STATUS)
            .collect();
        assert_eq!(2, statuses.len(), "one GAP, one cadence status");
        let mut sub_seqs: Vec<u16> = statuses.iter().map(|message| message.sub_seq).collect();
        sub_seqs.sort_unstable();
        assert_eq!(
            vec![0, GAP_SUB_SEQ],
            sub_seqs,
            "the anchor's keys stay unique"
        );
        assert_eq!(
            GAP_SUB_SEQ,
            statuses
                .iter()
                .find(|message| message.flags == flags::GAP)
                .expect("gap marker")
                .sub_seq
        );
        let status = statuses
            .iter()
            .find_map(|message| match &message.body {
                MessageBody::Status(status) => Some(status),
                _ => None,
            })
            .expect("cadence status");
        assert_eq!(0, status.last_sub_seq);
    }
}

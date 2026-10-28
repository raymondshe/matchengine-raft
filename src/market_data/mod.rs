//! Market-data egress: a tap on `apply`, a shadow book and a publisher thread.
//!
//! This is the deterministic feed pipeline from `docs/MARKET_DATA_PLAN.md` and
//! `docs/AERON_SOLUTION.md` Part II. The matching thread only records depth
//! deltas it already knows ([`crate::matchengine::OrderBook::take_depth_journal`])
//! and hands them, with the entry's [`MatchEvent`]s, to a bounded channel. A
//! dedicated thread owns the derived state and all encoding/IO:
//!
//! ```text
//! apply ──► MarketDataTap::publish (try_send, never blocks)
//!                │
//!                ▼
//!        market-data thread
//!          shadow book · throttling · OHLC bars · wire encoding
//!                │
//!                ▼
//!        MarketDataSink (JSONL today, Aeron next)
//! ```
//!
//! Timestamps are the intake timestamps carried in the replicated requests
//! (§13); nothing here reads a clock to label data. Wall clocks are used only
//! to pace coalescing windows, never to order or bucket records.

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
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::SyncSender;
use std::sync::mpsc::TrySendError;
use std::sync::mpsc::sync_channel;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use serde::Serialize;

use crate::match_result::MatchEvent;
use crate::match_result::MatchEventKind;
use crate::matchengine::DepthChange;
use crate::matchengine::DepthLevel;
use crate::matchengine::OrderSide;
use crate::matchengine::RejectReason;

pub mod wire;

/// Aeron stream IDs reserved by the design (§15).
pub const STREAM_TRADE: u16 = 2001;
/// Latest-value top of book.
pub const STREAM_BBO: u16 = 2002;
/// Full top-N depth snapshot (reset/recovery path).
pub const STREAM_DEPTH_SNAPSHOT: u16 = 2003;
/// Coalesced depth changes (post-batch totals).
pub const STREAM_DEPTH_DELTA: u16 = 2004;
/// Batched order-lifecycle events.
pub const STREAM_LIFECYCLE: u16 = 2005;
/// Heartbeat and gate state.
pub const STREAM_STATUS: u16 = 2006;
/// OHLCV bars.
pub const STREAM_BAR: u16 = 2007;

/// Header flags (§16).
pub mod flags {
    /// The message carries a full snapshot.
    pub const SNAPSHOT: u16 = 1 << 0;
    /// First message of a publication session / leader generation.
    pub const RESET: u16 = 1 << 1;
    /// Records were dropped before this message.
    pub const GAP: u16 = 1 << 2;
    /// Last message of a coalesced batch.
    pub const END_OF_BATCH: u16 = 1 << 3;
    /// A stream is being throttled (status/diagnostics).
    pub const THROTTLED: u16 = 1 << 4;
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
    /// Minimum spacing between BBO updates (default 1 ms).
    pub bbo_interval_ms: u64,
    /// Minimum spacing between depth-delta flushes (default 5 ms).
    pub depth_interval_ms: u64,
    /// Minimum spacing between lifecycle batches (default 10 ms).
    pub lifecycle_interval_ms: u64,
    /// Status heartbeat spacing (default 100 ms).
    pub status_interval_ms: u64,
    /// Maximum fills per `TRADE` message before an early flush (default 32).
    pub trade_batch_max: usize,
    /// Maximum lifecycle records per message (default 512); `rested` is shed
    /// first when the batch overflows.
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
            bbo_interval_ms: 1,
            depth_interval_ms: 5,
            lifecycle_interval_ms: 10,
            status_interval_ms: 100,
            trade_batch_max: 32,
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
    /// Status heartbeats published.
    pub status: AtomicU64,
    /// Gap markers published.
    pub gaps: AtomicU64,
    /// Frames dropped on a full queue.
    pub dropped_frames: AtomicU64,
    /// Snapshots installed.
    pub snapshots: AtomicU64,
    /// `log_index` of the last processed frame.
    pub last_log_index: AtomicU64,
    /// `ts_ms` of the last processed frame.
    pub last_ts_ms: AtomicU64,
    /// Whether this node currently publishes.
    pub publishing: AtomicBool,
}

impl MarketDataStats {
    /// Renders the counters as the JSON served by `/read marketdata_status`.
    pub fn status_json(&self, sink: &'static str) -> String {
        serde_json::json!({
            "sink": sink,
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
#[derive(Debug, Clone, Serialize)]
pub struct TradeRecord {
    /// Raft log index of the entry that produced the fill.
    pub log_index: u64,
    /// Ordinal of the fill inside that entry.
    pub ordinal: u32,
    /// Taker order's intake timestamp (Unix ms).
    pub ts_ms: u64,
    /// Execution price (maker price).
    pub price: f64,
    /// Executed volume.
    pub volume: f64,
    /// Taker side (the maker's side is the opposite).
    pub taker_side: OrderSide,
    /// Taker order sequence.
    pub taker_sequence: u64,
    /// Maker order sequence.
    pub maker_sequence: u64,
}

/// Latest-value top of book.
#[derive(Debug, Clone, Serialize)]
pub struct BboRecord {
    /// Last trade price, if any.
    pub last_trade: Option<f64>,
    /// Best bid price.
    pub bid: f64,
    /// Visible volume at the best bid.
    pub bid_volume: f64,
    /// Best ask price.
    pub ask: f64,
    /// Visible volume at the best ask.
    pub ask_volume: f64,
    /// `(bid + ask) / 2` when both sides exist.
    pub mid: Option<f64>,
    /// `ask - bid` when both sides exist.
    pub spread: Option<f64>,
}

/// Full top-N depth for one instrument.
#[derive(Debug, Clone, Serialize)]
pub struct DepthSnapshotRecord {
    /// Bid levels, best first.
    pub bids: Vec<DepthLevel>,
    /// Ask levels, best first.
    pub asks: Vec<DepthLevel>,
}

/// One touched price level with its post-batch totals.
#[derive(Debug, Clone, Serialize)]
pub struct LevelUpdate {
    /// Side of the level.
    pub side: OrderSide,
    /// Level after the batch.
    pub level: DepthLevel,
}

/// One order-lifecycle event.
#[derive(Debug, Clone, Serialize)]
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
    /// Price associated with the event.
    pub price: f64,
    /// Volume associated with the event.
    pub volume: f64,
    /// Rejection/expiry reason when present.
    pub reason: Option<RejectReason>,
}

/// One OHLCV bar.
#[derive(Debug, Clone, Serialize)]
pub struct BarRecord {
    /// Bar width.
    pub interval_ms: u64,
    /// Start of the window (Unix ms, aligned to `interval_ms`).
    pub window_start_ms: u64,
    /// Last trade timestamp included.
    pub last_ts_ms: u64,
    /// First trade price in the window.
    pub open: f64,
    /// Highest trade price.
    pub high: f64,
    /// Lowest trade price.
    pub low: f64,
    /// Last trade price.
    pub close: f64,
    /// Summed trade volume.
    pub volume: f64,
    /// Number of fills aggregated.
    pub trade_count: u32,
    /// Monotonic per `(symbol, interval)`.
    pub bar_seq: u64,
}

/// Feed heartbeat.
#[derive(Debug, Clone, Serialize)]
pub struct StatusRecord {
    /// Last processed log index.
    pub last_log_index: u64,
    /// Last processed ordinal.
    pub last_ordinal: u32,
    /// Last assigned stream sequence.
    pub last_stream_seq: u64,
    /// Last applied log index (same as `last_log_index` for a live feed).
    pub applied_index: u64,
    /// Replicated book sequence counter.
    pub book_sequence: u64,
    /// Parked stop orders at the snapshot.
    pub pending_stops: u32,
}

/// A hard-failure marker: frames were dropped before this message.
#[derive(Debug, Clone, Serialize)]
pub struct GapRecord {
    /// Last position published before the gap.
    pub last_good_log_index: u64,
    /// Last ordinal published before the gap.
    pub last_good_ordinal: u32,
    /// Frames dropped since the previous gap marker.
    pub dropped: u32,
}

/// A decoded market-data message: one wire frame.
#[derive(Debug, Clone, Serialize)]
pub struct MarketMessage {
    /// Destination stream (2001..2007).
    pub stream: u16,
    /// Header flags.
    pub flags: u16,
    /// Monotonic per stream within a session.
    pub stream_seq: u64,
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
    /// Instrument id (0 until the engine is multi-symbol).
    pub symbol_id: u32,
    /// Message payload.
    pub body: MessageBody,
}

/// Payload variants; one per stream.
#[derive(Debug, Clone, Serialize)]
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
    /// Feed heartbeat.
    Status(StatusRecord),
    /// Dropped-frame marker.
    Gap(GapRecord),
    /// OHLCV bar.
    Bar(BarRecord),
}

/// A frame handed from `apply` to the market-data thread.
#[derive(Debug, Clone)]
pub struct TapFrame {
    /// Raft log index of the entry.
    pub log_index: u64,
    /// Intake timestamp of the request (Unix ms).
    pub ts_ms: u64,
    /// Instrument id (0 today).
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
    /// Capture timestamp (Unix ms).
    pub ts_ms: u64,
    /// Instrument id (0 today).
    pub symbol_id: u32,
    /// Bid levels, best first.
    pub bids: Vec<DepthLevel>,
    /// Ask levels, best first.
    pub asks: Vec<DepthLevel>,
    /// Last trade price, if any.
    pub last_trade: Option<f64>,
    /// Replicated book sequence counter.
    pub book_sequence: u64,
    /// Parked stop orders.
    pub pending_stops: u32,
}

/// Messages consumed by the market-data thread.
#[derive(Debug)]
pub enum Ingest {
    /// One applied entry.
    Frame(TapFrame),
    /// A state capture that resets the shadow book.
    Snapshot(SnapshotFrame),
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
}

/// The matching thread's handle on the feed.
///
/// `publish` is a `try_send`: it never blocks and never fails `apply`. The tap
/// is cheap to clone; every clone feeds the same queue.
#[derive(Clone)]
pub struct MarketDataTap {
    sender: SyncSender<Ingest>,
    stats: Arc<MarketDataStats>,
    dropped: Arc<AtomicU64>,
    gap_pending: Arc<AtomicBool>,
}

impl fmt::Debug for MarketDataTap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MarketDataTap").finish_non_exhaustive()
    }
}

impl MarketDataTap {
    /// Creates a tap and the receiving side of its bounded queue.
    pub fn channel(capacity: usize) -> (Self, TapChannels) {
        let (sender, receiver) = sync_channel(capacity.max(1));
        let stats = Arc::new(MarketDataStats::default());
        let dropped = Arc::new(AtomicU64::new(0));
        let gap_pending = Arc::new(AtomicBool::new(false));
        (
            Self {
                sender,
                stats: stats.clone(),
                dropped: dropped.clone(),
                gap_pending: gap_pending.clone(),
            },
            TapChannels {
                receiver,
                stats,
                dropped,
                gap_pending,
            },
        )
    }

    /// Hands one applied entry to the feed. Never blocks.
    pub fn publish(&self, frame: TapFrame) {
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        match self.sender.try_send(Ingest::Frame(frame)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.gap_pending.store(true, Ordering::Relaxed);
                self.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Counters as JSON for `/read marketdata_status`.
    pub fn status_json(&self) -> String {
        self.stats.status_json("tap")
    }

    /// Shared counters.
    pub fn stats(&self) -> &Arc<MarketDataStats> {
        &self.stats
    }
}

/// A cheap, cloneable control handle for the leader watcher.
#[derive(Clone, Debug)]
pub struct MarketDataControl {
    tap: MarketDataTap,
    leader: Arc<AtomicBool>,
}

impl MarketDataControl {
    /// Opens or closes the publish gate.
    pub fn set_leader(&self, leader: bool) {
        self.leader.store(leader, Ordering::Relaxed);
    }

    /// Queues a reset snapshot for the processor.
    pub fn push_snapshot(&self, snapshot: SnapshotFrame) {
        if self
            .tap
            .sender
            .try_send(Ingest::Snapshot(snapshot))
            .is_err()
        {
            self.tap.dropped.fetch_add(1, Ordering::Relaxed);
            self.tap.gap_pending.store(true, Ordering::Relaxed);
        }
    }
}

/// Owns the processor thread; created by [`crate::build_match_engine_node`].
pub struct MarketDataHandle {
    tap: MarketDataTap,
    leader: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl fmt::Debug for MarketDataHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MarketDataHandle").finish_non_exhaustive()
    }
}

impl MarketDataHandle {
    /// Starts the processor thread with the configured sink.
    pub fn start(config: MarketDataConfig, node_id: u64) -> io::Result<Self> {
        let sink: Box<dyn MarketDataSink> = match &config.log_path {
            Some(path) => Box::new(JsonlSink::open(path, node_id)?),
            None => Box::new(NullSink),
        };
        let (tap, channels) = MarketDataTap::channel(config.queue_capacity);
        let leader = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let mut processor = MarketDataProcessor::new(
            config,
            sink,
            channels.stats,
            leader.clone(),
            channels.dropped,
            channels.gap_pending,
        );
        let processor_stop = stop.clone();
        let join = std::thread::Builder::new()
            .name("market-data".to_string())
            .spawn(move || processor.run(channels.receiver, processor_stop))?;

        Ok(Self {
            tap,
            leader,
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
        MarketDataControl {
            tap: self.tap.clone(),
            leader: self.leader.clone(),
        }
    }

    /// Updates the publish gate (driven by `raft.metrics()`).
    pub fn set_leader(&self, leader: bool) {
        self.leader.store(leader, Ordering::Relaxed);
    }

    /// Queues a state capture; the processor emits it before later deltas.
    pub fn push_snapshot(&self, snapshot: SnapshotFrame) {
        if self
            .tap
            .sender
            .try_send(Ingest::Snapshot(snapshot))
            .is_err()
        {
            self.tap.dropped.fetch_add(1, Ordering::Relaxed);
            self.tap.gap_pending.store(true, Ordering::Relaxed);
        }
    }

    /// Stops and joins the processor thread. Safe to call once.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Shadow book
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct LevelState {
    price: f64,
    volume: f64,
    orders: u32,
}

/// Visible book per instrument, fed by the depth journal.
#[derive(Debug, Default)]
struct ShadowBook {
    bids: BTreeMap<u64, LevelState>,
    asks: BTreeMap<u64, LevelState>,
    last_trade: Option<f64>,
}

impl ShadowBook {
    fn apply(&mut self, change: DepthChange) {
        let map = match change.side {
            OrderSide::Buy => &mut self.bids,
            OrderSide::Sell => &mut self.asks,
        };
        let key = change.price.to_bits();
        let level = map.entry(key).or_insert(LevelState {
            price: change.price,
            volume: 0.0,
            orders: 0,
        });
        level.volume += change.delta_volume;
        let orders = i64::from(level.orders) + i64::from(change.delta_orders);
        level.orders = orders.max(0) as u32;
        if level.volume <= 1e-12 && level.orders == 0 {
            map.remove(&key);
        }
    }

    fn reset(&mut self, bids: &[DepthLevel], asks: &[DepthLevel], last_trade: Option<f64>) {
        self.bids = bids
            .iter()
            .map(|level| {
                (
                    level.price.to_bits(),
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
                    level.price.to_bits(),
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

    fn level(&self, side: OrderSide, price_bits: u64) -> Option<LevelState> {
        match side {
            OrderSide::Buy => self.bids.get(&price_bits).copied(),
            OrderSide::Sell => self.asks.get(&price_bits).copied(),
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
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    trade_count: u32,
    last_live_ms: u64,
}

impl BarAgg {
    fn new(interval_ms: u64, window_start_ms: u64, ts_ms: u64, price: f64, volume: f64) -> Self {
        Self {
            interval_ms,
            window_start_ms,
            last_ts_ms: ts_ms,
            open: price,
            high: price,
            low: price,
            close: price,
            volume,
            trade_count: 1,
            last_live_ms: ts_ms,
        }
    }

    fn update(&mut self, ts_ms: u64, price: f64, volume: f64) {
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
    shadow: BTreeMap<u32, ShadowBook>,
    trade_batch: Vec<TradeRecord>,
    lifecycle_batch: Vec<LifecycleRecord>,
    dirty: BTreeSet<(u32, u8, u64)>,
    bbo_dirty: BTreeSet<u32>,
    bars: BTreeMap<(u32, u64), BarAgg>,
    bar_seq: BTreeMap<(u32, u64), u64>,
    stream_seq: BTreeMap<u16, u64>,
    last_position: (u64, u32),
    snapshot_boundary: Option<u64>,
    batch_symbol: u32,
    last_trade_flush: Option<Instant>,
    last_bbo: Option<Instant>,
    last_depth: Option<Instant>,
    last_lifecycle: Option<Instant>,
    last_status: Option<Instant>,
    book_sequence: u64,
    pending_stops: u32,
    dropped: Arc<AtomicU64>,
    dropped_seen: u64,
    gap_pending: Arc<AtomicBool>,
}

impl MarketDataProcessor {
    fn new(
        config: MarketDataConfig,
        sink: Box<dyn MarketDataSink>,
        stats: Arc<MarketDataStats>,
        leader: Arc<AtomicBool>,
        dropped: Arc<AtomicU64>,
        gap_pending: Arc<AtomicBool>,
    ) -> Self {
        Self {
            config,
            sink,
            stats,
            leader,
            shadow: BTreeMap::new(),
            trade_batch: Vec::new(),
            lifecycle_batch: Vec::new(),
            dirty: BTreeSet::new(),
            bbo_dirty: BTreeSet::new(),
            bars: BTreeMap::new(),
            bar_seq: BTreeMap::new(),
            stream_seq: BTreeMap::new(),
            last_position: (0, 0),
            snapshot_boundary: None,
            batch_symbol: 0,
            last_trade_flush: None,
            last_bbo: None,
            last_depth: None,
            last_lifecycle: None,
            last_status: None,
            book_sequence: 0,
            pending_stops: 0,
            dropped,
            dropped_seen: 0,
            gap_pending,
        }
    }

    /// Runs the processor loop until `stop` is set.
    fn run(&mut self, receiver: Receiver<Ingest>, stop: Arc<AtomicBool>) {
        while !stop.load(Ordering::Relaxed) {
            match receiver.recv_timeout(Duration::from_millis(5)) {
                Ok(Ingest::Frame(frame)) => self.ingest(frame),
                Ok(Ingest::Snapshot(snapshot)) => self.apply_snapshot(snapshot),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.tick(Instant::now());
        }
        self.flush_all();
    }

    fn publishing(&self) -> bool {
        !self.config.leader_only || self.leader.load(Ordering::Relaxed)
    }

    fn ingest(&mut self, frame: TapFrame) {
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        self.stats
            .last_log_index
            .store(frame.log_index, Ordering::Relaxed);
        self.stats.last_ts_ms.store(frame.ts_ms, Ordering::Relaxed);

        if let Some(boundary) = self.snapshot_boundary
            && frame.log_index <= boundary
        {
            return;
        }
        if !self.publishing() {
            return;
        }
        if frame.symbol_id != self.batch_symbol
            && (!self.trade_batch.is_empty() || !self.lifecycle_batch.is_empty())
        {
            let now = Instant::now();
            self.flush_trades(now);
            self.flush_lifecycle(now);
            self.batch_symbol = frame.symbol_id;
        }

        for change in frame.depth {
            let side = match change.side {
                OrderSide::Buy => 0u8,
                OrderSide::Sell => 1u8,
            };
            self.shadow
                .entry(frame.symbol_id)
                .or_default()
                .apply(change);
            self.dirty
                .insert((frame.symbol_id, side, change.price.to_bits()));
            self.bbo_dirty.insert(frame.symbol_id);
        }

        let mut last_ordinal = 0;
        for event in &frame.events {
            last_ordinal = event.event_id.ordinal;
            self.lifecycle_batch.push(LifecycleRecord {
                log_index: event.event_id.log_index,
                ordinal: event.event_id.ordinal,
                ts_ms: frame.ts_ms,
                kind: event.kind,
                order_sequence: event.order_sequence,
                side: event.side,
                counterparty_sequence: event.counterparty_sequence,
                price: event.price,
                volume: event.volume,
                reason: event.reason,
            });

            if event.kind == MatchEventKind::Fill {
                let Some(maker_sequence) = event.counterparty_sequence else {
                    continue;
                };
                self.trade_batch.push(TradeRecord {
                    log_index: event.event_id.log_index,
                    ordinal: event.event_id.ordinal,
                    ts_ms: frame.ts_ms,
                    price: event.price,
                    volume: event.volume,
                    taker_side: event.side,
                    taker_sequence: event.order_sequence,
                    maker_sequence,
                });
                self.shadow.entry(frame.symbol_id).or_default().last_trade = Some(event.price);
                self.update_bars(frame.symbol_id, frame.ts_ms, event.price, event.volume);
                self.bbo_dirty.insert(frame.symbol_id);
                self.stats.trades.fetch_add(1, Ordering::Relaxed);
            }
        }

        self.last_position = (frame.log_index, last_ordinal);
        self.batch_symbol = frame.symbol_id;
    }

    fn apply_snapshot(&mut self, snapshot: SnapshotFrame) {
        self.stats.snapshots.fetch_add(1, Ordering::Relaxed);
        self.shadow.entry(snapshot.symbol_id).or_default().reset(
            &snapshot.bids,
            &snapshot.asks,
            snapshot.last_trade,
        );
        // A snapshot is only meaningful as a reset point: clear anything that
        // predates it. With one instrument that is every accumulator.
        self.trade_batch.clear();
        self.lifecycle_batch.clear();
        self.dirty.clear();
        self.bbo_dirty.clear();
        self.bars.clear();
        self.snapshot_boundary = Some(snapshot.log_index);
        self.last_position = (snapshot.log_index, 0);
        self.book_sequence = snapshot.book_sequence;
        self.pending_stops = snapshot.pending_stops;
        self.last_trade_flush = None;
        self.last_bbo = None;
        self.last_depth = None;
        self.last_lifecycle = None;

        if !self.publishing() {
            return;
        }

        let shadow = self.shadow.entry(snapshot.symbol_id).or_default();
        let body = DepthSnapshotRecord {
            bids: shadow.depth(OrderSide::Buy, self.config.depth_levels),
            asks: shadow.depth(OrderSide::Sell, self.config.depth_levels),
        };
        self.emit(
            STREAM_DEPTH_SNAPSHOT,
            flags::SNAPSHOT | flags::RESET,
            snapshot.log_index,
            0,
            snapshot.ts_ms,
            snapshot.symbol_id,
            MessageBody::DepthSnapshot(body),
        );
        self.stats.depth_snapshots.fetch_add(1, Ordering::Relaxed);
        self.bbo_dirty.insert(snapshot.symbol_id);
    }

    fn update_bars(&mut self, symbol_id: u32, ts_ms: u64, price: f64, volume: f64) {
        for interval in self.config.bar_intervals_ms.clone() {
            let window = ts_ms - (ts_ms % interval);
            let key = (symbol_id, interval);
            let mut closed = None;

            match self.bars.get_mut(&key) {
                Some(bar) if bar.window_start_ms == window => {
                    // A late print is clamped into the open window (never reopen).
                    bar.update(ts_ms, price, volume);
                }
                Some(bar) => {
                    closed = Some(std::mem::replace(
                        bar,
                        BarAgg::new(interval, window, ts_ms, price, volume),
                    ));
                }
                None => {
                    self.bars
                        .insert(key, BarAgg::new(interval, window, ts_ms, price, volume));
                }
            }

            if let Some(bar) = closed
                && bar.trade_count > 0
            {
                self.emit_bar(symbol_id, bar, true);
            }
        }
    }

    fn emit_bar(&mut self, symbol_id: u32, bar: BarAgg, closed: bool) {
        let seq = {
            let entry = self
                .bar_seq
                .entry((symbol_id, bar.interval_ms))
                .or_insert(0);
            *entry += 1;
            *entry
        };
        let ts = if closed {
            bar.window_start_ms + bar.interval_ms
        } else {
            bar.last_ts_ms
        };
        let flags = if closed {
            flags::BAR_CLOSE
        } else {
            flags::PARTIAL_BAR
        };
        self.emit(
            STREAM_BAR,
            flags,
            self.last_position.0,
            0,
            ts,
            symbol_id,
            MessageBody::Bar(bar.record(seq)),
        );
        self.stats.bars.fetch_add(1, Ordering::Relaxed);
    }

    fn tick(&mut self, now: Instant) {
        let publishing = self.publishing();
        self.stats.publishing.store(publishing, Ordering::Relaxed);
        if !publishing {
            self.trade_batch.clear();
            self.lifecycle_batch.clear();
            self.dirty.clear();
            self.bbo_dirty.clear();
            return;
        }

        if self.gap_pending.swap(false, Ordering::Relaxed) {
            let dropped = self.dropped.load(Ordering::Relaxed);
            let delta = dropped.saturating_sub(self.dropped_seen);
            self.dropped_seen = dropped;
            self.emit(
                STREAM_STATUS,
                flags::GAP,
                self.last_position.0,
                self.last_position.1,
                self.stats.last_ts_ms.load(Ordering::Relaxed),
                0,
                MessageBody::Gap(GapRecord {
                    last_good_log_index: self.last_position.0,
                    last_good_ordinal: self.last_position.1,
                    dropped: delta.min(u64::from(u32::MAX)) as u32,
                }),
            );
            self.stats.gaps.fetch_add(1, Ordering::Relaxed);
        }

        let trade_interval = Duration::from_millis(1);
        if self.trade_batch.len() >= self.config.trade_batch_max
            || due(&mut self.last_trade_flush, now, trade_interval)
        {
            self.flush_trades(now);
        }
        if self.lifecycle_batch.len() >= self.config.lifecycle_batch_max
            || due(
                &mut self.last_lifecycle,
                now,
                Duration::from_millis(self.config.lifecycle_interval_ms),
            )
        {
            self.flush_lifecycle(now);
        }
        if due(
            &mut self.last_depth,
            now,
            Duration::from_millis(self.config.depth_interval_ms),
        ) {
            self.flush_depth(now);
        }
        if due(
            &mut self.last_bbo,
            now,
            Duration::from_millis(self.config.bbo_interval_ms),
        ) {
            self.flush_bbo(now);
        }
        if due(
            &mut self.last_status,
            now,
            Duration::from_millis(self.config.status_interval_ms),
        ) {
            self.flush_status(now);
        }

        self.flush_live_bars();
    }

    fn flush_all(&mut self) {
        if !self.publishing() {
            return;
        }
        let now = Instant::now();
        self.flush_trades(now);
        self.flush_lifecycle(now);
        self.flush_depth(now);
        self.flush_bbo(now);
        self.flush_status(now);
        self.flush_live_bars();
    }

    fn flush_trades(&mut self, now: Instant) {
        if self.trade_batch.is_empty() {
            return;
        }
        let records = std::mem::take(&mut self.trade_batch);
        let count = records.len();
        let last = records.last().expect("checked non-empty");
        let (log_index, ordinal, ts) = (last.log_index, last.ordinal, last.ts_ms);
        self.emit(
            STREAM_TRADE,
            flags::END_OF_BATCH,
            log_index,
            ordinal,
            ts,
            self.batch_symbol,
            MessageBody::Trade(records),
        );
        self.stats.trade_messages.fetch_add(1, Ordering::Relaxed);
        debug_assert!(count > 0);
        self.last_trade_flush = Some(now);
    }

    fn flush_lifecycle(&mut self, now: Instant) {
        if self.lifecycle_batch.is_empty() {
            return;
        }
        let max = self.config.lifecycle_batch_max.max(1);
        if self.lifecycle_batch.len() > max {
            let mut kept = Vec::with_capacity(max);
            let mut shed = 0usize;
            let overflow = self.lifecycle_batch.len() - max;
            for record in self.lifecycle_batch.drain(..) {
                if shed < overflow && record.kind == MatchEventKind::Rested {
                    shed += 1;
                } else {
                    kept.push(record);
                }
            }
            self.lifecycle_batch = kept;
            self.stats
                .lifecycle_shed
                .fetch_add(shed as u64, Ordering::Relaxed);
        }

        let records = std::mem::take(&mut self.lifecycle_batch);
        let last = records.last().expect("checked non-empty");
        let (log_index, ordinal, ts) = (last.log_index, last.ordinal, last.ts_ms);
        self.emit(
            STREAM_LIFECYCLE,
            flags::END_OF_BATCH,
            log_index,
            ordinal,
            ts,
            self.batch_symbol,
            MessageBody::Lifecycle(records),
        );
        self.stats
            .lifecycle_messages
            .fetch_add(1, Ordering::Relaxed);
        self.last_lifecycle = Some(now);
    }

    fn flush_depth(&mut self, now: Instant) {
        if self.dirty.is_empty() {
            return;
        }
        let dirty = std::mem::take(&mut self.dirty);
        let mut by_symbol: BTreeMap<u32, Vec<LevelUpdate>> = BTreeMap::new();
        for (symbol, side, price_bits) in dirty {
            let side = if side == 0 {
                OrderSide::Buy
            } else {
                OrderSide::Sell
            };
            let Some(shadow) = self.shadow.get(&symbol) else {
                continue;
            };
            let Some(level) = shadow.level(side, price_bits) else {
                continue;
            };
            by_symbol.entry(symbol).or_default().push(LevelUpdate {
                side,
                level: DepthLevel {
                    price: level.price,
                    volume: level.volume,
                    orders: level.orders,
                },
            });
        }
        for (symbol, updates) in by_symbol {
            if updates.is_empty() {
                continue;
            }
            self.emit(
                STREAM_DEPTH_DELTA,
                flags::END_OF_BATCH,
                self.last_position.0,
                self.last_position.1,
                self.stats.last_ts_ms.load(Ordering::Relaxed),
                symbol,
                MessageBody::DepthDelta(updates),
            );
            self.stats.depth_deltas.fetch_add(1, Ordering::Relaxed);
        }
        self.last_depth = Some(now);
    }

    fn flush_bbo(&mut self, now: Instant) {
        if self.bbo_dirty.is_empty() {
            return;
        }
        let dirty = std::mem::take(&mut self.bbo_dirty);
        for symbol in dirty {
            let Some(shadow) = self.shadow.get(&symbol) else {
                continue;
            };
            let bid = shadow.best_bid();
            let ask = shadow.best_ask();
            if bid.is_none() && ask.is_none() && shadow.last_trade.is_none() {
                continue;
            }
            let (bid_price, bid_volume) =
                bid.map_or((0.0, 0.0), |level| (level.price, level.volume));
            let (ask_price, ask_volume) =
                ask.map_or((0.0, 0.0), |level| (level.price, level.volume));
            let mid = if bid.is_some() && ask.is_some() {
                Some((bid_price + ask_price) / 2.0)
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
                self.last_position.0,
                self.last_position.1,
                self.stats.last_ts_ms.load(Ordering::Relaxed),
                symbol,
                MessageBody::Bbo(BboRecord {
                    last_trade: shadow.last_trade,
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
        self.last_bbo = Some(now);
    }

    fn flush_status(&mut self, now: Instant) {
        let last_seq = self.stream_seq.values().copied().max().unwrap_or(0);
        self.emit(
            STREAM_STATUS,
            0,
            self.last_position.0,
            self.last_position.1,
            self.stats.last_ts_ms.load(Ordering::Relaxed),
            0,
            MessageBody::Status(StatusRecord {
                last_log_index: self.last_position.0,
                last_ordinal: self.last_position.1,
                last_stream_seq: last_seq,
                applied_index: self.stats.last_log_index.load(Ordering::Relaxed),
                book_sequence: self.book_sequence,
                pending_stops: self.pending_stops,
            }),
        );
        self.stats.status.fetch_add(1, Ordering::Relaxed);
        self.last_status = Some(now);
    }

    fn flush_live_bars(&mut self) {
        let mut due_bars = Vec::new();
        for (key, bar) in &mut self.bars {
            if bar.trade_count > 0
                && bar.last_ts_ms > bar.last_live_ms
                && bar.last_ts_ms.saturating_sub(bar.last_live_ms) >= 1_000
            {
                bar.last_live_ms = bar.last_ts_ms;
                due_bars.push((key.0, bar.clone()));
            }
        }
        for (symbol, bar) in due_bars {
            self.emit_bar(symbol, bar, false);
        }
    }

    fn emit(
        &mut self,
        stream: u16,
        message_flags: u16,
        log_index: u64,
        ordinal: u32,
        ts_ms: u64,
        symbol_id: u32,
        body: MessageBody,
    ) {
        let stream_seq = {
            let entry = self.stream_seq.entry(stream).or_insert(0);
            *entry += 1;
            *entry
        };
        let message = MarketMessage {
            stream,
            flags: message_flags,
            stream_seq,
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

fn due(last: &mut Option<Instant>, now: Instant, interval: Duration) -> bool {
    match *last {
        Some(previous) if now.duration_since(previous) < interval => false,
        _ => {
            *last = Some(now);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::matchengine::MatchResult;
    use crate::matchengine::Order;

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
        dropped: Arc<AtomicU64>,
        gap_pending: Arc<AtomicBool>,
    }

    fn harness(config: MarketDataConfig, leader: bool) -> Harness {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let stats = Arc::new(MarketDataStats::default());
        let leader = Arc::new(AtomicBool::new(leader));
        let dropped = Arc::new(AtomicU64::new(0));
        let gap_pending = Arc::new(AtomicBool::new(false));
        let processor = MarketDataProcessor::new(
            config,
            Box::new(CollectSink(messages.clone())),
            stats.clone(),
            leader.clone(),
            dropped.clone(),
            gap_pending.clone(),
        );
        Harness {
            processor,
            messages,
            stats,
            leader,
            dropped,
            gap_pending,
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
    }

    fn frame(
        log_index: u64,
        ts_ms: u64,
        depth: Vec<DepthChange>,
        events: Vec<MatchEvent>,
    ) -> TapFrame {
        TapFrame {
            log_index,
            ts_ms,
            symbol_id: 0,
            events,
            depth,
        }
    }

    fn fill_event(
        log_index: u64,
        ordinal: u32,
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
            log_index,
            ordinal,
            side,
            taker,
            &[MatchResult::new(&taker_order, &maker_order, volume)],
        )
        .remove(0)
    }

    fn depth(side: OrderSide, price: f64, delta_volume: f64, delta_orders: i32) -> DepthChange {
        DepthChange {
            side,
            price,
            delta_volume,
            delta_orders,
        }
    }

    #[test]
    fn publishes_trades_bbo_and_coalesced_depth() {
        let mut h = harness(MarketDataConfig::default(), true);
        h.processor.ingest(frame(
            1,
            1_000,
            vec![
                depth(OrderSide::Sell, 100.5, 5.0, 1),
                depth(OrderSide::Buy, 99.5, 3.0, 1),
            ],
            vec![fill_event(1, 0, OrderSide::Buy, 100.5, 2.0, 10, 1)],
        ));
        h.processor.ingest(frame(
            2,
            1_001,
            vec![depth(OrderSide::Sell, 100.5, -2.0, 0)],
            vec![],
        ));
        h.processor
            .tick(Instant::now() + Duration::from_millis(250));

        let bodies = h.bodies();

        let trades: Vec<_> = bodies
            .iter()
            .filter_map(|body| match body {
                MessageBody::Trade(records) => Some(records),
                _ => None,
            })
            .collect();
        assert_eq!(1, trades.len(), "one batched trade message");
        assert_eq!(1, trades[0].len());
        assert_eq!(OrderSide::Buy, trades[0][0].taker_side);
        assert_eq!(100.5, trades[0][0].price);
        assert_eq!(2.0, trades[0][0].volume);

        let bbo = bodies
            .iter()
            .find_map(|body| match body {
                MessageBody::Bbo(bbo) => Some(bbo),
                _ => None,
            })
            .expect("bbo published");
        assert_eq!(Some(100.5), bbo.last_trade);
        assert_eq!(99.5, bbo.bid);
        assert_eq!(3.0, bbo.bid_volume);
        assert_eq!(100.5, bbo.ask);
        assert_eq!(3.0, bbo.ask_volume, "deltas coalesced before the BBO");
        assert_eq!(Some(100.0), bbo.mid);
        assert_eq!(Some(1.0), bbo.spread);

        let deltas: Vec<_> = bodies
            .iter()
            .filter_map(|body| match body {
                MessageBody::DepthDelta(updates) => Some(updates),
                _ => None,
            })
            .collect();
        assert_eq!(1, deltas.len(), "one coalesced delta message");
        assert_eq!(2, deltas[0].len(), "two touched levels");
        let ask = deltas[0]
            .iter()
            .find(|update| update.side == OrderSide::Sell)
            .expect("ask level");
        assert_eq!(3.0, ask.level.volume);
        assert_eq!(1, ask.level.orders);
    }

    #[test]
    fn bars_close_when_the_window_rolls() {
        let mut config = MarketDataConfig::default();
        config.bar_intervals_ms = vec![1_000];
        let mut h = harness(config, true);

        h.processor.ingest(frame(
            1,
            1_000,
            vec![],
            vec![fill_event(1, 0, OrderSide::Buy, 100.0, 1.0, 1, 2)],
        ));
        h.processor.ingest(frame(
            2,
            1_100,
            vec![],
            vec![fill_event(2, 0, OrderSide::Buy, 102.0, 2.0, 3, 4)],
        ));
        h.processor.ingest(frame(
            3,
            2_100,
            vec![],
            vec![fill_event(3, 0, OrderSide::Buy, 99.0, 1.0, 5, 6)],
        ));

        let bars: Vec<_> = h
            .bodies()
            .into_iter()
            .filter_map(|body| match body {
                MessageBody::Bar(bar) => Some(bar),
                _ => None,
            })
            .collect();
        let closed = bars
            .iter()
            .find(|bar| bar.window_start_ms == 1_000 && bar.trade_count > 0)
            .expect("closed bar");
        assert_eq!(100.0, closed.open);
        assert_eq!(102.0, closed.high);
        assert_eq!(100.0, closed.low);
        assert_eq!(102.0, closed.close);
        assert_eq!(3.0, closed.volume);
        assert_eq!(2, closed.trade_count);
        assert_eq!(1, closed.bar_seq);
        assert_eq!(2_000, closed.window_start_ms + closed.interval_ms);
    }

    #[test]
    fn lifecycle_sheds_rested_before_critical_events() {
        let mut config = MarketDataConfig::default();
        config.lifecycle_batch_max = 2;
        let mut h = harness(config, true);

        let mut rested = Vec::new();
        for _ in 0..3 {
            let mut order = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
            rested.extend(MatchEvent::for_place_at(1, 0, &order, &[], Some(1.0)));
            order.volume = 0.0;
        }
        let mut cancelled = Vec::new();
        for sequence in 0..2 {
            let mut order = Order::new(OrderSide::Buy, 100.0, 1.0, sequence);
            order.sequence = sequence;
            cancelled.push(MatchEvent::for_cancel_at(2, 0, &order));
        }
        let mut events = rested;
        events.extend(cancelled);

        h.processor.ingest(frame(3, 3_000, vec![], events));
        h.processor
            .tick(Instant::now() + Duration::from_millis(250));

        let lifecycle = h
            .bodies()
            .into_iter()
            .find_map(|body| match body {
                MessageBody::Lifecycle(records) => Some(records),
                _ => None,
            })
            .expect("lifecycle batch");
        assert_eq!(2, lifecycle.len(), "only the critical events survive");
        assert!(
            lifecycle
                .iter()
                .all(|record| record.kind == MatchEventKind::Cancelled)
        );
        assert_eq!(3, h.stats.lifecycle_shed.load(Ordering::Relaxed));
    }

    #[test]
    fn gap_marker_and_snapshot_reset_the_feed() {
        let mut h = harness(MarketDataConfig::default(), true);
        h.dropped.store(3, Ordering::Relaxed);
        h.gap_pending.store(true, Ordering::Relaxed);
        h.processor.tick(Instant::now() + Duration::from_millis(10));

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

        h.processor.apply_snapshot(SnapshotFrame {
            log_index: 10,
            ts_ms: 5_000,
            symbol_id: 0,
            bids: vec![DepthLevel {
                price: 100.0,
                volume: 5.0,
                orders: 1,
            }],
            asks: vec![DepthLevel {
                price: 101.0,
                volume: 4.0,
                orders: 1,
            }],
            last_trade: Some(100.5),
            book_sequence: 12,
            pending_stops: 1,
        });
        h.processor
            .tick(Instant::now() + Duration::from_millis(250));

        let bodies = h.bodies();
        let snapshot = bodies
            .iter()
            .find_map(|body| match body {
                MessageBody::DepthSnapshot(snapshot) => Some(snapshot),
                _ => None,
            })
            .expect("snapshot published");
        assert_eq!(1, snapshot.bids.len());
        assert_eq!(1, snapshot.asks.len());

        let bbo = bodies
            .iter()
            .find_map(|body| match body {
                MessageBody::Bbo(bbo) => Some(bbo),
                _ => None,
            })
            .expect("bbo after snapshot");
        assert_eq!(100.0, bbo.bid);
        assert_eq!(101.0, bbo.ask);
        assert_eq!(Some(100.5), bbo.last_trade);

        // Frames already covered by the snapshot are suppressed.
        let before = h
            .messages()
            .iter()
            .filter(|message| matches!(message.body, MessageBody::Trade(_)))
            .count();
        h.processor.ingest(frame(
            9,
            4_000,
            vec![],
            vec![fill_event(9, 0, OrderSide::Buy, 99.0, 1.0, 7, 8)],
        ));
        h.processor
            .tick(Instant::now() + Duration::from_millis(500));
        let after = h
            .messages()
            .iter()
            .filter(|message| matches!(message.body, MessageBody::Trade(_)))
            .count();
        assert_eq!(before, after, "pre-snapshot frames are suppressed");
    }

    #[test]
    fn leader_gate_blocks_until_a_snapshot_arrives() {
        let mut h = harness(MarketDataConfig::default(), false);
        h.processor.ingest(frame(
            1,
            1_000,
            vec![depth(OrderSide::Buy, 100.0, 1.0, 1)],
            vec![fill_event(1, 0, OrderSide::Buy, 100.0, 1.0, 1, 2)],
        ));
        h.processor
            .tick(Instant::now() + Duration::from_millis(250));
        assert!(h.bodies().is_empty(), "a follower publishes nothing");
        assert!(!h.stats.publishing.load(Ordering::Relaxed));

        h.leader.store(true, Ordering::Relaxed);
        h.processor.apply_snapshot(SnapshotFrame {
            log_index: 0,
            ts_ms: 2_000,
            symbol_id: 0,
            bids: vec![],
            asks: vec![],
            last_trade: None,
            book_sequence: 0,
            pending_stops: 0,
        });
        h.processor
            .tick(Instant::now() + Duration::from_millis(300));
        assert!(h.stats.publishing.load(Ordering::Relaxed));
        assert!(
            h.bodies()
                .iter()
                .any(|body| matches!(body, MessageBody::DepthSnapshot(_))),
            "leadership gain starts with a snapshot"
        );
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
}

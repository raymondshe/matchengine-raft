//! `StateMachineStore`: an in-memory order book, made durable by snapshots.
//!
//! The state machine itself is intentionally kept in memory: applying a Raft entry must be fast
//! and the log already provides durability. Every applied batch is recoverable by replaying the
//! retained log tail from the latest snapshot (see [`crate::store::log_store`]).
//!
//! Snapshots are written to `<dir>/current.snap` with a small framing header, then fsynced and
//! renamed into place, so a crash never leaves a half-written snapshot visible.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::io::Cursor;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

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
use smallvec::SmallVec;
use tokio::sync::watch;

use crate::market_data::MarketDataControl;
use crate::market_data::MarketDataTap;
use crate::market_data::SnapshotFrame;
use crate::market_data::TapFrame;
use crate::match_result::EventId;
use crate::match_result::MatchEvent;
use crate::match_result::MatchEventKind;
use crate::match_result::MatchResultSink;
use crate::match_result::NoopSink;
use crate::order_book::Fixed;
use crate::order_book::Order;
use crate::order_book::OrderBook;
use crate::order_book::OrderSide;
use crate::order_book::RejectReason;
use crate::order_book::SymbolConfig;
use crate::store::config::sync_dir;
use crate::store::types::MatchEngineRequest;
use crate::store::types::MatchEngineResponse;

/// Name of the durable snapshot file inside the snapshot directory.
const SNAPSHOT_FILE: &str = "current.snap";
/// Magic prefix of a snapshot file.
const SNAPSHOT_MAGIC: &[u8; 7] = b"MRFSNAP";
/// Snapshot file format version.
///
/// Version 2 dropped the key-value store from [`StateMachineData`]. Version 3 added the
/// extended [`Order`] fields (time in force, post-only, stop price, iceberg tranches,
/// timestamp) together with parked stops and the last trade price. Version 4 adds the
/// retained match-event window and the client-order-id dedupe map. Version 5 stores
/// order prices and quantities as 18-scale fixed-point integers. Version 6 stores the
/// last trade price and match events as fixed-point values. Version 7 adds the replicated
/// single-symbol precision configuration; version 8 adds maker/taker fee rates to orders and fee
/// fields to stored match events; version 9 adds base/quote asset metadata, tick/lot rules, volume
/// units, fee assets and gross quote notional; version 10 replaces the boolean fee-overflow flag
/// with the `MatchErrors` bit set. Postcard is not self-describing, so an older file is rejected
/// with an explicit error instead of being decoded into a different shape.
const SNAPSHOT_VERSION: u32 = 10;
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
#[derive(Deserialize)]
#[serde(bound = "")]
struct SnapshotPayload<C: RaftTypeConfig> {
    meta: SnapshotMetaOf<C>,
    data: Vec<u8>,
}

/// Borrowed snapshot body used while writing, so the state-machine bytes are
/// copied once into the postcard buffer rather than into an intermediate Vec.
#[derive(Serialize)]
#[serde(bound = "")]
struct SnapshotPayloadRef<'a, C: RaftTypeConfig> {
    meta: &'a SnapshotMetaOf<C>,
    data: &'a [u8],
}

/// Default number of match events retained in the state machine.
pub const DEFAULT_MAX_EVENTS: usize = 262_144;

/// Number of `client_order_id` records retained for submission dedupe.
///
/// This is intentionally independent of [`DEFAULT_MAX_EVENTS`]: the dedupe
/// map influences matching (a retry must resolve identically on every replica),
/// while the event window is feed-only. Keeping it fixed avoids making state
/// transitions depend on a node-local CLI flag.
pub const MAX_SUBMISSIONS: usize = 262_144;

/// Snapshot representation of [`MatchEvent`].
///
/// `MatchEvent` omits absent optional fields from JSON, which postcard cannot
/// decode positionally. This mirror always writes both options, so snapshots
/// stay self-consistent without changing the debug JSONL format.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct StoredMatchEvent {
    pub event_id: EventId,
    pub kind: MatchEventKind,
    pub order_sequence: u64,
    pub side: OrderSide,
    pub counterparty_sequence: Option<u64>,
    pub previous_sequence: Option<u64>,
    pub price: Fixed,
    pub volume: Fixed,
    pub quote_volume: Fixed,
    pub volume_unit: crate::order_book::VolumeUnit,
    pub taker_fee: Fixed,
    pub taker_fee_asset: Option<crate::order_book::FeeAsset>,
    pub maker_fee: Fixed,
    pub maker_fee_asset: Option<crate::order_book::FeeAsset>,
    pub error: crate::order_book::MatchErrors,
    pub reason: Option<RejectReason>,
}

impl From<MatchEvent> for StoredMatchEvent {
    fn from(event: MatchEvent) -> Self {
        Self {
            event_id: event.event_id,
            kind: event.kind,
            order_sequence: event.order_sequence,
            side: event.side,
            counterparty_sequence: event.counterparty_sequence,
            previous_sequence: event.previous_sequence,
            price: event.price,
            volume: event.volume,
            quote_volume: event.quote_volume,
            volume_unit: event.volume_unit,
            taker_fee: event.taker_fee,
            taker_fee_asset: event.taker_fee_asset,
            maker_fee: event.maker_fee,
            maker_fee_asset: event.maker_fee_asset,
            error: event.error,
            reason: event.reason,
        }
    }
}

impl From<StoredMatchEvent> for MatchEvent {
    fn from(event: StoredMatchEvent) -> Self {
        Self {
            event_id: event.event_id,
            kind: event.kind,
            order_sequence: event.order_sequence,
            side: event.side,
            counterparty_sequence: event.counterparty_sequence,
            previous_sequence: event.previous_sequence,
            price: event.price,
            volume: event.volume,
            quote_volume: event.quote_volume,
            volume_unit: event.volume_unit,
            taker_fee: event.taker_fee,
            taker_fee_asset: event.taker_fee_asset,
            maker_fee: event.maker_fee,
            maker_fee_asset: event.maker_fee_asset,
            error: event.error,
            reason: event.reason,
        }
    }
}

/// One successful `Place` keyed by its client idempotency key.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SubmissionRecord {
    /// The client-supplied idempotency key.
    pub client_order_id: String,
    /// Sequence assigned to the original placement.
    pub order_sequence: u64,
    /// Log index of the original placement, used to recover its events.
    pub log_index: u64,
    /// Number of events the original entry produced.
    pub event_count: u32,
    /// Ordinal of the submission's first event inside its entry.
    ///
    /// Batches share one log index across sub-requests, so the log index alone
    /// does not identify which events belong to this submission. This offset is
    /// derived while applying and is not snapshotted; a record restored from a
    /// snapshot falls back to the whole entry (`None`).
    #[serde(skip)]
    pub first_ordinal: Option<u32>,
}

/// Data contained in the Raft state machine.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct StateMachineData {
    /// All orders resting in the book, bids then asks.
    pub orders: Vec<Order>,

    /// The current sequence number for order placement.
    pub sequence: u64,

    /// Stop orders parked off the book, in admission order.
    pub stops: Vec<Order>,

    /// Price of the most recent trade, which elects parked stops.
    pub last_trade_price: Option<Fixed>,

    /// The one symbol this process/cluster trades and its accepted precision.
    #[serde(default)]
    pub symbol_config: SymbolConfig,

    /// Set once the first order is accepted, so the process cannot switch
    /// symbols after trading has started.
    #[serde(default)]
    pub symbol_locked: bool,

    /// Retained match events, oldest first.
    #[serde(default)]
    pub events: Vec<StoredMatchEvent>,

    /// Number of events evicted from the retained window.
    #[serde(default)]
    pub events_evicted: u64,

    /// Client-order-id dedupe records, oldest first.
    #[serde(default)]
    pub submissions: Vec<SubmissionRecord>,

    /// Number of client-order-id records evicted.
    #[serde(default)]
    pub submissions_evicted: u64,
}

/// Inner storage for the state machine.
#[derive(Debug)]
pub struct StateMachineStoreInner<C: RaftTypeConfig> {
    pub last_applied_log: Option<LogIdOf<C>>,

    pub last_membership: StoredMembershipOf<C>,

    /// The order book for the matching engine.
    pub orderbook: OrderBook,

    /// The one symbol this process/cluster trades. Replicated so every node
    /// validates orders identically; a process never trades two symbols.
    symbol_config: SymbolConfig,

    /// Whether trading has started under `symbol_config`.
    symbol_locked: bool,

    /// The last built or installed snapshot, kept so it can be shipped to lagging peers.
    pub current_snapshot: Option<StoredSnapshot<C>>,

    /// Optional market-data tap: node-local, derived, never part of the
    /// replicated state or of snapshots.
    pub market_data: Option<MarketDataTap>,

    /// Bounded, replicated window of match events, oldest first.
    events: VecDeque<MatchEvent>,

    /// Events evicted from the front of `events`.
    events_evicted: u64,

    /// Client-order-id dedupe lookup.
    submissions: BTreeMap<String, SubmissionRecord>,

    /// Submission lookup order, used to evict the oldest ids first.
    submission_order: VecDeque<SubmissionRecord>,

    /// Client-order-id records evicted from the map.
    submissions_evicted: u64,

    /// Maximum number of retained events.
    max_events: usize,

    /// Maximum number of retained client-order-id records.
    max_submissions: usize,
}

impl<C: RaftTypeConfig> Default for StateMachineStoreInner<C> {
    fn default() -> Self {
        Self {
            last_applied_log: None,
            last_membership: StoredMembership::default(),
            orderbook: OrderBook::new(),
            symbol_config: SymbolConfig::default(),
            symbol_locked: false,
            current_snapshot: None,
            market_data: None,
            events: VecDeque::new(),
            events_evicted: 0,
            submissions: BTreeMap::new(),
            submission_order: VecDeque::new(),
            submissions_evicted: 0,
            max_events: DEFAULT_MAX_EVENTS,
            max_submissions: MAX_SUBMISSIONS,
        }
    }
}

impl<C: RaftTypeConfig> StateMachineStoreInner<C> {
    /// Replaces the state with serialized content, rebuilding the order book indices.
    fn install_content(&mut self, content: StateMachineData) {
        self.orderbook.asks.clear();
        self.orderbook.bids.clear();
        self.orderbook.stops.clear();
        self.orderbook.sequence = content.sequence;
        self.orderbook.last_trade_price = content.last_trade_price;

        self.symbol_config = content
            .symbol_config
            .validate()
            .unwrap_or_else(|_| SymbolConfig::default());
        self.symbol_locked = content.symbol_locked;

        for order in &content.orders {
            self.orderbook.insert_order(order);
        }
        self.orderbook.stops.extend(content.stops.iter().copied());
        // A restored state starts a new journal; the tap gets a snapshot from
        // the seeder rather than deltas that predate the restore.
        self.orderbook.take_depth_journal();
        self.orderbook.rebuild_derived_indexes();

        self.events = content.events.into_iter().map(MatchEvent::from).collect();
        self.events_evicted = content.events_evicted;
        self.submissions.clear();
        self.submission_order.clear();
        for record in content.submissions {
            self.submissions
                .insert(record.client_order_id.clone(), record.clone());
            self.submission_order.push_back(record);
        }
        self.submissions_evicted = content.submissions_evicted;
        self.trim_events();
        self.trim_submissions();
    }

    /// Appends one applied entry's events and trims the retained window.
    fn record_events(&mut self, events: &[MatchEvent]) {
        self.events.extend(events.iter().copied());
        self.trim_events();
    }

    fn trim_events(&mut self) {
        while self.events.len() > self.max_events {
            self.events.pop_front();
            self.events_evicted += 1;
        }
    }

    /// Records a successful (or rejected) `Place` for idempotent retries.
    fn record_submission(
        &mut self,
        client_order_id: String,
        order_sequence: u64,
        log_index: u64,
        first_ordinal: u32,
        event_count: u32,
    ) {
        let record = SubmissionRecord {
            client_order_id: client_order_id.clone(),
            order_sequence,
            log_index,
            event_count,
            first_ordinal: Some(first_ordinal),
        };
        self.submissions.insert(client_order_id, record.clone());
        self.submission_order.push_back(record);
        self.trim_submissions();
    }

    fn trim_submissions(&mut self) {
        while self.submission_order.len() > self.max_submissions {
            let Some(old) = self.submission_order.pop_front() else {
                break;
            };
            let still_current = self
                .submissions
                .get(&old.client_order_id)
                .is_some_and(|current| {
                    current.order_sequence == old.order_sequence
                        && current.log_index == old.log_index
                });
            if still_current {
                self.submissions.remove(&old.client_order_id);
            }
            self.submissions_evicted += 1;
        }
    }

    /// Events retained for the entry that produced `record`, capped at the
    /// response limit. The boolean is true when the response is incomplete.
    fn submission_events(&self, record: &SubmissionRecord) -> (Vec<MatchEvent>, bool) {
        let cap = MatchEngineResponse::MAX_EVENTS;
        // Events are ordered by (log_index, ordinal), so jump to the entry and
        // copy only the submission's ordinal range instead of scanning the
        // whole retained window.
        let first = record.first_ordinal.unwrap_or(0);
        let start_id = EventId {
            log_index: record.log_index,
            ordinal: first,
        };
        let start = self
            .events
            .partition_point(|event| event.event_id < start_id);
        let end = first.saturating_add(record.event_count);

        let mut events = Vec::new();
        let mut total = 0u32;
        for event in self.events.range(start..) {
            if event.event_id.log_index != record.log_index {
                break;
            }
            if record.first_ordinal.is_some() && event.event_id.ordinal >= end {
                break;
            }
            total = total.saturating_add(1);
            if events.len() < cap {
                events.push(*event);
            }
        }
        let truncated = total > cap as u32 || total < record.event_count;
        (events, truncated)
    }

    fn duplicate_response(&self, record: &SubmissionRecord) -> MatchEngineResponse {
        let (events, truncated) = self.submission_events(record);
        MatchEngineResponse::duplicate(record.order_sequence, events, truncated)
    }

    /// Returns events strictly after `after`, up to `limit`, and whether more
    /// remain in the retained window.
    pub fn events_since(
        &self,
        after: Option<EventId>,
        limit: usize,
    ) -> (Vec<MatchEvent>, Option<EventId>, bool) {
        let mut events = Vec::new();
        let mut has_more = false;
        for event in &self.events {
            if after.is_some_and(|cursor| event.event_id <= cursor) {
                continue;
            }
            if events.len() == limit {
                has_more = true;
                break;
            }
            events.push(*event);
        }
        let next_after = events.last().map_or(after, |event| Some(event.event_id));
        (events, next_after, has_more)
    }

    pub fn oldest_event_id(&self) -> Option<EventId> {
        self.events.front().map(|event| event.event_id)
    }

    pub fn last_event_id(&self) -> Option<EventId> {
        self.events.back().map(|event| event.event_id)
    }

    pub fn retained_event_count(&self) -> usize {
        self.events.len()
    }

    pub fn events_evicted(&self) -> u64 {
        self.events_evicted
    }

    pub fn submissions_evicted(&self) -> u64 {
        self.submissions_evicted
    }

    pub fn event_retention_limit(&self) -> usize {
        self.max_events
    }

    pub fn submission_retention_limit(&self) -> usize {
        self.max_submissions
    }

    /// Resting orders without cloning the event log or submission map.
    pub fn orders(&self) -> Vec<Order> {
        self.orderbook
            .bids
            .values()
            .chain(self.orderbook.asks.values())
            .copied()
            .collect()
    }

    /// The one symbol this process/cluster trades.
    pub fn symbol_config(&self) -> SymbolConfig {
        self.symbol_config.clone()
    }

    /// The id of the one symbol this process/cluster trades.
    pub fn symbol_id(&self) -> u32 {
        self.symbol_config.symbol_id
    }

    /// Adds or updates this process/cluster's symbol and precision configuration.
    ///
    /// A process trades exactly one symbol for its lifetime. The configuration
    /// may only change before the first accepted order (and while the book is
    /// empty); it is locked once trading starts. The change is replicated and
    /// snapshotted, keeping every node's validation identical.
    pub fn configure_symbol(&mut self, config: SymbolConfig) -> Result<(), String> {
        let config = config
            .validate()
            .map_err(|_| "invalid symbol precision, tick, lot or fee policy".to_string())?;
        if self.symbol_locked {
            return Err("symbol configuration is locked after trading starts".to_string());
        }
        let has_open_orders = !self.orderbook.bids.is_empty()
            || !self.orderbook.asks.is_empty()
            || !self.orderbook.stops.is_empty();
        if has_open_orders {
            return Err("cannot change the symbol configuration while orders are open".to_string());
        }
        self.symbol_config = config;
        // A later stop must not compare against a print admitted under the
        // previous symbol or precision.
        self.orderbook.last_trade_price = None;
        Ok(())
    }

    /// Resolves the configured symbol for an order, rejecting an order routed
    /// to a different symbol.
    fn configured_symbol_for(&self, order: &Order) -> Result<SymbolConfig, RejectReason> {
        if order.symbol_id != self.symbol_config.symbol_id {
            return Err(RejectReason::UnsupportedOrder);
        }
        Ok(self.symbol_config.clone())
    }

    /// Validates the replacement values of an amendment at the configured
    /// symbol's precision. The key fields were validated when the resting order
    /// entered.
    fn validate_amend_values(
        &self,
        order: &Order,
        price: Option<Fixed>,
        volume: Option<Fixed>,
    ) -> Result<SymbolConfig, RejectReason> {
        let config = self.configured_symbol_for(order)?;
        let price_ok = price.is_none_or(|value| {
            value.has_at_most_decimals(config.price_decimals)
                && value.is_multiple_of(config.price_tick)
        });
        let volume_ok = volume.is_none_or(|value| {
            value.has_at_most_decimals(config.base_asset.decimals)
                && value.is_multiple_of(config.base_lot)
        });
        if price_ok && volume_ok {
            Ok(config)
        } else {
            Err(RejectReason::InvalidOrder)
        }
    }

    /// Converts the state machine to a serializable content format.
    pub fn to_content(&self) -> StateMachineData {
        let mut content = StateMachineData {
            orders: Vec::new(),
            sequence: self.orderbook.sequence,
            stops: self.orderbook.stops.clone(),
            last_trade_price: self.orderbook.last_trade_price,
            symbol_config: self.symbol_config.clone(),
            symbol_locked: self.symbol_locked,
            events: self
                .events
                .iter()
                .copied()
                .map(StoredMatchEvent::from)
                .collect(),
            events_evicted: self.events_evicted,
            submissions: self.submission_order.iter().cloned().collect(),
            submissions_evicted: self.submissions_evicted,
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
///
/// It also owns the [`MatchResultSink`] this node publishes fills to. The sink is node-local
/// configuration and never affects the replicated state.
pub struct StateMachineStore<C: RaftTypeConfig> {
    inner: Arc<Mutex<StateMachineStoreInner<C>>>,
    dir: Arc<PathBuf>,
    sink: Arc<dyn MatchResultSink>,
    /// Set by `build_match_engine_node` when a market-data feed is enabled. It
    /// lets a symbol configuration change reset consumers to the new symbol.
    market_data_control: Arc<OnceLock<(MarketDataControl, usize)>>,
    /// Last retained event id, updated after every apply that appended events.
    /// Node-local wakeup for long-poll readers; never part of snapshots.
    events_changed: watch::Sender<Option<EventId>>,
}

/// A sink/tap publication produced while applying one entry.
///
/// It is collected under the state-machine lock and published after the guard
/// is dropped, so a slow sink cannot extend the lock hold time.
struct PendingPublish {
    events: EventBuffer,
    market: Option<(MarketDataTap, TapFrame)>,
}

/// Inline event buffer for the common 1–8 event entry.
///
/// Most applied entries produce a single rest, cancel or fill event. Keeping
/// that buffer inline avoids a heap allocation on every request; a large sweep
/// spills to the heap as usual.
type EventBuffer = SmallVec<[MatchEvent; 4]>;

/// Inline publish list for the common single-request entry.
type PendingPublishes = SmallVec<[PendingPublish; 1]>;

impl<C: RaftTypeConfig> Clone for StateMachineStore<C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            dir: self.dir.clone(),
            sink: self.sink.clone(),
            market_data_control: self.market_data_control.clone(),
            events_changed: self.events_changed.clone(),
        }
    }
}

impl<C: RaftTypeConfig> fmt::Debug for StateMachineStore<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StateMachineStore")
            .field("inner", &self.inner)
            .field("dir", &self.dir)
            .field("match_result_sink", &self.sink.name())
            .finish()
    }
}

impl<C: RaftTypeConfig> StateMachineStore<C> {
    /// Opens the state machine in `dir`, restoring the latest durable snapshot if one exists.
    ///
    /// Match results are computed and dropped; use [`StateMachineStore::open_with_sink`] to
    /// observe them.
    ///
    /// A corrupt snapshot is reported as an error instead of silently starting empty: silently
    /// dropping committed data would be worse than refusing to start.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        Self::open_with_sinks(dir, Arc::new(NoopSink), None)
    }

    /// Like [`StateMachineStore::open`], but publishes match results to `sink`.
    ///
    /// The sink is not part of the Raft state: two nodes applying the same log with different
    /// sinks still end up with identical state machines.
    pub fn open_with_sink(
        dir: impl Into<PathBuf>,
        sink: Arc<dyn MatchResultSink>,
    ) -> io::Result<Self> {
        Self::open_with_sinks(dir, sink, None)
    }

    /// Like [`StateMachineStore::open_with_sink`], with an optional market-data
    /// tap. The tap is node-local: it never affects the replicated state, and a
    /// node without one applies exactly the same entries.
    pub fn open_with_sinks(
        dir: impl Into<PathBuf>,
        sink: Arc<dyn MatchResultSink>,
        market_data: Option<MarketDataTap>,
    ) -> io::Result<Self> {
        Self::open_with_sinks_and_retention(dir, sink, market_data, DEFAULT_MAX_EVENTS)
    }

    /// Like [`StateMachineStore::open_with_sinks`], with an explicit event
    /// retention limit. The same limit bounds the client-order-id dedupe map.
    pub fn open_with_sinks_and_retention(
        dir: impl Into<PathBuf>,
        sink: Arc<dyn MatchResultSink>,
        market_data: Option<MarketDataTap>,
        max_events: usize,
    ) -> io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;

        let mut inner = StateMachineStoreInner::<C> {
            max_events,
            ..StateMachineStoreInner::default()
        };

        if let Some((meta, data)) = read_snapshot_file::<C>(&dir)? {
            let content = decode_content(&data)?;
            let snapshot_last_log_id = meta.last_log_id.clone();
            let snapshot_events = content.events.len();
            let snapshot_orders = content.orders.len();
            inner.install_content(content);
            inner.last_applied_log = meta.last_log_id.clone();
            inner.last_membership = meta.last_membership.clone();
            inner.current_snapshot = Some(StoredSnapshot { meta, data });
            tracing::info!(
                snapshot_last_log_id = ?snapshot_last_log_id,
                snapshot_events,
                snapshot_orders,
                "loaded latest snapshot; WAL replay starts after its last log id"
            );
        }

        if market_data.is_some() {
            inner.orderbook.enable_depth_journal();
        }
        inner.market_data = market_data;

        let (events_changed, _) = watch::channel(inner.last_event_id());

        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
            dir: Arc::new(dir),
            sink,
            market_data_control: Arc::new(OnceLock::new()),
            events_changed,
        })
    }

    /// Test-only constructor with independent event and submission limits.
    #[cfg(test)]
    pub(crate) async fn open_with_limits(
        dir: impl Into<PathBuf>,
        sink: Arc<dyn MatchResultSink>,
        market_data: Option<MarketDataTap>,
        max_events: usize,
        max_submissions: usize,
    ) -> io::Result<Self> {
        let store = Self::open_with_sinks_and_retention(dir, sink, market_data, max_events)?;
        {
            let mut inner = store.inner.lock().await;
            inner.max_submissions = max_submissions;
            inner.trim_submissions();
        }
        Ok(store)
    }

    /// Directory that holds this state machine's snapshot.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn inner(&self) -> &Arc<Mutex<StateMachineStoreInner<C>>> {
        &self.inner
    }

    /// Subscribes to retained-event-window changes for long-poll readers.
    ///
    /// The value is the window's last event id (`Option<EventId>`); it changes
    /// only when an apply appended events (or a snapshot install replaced the
    /// window), so a waiter can sleep between checks.
    pub fn subscribe_events(&self) -> watch::Receiver<Option<EventId>> {
        self.events_changed.subscribe()
    }

    /// Wakes long-poll readers when the retained event window advances.
    fn notify_events(&self, last_event: Option<EventId>) {
        self.events_changed.send_if_modified(|current| {
            if *current == last_event {
                false
            } else {
                *current = last_event;
                true
            }
        });
    }

    /// The sink this store publishes match results to.
    pub fn match_result_sink(&self) -> &Arc<dyn MatchResultSink> {
        &self.sink
    }

    /// Installs the market-data control handle and snapshot depth used when a
    /// symbol configuration change must reset consumers.
    pub fn set_market_data_control(&self, control: MarketDataControl, depth_levels: usize) {
        let _ = self.market_data_control.set((control, depth_levels));
    }

    /// Resets a publishing feed to the current symbol after a config change.
    fn publish_market_data_reset(&self, inner: &StateMachineStoreInner<C>) {
        let Some((control, depth_levels)) = self.market_data_control.get() else {
            return;
        };
        if control.is_publishing() {
            control.push_snapshot(snapshot_from_inner(inner, *depth_levels));
        }
    }

    /// Applies a symbol configuration and, if this node publishes market data,
    /// resets the feed to the new symbol.
    fn configure_symbol_locked(
        &self,
        inner: &mut StateMachineStoreInner<C>,
        config: SymbolConfig,
    ) -> Result<(), String> {
        inner.configure_symbol(config)?;
        self.publish_market_data_reset(inner);
        Ok(())
    }

    /// Publishes one entry's events, logging (but swallowing) sink failures.
    ///
    /// Failure must never fail `apply`: replicas that cannot reach their sink have to keep
    /// converging with replicas that can. Blocking sinks run on the blocking pool so a file
    /// write cannot stall a Tokio worker.
    async fn publish_match_events(&self, events: EventBuffer) {
        if events.is_empty() {
            return;
        }
        let count = events.len();
        let result = if self.sink.is_blocking() {
            let sink = self.sink.clone();
            tokio::task::spawn_blocking(move || sink.publish(&events)).await
        } else {
            Ok(self.sink.publish(&events))
        };
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(
                sink = self.sink.name(),
                events = count,
                %error,
                "match result sink failed; the state machine is unaffected"
            ),
            Err(error) => tracing::warn!(
                sink = self.sink.name(),
                events = count,
                %error,
                "match result sink task failed; the state machine is unaffected"
            ),
        }
    }

    /// Builds the market-data frame for one applied entry while the
    /// state-machine lock is held.
    ///
    /// The frame is sent by [`Self::publish_pending`] after the lock is
    /// released. A closed publishing gate drains the depth journal but skips the
    /// event clone, so followers and deposed leaders do not pay for a frame the
    /// tap would drop.
    fn take_market_frame(
        &self,
        inner: &mut StateMachineStoreInner<C>,
        log_index: u64,
        market_seq: u64,
        ts_ms: u64,
        symbol_id: u32,
        events: &[MatchEvent],
    ) -> Option<(MarketDataTap, TapFrame)> {
        let tap = inner.market_data.clone()?;
        let depth = inner.orderbook.take_depth_journal();
        if !tap.is_enabled() {
            return None;
        }
        if events.is_empty() && depth.is_empty() {
            return None;
        }
        Some((
            tap,
            TapFrame {
                log_index,
                market_seq,
                ts_ms,
                symbol_id,
                events: events.to_vec(),
                depth,
            },
        ))
    }

    /// Publishes one entry's sink and tap work after the state-machine lock is
    /// released. Ordering matches the order the entries were applied in.
    async fn publish_pending(&self, pending: impl IntoIterator<Item = PendingPublish>) {
        for publish in pending {
            self.publish_match_events(publish.events).await;
            if let Some((tap, frame)) = publish.market {
                tap.publish(frame);
            }
        }
    }
}

/// Public access to the state machine internals for our application
impl<C> StateMachineStore<C>
where
    C: RaftTypeConfig<D = MatchEngineRequest, R = MatchEngineResponse>,
{
    pub async fn orderbook_sequence(&self) -> u64 {
        let inner = self.inner.lock().await;
        inner.orderbook.sequence
    }

    /// The symbol id this process/cluster trades, read from the applied state.
    pub async fn symbol_id(&self) -> u32 {
        let inner = self.inner.lock().await;
        inner.symbol_id()
    }

    pub async fn to_content(&self) -> StateMachineData {
        let inner = self.inner.lock().await;
        inner.to_content()
    }

    /// Captures the market-data feed's reset point at the current applied index.
    ///
    /// Called by the leader watcher on start-up and every leadership gain; it
    /// reads the book once (top-N levels) and never runs per entry.
    pub async fn capture_market_data_snapshot(&self, depth_levels: usize) -> SnapshotFrame {
        let inner = self.inner.lock().await;
        snapshot_from_inner(&inner, depth_levels)
    }

    /// Captures a reset snapshot, queues it and only then opens the feed gate.
    ///
    /// Holding the state-machine lock across "build and enqueue" is what makes
    /// the ordering guarantee real: no frame applied before the capture can be
    /// published after the snapshot, and no frame applied after the capture can
    /// reach the processor before it (the snapshot is queued first, and the
    /// gate opens only after it).
    pub async fn start_market_data_feed(&self, control: &MarketDataControl, depth_levels: usize) {
        let inner = self.inner.lock().await;
        control.push_snapshot(snapshot_from_inner(&inner, depth_levels));
        control.set_leader(true);
    }

    /// Writes the current in-memory content as a durable snapshot.
    ///
    /// This is what [`RaftSnapshotBuilder::build_snapshot`] uses; it is also handy in tests that
    /// want a snapshot without waiting for openraft's snapshot policy to trigger.
    pub async fn persist_snapshot(&self) -> io::Result<SnapshotMetaOf<C>> {
        // Capture a consistent content image, then release the state-machine
        // lock before encoding and writing. Applying new entries can continue
        // while a large snapshot is serialized to disk.
        let (meta, content) = {
            let inner = self.inner.lock().await;
            (
                SnapshotMetaOf::<C> {
                    last_log_id: inner.last_applied_log.clone(),
                    last_membership: inner.last_membership.clone(),
                },
                inner.to_content(),
            )
        };
        let dir = self.dir.clone();
        let write_meta = meta.clone();
        blocking(move || {
            let data = encode_content(&content)?;
            write_snapshot_file::<C>(&dir, &write_meta, &data)
        })
        .await?;
        Ok(meta)
    }
}

impl<C> RaftSnapshotBuilder<C> for StateMachineStore<C>
where
    C: RaftTypeConfig<D = MatchEngineRequest, R = MatchEngineResponse>,
{
    type SnapshotData = Cursor<Vec<u8>>;

    #[tracing::instrument(level = "trace", skip(self))]
    async fn build_snapshot(&mut self) -> Result<SnapshotOf<C, Cursor<Vec<u8>>>, io::Error> {
        // Capture a consistent image under the lock, then encode and write it
        // without blocking apply. The snapshot still corresponds to exactly one
        // applied log id captured above.
        let (meta, content) = {
            let inner = self.inner.lock().await;
            (
                SnapshotMetaOf::<C> {
                    last_log_id: inner.last_applied_log.clone(),
                    last_membership: inner.last_membership.clone(),
                },
                inner.to_content(),
            )
        };
        let dir = self.dir.clone();
        let write_meta = meta.clone();
        let data = blocking(move || {
            let data = encode_content(&content)?;
            write_snapshot_file::<C>(&dir, &write_meta, &data)?;
            Ok::<_, io::Error>(data)
        })
        .await?;

        {
            let mut inner = self.inner.lock().await;
            inner.current_snapshot = Some(StoredSnapshot {
                meta: meta.clone(),
                data: data.clone(),
            });
        }

        Ok(SnapshotOf::<C, Cursor<Vec<u8>>> {
            meta,
            snapshot: Cursor::new(data),
        })
    }
}

impl<C> StateMachineStore<C>
where
    C: RaftTypeConfig<D = MatchEngineRequest, R = MatchEngineResponse>,
{
    /// Applies one non-batch request and returns its response plus the number
    /// of events it produced. Batch callers use the count to keep ordinals
    /// continuous across sub-requests in the same Raft entry.
    fn apply_one(
        &self,
        inner: &mut StateMachineStoreInner<C>,
        request: &MatchEngineRequest,
        log_index: u64,
        first_ordinal: u32,
        publishes: &mut PendingPublishes,
    ) -> (MatchEngineResponse, usize) {
        match request {
            MatchEngineRequest::Place {
                order,
                client_order_id,
            } => {
                if let Some(client_order_id) = client_order_id
                    && let Some(record) = inner.submissions.get(client_order_id).cloned()
                {
                    return (inner.duplicate_response(&record), 0);
                }

                let mut o = *order;
                let outcome = match inner.configured_symbol_for(&o) {
                    Ok(config) => inner.orderbook.execute_order_with_symbol(&mut o, config),
                    Err(reason) => inner.orderbook.reject_order(&mut o, reason),
                };
                if outcome.rejected.is_none() {
                    // One process serves one symbol: lock the provisioned
                    // configuration once trading starts.
                    inner.symbol_locked = true;
                }

                let mut events = EventBuffer::new();
                match outcome.rejected {
                    Some(reason) => {
                        events.push(MatchEvent::for_rejected(
                            log_index,
                            first_ordinal,
                            &o,
                            reason,
                        ));
                    }
                    None => {
                        for expired in &outcome.expired {
                            events.push(MatchEvent::for_expired(
                                log_index,
                                first_ordinal + events.len() as u32,
                                expired,
                            ));
                        }
                        MatchEvent::for_place_at_into(
                            log_index,
                            first_ordinal + events.len() as u32,
                            &o,
                            &outcome.fills,
                            outcome.resting,
                            &mut events,
                        );
                        if let Some(volume) = outcome.cancelled {
                            events.push(MatchEvent::for_cancelled_volume(
                                log_index,
                                first_ordinal + events.len() as u32,
                                &o,
                                volume,
                            ));
                        }
                        for trigger in &outcome.triggers {
                            events.push(MatchEvent::for_triggered(
                                log_index,
                                first_ordinal + events.len() as u32,
                                &trigger.stop,
                                trigger.trigger_price,
                            ));
                            MatchEvent::for_fills_into(
                                log_index,
                                first_ordinal + events.len() as u32,
                                trigger.stop.side,
                                trigger.stop.sequence,
                                &trigger.fills,
                                &mut events,
                            );
                            if let Some(volume) = trigger.cancelled {
                                events.push(MatchEvent::for_cancelled_volume(
                                    log_index,
                                    first_ordinal + events.len() as u32,
                                    &trigger.stop,
                                    volume,
                                ));
                            }
                        }
                    }
                }
                inner.record_events(&events);
                if let Some(client_order_id) = client_order_id {
                    inner.record_submission(
                        client_order_id.clone(),
                        o.sequence,
                        log_index,
                        first_ordinal,
                        events.len() as u32,
                    );
                }
                let market = self.take_market_frame(
                    inner,
                    log_index,
                    outcome.market_seq,
                    o.timestamp,
                    o.symbol_id,
                    &events,
                );
                let count = events.len();
                let response = MatchEngineResponse::applied(o.sequence, &events);
                if !events.is_empty() || market.is_some() {
                    publishes.push(PendingPublish { events, market });
                }
                (response, count)
            }
            MatchEngineRequest::Cancel { order } => {
                let market_seq = inner.orderbook.next_market_sequence();
                let mut events = EventBuffer::new();
                let expired_orders = if inner.orderbook.has_gtd_deadlines() {
                    inner.orderbook.purge_due_gtd(order.timestamp)
                } else {
                    Vec::new()
                };
                for expired in expired_orders {
                    events.push(MatchEvent::for_expired(
                        log_index,
                        first_ordinal + events.len() as u32,
                        &expired,
                    ));
                }
                if inner.configured_symbol_for(order).is_ok() {
                    if let Some(cancelled) = inner.orderbook.cancel(order) {
                        events.push(MatchEvent::for_cancel_at(
                            log_index,
                            first_ordinal + events.len() as u32,
                            &cancelled,
                        ));
                    }
                } else {
                    events.push(MatchEvent::for_rejected(
                        log_index,
                        first_ordinal + events.len() as u32,
                        order,
                        RejectReason::UnsupportedOrder,
                    ));
                }
                inner.record_events(&events);
                let market = self.take_market_frame(
                    inner,
                    log_index,
                    market_seq,
                    order.timestamp,
                    order.symbol_id,
                    &events,
                );
                let count = events.len();
                let response = MatchEngineResponse::applied(market_seq, &events);
                if !events.is_empty() || market.is_some() {
                    publishes.push(PendingPublish { events, market });
                }
                (response, count)
            }
            MatchEngineRequest::Amend {
                order,
                price,
                volume,
            } => {
                let market_seq = inner.orderbook.next_market_sequence();
                let mut events = EventBuffer::new();
                let expired_orders = if inner.orderbook.has_gtd_deadlines() {
                    inner.orderbook.purge_due_gtd(order.timestamp)
                } else {
                    Vec::new()
                };
                for expired in expired_orders {
                    events.push(MatchEvent::for_expired(
                        log_index,
                        first_ordinal + events.len() as u32,
                        &expired,
                    ));
                }
                match inner.validate_amend_values(order, *price, *volume) {
                    Err(reason) => events.push(MatchEvent::for_rejected(
                        log_index,
                        first_ordinal + events.len() as u32,
                        order,
                        reason,
                    )),
                    Ok(_) => match inner
                        .orderbook
                        .amend_with_sequence(order, *price, *volume, market_seq)
                    {
                        Ok(amendment) => events.push(MatchEvent::for_amended(
                            log_index,
                            first_ordinal + events.len() as u32,
                            &amendment,
                        )),
                        Err(err) => events.push(MatchEvent::for_rejected(
                            log_index,
                            first_ordinal + events.len() as u32,
                            order,
                            err.reason(),
                        )),
                    },
                }
                inner.record_events(&events);
                let market = self.take_market_frame(
                    inner,
                    log_index,
                    market_seq,
                    order.timestamp,
                    order.symbol_id,
                    &events,
                );
                let count = events.len();
                let response = MatchEngineResponse::applied(market_seq, &events);
                if !events.is_empty() || market.is_some() {
                    publishes.push(PendingPublish { events, market });
                }
                (response, count)
            }
            MatchEngineRequest::PurgeExpired {
                now_ms,
                day_close_ms,
            } => {
                let market_seq = inner.orderbook.next_market_sequence();
                let expired = inner.orderbook.purge_expired(*now_ms, *day_close_ms);
                let mut events = EventBuffer::new();
                for (ordinal, order) in expired.iter().enumerate() {
                    events.push(MatchEvent::for_expired(
                        log_index,
                        first_ordinal + ordinal as u32,
                        order,
                    ));
                }
                inner.record_events(&events);
                let symbol_id = inner.symbol_config.symbol_id;
                let market = self
                    .take_market_frame(inner, log_index, market_seq, *now_ms, symbol_id, &events);
                let count = events.len();
                let response = MatchEngineResponse::applied(market_seq, &events);
                if !events.is_empty() || market.is_some() {
                    publishes.push(PendingPublish { events, market });
                }
                (response, count)
            }
            MatchEngineRequest::ConfigureSymbol { config } => {
                match self.configure_symbol_locked(inner, config.clone()) {
                    Ok(()) => (
                        MatchEngineResponse::new(format!("symbol {} configured", config.symbol_id)),
                        0,
                    ),
                    Err(error) => (MatchEngineResponse::error(error), 0),
                }
            }
            MatchEngineRequest::Batch { .. } => (
                MatchEngineResponse::error("nested batches are not supported"),
                0,
            ),
        }
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

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogIdOf<C>>, StoredMembershipOf<C>), io::Error> {
        let inner = self.inner.lock().await;
        Ok((
            inner.last_applied_log.clone(),
            inner.last_membership.clone(),
        ))
    }

    #[tracing::instrument(level = "trace", skip(self, entries))]
    async fn apply<Strm>(&mut self, mut entries: Strm) -> Result<(), io::Error>
    where
        Strm: Stream<Item = Result<EntryResponder<C>, io::Error>> + Unpin + OptionalSend,
    {
        while let Some((entry, responder)) = entries.try_next().await? {
            tracing::debug!(log_id = %entry.log_id, "replicate to sm");

            let mut publishes = match &entry.payload {
                EntryPayload::Normal(MatchEngineRequest::Batch { requests }) => {
                    // Reserve once when the batch is expected to publish. An
                    // all-PurgeExpired batch usually produces no events; avoid
                    // reserving a large publish list for it.
                    let capacity = if requests
                        .iter()
                        .any(|request| !matches!(request, MatchEngineRequest::PurgeExpired { .. }))
                    {
                        requests.len()
                    } else {
                        0
                    };
                    PendingPublishes::with_capacity(capacity)
                }
                _ => PendingPublishes::new(),
            };
            let (response, last_event) = {
                // Lock per entry, not for the whole Apply range: reads and
                // snapshot capture can interleave between entries, and the log
                // reader is not called while the lock is held.
                let mut inner = self.inner.lock().await;
                inner.last_applied_log = Some(entry.log_id.clone());

                // Application code below stays deterministic: events are derived from the entry
                // alone, and publishing them is best-effort (a sink failure is logged, never
                // propagated) so replicas with different sinks still converge.
                let response = match &entry.payload {
                    EntryPayload::Blank => MatchEngineResponse::none(),
                    EntryPayload::Normal(MatchEngineRequest::Batch { requests }) => {
                        let mut responses = Vec::with_capacity(requests.len());
                        let mut ordinal = 0u32;
                        for request in requests {
                            let (response, event_count) = self.apply_one(
                                &mut inner,
                                request,
                                entry.log_id.index,
                                ordinal,
                                &mut publishes,
                            );
                            ordinal = ordinal.saturating_add(event_count as u32);
                            responses.push(response);
                        }
                        MatchEngineResponse::batch(responses)
                    }
                    EntryPayload::Normal(req) => {
                        let (response, _) =
                            self.apply_one(&mut inner, req, entry.log_id.index, 0, &mut publishes);
                        response
                    }
                    EntryPayload::Membership(mem) => {
                        inner.last_membership =
                            StoredMembership::new(Some(entry.log_id.clone()), mem.clone());
                        MatchEngineResponse::none()
                    }
                };
                (response, inner.last_event_id())
            };

            // Sink and tap work happens outside the state-machine lock.
            self.publish_pending(publishes).await;
            // Wake long-poll readers only when the retained window advanced.
            self.notify_events(last_event);

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

        let dir = self.dir.clone();
        let write_meta = meta.clone();
        // Decode and validate on the blocking pool, before touching the durable copy.
        let (content, data) = blocking(move || {
            let content = decode_content(&data)?;
            write_snapshot_file::<C>(&dir, &write_meta, &data)?;
            Ok::<_, io::Error>((content, data))
        })
        .await?;

        let last_event = {
            let mut inner = self.inner.lock().await;
            inner.last_applied_log = meta.last_log_id.clone();
            inner.last_membership = meta.last_membership.clone();
            inner.install_content(content);
            inner.current_snapshot = Some(StoredSnapshot {
                meta: meta.clone(),
                data,
            });
            inner.last_event_id()
        };
        self.notify_events(last_event);

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    #[allow(clippy::type_complexity)]
    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<SnapshotOf<C, Cursor<Vec<u8>>>>, io::Error> {
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
    postcard::from_bytes(data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("corrupt snapshot: {e}")))
}

/// Decodes one snapshot from memory, validating its magic, version and CRC.
///
/// This is the pure decoder behind [`read_snapshot_file`]; it is public so fuzz
/// targets and diagnostic tools can exercise the framing without touching the
/// filesystem. The returned data is the postcard-encoded [`StateMachineData`]
/// body.
pub fn read_snapshot_bytes<C: RaftTypeConfig>(
    bytes: &[u8],
) -> io::Result<(SnapshotMetaOf<C>, Vec<u8>)> {
    decode_snapshot_bytes::<C>(bytes, "snapshot bytes")
}

fn decode_snapshot_bytes<C: RaftTypeConfig>(
    bytes: &[u8],
    source: &str,
) -> io::Result<(SnapshotMetaOf<C>, Vec<u8>)> {
    if bytes.len() < SNAPSHOT_HEADER_LEN || &bytes[0..SNAPSHOT_MAGIC.len()] != SNAPSHOT_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{source} is not a snapshot file"),
        ));
    }
    let version = u32::from_le_bytes(
        bytes[SNAPSHOT_MAGIC.len()..SNAPSHOT_MAGIC.len() + 4]
            .try_into()
            .unwrap(),
    );
    if version != SNAPSHOT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{source} has unsupported snapshot version {version}"),
        ));
    }

    let expected_crc = u32::from_le_bytes(
        bytes[SNAPSHOT_MAGIC.len() + 4..SNAPSHOT_HEADER_LEN]
            .try_into()
            .unwrap(),
    );
    if crc32fast::hash(&bytes[SNAPSHOT_HEADER_LEN..]) != expected_crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{source} has a snapshot checksum mismatch"),
        ));
    }

    let payload: SnapshotPayload<C> =
        postcard::from_bytes(&bytes[SNAPSHOT_HEADER_LEN..]).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("corrupt snapshot: {e}"))
        })?;

    Ok((payload.meta, payload.data))
}

/// Reads and validates the latest snapshot file.
///
/// `current.snap` is replaced atomically, so the file present at startup is the
/// newest complete snapshot; a crash can leave only a `.tmp` file, which is
/// never selected. The returned data is the postcard-encoded
/// [`StateMachineData`] body.
pub fn read_snapshot_file<C: RaftTypeConfig>(
    dir: &Path,
) -> io::Result<Option<(SnapshotMetaOf<C>, Vec<u8>)>> {
    let path = dir.join(SNAPSHOT_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    decode_snapshot_bytes::<C>(&bytes, &path.display().to_string()).map(Some)
}

/// A writer that computes a CRC32 over exactly the bytes accepted by the
/// underlying writer. Used to stream a postcard snapshot body to disk without
/// first materializing the whole body in memory.
struct CrcWriter<W> {
    inner: W,
    hasher: crc32fast::Hasher,
}

impl<W: std::io::Write> CrcWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: crc32fast::Hasher::new(),
        }
    }

    fn finish(mut self) -> io::Result<(W, u32)> {
        use std::io::Write;
        self.flush()?;
        let crc = self.hasher.finalize();
        Ok((self.inner, crc))
    }
}

impl<W: std::io::Write> std::io::Write for CrcWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Writes a snapshot atomically: temp file, fsync, rename, fsync directory.
///
/// The postcard body is streamed through [`CrcWriter`] instead of being built
/// in a second large `Vec`; only the 4-byte CRC is patched afterwards.
fn write_snapshot_file<C: RaftTypeConfig>(
    dir: &Path,
    meta: &SnapshotMetaOf<C>,
    data: &[u8],
) -> io::Result<()> {
    use std::io::Seek;
    use std::io::SeekFrom;
    use std::io::Write;

    let payload = SnapshotPayloadRef::<C> { meta, data };
    let tmp = dir.join(format!("{SNAPSHOT_FILE}.tmp"));
    let path = dir.join(SNAPSHOT_FILE);

    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(SNAPSHOT_MAGIC)?;
    file.write_all(&SNAPSHOT_VERSION.to_le_bytes())?;
    file.write_all(&0u32.to_le_bytes())?;

    let mut writer = CrcWriter::new(file);
    postcard::to_io(&payload, &mut writer)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let (mut file, crc) = writer.finish()?;

    file.seek(SeekFrom::Start((SNAPSHOT_MAGIC.len() + 4) as u64))?;
    file.write_all(&crc.to_le_bytes())?;
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

/// Builds a feed snapshot from the current in-memory state.
fn snapshot_from_inner<C: RaftTypeConfig>(
    inner: &StateMachineStoreInner<C>,
    depth_levels: usize,
) -> SnapshotFrame {
    let log_index = inner
        .last_applied_log
        .as_ref()
        .map_or(0, |log_id| log_id.index);
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default();
    SnapshotFrame {
        log_index,
        market_seq: inner.orderbook.sequence, // next sequence: the reset boundary
        ts_ms,
        symbol_id: inner.symbol_config.symbol_id,
        bids: inner.orderbook.depth(OrderSide::Buy, depth_levels),
        asks: inner.orderbook.depth(OrderSide::Sell, depth_levels),
        last_trade: inner.orderbook.last_trade_price,
        book_sequence: inner.orderbook.sequence,
        pending_stops: inner.orderbook.stops.len() as u32,
    }
}

#[cfg(test)]
mod tests;

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
/// units, fee assets and gross quote notional. Postcard is not self-describing, so an older file is
/// rejected with an explicit error instead of being decoded into a different shape.
const SNAPSHOT_VERSION: u32 = 9;
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
    pub fee_overflow: bool,
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
            fee_overflow: event.fee_overflow,
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
            fee_overflow: event.fee_overflow,
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
}

/// A sink/tap publication produced while applying one entry.
///
/// It is collected under the state-machine lock and published after the guard
/// is dropped, so a slow sink cannot extend the lock hold time.
struct PendingPublish {
    events: Vec<MatchEvent>,
    market: Option<(MarketDataTap, TapFrame)>,
}

impl<C: RaftTypeConfig> Clone for StateMachineStore<C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            dir: self.dir.clone(),
            sink: self.sink.clone(),
            market_data_control: self.market_data_control.clone(),
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

        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
            dir: Arc::new(dir),
            sink,
            market_data_control: Arc::new(OnceLock::new()),
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
    /// converging with replicas that can.
    fn publish_match_events(&self, events: &[MatchEvent]) {
        if events.is_empty() {
            return;
        }
        if let Err(e) = self.sink.publish(events) {
            tracing::warn!(
                sink = self.sink.name(),
                events = events.len(),
                error = %e,
                "match result sink failed; the state machine is unaffected"
            );
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
    fn publish_pending(&self, pending: Vec<PendingPublish>) {
        for publish in pending {
            self.publish_match_events(&publish.events);
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
        publishes: &mut Vec<PendingPublish>,
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

                let mut events = Vec::new();
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
                        events.extend(MatchEvent::for_place_at(
                            log_index,
                            first_ordinal + events.len() as u32,
                            &o,
                            &outcome.fills,
                            outcome.resting,
                        ));
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
                            events.extend(MatchEvent::for_fills(
                                log_index,
                                first_ordinal + events.len() as u32,
                                trigger.stop.side,
                                trigger.stop.sequence,
                                &trigger.fills,
                            ));
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
                let mut events = Vec::new();
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
                let mut events = Vec::new();
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
                let events: Vec<MatchEvent> = expired
                    .iter()
                    .enumerate()
                    .map(|(ordinal, order)| {
                        MatchEvent::for_expired(log_index, first_ordinal + ordinal as u32, order)
                    })
                    .collect();
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

            let mut publishes = Vec::new();
            let response = {
                // Lock per entry, not for the whole Apply range: reads and
                // snapshot capture can interleave between entries, and the log
                // reader is not called while the lock is held.
                let mut inner = self.inner.lock().await;
                inner.last_applied_log = Some(entry.log_id.clone());

                // Application code below stays deterministic: events are derived from the entry
                // alone, and publishing them is best-effort (a sink failure is logged, never
                // propagated) so replicas with different sinks still converge.
                match &entry.payload {
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
                }
            };

            // Sink and tap work happens outside the state-machine lock.
            self.publish_pending(publishes);

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

/// Reads and validates the latest snapshot file.
///
/// `current.snap` is replaced atomically, so the file present at startup is the
/// newest complete snapshot; a crash can leave only a `.tmp` file, which is
/// never selected.
fn read_snapshot_file<C: RaftTypeConfig>(
    dir: &Path,
) -> io::Result<Option<(SnapshotMetaOf<C>, Vec<u8>)>> {
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
    let version = u32::from_le_bytes(
        bytes[SNAPSHOT_MAGIC.len()..SNAPSHOT_MAGIC.len() + 4]
            .try_into()
            .unwrap(),
    );
    if version != SNAPSHOT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} has unsupported snapshot version {version}",
                path.display()
            ),
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
            format!("{} has a snapshot checksum mismatch", path.display()),
        ));
    }

    let payload: SnapshotPayload<C> =
        postcard::from_bytes(&bytes[SNAPSHOT_HEADER_LEN..]).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("corrupt snapshot: {e}"))
        })?;

    Ok(Some((payload.meta, payload.data)))
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
mod tests {
    use super::*;
    use crate::MatchEngineTypeConfig;
    use openraft::LogId;
    use openraft::alias::CommittedLeaderIdOf;
    use openraft::vote::RaftLeaderId;

    fn log_id(term: u64, index: u64) -> LogIdOf<MatchEngineTypeConfig> {
        LogId::new(
            CommittedLeaderIdOf::<MatchEngineTypeConfig>::new(term, 1),
            index,
        )
    }

    #[test]
    fn symbol_config_controls_precision_and_rejects_other_symbols() {
        use crate::order_book::SymbolConfig;

        let mut inner = StateMachineStoreInner::<MatchEngineTypeConfig>::default();
        assert!(
            inner
                .configure_symbol(SymbolConfig {
                    price_decimals: 19,
                    ..SymbolConfig::default()
                })
                .is_err(),
            "more than 18 decimals is invalid"
        );
        inner
            .configure_symbol(SymbolConfig::new(0, 2, 2).unwrap())
            .unwrap();

        let mut too_precise = Order::new(
            crate::order_book::OrderSide::Buy,
            "100.255".parse::<Fixed>().unwrap(),
            "1.5".parse::<Fixed>().unwrap(),
            0,
        );
        let outcome = inner
            .orderbook
            .execute_order_with_symbol(&mut too_precise, inner.symbol_config());
        assert_eq!(Some(RejectReason::InvalidOrder), outcome.rejected);

        let mut valid = Order::new(
            crate::order_book::OrderSide::Buy,
            "100.25".parse::<Fixed>().unwrap(),
            "1.5".parse::<Fixed>().unwrap(),
            0,
        );
        let outcome = inner
            .orderbook
            .execute_order_with_symbol(&mut valid, inner.symbol_config());
        assert_eq!(None, outcome.rejected);
        assert_eq!(1, inner.orderbook.bids.len());

        // The configured symbol precision cannot change after trading starts.
        assert!(
            inner
                .configure_symbol(SymbolConfig::new(0, 3, 3).unwrap())
                .is_err()
        );

        // A process configured for symbol 0 rejects an order routed to symbol 1.
        let mut other = Order::new(
            crate::order_book::OrderSide::Buy,
            "100.25".parse::<Fixed>().unwrap(),
            "1.5".parse::<Fixed>().unwrap(),
            0,
        )
        .with_symbol(1);
        let outcome = inner
            .orderbook
            .execute_order_with_symbol(&mut other, inner.symbol_config());
        assert_eq!(Some(RejectReason::UnsupportedOrder), outcome.rejected);

        // A fresh process can be provisioned for a different symbol before trading.
        let mut other_process = StateMachineStoreInner::<MatchEngineTypeConfig>::default();
        other_process
            .configure_symbol(SymbolConfig::new(7, 2, 2).unwrap())
            .unwrap();
        let mut symbol_seven = Order::new(
            crate::order_book::OrderSide::Buy,
            "100.25".parse::<Fixed>().unwrap(),
            "1.5".parse::<Fixed>().unwrap(),
            0,
        )
        .with_symbol(7);
        let outcome = other_process
            .orderbook
            .execute_order_with_symbol(&mut symbol_seven, other_process.symbol_config());
        assert_eq!(None, outcome.rejected);
        assert_eq!(7, other_process.symbol_config().symbol_id);
    }

    #[tokio::test]
    async fn symbol_configuration_resets_a_publishing_feed() {
        use crate::order_book::SymbolConfig;
        use std::sync::atomic::AtomicBool;

        let dir = tempdir();
        let store = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
        let leader = Arc::new(AtomicBool::new(true));
        let resetting = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(std::sync::Mutex::new(None));
        let control = MarketDataControl::new(
            leader,
            true,
            resetting,
            pending,
            Arc::new(tokio::sync::Notify::new()),
        );
        store.set_market_data_control(control.clone(), 5);

        let mut inner = store.inner().lock().await;
        store
            .configure_symbol_locked(&mut inner, SymbolConfig::new(7, 2, 2).unwrap())
            .unwrap();
        let snapshot = control.take_pending().expect("a symbol reset was queued");
        assert_eq!(7, snapshot.symbol_id);
    }

    #[tokio::test]
    async fn market_data_snapshot_uses_configured_symbol() {
        use crate::order_book::SymbolConfig;

        let dir = tempdir();
        let store = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
        {
            let mut inner = store.inner().lock().await;
            inner
                .configure_symbol(SymbolConfig::new(7, 2, 2).unwrap())
                .unwrap();
        }
        let snapshot = store.capture_market_data_snapshot(5).await;
        assert_eq!(7, snapshot.symbol_id);
    }

    #[tokio::test]
    async fn snapshot_survives_reopen() {
        let dir = tempdir();

        let store = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
        {
            let mut inner = store.inner().lock().await;
            inner.orderbook.place_order(&mut Order::new(
                crate::order_book::OrderSide::Buy,
                10.0,
                2.0,
                0,
            ));
            inner.last_applied_log = Some(log_id(1, 3));
            inner.last_membership = StoredMembership::new(Some(log_id(1, 1)), Default::default());
        }
        store.persist_snapshot().await.unwrap();

        let reopened = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
        let content = reopened.to_content().await;
        assert_eq!(1, content.orders.len());
        assert_eq!(1, content.sequence);
    }

    #[tokio::test]
    async fn corrupt_snapshot_is_an_error() {
        let dir = tempdir();
        std::fs::write(dir.path().join(SNAPSHOT_FILE), b"not a snapshot").unwrap();

        let err = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap_err();
        assert_eq!(io::ErrorKind::InvalidData, err.kind());
    }

    /// Applies real entries so the sink wiring is covered, not just event construction.
    #[tokio::test]
    async fn apply_publishes_rests_fills_and_cancels_to_the_sink() {
        use crate::match_result::MatchEventKind;
        use crate::match_result::MemorySink;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use openraft::alias::EntryOf;
        use openraft::alias::EntryPayloadOf;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        let dir = tempdir();
        let sink = Arc::new(MemorySink::new());
        let mut store =
            StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
                .unwrap();

        // A sell of 5 rests, then a buy of 8 crosses it: 5 fills and 3 rest as a bid.
        let entries = futures::stream::iter(vec![
            Ok((
                entry(
                    1,
                    EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                        OrderSide::Sell,
                        100.0,
                        5.0,
                        0,
                    ))),
                ),
                None,
            )),
            Ok((
                entry(
                    2,
                    EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                        OrderSide::Buy,
                        100.0,
                        8.0,
                        0,
                    ))),
                ),
                None,
            )),
        ]);
        store.apply(entries).await.unwrap();

        let events = sink.events();
        assert_eq!(3, events.len(), "expected rested, fill, rested: {events:?}");

        assert_eq!(MatchEventKind::Rested, events[0].kind);
        assert_eq!(0, events[0].order_sequence);
        assert_eq!(5.0, events[0].volume);
        assert_eq!(1, events[0].event_id.log_index);

        assert_eq!(MatchEventKind::Fill, events[1].kind);
        assert_eq!(1, events[1].order_sequence);
        assert_eq!(Some(0), events[1].counterparty_sequence);
        assert_eq!(100.0, events[1].price);
        assert_eq!(5.0, events[1].volume);
        assert_eq!(2, events[1].event_id.log_index);
        assert_eq!(0, events[1].event_id.ordinal);

        assert_eq!(MatchEventKind::Rested, events[2].kind);
        assert_eq!(1, events[2].order_sequence);
        assert_eq!(3.0, events[2].volume);
        assert_eq!(2, events[2].event_id.log_index);
        assert_eq!(1, events[2].event_id.ordinal);

        // Cancelling the 3 that rested reports the remaining volume from the book.
        let entries = futures::stream::iter(vec![Ok((
            entry(
                3,
                EntryPayload::Normal(MatchEngineRequest::Cancel {
                    order: Order::new(OrderSide::Buy, 100.0, 3.0, 1),
                }),
            ),
            None,
        ))]);
        store.apply(entries).await.unwrap();

        let events = sink.events();
        assert_eq!(4, events.len());
        let cancelled = &events[3];
        assert_eq!(MatchEventKind::Cancelled, cancelled.kind);
        assert_eq!(1, cancelled.order_sequence);
        assert_eq!(3.0, cancelled.volume);
        assert_eq!(3, cancelled.event_id.log_index);

        // A cancel of something that is not in the book publishes nothing.
        let entries = futures::stream::iter(vec![Ok((
            entry(
                4,
                EntryPayload::Normal(MatchEngineRequest::Cancel {
                    order: Order::new(OrderSide::Buy, 100.0, 3.0, 1),
                }),
            ),
            None,
        ))]);
        store.apply(entries).await.unwrap();
        assert_eq!(4, sink.len());
    }

    /// A batch applies every sub-request in one entry, with continuous event
    /// ordinals and one response per item.
    #[tokio::test]
    async fn apply_batch_preserves_sequences_and_event_ordinals() {
        use crate::match_result::MatchEventKind;
        use crate::match_result::MemorySink;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use openraft::alias::EntryOf;
        use openraft::alias::EntryPayloadOf;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        let dir = tempdir();
        let sink = Arc::new(MemorySink::new());
        let mut store =
            StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
                .unwrap();
        let batch = MatchEngineRequest::Batch {
            requests: vec![
                MatchEngineRequest::place(Order::new(OrderSide::Buy, 100.0, 1.0, 0)),
                MatchEngineRequest::place(Order::new(OrderSide::Buy, 99.0, 2.0, 0)),
            ],
        };
        let entries =
            futures::stream::iter(vec![Ok((entry(1, EntryPayload::Normal(batch)), None))]);
        store.apply(entries).await.unwrap();

        let events = sink.events();
        assert_eq!(2, events.len(), "one event per resting order: {events:?}");
        assert_eq!(MatchEventKind::Rested, events[0].kind);
        assert_eq!(0, events[0].event_id.ordinal);
        assert_eq!(MatchEventKind::Rested, events[1].kind);
        assert_eq!(1, events[1].event_id.ordinal);
        assert_eq!(events[0].event_id.log_index, events[1].event_id.log_index);

        let content = store.to_content().await;
        assert_eq!(2, content.orders.len());
        assert_eq!(2, content.sequence);
        assert_eq!(2, content.events.len());
    }

    /// Event ordinals continue across mixed sub-requests in one batch entry.
    #[tokio::test]
    async fn batch_ordinals_continue_across_mixed_requests() {
        use crate::match_result::MatchEventKind;
        use crate::match_result::MemorySink;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use openraft::alias::EntryOf;
        use openraft::alias::EntryPayloadOf;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        let dir = tempdir();
        let sink = Arc::new(MemorySink::new());
        let mut store =
            StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
                .unwrap();
        let batch = MatchEngineRequest::Batch {
            requests: vec![
                MatchEngineRequest::place(Order::new(OrderSide::Buy, 100.0, 1.0, 0)),
                MatchEngineRequest::Cancel {
                    order: Order::new(OrderSide::Buy, 100.0, 1.0, 0),
                },
                MatchEngineRequest::Amend {
                    order: Order::new(OrderSide::Buy, 100.0, 1.0, 99),
                    price: Some(101.0.into()),
                    volume: None,
                },
            ],
        };
        let entries =
            futures::stream::iter(vec![Ok((entry(1, EntryPayload::Normal(batch)), None))]);
        store.apply(entries).await.unwrap();

        let events = sink.events();
        assert_eq!(
            3,
            events.len(),
            "rest, cancel and rejected amend: {events:?}"
        );
        assert_eq!(MatchEventKind::Rested, events[0].kind);
        assert_eq!(0, events[0].event_id.ordinal);
        assert_eq!(MatchEventKind::Cancelled, events[1].kind);
        assert_eq!(1, events[1].event_id.ordinal);
        assert_eq!(MatchEventKind::Rejected, events[2].kind);
        assert_eq!(2, events[2].event_id.ordinal);
        assert!(
            events.iter().all(|event| event.event_id.log_index == 1),
            "one entry, one log index"
        );

        let content = store.to_content().await;
        assert_eq!(3, content.sequence, "place + cancel + amend market seqs");
        assert!(content.orders.is_empty(), "the cancel removed the order");
        assert_eq!(3, content.events.len());
    }

    /// A retried batch item recovers only its own events, even though every
    /// sub-request in the batch shares one log index.
    #[tokio::test]
    async fn batch_duplicate_returns_only_its_own_events() {
        use crate::match_result::MatchEventKind;
        use crate::match_result::MemorySink;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use openraft::alias::EntryOf;
        use openraft::alias::EntryPayloadOf;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        let dir = tempdir();
        let sink = Arc::new(MemorySink::new());
        let mut store =
            StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
                .unwrap();
        let batch = MatchEngineRequest::Batch {
            requests: vec![
                MatchEngineRequest::place_with_client_id(
                    Order::new(OrderSide::Buy, 100.0, 1.0, 0),
                    "first",
                ),
                MatchEngineRequest::place_with_client_id(
                    Order::new(OrderSide::Buy, 99.0, 1.0, 0),
                    "second",
                ),
            ],
        };
        let entries =
            futures::stream::iter(vec![Ok((entry(1, EntryPayload::Normal(batch)), None))]);
        store.apply(entries).await.unwrap();
        assert_eq!(2, sink.events().len());
        assert_eq!(MatchEventKind::Rested, sink.events()[1].kind);

        let inner = store.inner().lock().await;
        let second = inner
            .submissions
            .get("second")
            .expect("second submission recorded")
            .clone();
        let duplicate = inner.duplicate_response(&second);
        assert_eq!(Some(1), duplicate.sequence);
        assert_eq!(1, duplicate.events.len(), "only the second place's event");
        assert_eq!(1, duplicate.events[0].event_id.ordinal);
        assert_eq!(1, duplicate.events[0].order_sequence);
        assert!(!duplicate.events_truncated);

        let first = inner
            .submissions
            .get("first")
            .expect("first submission recorded")
            .clone();
        let duplicate = inner.duplicate_response(&first);
        assert_eq!(Some(0), duplicate.sequence);
        assert_eq!(1, duplicate.events.len(), "only the first place's event");
        assert_eq!(0, duplicate.events[0].event_id.ordinal);
    }

    /// A retry whose events were evicted still returns the original sequence,
    /// with an empty, explicitly truncated event list.
    #[tokio::test]
    async fn duplicate_response_reports_evicted_events() {
        use crate::match_result::MemorySink;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use openraft::alias::EntryOf;
        use openraft::alias::EntryPayloadOf;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        let dir = tempdir();
        let sink = Arc::new(MemorySink::new());
        let mut store = StateMachineStore::<MatchEngineTypeConfig>::open_with_limits(
            dir.path(),
            sink,
            None,
            1,
            5,
        )
        .await
        .unwrap();
        let entries = futures::stream::iter(vec![
            Ok((
                entry(
                    1,
                    EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
                        Order::new(OrderSide::Buy, 100.0, 1.0, 0),
                        "a",
                    )),
                ),
                None,
            )),
            Ok((
                entry(
                    2,
                    EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
                        Order::new(OrderSide::Buy, 99.0, 1.0, 0),
                        "b",
                    )),
                ),
                None,
            )),
            Ok((
                entry(
                    3,
                    EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
                        Order::new(OrderSide::Buy, 98.0, 1.0, 0),
                        "c",
                    )),
                ),
                None,
            )),
        ]);
        store.apply(entries).await.unwrap();

        let inner = store.inner().lock().await;
        let record = inner
            .submissions
            .get("a")
            .expect("the submission is still retained")
            .clone();
        let duplicate = inner.duplicate_response(&record);
        assert_eq!(Some(0), duplicate.sequence);
        assert!(duplicate.events.is_empty(), "the event window dropped it");
        assert!(duplicate.events_truncated);
    }

    /// A failing sink must not fail `apply` or change the resulting state.
    #[tokio::test]
    async fn sink_failure_does_not_fail_apply() {
        use crate::match_result::MatchResultSink;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use openraft::alias::EntryOf;

        #[derive(Debug)]
        struct FailingSink;

        impl MatchResultSink for FailingSink {
            fn name(&self) -> &'static str {
                "failing"
            }

            fn publish(&self, _events: &[crate::match_result::MatchEvent]) -> io::Result<()> {
                Err(io::Error::other("sink is down"))
            }
        }

        let dir = tempdir();
        let mut store = StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(
            dir.path(),
            Arc::new(FailingSink),
        )
        .unwrap();

        let entry = EntryOf::<MatchEngineTypeConfig> {
            log_id: log_id(1, 1),
            payload: EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                100.0,
                1.0,
                0,
            ))),
        };
        let entries = futures::stream::iter(vec![Ok((entry, None))]);

        store.apply(entries).await.unwrap();

        let inner = store.inner().lock().await;
        assert_eq!(1.0, inner.orderbook.best_bid().unwrap().volume);
        assert_eq!(Some(log_id(1, 1)), inner.last_applied_log);
    }

    /// The time-in-force and stop paths publish their own event kinds, and one
    /// entry's events (a stop cascade) keep increasing ordinals.
    #[tokio::test]
    async fn apply_publishes_ioc_stop_and_expiry_events() {
        use crate::match_result::MatchEventKind;
        use crate::match_result::MemorySink;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use crate::order_book::OrderType;
        use crate::order_book::TimeInForce;
        use openraft::alias::EntryOf;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        let dir = tempdir();
        let sink = Arc::new(MemorySink::new());
        let mut store =
            StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
                .unwrap();

        let mut gtd = Order::new(OrderSide::Buy, 90.0, 1.0, 0);
        gtd.tif = TimeInForce::Gtd(2_000);
        gtd.timestamp = 1_000;

        let mut stop = Order::new(OrderSide::Sell, 0.0, 5.0, 0);
        stop.live = OrderType::Market;
        stop.stop_price = 95.0.into();

        let entries = futures::stream::iter(vec![
            Ok((
                entry(1, EntryPayload::Normal(MatchEngineRequest::place(gtd))),
                None,
            )),
            Ok((
                entry(
                    2,
                    EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                        OrderSide::Buy,
                        100.0,
                        10.0,
                        0,
                    ))),
                ),
                None,
            )),
            Ok((
                entry(
                    3,
                    EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                        OrderSide::Buy,
                        95.0,
                        10.0,
                        0,
                    ))),
                ),
                None,
            )),
            Ok((
                entry(4, EntryPayload::Normal(MatchEngineRequest::place(stop))),
                None,
            )),
            Ok((
                entry(
                    5,
                    EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                        OrderSide::Sell,
                        100.0,
                        10.0,
                        0,
                    ))),
                ),
                None,
            )),
            Ok((
                entry(
                    6,
                    EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                        OrderSide::Sell,
                        95.0,
                        3.0,
                        0,
                    ))),
                ),
                None,
            )),
            Ok((
                entry(
                    7,
                    EntryPayload::Normal(MatchEngineRequest::PurgeExpired {
                        now_ms: 2_000,
                        day_close_ms: None,
                    }),
                ),
                None,
            )),
        ]);
        store.apply(entries).await.unwrap();

        let events = sink.events();
        // Rests for the GTD order and the two bids; a fill for entry 5; then the
        // elected stop's trigger + child fill; finally the expiry.
        assert_eq!(8, events.len(), "unexpected events: {events:?}");

        assert_eq!(MatchEventKind::Rested, events[0].kind);
        assert_eq!(MatchEventKind::Fill, events[3].kind);
        assert_eq!(10.0, events[3].volume);

        let triggered = &events[5];
        assert_eq!(MatchEventKind::Triggered, triggered.kind);
        assert_eq!(3, triggered.order_sequence, "the stop's sequence");
        assert_eq!(95.0, triggered.price, "the trade that elected it");
        assert_eq!(5.0, triggered.volume);
        assert_eq!(
            [6, 1],
            [
                triggered.event_id.log_index,
                u64::from(triggered.event_id.ordinal)
            ]
        );

        let child_fill = &events[6];
        assert_eq!(MatchEventKind::Fill, child_fill.kind);
        assert_eq!(
            3, child_fill.order_sequence,
            "child fills belong to the stop"
        );
        assert_eq!(5.0, child_fill.volume);
        assert_eq!(2, child_fill.event_id.ordinal, "same entry, next ordinal");

        let expired = &events[7];
        assert_eq!(MatchEventKind::Expired, expired.kind);
        assert_eq!(0, expired.order_sequence, "the GTD order");
        assert_eq!(1.0, expired.volume);
        assert_eq!(7, expired.event_id.log_index);
    }

    /// Market, IOC, FOK, post-only and iceberg paths all emit the same
    /// match-result fields as plain limit orders.
    #[tokio::test]
    async fn apply_publishes_market_ioc_fok_post_only_and_iceberg_events() {
        use crate::match_result::MatchEventKind;
        use crate::match_result::MemorySink;
        use crate::order_book::FeeAsset;
        use crate::order_book::Fixed;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use crate::order_book::OrderType;
        use crate::order_book::RejectReason;
        use crate::order_book::TimeInForce;
        use openraft::alias::EntryOf;
        use openraft::alias::EntryPayloadOf;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        fn rate(value: &str) -> Fixed {
            value.parse().expect("valid fixed-point rate")
        }

        let dir = tempdir();
        let sink = Arc::new(MemorySink::new());
        let mut store =
            StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
                .unwrap();

        let maker =
            Order::new(OrderSide::Sell, 100.0, 5.0, 0).with_fee_rates(rate("0.001"), rate("0.002"));
        let mut market =
            Order::new(OrderSide::Buy, 0.0, 8.0, 0).with_fee_rates(rate("0.003"), rate("0.004"));
        market.live = OrderType::Market;
        let mut ioc = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
        ioc.tif = TimeInForce::Ioc;
        let mut fok = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
        fok.tif = TimeInForce::Fok;
        let ask = Order::new(OrderSide::Sell, 100.0, 5.0, 0);
        let mut post_cross = Order::new(OrderSide::Buy, 100.0, 4.0, 0);
        post_cross.post_only = true;
        let mut post_rest = Order::new(OrderSide::Buy, 99.0, 4.0, 0);
        post_rest.post_only = true;
        let mut iceberg = Order::new(OrderSide::Sell, 101.0, 10.0, 0)
            .with_fee_rates(rate("0.001"), rate("0.002"));
        iceberg.display_volume = rate("4");
        let mut sweep =
            Order::new(OrderSide::Buy, 0.0, 6.0, 0).with_fee_rates(rate("0.003"), rate("0.004"));
        sweep.live = OrderType::Market;

        let entries = futures::stream::iter(vec![
            Ok((
                entry(1, EntryPayload::Normal(MatchEngineRequest::place(maker))),
                None,
            )),
            Ok((
                entry(2, EntryPayload::Normal(MatchEngineRequest::place(market))),
                None,
            )),
            Ok((
                entry(3, EntryPayload::Normal(MatchEngineRequest::place(ioc))),
                None,
            )),
            Ok((
                entry(4, EntryPayload::Normal(MatchEngineRequest::place(fok))),
                None,
            )),
            Ok((
                entry(5, EntryPayload::Normal(MatchEngineRequest::place(ask))),
                None,
            )),
            Ok((
                entry(
                    6,
                    EntryPayload::Normal(MatchEngineRequest::place(post_cross)),
                ),
                None,
            )),
            Ok((
                entry(
                    7,
                    EntryPayload::Normal(MatchEngineRequest::Cancel {
                        order: Order::new(OrderSide::Sell, 100.0, 5.0, 4),
                    }),
                ),
                None,
            )),
            Ok((
                entry(
                    8,
                    EntryPayload::Normal(MatchEngineRequest::place(post_rest)),
                ),
                None,
            )),
            Ok((
                entry(9, EntryPayload::Normal(MatchEngineRequest::place(iceberg))),
                None,
            )),
            Ok((
                entry(10, EntryPayload::Normal(MatchEngineRequest::place(sweep))),
                None,
            )),
        ]);
        store.apply(entries).await.unwrap();

        let events = sink.events();
        assert_eq!(12, events.len(), "unexpected events: {events:?}");

        assert_eq!(MatchEventKind::Rested, events[0].kind);
        assert_eq!(0, events[0].order_sequence);

        let market_fill = &events[1];
        assert_eq!(MatchEventKind::Fill, market_fill.kind);
        assert_eq!(1, market_fill.order_sequence);
        assert_eq!(Some(0), market_fill.counterparty_sequence);
        assert_eq!(Fixed::from(5), market_fill.volume);
        assert_eq!(Fixed::from(500), market_fill.quote_volume);
        assert_eq!(Fixed::from(0.02), market_fill.taker_fee);
        assert_eq!(Some(FeeAsset::Base), market_fill.taker_fee_asset);
        assert_eq!(Fixed::from(0.5), market_fill.maker_fee);
        assert_eq!(Some(FeeAsset::Quote), market_fill.maker_fee_asset);

        assert_eq!(MatchEventKind::Cancelled, events[2].kind);
        assert_eq!(1, events[2].order_sequence);
        assert_eq!(Fixed::from(3), events[2].volume);

        assert_eq!(MatchEventKind::Cancelled, events[3].kind);
        assert_eq!(2, events[3].order_sequence);
        assert_eq!(Fixed::from(5), events[3].volume);

        assert_eq!(MatchEventKind::Rejected, events[4].kind);
        assert_eq!(3, events[4].order_sequence);
        assert_eq!(Some(RejectReason::FokUnfillable), events[4].reason);

        assert_eq!(MatchEventKind::Rested, events[5].kind);
        assert_eq!(4, events[5].order_sequence);

        assert_eq!(MatchEventKind::Rejected, events[6].kind);
        assert_eq!(5, events[6].order_sequence);
        assert_eq!(Some(RejectReason::PostOnlyWouldCross), events[6].reason);

        assert_eq!(MatchEventKind::Cancelled, events[7].kind);
        assert_eq!(4, events[7].order_sequence);
        assert_eq!(Fixed::from(5), events[7].volume);

        assert_eq!(MatchEventKind::Rested, events[8].kind);
        assert_eq!(7, events[8].order_sequence);
        assert_eq!(Fixed::from(4), events[8].volume);

        assert_eq!(MatchEventKind::Rested, events[9].kind);
        assert_eq!(8, events[9].order_sequence);
        assert_eq!(Fixed::from(4), events[9].volume, "only the visible tranche");

        let first_iceberg_fill = &events[10];
        assert_eq!(MatchEventKind::Fill, first_iceberg_fill.kind);
        assert_eq!(9, first_iceberg_fill.order_sequence);
        assert_eq!(Some(8), first_iceberg_fill.counterparty_sequence);
        assert_eq!(Fixed::from(4), first_iceberg_fill.volume);
        assert_eq!(Fixed::from(404), first_iceberg_fill.quote_volume);
        assert_eq!(Fixed::from(0.016), first_iceberg_fill.taker_fee);
        assert_eq!(Fixed::from(0.404), first_iceberg_fill.maker_fee);
        assert_eq!(0, first_iceberg_fill.event_id.ordinal);

        let second_iceberg_fill = &events[11];
        assert_eq!(MatchEventKind::Fill, second_iceberg_fill.kind);
        assert_eq!(Some(8), second_iceberg_fill.counterparty_sequence);
        assert_eq!(Fixed::from(2), second_iceberg_fill.volume);
        assert_eq!(Fixed::from(202), second_iceberg_fill.quote_volume);
        assert_eq!(Fixed::from(0.008), second_iceberg_fill.taker_fee);
        assert_eq!(Fixed::from(0.202), second_iceberg_fill.maker_fee);
        assert_eq!(1, second_iceberg_fill.event_id.ordinal);
    }

    /// A stop child that cannot fully execute emits the trigger, its fills and
    /// the cancelled remainder in one entry, all tied to the stop's identity.
    #[tokio::test]
    async fn apply_publishes_stop_trigger_with_cancelled_child() {
        use crate::match_result::MatchEventKind;
        use crate::match_result::MemorySink;
        use crate::order_book::FeeAsset;
        use crate::order_book::Fixed;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use crate::order_book::OrderType;
        use openraft::alias::EntryOf;
        use openraft::alias::EntryPayloadOf;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        fn rate(value: &str) -> Fixed {
            value.parse().expect("valid fixed-point rate")
        }

        let dir = tempdir();
        let sink = Arc::new(MemorySink::new());
        let mut store =
            StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
                .unwrap();

        let bid =
            Order::new(OrderSide::Buy, 95.0, 3.0, 0).with_fee_rates(rate("0.001"), rate("0.002"));
        let mut stop =
            Order::new(OrderSide::Sell, 0.0, 5.0, 0).with_fee_rates(rate("0.003"), rate("0.004"));
        stop.live = OrderType::Market;
        stop.stop_price = rate("95");
        let trigger =
            Order::new(OrderSide::Sell, 95.0, 1.0, 0).with_fee_rates(rate("0.003"), rate("0.004"));

        let entries = futures::stream::iter(vec![
            Ok((
                entry(1, EntryPayload::Normal(MatchEngineRequest::place(bid))),
                None,
            )),
            Ok((
                entry(2, EntryPayload::Normal(MatchEngineRequest::place(stop))),
                None,
            )),
            Ok((
                entry(3, EntryPayload::Normal(MatchEngineRequest::place(trigger))),
                None,
            )),
        ]);
        store.apply(entries).await.unwrap();

        let events = sink.events();
        assert_eq!(5, events.len(), "unexpected events: {events:?}");
        assert_eq!(MatchEventKind::Rested, events[0].kind);
        assert_eq!(MatchEventKind::Fill, events[1].kind);
        assert_eq!(Fixed::from(1), events[1].volume);
        assert_eq!(Fixed::from(0.38), events[1].taker_fee);
        assert_eq!(Some(FeeAsset::Quote), events[1].taker_fee_asset);
        assert_eq!(Fixed::from(0.001), events[1].maker_fee);
        assert_eq!(Some(FeeAsset::Base), events[1].maker_fee_asset);

        assert_eq!(MatchEventKind::Triggered, events[2].kind);
        assert_eq!(1, events[2].order_sequence);
        assert_eq!(Fixed::from(95), events[2].price);
        assert_eq!(Fixed::from(5), events[2].volume);

        assert_eq!(MatchEventKind::Fill, events[3].kind);
        assert_eq!(1, events[3].order_sequence);
        assert_eq!(Fixed::from(2), events[3].volume);
        assert_eq!(Fixed::from(190), events[3].quote_volume);
        assert_eq!(Fixed::from(0.76), events[3].taker_fee);
        assert_eq!(Some(FeeAsset::Quote), events[3].taker_fee_asset);
        assert_eq!(Fixed::from(0.002), events[3].maker_fee);
        assert_eq!(Some(FeeAsset::Base), events[3].maker_fee_asset);

        assert_eq!(MatchEventKind::Cancelled, events[4].kind);
        assert_eq!(1, events[4].order_sequence);
        assert_eq!(Fixed::from(3), events[4].volume);
        assert_eq!(3, events[4].event_id.ordinal);
    }

    /// A later request's intake timestamp purges an expired GTD order before
    /// matching; the expiry is published before the new order's events.
    #[tokio::test]
    async fn apply_publishes_automatic_gtd_expiry_before_matching() {
        use crate::match_result::MatchEventKind;
        use crate::match_result::MemorySink;
        use crate::order_book::Fixed;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use crate::order_book::TimeInForce;
        use openraft::alias::EntryOf;
        use openraft::alias::EntryPayloadOf;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        let dir = tempdir();
        let sink = Arc::new(MemorySink::new());
        let mut store =
            StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
                .unwrap();

        let mut gtd = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
        gtd.tif = TimeInForce::Gtd(2_000);
        gtd.timestamp = 1_000;
        let mut taker = Order::new(OrderSide::Sell, 100.0, 1.0, 0);
        taker.timestamp = 2_000;

        let entries = futures::stream::iter(vec![
            Ok((
                entry(1, EntryPayload::Normal(MatchEngineRequest::place(gtd))),
                None,
            )),
            Ok((
                entry(2, EntryPayload::Normal(MatchEngineRequest::place(taker))),
                None,
            )),
        ]);
        store.apply(entries).await.unwrap();

        let events = sink.events();
        assert_eq!(3, events.len(), "unexpected events: {events:?}");
        assert_eq!(MatchEventKind::Rested, events[0].kind);
        assert_eq!(MatchEventKind::Expired, events[1].kind);
        assert_eq!(0, events[1].order_sequence);
        assert_eq!(Fixed::from(5), events[1].volume);
        assert_eq!(MatchEventKind::Rested, events[2].kind);
        assert_eq!(1, events[2].order_sequence);
        assert_eq!(Fixed::from(1), events[2].volume);
    }

    /// Amendment goes through `apply`: the event carries both identities and the
    /// book keeps the new size; a failed amend publishes a rejection.
    #[tokio::test]
    async fn apply_publishes_amended_and_rejected_events() {
        use crate::match_result::MatchEventKind;
        use crate::match_result::MemorySink;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use crate::order_book::RejectReason;
        use openraft::alias::EntryOf;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        let dir = tempdir();
        let sink = Arc::new(MemorySink::new());
        let mut store =
            StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
                .unwrap();

        // The key of an order uses side + price + sequence; `Order::new` sets the
        // sequence, and the taker flow assigns the same one on apply.
        let bid = Order::new(OrderSide::Buy, 100.0, 10.0, 0);
        let entries = futures::stream::iter(vec![
            Ok((
                entry(1, EntryPayload::Normal(MatchEngineRequest::place(bid))),
                None,
            )),
            Ok((
                entry(
                    2,
                    EntryPayload::Normal(MatchEngineRequest::Amend {
                        order: bid,
                        price: None,
                        volume: Some(12.0.into()),
                    }),
                ),
                None,
            )),
            Ok((
                entry(
                    3,
                    EntryPayload::Normal(MatchEngineRequest::Amend {
                        order: Order::new(OrderSide::Buy, 100.0, 1.0, 42),
                        price: Some(99.0.into()),
                        volume: None,
                    }),
                ),
                None,
            )),
        ]);
        store.apply(entries).await.unwrap();

        let events = sink.events();
        assert_eq!(3, events.len(), "unexpected events: {events:?}");

        assert_eq!(MatchEventKind::Rested, events[0].kind);

        let amended = &events[1];
        assert_eq!(MatchEventKind::Amended, amended.kind);
        assert_eq!(
            1, amended.order_sequence,
            "the amendment takes a new sequence"
        );
        assert_eq!(Some(0), amended.previous_sequence);
        assert_eq!(100.0, amended.price);
        assert_eq!(12.0, amended.volume);

        let rejected = &events[2];
        assert_eq!(MatchEventKind::Rejected, rejected.kind);
        assert_eq!(Some(RejectReason::OrderNotFound), rejected.reason);

        let inner = store.inner().lock().await;
        let best = inner.orderbook.best_bid().expect("the bid remains");
        assert_eq!(12.0, best.volume);
        assert_eq!(1, best.sequence);
        assert_eq!(
            3, inner.orderbook.sequence,
            "the failed amend consumed a sequence"
        );
    }

    /// `apply` hands events and the depth journal to the market-data tap with
    /// the request's intake timestamp; the snapshot capture reads the book.
    #[tokio::test]
    async fn apply_publishes_market_data_frames_and_captures_snapshots() {
        use crate::market_data::Ingest;
        use crate::market_data::MarketDataTap;
        use crate::match_result::MatchEventKind;
        use crate::match_result::NoopSink;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use openraft::alias::EntryOf;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        async fn next_frame(
            receiver: &mut tokio::sync::mpsc::Receiver<Ingest>,
        ) -> crate::market_data::TapFrame {
            match receiver.recv().await {
                Some(Ingest::Frame(frame)) => frame,
                other => panic!("expected a frame, got {other:?}"),
            }
        }

        let dir = tempdir();
        let (tap, mut channels) = MarketDataTap::channel(16);
        let mut store = StateMachineStore::<MatchEngineTypeConfig>::open_with_sinks(
            dir.path(),
            Arc::new(NoopSink),
            Some(tap),
        )
        .unwrap();

        let mut bid = Order::new(OrderSide::Buy, 100.0, 10.0, 0);
        bid.timestamp = 1_234;
        let entries = futures::stream::iter(vec![Ok((
            entry(1, EntryPayload::Normal(MatchEngineRequest::place(bid))),
            None,
        ))]);
        store.apply(entries).await.unwrap();

        let frame = next_frame(&mut channels.receiver).await;
        assert_eq!(1, frame.log_index);
        assert_eq!(0, frame.market_seq, "the bid's order sequence");
        assert_eq!(1_234, frame.ts_ms);
        assert_eq!(0, frame.symbol_id);
        assert_eq!(1, frame.events.len());
        assert_eq!(MatchEventKind::Rested, frame.events[0].kind);
        assert_eq!(1, frame.depth.len());
        assert_eq!(10.0, frame.depth[0].delta_volume);
        assert_eq!(1, frame.depth[0].delta_orders);

        // A fill carries both its event and the depth change.
        let mut ask = Order::new(OrderSide::Sell, 100.0, 4.0, 0);
        ask.timestamp = 2_000;
        let entries = futures::stream::iter(vec![Ok((
            entry(2, EntryPayload::Normal(MatchEngineRequest::place(ask))),
            None,
        ))]);
        store.apply(entries).await.unwrap();

        let frame = next_frame(&mut channels.receiver).await;
        assert_eq!(2, frame.log_index);
        assert_eq!(1, frame.market_seq, "the ask's order sequence");
        assert_eq!(2_000, frame.ts_ms);
        assert!(
            frame
                .events
                .iter()
                .any(|event| event.kind == MatchEventKind::Fill)
        );
        assert!(frame.depth.iter().any(|change| change.delta_volume < 0.0));
        assert_eq!(
            2,
            channels
                .stats
                .frames
                .load(std::sync::atomic::Ordering::Relaxed)
        );

        let snapshot = store.capture_market_data_snapshot(5).await;
        assert_eq!(2, snapshot.log_index);
        assert_eq!(2, snapshot.market_seq, "the next sequence is the boundary");
        assert_eq!(1, snapshot.bids.len(), "the bid remainder is captured");
        assert_eq!(6.0, snapshot.bids[0].volume);
        assert_eq!(Some(100.0.into()), snapshot.last_trade);

        let status = store
            .inner()
            .lock()
            .await
            .market_data
            .as_ref()
            .expect("tap installed")
            .status_json();
        let status: serde_json::Value = serde_json::from_str(&status).expect("status json");
        assert_eq!(2, status["frames"]);
        assert_eq!(0, status["dropped_frames"]);
    }

    /// A takeover snapshots and enqueues first, and only then opens the tap
    /// gate; a follower's frames never queue ahead of the reset.
    #[tokio::test]
    async fn market_data_feed_opens_only_after_the_snapshot_is_queued() {
        use crate::market_data::Ingest;
        use crate::market_data::MarketDataControl;
        use crate::market_data::MarketDataTap;
        use crate::match_result::NoopSink;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use openraft::alias::EntryOf;
        use std::sync::Mutex;
        use std::sync::atomic::Ordering;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        let dir = tempdir();
        let (tap, mut channels) = MarketDataTap::channel(16);
        // A follower: the tap gate is closed, so this entry never queues a frame.
        channels.enabled.store(false, Ordering::Relaxed);
        let mut store = StateMachineStore::<MatchEngineTypeConfig>::open_with_sinks(
            dir.path(),
            Arc::new(NoopSink),
            Some(tap.clone()),
        )
        .unwrap();

        let mut bid = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
        bid.timestamp = 1_000;
        let entries = futures::stream::iter(vec![Ok((
            entry(1, EntryPayload::Normal(MatchEngineRequest::place(bid))),
            None,
        ))]);
        store.apply(entries).await.unwrap();

        let control = MarketDataControl::new(
            channels.enabled.clone(),
            true,
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            Arc::new(Mutex::new(None)),
            Arc::new(tokio::sync::Notify::new()),
        );
        store.start_market_data_feed(&control, 5).await;
        assert!(channels.enabled.load(Ordering::Relaxed), "gate opened");

        let snapshot = control.take_pending().expect("snapshot queued");
        assert_eq!(1, snapshot.log_index, "snapshot covers the follower entry");
        assert_eq!(
            1, snapshot.market_seq,
            "the next market sequence after the bid"
        );
        assert_eq!(1, snapshot.bids.len());
        assert_eq!(5.0, snapshot.bids[0].volume);

        let mut ask = Order::new(OrderSide::Sell, 101.0, 1.0, 0);
        ask.timestamp = 2_000;
        let entries = futures::stream::iter(vec![Ok((
            entry(2, EntryPayload::Normal(MatchEngineRequest::place(ask))),
            None,
        ))]);
        store.apply(entries).await.unwrap();

        match channels.receiver.recv().await.expect("post-snapshot frame") {
            Ingest::Frame(frame) => {
                assert_eq!(2, frame.log_index);
                assert_eq!(1, frame.market_seq);
            }
        }
    }

    /// Cancel/Amend consume a market sequence and run the automatic GTD purge,
    /// so an order due at the request's intake time cannot be cancelled or filled.
    #[tokio::test]
    async fn cancel_and_amend_consume_sequences_and_purge_due_gtd() {
        use crate::match_result::MatchEventKind;
        use crate::match_result::MemorySink;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use crate::order_book::TimeInForce;
        use openraft::alias::EntryOf;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        let dir = tempdir();
        let sink = Arc::new(MemorySink::new());
        let mut store =
            StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
                .unwrap();

        let mut gtd = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
        gtd.tif = TimeInForce::Gtd(2_000);
        gtd.timestamp = 1_000;
        let mut cancel_key = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
        cancel_key.timestamp = 2_000;
        let entries = futures::stream::iter(vec![
            Ok((
                entry(1, EntryPayload::Normal(MatchEngineRequest::place(gtd))),
                None,
            )),
            Ok((
                entry(
                    2,
                    EntryPayload::Normal(MatchEngineRequest::Cancel { order: cancel_key }),
                ),
                None,
            )),
        ]);
        store.apply(entries).await.unwrap();

        let events = sink.events();
        assert_eq!(2, events.len(), "rest then expiry: {events:?}");
        assert_eq!(MatchEventKind::Rested, events[0].kind);
        assert_eq!(MatchEventKind::Expired, events[1].kind);
        assert_eq!(5.0, events[1].volume);
        assert!(
            store.inner().lock().await.orderbook.bids.is_empty(),
            "the expired bid is gone and the cancel found nothing"
        );
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
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    /// Events are replicated, bounded, paged and deduplicated by client id.
    #[tokio::test]
    async fn event_log_retains_pages_and_dedupes_submissions() {
        use crate::match_result::MatchEventKind;
        use crate::match_result::MemorySink;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use openraft::alias::EntryOf;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        fn place(
            index: u64,
            side: OrderSide,
            price: f64,
            client_id: &str,
        ) -> EntryResponder<MatchEngineTypeConfig> {
            (
                entry(
                    index,
                    EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
                        Order::new(side, price, 1.0, 0),
                        client_id,
                    )),
                ),
                None,
            )
        }

        let dir = tempdir();
        let sink = Arc::new(MemorySink::new());
        let mut store = StateMachineStore::<MatchEngineTypeConfig>::open_with_limits(
            dir.path(),
            sink.clone(),
            None,
            3,
            3,
        )
        .await
        .unwrap();

        let entries = futures::stream::iter(vec![
            Ok(place(1, OrderSide::Buy, 100.0, "a")),
            Ok(place(2, OrderSide::Buy, 99.0, "b")),
            // Same id, different order: must not place a second order.
            Ok(place(3, OrderSide::Sell, 50.0, "a")),
        ]);
        store.apply(entries).await.unwrap();

        let content = store.to_content().await;
        assert_eq!(2, content.orders.len(), "the duplicate did not place");
        assert_eq!(
            2, content.sequence,
            "the duplicate did not consume a sequence"
        );
        assert_eq!(2, content.submissions.len());
        assert_eq!(2, content.events.len());
        assert_eq!(2, sink.events().len(), "the duplicate produced no events");

        // Two more entries push the oldest events and submission out of the window.
        let entries = futures::stream::iter(vec![
            Ok(place(4, OrderSide::Buy, 98.0, "c")),
            Ok(place(5, OrderSide::Buy, 97.0, "d")),
        ]);
        store.apply(entries).await.unwrap();

        let content = store.to_content().await;
        assert_eq!(3, content.events.len(), "window is capped");
        assert_eq!(1, content.events_evicted);
        assert_eq!(3, content.submissions.len(), "submission map is capped");
        assert_eq!(1, content.submissions_evicted);

        let inner = store.inner().lock().await;
        assert_eq!(
            Some(EventId {
                log_index: 2,
                ordinal: 0
            }),
            inner.oldest_event_id()
        );
        assert_eq!(
            Some(EventId {
                log_index: 5,
                ordinal: 0
            }),
            inner.last_event_id()
        );
        let (page, next, has_more) = inner.events_since(None, 2);
        assert_eq!(2, page.len());
        assert!(has_more);
        assert_eq!(Some(page[1].event_id), next);
        let (page2, _, has_more2) = inner.events_since(next, 2);
        assert_eq!(1, page2.len());
        assert!(!has_more2);
        assert_eq!(MatchEventKind::Rested, page2[0].kind);

        // An evicted client id is no longer deduplicated; it is a new submission.
        drop(inner);
        let entries = futures::stream::iter(vec![Ok(place(6, OrderSide::Buy, 96.0, "a"))]);
        store.apply(entries).await.unwrap();
        assert_eq!(5, store.to_content().await.orders.len());
    }

    #[tokio::test]
    async fn event_log_and_dedupe_survive_a_snapshot() {
        use crate::match_result::NoopSink;
        use crate::order_book::Order;
        use crate::order_book::OrderSide;
        use openraft::alias::EntryOf;

        fn entry(
            index: u64,
            payload: EntryPayloadOf<MatchEngineTypeConfig>,
        ) -> EntryOf<MatchEngineTypeConfig> {
            Entry {
                log_id: log_id(1, index),
                payload,
            }
        }

        let dir = tempdir();
        let mut store = StateMachineStore::<MatchEngineTypeConfig>::open_with_sinks_and_retention(
            dir.path(),
            Arc::new(NoopSink),
            None,
            8,
        )
        .unwrap();

        let entries = futures::stream::iter(vec![Ok((
            entry(
                1,
                EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
                    Order::new(OrderSide::Buy, 100.0, 5.0, 0),
                    "snapshot-id",
                )),
            ),
            None,
        ))]);
        store.apply(entries).await.unwrap();
        store.persist_snapshot().await.unwrap();

        let reopened = StateMachineStore::<MatchEngineTypeConfig>::open_with_sinks_and_retention(
            dir.path(),
            Arc::new(NoopSink),
            None,
            8,
        )
        .unwrap();
        let content = reopened.to_content().await;
        assert_eq!(1, content.orders.len());
        assert_eq!(1, content.sequence);
        assert_eq!(1, content.events.len());
        assert_eq!(1, content.submissions.len());

        // The retry is still a duplicate after restarting from the snapshot.
        let mut store = reopened;
        let entries = futures::stream::iter(vec![Ok((
            entry(
                2,
                EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
                    Order::new(OrderSide::Sell, 50.0, 1.0, 0),
                    "snapshot-id",
                )),
            ),
            None,
        ))]);
        store.apply(entries).await.unwrap();
        let content = store.to_content().await;
        assert_eq!(1, content.orders.len());
        assert_eq!(1, content.sequence);
    }
}

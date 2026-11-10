//! Feed assembly, depth conflation and fan-out.
//!
//! One task consumes [`MarketMessage`]s, maintains the derived book/BBO/bars,
//! conflates depth deltas at 100 ms and 1000 ms, serializes each stream payload
//! once and broadcasts it to the per-stream `tokio::sync::broadcast` channels.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::broadcast;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::MissedTickBehavior;

use matchengine_raft::market_data::HEARTBEAT_SUB_SEQ;
use matchengine_raft::market_data::MarketMessage;
use matchengine_raft::market_data::MessageBody;
use matchengine_raft::market_data::STREAM_STATUS;
use matchengine_raft::market_data::flags;
use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::OrderSide;

use crate::config::SymbolSpec;
use crate::protocol;
use crate::protocol::Speed;
use crate::source::BoxedSource;
use crate::source::SnapshotRequester;

/// One aggregated price level.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Level {
    pub volume: Fixed,
    pub orders: u32,
}

/// Visible depth maintained from snapshots and post-entry deltas.
#[derive(Clone, Debug, Default)]
pub struct DepthBook {
    pub bids: BTreeMap<Fixed, Level>,
    pub asks: BTreeMap<Fixed, Level>,
}

impl DepthBook {
    fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
    }

    fn apply(&mut self, side: OrderSide, price: Fixed, volume: Fixed, orders: u32) {
        let levels = match side {
            OrderSide::Buy => &mut self.bids,
            OrderSide::Sell => &mut self.asks,
        };
        if volume <= Fixed::ZERO {
            levels.remove(&price);
        } else {
            levels.insert(price, Level { volume, orders });
        }
    }

    fn total(&self, side: OrderSide, price: Fixed) -> Fixed {
        let levels = match side {
            OrderSide::Buy => &self.bids,
            OrderSide::Sell => &self.asks,
        };
        levels.get(&price).map_or(Fixed::ZERO, |level| level.volume)
    }

    /// Best-first `(price, volume, orders)` levels.
    pub fn top(&self, side: OrderSide, limit: usize) -> Vec<(Fixed, Fixed, u32)> {
        match side {
            OrderSide::Buy => self
                .bids
                .iter()
                .rev()
                .take(limit)
                .map(|(price, level)| (*price, level.volume, level.orders))
                .collect(),
            OrderSide::Sell => self
                .asks
                .iter()
                .take(limit)
                .map(|(price, level)| (*price, level.volume, level.orders))
                .collect(),
        }
    }

    /// Best-first `(price, volume)` levels for a diff payload.
    pub fn touched(&self, side: OrderSide, prices: &BTreeSet<Fixed>) -> Vec<(Fixed, Fixed)> {
        let mut levels: Vec<(Fixed, Fixed)> = prices
            .iter()
            .map(|price| (*price, self.total(side, *price)))
            .collect();
        match side {
            OrderSide::Buy => levels.sort_by_key(|level| Reverse(level.0)),
            OrderSide::Sell => levels.sort_by_key(|level| level.0),
        }
        levels
    }
}

/// State shared with WebSocket sessions for subscribe-time snapshots.
#[derive(Debug, Default)]
pub struct SharedState {
    pub depth: DepthBook,
    pub bbo: Option<matchengine_raft::market_data::BboRecord>,
    pub bbo_update_id: u64,
    pub bars: HashMap<u64, matchengine_raft::market_data::BarRecord>,
    /// Quote volume per `(interval_ms, window_start_ms)`, accumulated from fills.
    pub bar_quote: HashMap<(u64, u64), Fixed>,
    pub status: Option<matchengine_raft::market_data::StatusRecord>,
    /// Update id (= `market_seq + 1`) of the last applied diff depth.
    pub last_update_id: u64,
    pub last_ts_ms: u64,
    pub snapshot_seen: bool,
    pub depth_stale: bool,
    /// Local deadline refreshed by every frame (including the idle heartbeat).
    pub heartbeat_deadline: Option<std::time::Instant>,
    pub epoch: u64,
    pub leader_id: u64,
}

impl SharedState {
    pub fn ready(&self) -> bool {
        self.snapshot_seen
            && !self.depth_stale
            && self
                .heartbeat_deadline
                .is_some_and(|deadline| std::time::Instant::now() <= deadline)
    }
}

/// One symbol's feed state, shared with WebSocket sessions.
pub struct SymbolRuntime {
    pub spec: SymbolSpec,
    pub shared: Arc<Mutex<SharedState>>,
    /// Optional stream-2010 snapshot request channel.
    pub snapshot_requester: Option<crate::source::SnapshotRequester>,
}

/// Per-stream broadcast senders, created lazily on first subscribe.
#[derive(Debug, Default)]
pub struct Registry {
    streams: HashMap<String, broadcast::Sender<Bytes>>,
}

impl Registry {
    pub fn subscribe(&mut self, key: &str, capacity: usize) -> broadcast::Receiver<Bytes> {
        self.streams
            .entry(key.to_string())
            .or_insert_with(|| broadcast::channel(capacity).0)
            .subscribe()
    }

    /// Sends a payload; returns false when nobody is subscribed.
    pub fn publish(&mut self, key: &str, payload: Bytes) -> bool {
        self.streams
            .get(key)
            .is_some_and(|sender| sender.send(payload).is_ok())
    }

    pub fn has_subscribers(&self, key: &str) -> bool {
        self.streams
            .get(key)
            .is_some_and(|sender| sender.receiver_count() > 0)
    }

    /// Drops the sender when the last subscriber left.
    pub fn remove_if_unused(&mut self, key: &str) {
        if self
            .streams
            .get(key)
            .is_some_and(|sender| sender.receiver_count() == 0)
        {
            self.streams.remove(key);
        }
    }

    /// All stream keys that carry depth; used to broadcast resync notices.
    pub fn depth_keys(&self) -> Vec<String> {
        self.streams
            .keys()
            .filter(|key| key.contains("@depth"))
            .cloned()
            .collect()
    }
}

/// Service counters exposed at `/metrics`.
#[derive(Debug, Default)]
pub struct Stats {
    pub connections: AtomicU64,
    pub messages: AtomicU64,
    pub published: AtomicU64,
    pub resyncs: AtomicU64,
    pub gaps: AtomicU64,
    pub snapshot_requests: AtomicU64,
    pub slow_closes: AtomicU64,
    pub last_ts_ms: AtomicU64,
}

impl Stats {
    pub fn metrics_json(&self, registry: &Registry) -> serde_json::Value {
        json!({
            "connections": self.connections.load(Ordering::Relaxed),
            "streams": registry.streams.len(),
            "messages": self.messages.load(Ordering::Relaxed),
            "published": self.published.load(Ordering::Relaxed),
            "resyncs": self.resyncs.load(Ordering::Relaxed),
            "gaps": self.gaps.load(Ordering::Relaxed),
            "snapshot_requests": self.snapshot_requests.load(Ordering::Relaxed),
            "slow_closes": self.slow_closes.load(Ordering::Relaxed),
            "last_ts_ms": self.last_ts_ms.load(Ordering::Relaxed),
        })
    }
}

#[derive(Debug, Default)]
struct DepthWindow {
    bids: BTreeSet<Fixed>,
    asks: BTreeSet<Fixed>,
    first_seq: Option<u64>,
    last_seq: Option<u64>,
    ts_ms: u64,
    changed: bool,
}

impl DepthWindow {
    fn add(&mut self, msg: &MarketMessage, updates: &[matchengine_raft::market_data::LevelUpdate]) {
        for update in updates {
            match update.side {
                OrderSide::Buy => {
                    self.bids.insert(update.level.price);
                }
                OrderSide::Sell => {
                    self.asks.insert(update.level.price);
                }
            }
        }
        self.first_seq = Some(
            self.first_seq
                .map_or(msg.market_seq, |seq| seq.min(msg.market_seq)),
        );
        self.last_seq = Some(
            self.last_seq
                .map_or(msg.market_seq, |seq| seq.max(msg.market_seq)),
        );
        self.ts_ms = msg.ts_ms;
        self.changed = true;
    }

    fn reset(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.first_seq = None;
        self.last_seq = None;
        self.changed = false;
    }
}

/// The feed task's owned state.
pub struct Feed {
    symbol: SymbolSpec,
    bar_intervals_ms: Vec<u64>,
    lifecycle: bool,
    snapshot_levels: usize,
    heartbeat_timeout: Duration,
    snapshot_requester: Option<SnapshotRequester>,
    shared: Arc<Mutex<SharedState>>,
    registry: Arc<Mutex<Registry>>,
    stats: Arc<Stats>,
    /// Last seen `(market_seq, sub_seq)` per stream id, for replay dedupe.
    seen: HashMap<u16, (u64, u16)>,
    fast: DepthWindow,
    slow: DepthWindow,
    last_depth_u: Option<u64>,
}

impl Feed {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        symbol: SymbolSpec,
        bar_intervals_ms: Vec<u64>,
        lifecycle: bool,
        snapshot_levels: usize,
        heartbeat_timeout: Duration,
        shared: Arc<Mutex<SharedState>>,
        registry: Arc<Mutex<Registry>>,
        stats: Arc<Stats>,
        snapshot_requester: Option<SnapshotRequester>,
    ) -> Self {
        Self {
            symbol,
            bar_intervals_ms,
            lifecycle,
            snapshot_levels,
            heartbeat_timeout,
            snapshot_requester,
            shared,
            registry,
            stats,
            seen: HashMap::new(),
            fast: DepthWindow::default(),
            slow: DepthWindow::default(),
            last_depth_u: None,
        }
    }

    /// Asks the engine for a fresh snapshot when the optional transport
    /// requester exists. Returns false when no requester is configured or the
    /// request queue is full.
    pub fn request_snapshot(&self, reason: &str) -> bool {
        let Some(requester) = &self.snapshot_requester else {
            return false;
        };
        let depth = self.snapshot_levels.clamp(1, usize::from(u16::MAX)) as u16;
        let sent = requester.request(depth);
        if sent {
            self.stats.snapshot_requests.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(%reason, depth, "requested fresh market-data snapshot");
        } else {
            tracing::warn!(%reason, "snapshot request queue full or transport stopped");
        }
        sent
    }

    /// Runs until the source ends or the service stops.
    pub async fn run(mut self, mut source: BoxedSource, mut stop: watch::Receiver<bool>) {
        let mut fast = tokio::time::interval(Duration::from_millis(Speed::Ms100.millis()));
        let mut slow = tokio::time::interval(Duration::from_millis(Speed::Ms1000.millis()));
        fast.set_missed_tick_behavior(MissedTickBehavior::Skip);
        slow.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                _ = fast.tick() => self.flush(Speed::Ms100),
                _ = slow.tick() => self.flush(Speed::Ms1000),
                item = source.next() => match item {
                    Some(Ok(message)) => self.apply(message),
                    Some(Err(error)) => tracing::warn!(%error, "market-data source error"),
                    None => break,
                },
            }
        }
    }

    fn apply(&mut self, message: MarketMessage) {
        // Idle heartbeats intentionally repeat the same `(market_seq, sub_seq)`
        // key while the book is quiet; they are liveness/diagnostic frames, not
        // deterministic tape entries, so let every one through.
        let heartbeat = message.stream == STREAM_STATUS && message.sub_seq == HEARTBEAT_SUB_SEQ;
        if !heartbeat {
            // Dedupe replayed JSONL lines and duplicate feeds by stream position.
            let position = (message.market_seq, message.sub_seq);
            if let Some(previous) = self.seen.get(&message.stream)
                && position <= *previous
            {
                return;
            }
            self.seen.insert(message.stream, position);
        }
        self.stats.messages.fetch_add(1, Ordering::Relaxed);
        self.stats
            .last_ts_ms
            .store(message.ts_ms, Ordering::Relaxed);

        {
            let mut state = self.shared.lock().expect("state lock");
            state.last_ts_ms = message.ts_ms;
            state.epoch = message.epoch;
            state.leader_id = message.leader_id;
            state.heartbeat_deadline = Some(std::time::Instant::now() + self.heartbeat_timeout);
        }

        match &message.body {
            MessageBody::DepthSnapshot(snapshot) => self.apply_snapshot(&message, snapshot),
            MessageBody::DepthDelta(updates) => self.apply_delta(&message, updates),
            MessageBody::Bbo(bbo) => {
                let update_id = message.market_seq + 1;
                {
                    let mut state = self.shared.lock().expect("state lock");
                    state.bbo = Some(bbo.clone());
                    state.bbo_update_id = update_id;
                }
                let payload =
                    protocol::book_ticker_payload(&self.symbol, bbo, update_id, message.ts_ms);
                self.publish("bookTicker", payload);
            }
            MessageBody::Trade(fills) => {
                self.accumulate_quotes(fills);
                for fill in fills {
                    let payload = protocol::trade_payload(&self.symbol, fill);
                    self.publish("trade", payload);
                }
                let payload = protocol::sweep_payload(&self.symbol, &message);
                if !payload.is_empty() {
                    self.publish("sweep", payload);
                }
            }
            MessageBody::Lifecycle(records) => {
                if self.lifecycle {
                    let payload = protocol::lifecycle_payload(&self.symbol, records, message.ts_ms);
                    self.publish("lifecycle", payload);
                }
            }
            MessageBody::Bar(bar) => {
                let quote = self.quote_volume(bar);
                {
                    let mut state = self.shared.lock().expect("state lock");
                    state.bars.insert(bar.interval_ms, bar.clone());
                }
                let closed = message.flags & flags::BAR_CLOSE != 0;
                let payload =
                    protocol::kline_payload(&self.symbol, bar, quote, message.ts_ms, closed);
                let key = format!("kline_{}", protocol::kline_label(bar.interval_ms));
                self.publish(&key, payload);
            }
            MessageBody::Status(status) => {
                {
                    let mut state = self.shared.lock().expect("state lock");
                    state.status = Some(status.clone());
                }
                let (last_update_id, ready) = {
                    let state = self.shared.lock().expect("state lock");
                    (state.last_update_id, state.ready())
                };
                let payload = protocol::status_payload(
                    &self.symbol,
                    status,
                    last_update_id,
                    message.epoch,
                    message.leader_id,
                    ready,
                    message.ts_ms,
                );
                self.publish("status", payload);
            }
            MessageBody::Gap(gap) => {
                self.stats.gaps.fetch_add(1, Ordering::Relaxed);
                {
                    let mut state = self.shared.lock().expect("state lock");
                    state.depth_stale = true;
                }
                self.broadcast_resync("gap", gap.dropped);
                self.request_snapshot("gap");
            }
        }
    }

    fn apply_snapshot(
        &mut self,
        message: &MarketMessage,
        snapshot: &matchengine_raft::market_data::DepthSnapshotRecord,
    ) {
        // Periodic reset snapshots refresh the book for late joiners. A
        // continuous consumer already has the same state and must keep its
        // update-id stream contiguous, so it ignores them; only an initial
        // snapshot or one that follows a gap resets and notifies clients.
        let (seen, stale) = {
            let state = self.shared.lock().expect("state lock");
            (state.snapshot_seen, state.depth_stale)
        };
        if seen && !stale {
            return;
        }

        let mut state = self.shared.lock().expect("state lock");
        state.depth.clear();
        for level in &snapshot.bids {
            state
                .depth
                .apply(OrderSide::Buy, level.price, level.volume, level.orders);
        }
        for level in &snapshot.asks {
            state
                .depth
                .apply(OrderSide::Sell, level.price, level.volume, level.orders);
        }
        // The snapshot is anchored at the next market sequence; its last
        // included update id is therefore `market_seq` itself in 1-based form.
        state.last_update_id = message.market_seq;
        state.snapshot_seen = true;
        state.depth_stale = false;
        drop(state);

        self.last_depth_u = Some(message.market_seq);
        self.fast.reset();
        self.slow.reset();

        if message.flags & flags::RESET != 0 {
            self.broadcast_resync("reset", 0);
            // Give every depth subscriber a fresh snapshot frame.
            let payload = {
                let state = self.shared.lock().expect("state lock");
                let bids = state.depth.top(OrderSide::Buy, self.snapshot_levels);
                let asks = state.depth.top(OrderSide::Sell, self.snapshot_levels);
                protocol::depth_snapshot_payload(
                    &self.symbol,
                    state.last_update_id,
                    &bids,
                    &asks,
                    state.last_ts_ms,
                )
            };
            let prefix = format!("{}@", self.symbol.symbol);
            let keys: Vec<String> = self
                .registry
                .lock()
                .expect("registry lock")
                .depth_keys()
                .into_iter()
                .filter(|key| key.starts_with(&prefix) && key.contains("@depth@"))
                .collect();
            for key in keys {
                self.publish_key(&key, payload.clone());
            }
        }
    }

    fn apply_delta(
        &mut self,
        message: &MarketMessage,
        updates: &[matchengine_raft::market_data::LevelUpdate],
    ) {
        let stale = {
            let state = self.shared.lock().expect("state lock");
            !state.snapshot_seen || state.depth_stale
        };
        if stale {
            return;
        }
        {
            let mut state = self.shared.lock().expect("state lock");
            for update in updates {
                state.depth.apply(
                    update.side,
                    update.level.price,
                    update.level.volume,
                    update.level.orders,
                );
            }
        }
        self.fast.add(message, updates);
        self.slow.add(message, updates);
    }

    fn flush(&mut self, speed: Speed) {
        let (first_seq, last_seq, ts_ms, bid_prices, ask_prices) = {
            let window = match speed {
                Speed::Ms100 => &self.fast,
                Speed::Ms1000 => &self.slow,
            };
            if !window.changed {
                return;
            }
            (
                window.first_seq.expect("changed window has a sequence"),
                window.last_seq.expect("changed window has a sequence"),
                window.ts_ms,
                window.bids.clone(),
                window.asks.clone(),
            )
        };

        let last_update_id = last_seq + 1;
        let first_update_id = match self.last_depth_u {
            Some(previous) if previous < last_update_id => previous + 1,
            _ => first_seq + 1,
        }
        .min(last_update_id);
        self.last_depth_u = Some(last_update_id);

        let (bids, asks) = {
            let state = self.shared.lock().expect("state lock");
            (
                state.depth.touched(OrderSide::Buy, &bid_prices),
                state.depth.touched(OrderSide::Sell, &ask_prices),
            )
        };
        let payload = protocol::depth_update_payload(
            &self.symbol,
            first_update_id,
            last_update_id,
            &bids,
            &asks,
            ts_ms,
        );
        self.publish(&format!("depth@{}", speed.label()), payload);

        // Partial book depth: emit the current top N on the same cadence.
        for levels in [5usize, 10, 20] {
            let key = format!("{}@depth{levels}@{}", self.symbol.symbol, speed.label());
            if !self
                .registry
                .lock()
                .expect("registry lock")
                .has_subscribers(&key)
            {
                continue;
            }
            let payload = {
                let state = self.shared.lock().expect("state lock");
                let bids = state.depth.top(OrderSide::Buy, levels);
                let asks = state.depth.top(OrderSide::Sell, levels);
                protocol::depth_partial_payload(&self.symbol, last_update_id, &bids, &asks)
            };
            self.publish_key(&key, payload);
        }

        {
            let mut state = self.shared.lock().expect("state lock");
            state.last_update_id = last_update_id;
        }
        match speed {
            Speed::Ms100 => self.fast.reset(),
            Speed::Ms1000 => self.slow.reset(),
        }
    }

    fn accumulate_quotes(&mut self, fills: &[matchengine_raft::market_data::TradeRecord]) {
        let mut state = self.shared.lock().expect("state lock");
        for fill in fills {
            for interval in &self.bar_intervals_ms {
                let window = (fill.ts_ms / interval) * interval;
                let entry = state
                    .bar_quote
                    .entry((*interval, window))
                    .or_insert(Fixed::ZERO);
                *entry = entry.saturating_add(fill.quote_volume);
            }
        }
        // Bound the accumulator to the recent windows of each interval.
        let newest = state.last_ts_ms;
        state
            .bar_quote
            .retain(|key, _| key.1.saturating_add(key.0 * 2) >= newest);
    }

    fn quote_volume(&self, bar: &matchengine_raft::market_data::BarRecord) -> Fixed {
        let state = self.shared.lock().expect("state lock");
        state
            .bar_quote
            .get(&(bar.interval_ms, bar.window_start_ms))
            .copied()
            .unwrap_or(Fixed::ZERO)
    }

    fn publish(&mut self, suffix: &str, payload: Vec<u8>) {
        let key = format!("{}@{}", self.symbol.symbol, suffix);
        self.publish_key(&key, payload);
    }

    fn publish_key(&mut self, key: &str, payload: Vec<u8>) {
        let published = self
            .registry
            .lock()
            .expect("registry lock")
            .publish(key, Bytes::from(payload));
        if published {
            self.stats.published.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn broadcast_resync(&mut self, reason: &str, dropped: u32) {
        let payload = Bytes::from(protocol::resync_payload(&self.symbol, reason, dropped));
        self.stats.resyncs.fetch_add(1, Ordering::Relaxed);
        let prefix = format!("{}@", self.symbol.symbol);
        let keys: Vec<String> = self
            .registry
            .lock()
            .expect("registry lock")
            .depth_keys()
            .into_iter()
            .filter(|key| key.starts_with(&prefix))
            .collect();
        for key in keys {
            self.publish_key(&key, payload.to_vec());
        }
    }
}

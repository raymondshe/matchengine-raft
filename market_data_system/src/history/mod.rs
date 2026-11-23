//! TDengine-backed historical bars (phase P0 of `docs/HISTORICAL_BARS.md`).
//!
//! The existing feed still owns depth/BBO/live fan-out. This tier taps its
//! `Trade` bodies, builds sparse traded `1s` rows plus a materialized `1m`
//! rollup, upserts them into TDengine as exact `DECIMAL(38,18)` text, and
//! serves `GET /klines` plus WebSocket subscribe-time replay from those rows.
//! Nothing here touches the engine's replicated state.

pub mod builder;
pub mod decimal;
pub mod query;
pub mod runtime;
pub mod tdengine;

pub use runtime::HistoryRuntime;

use matchengine_raft::order_book::Fixed;

/// Interval labels are in milliseconds, matching the Binance wire contract.
pub const SEC_MS: u64 = 1_000;
pub const MIN_MS: u64 = 60 * SEC_MS;
/// Window math and TDengine timestamps are in microseconds.
pub const SEC_US: u64 = 1_000_000;
pub const MIN_US: u64 = 60 * SEC_US;

/// One stored bar row. Timestamps are microseconds of the window open; the
/// engine's taker stamp supplies every event time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BarRow {
    pub ts_us: u64,
    pub open: Fixed,
    pub high: Fixed,
    pub low: Fixed,
    pub close: Fixed,
    pub volume: Fixed,
    pub quote_volume: Fixed,
    pub taker_base: Fixed,
    pub taker_quote: Fixed,
    pub trades: u64,
    pub last_ts_us: Option<u64>,
    pub first_log: Option<u64>,
    pub first_ord: Option<u32>,
    pub last_log: Option<u64>,
    pub last_ord: Option<u32>,
    pub rev: i32,
    pub source: i8,
    pub sealed: bool,
    /// Engine `market_seq` of the last message that changed the bar.
    pub seq: u64,
}

impl BarRow {
    pub fn new(ts_us: u64) -> Self {
        Self {
            ts_us,
            open: Fixed::ZERO,
            high: Fixed::ZERO,
            low: Fixed::ZERO,
            close: Fixed::ZERO,
            volume: Fixed::ZERO,
            quote_volume: Fixed::ZERO,
            taker_base: Fixed::ZERO,
            taker_quote: Fixed::ZERO,
            trades: 0,
            last_ts_us: None,
            first_log: None,
            first_ord: None,
            last_log: None,
            last_ord: None,
            rev: 0,
            source: 0,
            sealed: false,
            seq: 0,
        }
    }

    /// Folds one fill into this window. `taker_buy` selects the taker-side volumes.
    pub fn apply_trade(&mut self, trade: &HistoryTrade) {
        if self.trades == 0 {
            self.open = trade.price;
            self.high = trade.price;
            self.low = trade.price;
        } else {
            self.high = self.high.max(trade.price);
            self.low = self.low.min(trade.price);
        }
        self.close = trade.price;
        self.volume = self.volume.saturating_add(trade.volume);
        self.quote_volume = self.quote_volume.saturating_add(trade.quote_volume);
        if trade.taker_buy {
            self.taker_base = self.taker_base.saturating_add(trade.volume);
            self.taker_quote = self.taker_quote.saturating_add(trade.quote_volume);
        }
        self.trades += 1;
        let ts_us = trade.ts_ms.saturating_mul(1_000);
        self.last_ts_us = Some(ts_us);
        if self.first_log.is_none() {
            self.first_log = Some(trade.log_index);
            self.first_ord = Some(trade.ordinal);
        }
        self.last_log = Some(trade.log_index);
        self.last_ord = Some(trade.ordinal);
        self.seq = trade.market_seq;
    }
}

/// A typed tap from `Feed::apply` — no JSON round-trip and no wall clock.
#[derive(Clone, Debug)]
pub struct HistoryTrade {
    pub symbol: String,
    pub symbol_id: u64,
    pub ts_ms: u64,
    pub price: Fixed,
    pub volume: Fixed,
    pub quote_volume: Fixed,
    pub taker_buy: bool,
    pub log_index: u64,
    pub ordinal: u32,
    /// Envelope `market_seq`: `TradeRecord` itself does not carry it.
    pub market_seq: u64,
}

#[derive(Clone, Debug)]
pub enum HistoryEvent {
    Trade(HistoryTrade),
}

/// The two intervals materialized in phase P0. Every coarser interval is a
/// read-time `INTERVAL` rollup of `bars_1m`.
pub fn is_materialized(interval_ms: u64) -> bool {
    interval_ms == SEC_MS || interval_ms == MIN_MS
}

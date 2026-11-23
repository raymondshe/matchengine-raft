//! Builds the materialized bar series from the trade tape.
//!
//! It reads no clock: a trade's window is derived from the taker's engine
//! intake stamp, so a replay or a restarted writer produces the same rows.
//! Each interval folds the same fills; the `1m` row is therefore identical to
//! a rollup of the traded `1s` children, and `5m` to a rollup of `1m` children.

use super::BarRow;
use super::FIVE_MIN_MS;
use super::HistoryTrade;
use super::MIN_MS;
use super::MIN_US;
use super::SEC_MS;
use super::SEC_US;

const FIVE_MIN_US: u64 = 5 * MIN_US;

pub struct Builder {
    symbol: String,
    #[allow(dead_code)] // retained for the P1 spool/cursor work
    symbol_id: u64,
    seal_lag_us: u64,
    open_1s: BarRow,
    open_1m: BarRow,
    open_5m: BarRow,
    last_ts_us: u64,
    sealed_1s_emitted: bool,
    sealed_1m_emitted: bool,
    sealed_5m_emitted: bool,
}

impl Builder {
    pub fn new(symbol: String, symbol_id: u64, seal_lag_ms: u64) -> Self {
        Self {
            symbol,
            symbol_id,
            seal_lag_us: seal_lag_ms.saturating_mul(1_000),
            open_1s: BarRow::new(0),
            open_1m: BarRow::new(0),
            open_5m: BarRow::new(0),
            last_ts_us: 0,
            sealed_1s_emitted: false,
            sealed_1m_emitted: false,
            sealed_5m_emitted: false,
        }
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn last_ts_us(&self) -> u64 {
        self.last_ts_us
    }

    /// Folds a trade and returns the rows to upsert, labelled by interval ms.
    pub fn on_trade(&mut self, trade: HistoryTrade) -> Vec<(u64, BarRow)> {
        let ts_us = trade.ts_ms.saturating_mul(1_000);
        self.last_ts_us = self.last_ts_us.max(ts_us);
        let window_1s = ts_us / SEC_US * SEC_US;
        let window_1m = ts_us / MIN_US * MIN_US;
        let window_5m = ts_us / FIVE_MIN_US * FIVE_MIN_US;

        let mut rows = Vec::with_capacity(6);
        if self.open_1s.trades == 0 {
            // First trade in this symbol: start at the taker's window, not at
            // the zero-initialized placeholder.
            self.open_1s = BarRow::new(window_1s);
        } else if self.open_1s.ts_us != window_1s {
            let mut sealed = self.open_1s.clone();
            sealed.sealed = true;
            rows.push((SEC_MS, sealed));
            self.open_1s = BarRow::new(window_1s);
        }
        if self.open_1m.trades == 0 {
            self.open_1m = BarRow::new(window_1m);
        } else if self.open_1m.ts_us != window_1m {
            let mut sealed = self.open_1m.clone();
            sealed.sealed = true;
            rows.push((MIN_MS, sealed));
            self.open_1m = BarRow::new(window_1m);
        }
        if self.open_5m.trades == 0 {
            self.open_5m = BarRow::new(window_5m);
        } else if self.open_5m.ts_us != window_5m {
            let mut sealed = self.open_5m.clone();
            sealed.sealed = true;
            rows.push((FIVE_MIN_MS, sealed));
            self.open_5m = BarRow::new(window_5m);
        }

        self.open_1s.apply_trade(&trade);
        self.open_1m.apply_trade(&trade);
        self.open_5m.apply_trade(&trade);
        self.sealed_1s_emitted = false;
        self.sealed_1m_emitted = false;
        self.sealed_5m_emitted = false;
        // A late trade into a sealed window reopens it; P1 adds the audit row
        // and revision counter for this case.
        self.open_1s.sealed = false;
        self.open_1m.sealed = false;
        self.open_5m.sealed = false;
        rows.push((SEC_MS, self.open_1s.clone()));
        rows.push((MIN_MS, self.open_1m.clone()));
        rows.push((FIVE_MIN_MS, self.open_5m.clone()));
        rows
    }

    /// Seals open windows once the event-time watermark has passed them.
    pub fn seal_due(&mut self, now_us: u64) -> Vec<(u64, BarRow)> {
        if self.last_ts_us == 0 || self.last_ts_us.saturating_add(self.seal_lag_us) > now_us {
            return Vec::new();
        }
        let mut rows = Vec::with_capacity(3);
        if self.open_1s.trades > 0 && !self.sealed_1s_emitted {
            let mut sealed = self.open_1s.clone();
            sealed.sealed = true;
            rows.push((SEC_MS, sealed));
            self.sealed_1s_emitted = true;
        }
        if self.open_1m.trades > 0 && !self.sealed_1m_emitted {
            let mut sealed = self.open_1m.clone();
            sealed.sealed = true;
            rows.push((MIN_MS, sealed));
            self.sealed_1m_emitted = true;
        }
        if self.open_5m.trades > 0 && !self.sealed_5m_emitted {
            let mut sealed = self.open_5m.clone();
            sealed.sealed = true;
            rows.push((FIVE_MIN_MS, sealed));
            self.sealed_5m_emitted = true;
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::decimal;

    fn trade(ts_ms: u64, price: &str, volume: &str, seq: u64) -> HistoryTrade {
        HistoryTrade {
            symbol: "eurusd".to_string(),
            symbol_id: 0,
            ts_ms,
            price: decimal::parse(price).unwrap(),
            volume: decimal::parse(volume).unwrap(),
            quote_volume: decimal::parse(price).unwrap(),
            taker_buy: true,
            log_index: seq,
            ordinal: 1,
            market_seq: seq,
        }
    }

    #[test]
    fn separate_windows_seal_and_roll_up() {
        let mut builder = Builder::new("eurusd".to_string(), 0, 2_000);
        let t0 = 1_735_689_600_000; // aligned minute
        let mut rows = Vec::new();
        rows.extend(builder.on_trade(trade(t0, "1.1", "2", 1)));
        rows.extend(builder.on_trade(trade(t0 + 500, "1.2", "3", 2)));
        rows.extend(builder.on_trade(trade(t0 + 1_000, "1.3", "4", 3)));

        let sealed_1s: Vec<_> = rows
            .iter()
            .filter(|(interval, row)| *interval == SEC_MS && row.sealed)
            .collect();
        assert_eq!(sealed_1s.len(), 1, "the first one-second window closed");

        let minutes: Vec<_> = rows
            .iter()
            .filter(|(interval, _)| *interval == MIN_MS)
            .collect();
        let last = &minutes.last().expect("minute row").1;
        assert_eq!(last.open, decimal::parse("1.1").unwrap());
        assert_eq!(last.close, decimal::parse("1.3").unwrap());
        assert_eq!(last.high, decimal::parse("1.3").unwrap());
        assert_eq!(last.low, decimal::parse("1.1").unwrap());
        assert_eq!(last.trades, 3);
        assert_eq!(last.seq, 3);
        assert_eq!(last.last_log, Some(3));
    }

    #[test]
    fn seal_due_emits_once() {
        let mut builder = Builder::new("eurusd".to_string(), 0, 2_000);
        let t = 1_735_689_600_000;
        builder.on_trade(trade(t, "1.1", "2", 1));
        let now = (t + 3_000) * 1_000;
        assert_eq!(builder.seal_due(now).len(), 3);
        assert!(builder.seal_due(now).is_empty());
    }

    #[test]
    fn taker_side_splits_volumes() {
        let mut trade = trade(1_735_689_600_000, "1.1", "2", 1);
        trade.taker_buy = false;
        let mut builder = Builder::new("eurusd".to_string(), 0, 2_000);
        let rows = builder.on_trade(trade);
        let row = &rows
            .iter()
            .find(|(interval, _)| *interval == SEC_MS)
            .unwrap()
            .1;
        assert_eq!(row.taker_base, matchengine_raft::order_book::Fixed::ZERO);
        assert_eq!(row.volume, decimal::parse("2").unwrap());
    }
}

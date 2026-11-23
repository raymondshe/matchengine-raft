//! Densification and Binance-shaped kline assembly.
//!
//! Storage is sparse: only traded windows exist. The wire stays dense, exactly
//! like the reference: an empty window carries the previous close forward with
//! zero volume. This is the only place that distinction is resolved, so REST,
//! WS replay and export cannot drift apart.

use std::collections::HashMap;

use matchengine_raft::order_book::Fixed;
use serde_json::Value;
use serde_json::json;

use super::BarRow;
use super::decimal;

#[derive(Clone, Debug)]
pub struct KlineWindow {
    pub start_ms: u64,
    pub end_ms: u64,
    pub limit: usize,
    /// Event-time watermark minus the seal lag. A window ending after this is
    /// still open (`x=false`).
    pub sealed_through_ms: u64,
    /// Close of the last stored bar before the requested range, used to carry
    /// the correct price into the first synthetic window.
    pub initial_close: Option<Fixed>,
}

/// Builds dense `[openTime, o, h, l, c, v, closeTime, q, n, ...]` arrays.
pub fn klines(rows: Vec<BarRow>, interval_ms: u64, window: &KlineWindow) -> Vec<Value> {
    let width_ms = interval_ms.max(1);
    let by_open: HashMap<u64, BarRow> = rows
        .into_iter()
        .map(|row| (row.ts_us / 1_000, row))
        .collect();
    let mut previous_close = window.initial_close.unwrap_or(Fixed::ZERO);
    let mut result = Vec::new();
    let mut open_ms = window.start_ms;
    while open_ms <= window.end_ms && result.len() < window.limit {
        let close_ms = open_ms.saturating_add(width_ms).saturating_sub(1);
        let row = by_open.get(&open_ms);
        let (bar, synthetic) = match row {
            Some(row) => {
                previous_close = row.close;
                (row.clone(), false)
            }
            None => {
                let mut flat = BarRow::new(open_ms.saturating_mul(1_000));
                flat.open = previous_close;
                flat.high = previous_close;
                flat.low = previous_close;
                flat.close = previous_close;
                flat.sealed = true;
                (flat, true)
            }
        };
        let sealed = if synthetic {
            close_ms <= window.sealed_through_ms
        } else {
            bar.sealed || close_ms <= window.sealed_through_ms
        };
        result.push(json!([
            open_ms,
            decimal::to_wire(bar.open),
            decimal::to_wire(bar.high),
            decimal::to_wire(bar.low),
            decimal::to_wire(bar.close),
            decimal::to_wire(bar.volume),
            close_ms,
            decimal::to_wire(bar.quote_volume),
            bar.trades,
            decimal::to_wire(bar.taker_base),
            decimal::to_wire(bar.taker_quote),
            if sealed { "1" } else { "0" },
        ]));
        open_ms = open_ms.saturating_add(width_ms);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::decimal;
    use matchengine_raft::order_book::Fixed;

    fn row(open_ms: u64, close: &str) -> BarRow {
        let mut bar = BarRow::new(open_ms * 1_000);
        bar.open = decimal::parse("1.0").unwrap();
        bar.high = decimal::parse("1.2").unwrap();
        bar.low = decimal::parse("0.9").unwrap();
        bar.close = decimal::parse(close).unwrap();
        bar.volume = decimal::parse("2").unwrap();
        bar.quote_volume = decimal::parse("2.2").unwrap();
        bar.trades = 3;
        bar.sealed = true;
        bar
    }

    #[test]
    fn fills_empty_windows_with_previous_close() {
        let start = 1_735_689_600_000u64;
        let rows = vec![row(start, "1.1")];
        let window = KlineWindow {
            start_ms: start,
            end_ms: start + 2_000,
            limit: 10,
            sealed_through_ms: start + 10_000,
            initial_close: None,
        };
        let bars = klines(rows, 1_000, &window);
        assert_eq!(bars.len(), 3);
        assert_eq!(bars[1][1], "1.100000000000000000");
        assert_eq!(bars[1][5], decimal::zero_wire());
        assert_eq!(bars[1][8], 0);
        assert_eq!(bars[0][11], "1");
        assert_eq!(bars[2][6], start + 2_999);
    }

    #[test]
    fn open_window_is_not_final() {
        let start = 1_735_689_600_000u64;
        let mut bar = row(start, "1.1");
        bar.sealed = false; // the currently forming window
        let rows = vec![bar];
        let window = KlineWindow {
            start_ms: start,
            end_ms: start,
            limit: 10,
            sealed_through_ms: start - 1,
            initial_close: None,
        };
        let bars = klines(rows, 1_000, &window);
        assert_eq!(bars[0][11], "0");
    }

    #[test]
    fn limit_bounds_the_grid() {
        let start = 1_735_689_600_000u64;
        let window = KlineWindow {
            start_ms: start,
            end_ms: start + 100_000,
            limit: 3,
            sealed_through_ms: start + 200_000,
            initial_close: None,
        };
        assert_eq!(klines(Vec::new(), 1_000, &window).len(), 3);
    }

    #[test]
    fn canonical_strings_carry_all_eighteen_digits() {
        let mut bar = row(1_735_689_600_000, "1.1");
        bar.close = Fixed::from_raw(1_000_000_000_000_000_001);
        let window = KlineWindow {
            start_ms: bar.ts_us / 1_000,
            end_ms: bar.ts_us / 1_000,
            limit: 1,
            sealed_through_ms: u64::MAX,
            initial_close: None,
        };
        let bars = klines(vec![bar], 1_000, &window);
        assert_eq!(bars[0][4], "1.000000000000000001");
    }
}

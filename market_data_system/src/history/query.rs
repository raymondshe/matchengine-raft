//! Densification and Binance-shaped kline assembly.
//!
//! Storage is sparse: only traded windows exist. The wire stays dense, exactly
//! like the reference: an empty window carries the previous close forward with
//! zero volume. This is the only place that distinction is resolved, so REST,
//! WS replay and export cannot drift apart.

use std::collections::BTreeMap;
use std::collections::HashMap;

pub use super::rollup_source_interval;

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

/// Reference rollup of materialized rows into a coarser interval.
///
/// This is the algorithm TDengine's `INTERVAL` query implements, kept in Rust
/// so the 1m → 5m → 30m chain can be asserted field by field without a DB.
/// Only traded children are folded in: sparse windows never invent a trade.
pub fn rollup_bars(rows: &[BarRow], target_ms: u64) -> Vec<BarRow> {
    let width_us = target_ms.saturating_mul(1_000);
    let mut books: BTreeMap<u64, BarRow> = BTreeMap::new();
    for row in rows {
        if row.trades == 0 {
            continue;
        }
        let window = row.ts_us / width_us * width_us;
        let bar = books.entry(window).or_insert_with(|| {
            let mut bar = BarRow::new(window);
            bar.sealed = true;
            bar
        });
        if bar.trades == 0 {
            bar.open = row.open;
            bar.high = row.high;
            bar.low = row.low;
            bar.close = row.close;
            bar.first_log = row.first_log;
            bar.first_ord = row.first_ord;
        } else {
            bar.high = bar.high.max(row.high);
            bar.low = bar.low.min(row.low);
            bar.close = row.close;
            bar.first_log = bar.first_log.or(row.first_log);
            bar.first_ord = bar.first_ord.or(row.first_ord);
        }
        bar.volume = bar.volume.saturating_add(row.volume);
        bar.quote_volume = bar.quote_volume.saturating_add(row.quote_volume);
        bar.taker_base = bar.taker_base.saturating_add(row.taker_base);
        bar.taker_quote = bar.taker_quote.saturating_add(row.taker_quote);
        bar.trades += row.trades;
        bar.last_ts_us = bar.last_ts_us.max(row.last_ts_us);
        // Match the SQL rollup, which uses MAX(last_log)/MAX(last_ord). Rows are
        // normally time-ordered, but `or` would also keep a stale larger key if
        // the feed ever delivers a child out of order.
        bar.last_log = match (bar.last_log, row.last_log) {
            (Some(current), Some(next)) => Some(current.max(next)),
            (current, next) => current.or(next),
        };
        bar.last_ord = match (bar.last_ord, row.last_ord) {
            (Some(current), Some(next)) => Some(current.max(next)),
            (current, next) => current.or(next),
        };
        bar.seq = bar.seq.max(row.seq);
        bar.sealed &= row.sealed;
    }
    books.into_values().collect()
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
        let bar = match row {
            Some(row) => {
                previous_close = row.close;
                row.clone()
            }
            None => {
                let mut flat = BarRow::new(open_ms.saturating_mul(1_000));
                flat.open = previous_close;
                flat.high = previous_close;
                flat.low = previous_close;
                flat.close = previous_close;
                flat.sealed = true;
                flat
            }
        };
        // Finality is a property of the *window*, not of its children: a
        // sealed 5m child inside the still-open 30m window must not mark the
        // 30m bar closed. The event-time watermark is the only authority.
        let sealed = close_ms <= window.sealed_through_ms;
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
    use crate::history::FIVE_MIN_MS;
    use crate::history::MIN_MS;
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
        bar.sealed = true; // even a sealed child cannot close a future window
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

    #[test]
    fn thirty_minutes_from_five_matches_direct_rollup() {
        // 2025-01-01T00:00:00Z is aligned to a 30-minute epoch window.
        let base = 1_735_689_600_000u64;
        let minutes: Vec<BarRow> = (0..6)
            .map(|i| {
                let mut bar = row(base + i * MIN_MS, "1");
                let price = Fixed::from_raw(
                    1_000_000_000_000_000_000i128 + i as i128 * 10_000_000_000_000_000,
                );
                bar.open = price;
                bar.high = price.saturating_add(Fixed::from_raw(5_000_000_000_000_000));
                bar.low = price.saturating_sub(Fixed::from_raw(2_000_000_000_000_000));
                bar.close = price.saturating_add(Fixed::from_raw(1_000_000_000_000_000));
                bar.last_ts_us = Some((base + i * MIN_MS) * 1_000 + 900_000);
                bar.first_log = Some(i);
                bar.last_log = Some(i);
                bar.seq = i;
                bar
            })
            .collect();

        let fives = rollup_bars(&minutes, FIVE_MIN_MS);
        assert_eq!(fives.len(), 2, "six traded minutes make two 5m windows");
        assert_eq!(
            fives[0].trades, 15,
            "five minutes land in the first 5m window"
        );

        let direct = rollup_bars(&minutes, 30 * MIN_MS);
        let via_fives = rollup_bars(&fives, 30 * MIN_MS);
        assert_eq!(direct.len(), 1);
        assert_eq!(via_fives, direct, "1m→5m→30m must equal 1m→30m");
        let bar = &via_fives[0];
        assert_eq!(bar.ts_us, base * 1_000);
        assert_eq!(bar.open, minutes[0].open);
        assert_eq!(bar.close, minutes[5].close);
        assert_eq!(bar.high, minutes.iter().map(|m| m.high).max().unwrap());
        assert_eq!(bar.low, minutes.iter().map(|m| m.low).min().unwrap());
        assert_eq!(bar.trades, 18);
        assert_eq!(
            bar.volume,
            minutes
                .iter()
                .fold(Fixed::ZERO, |a, m| a.saturating_add(m.volume))
        );
        assert_eq!(bar.last_ts_us, minutes[5].last_ts_us);
    }

    #[test]
    fn sparse_minutes_do_not_become_flat_children() {
        let base = 1_735_689_600_000u64;
        let one = row(base, "1.1");
        let rolled = rollup_bars(std::slice::from_ref(&one), 30 * MIN_MS);
        assert_eq!(rolled.len(), 1, "only the traded window exists");
        assert_eq!(rolled[0].trades, one.trades);
        assert_eq!(rolled[0].open, one.open);
    }

    #[test]
    fn rollup_uses_max_last_log_and_last_ord_like_sql() {
        let base = 1_735_689_600_000u64;
        let mut first = row(base, "1.1");
        first.last_log = Some(9);
        first.last_ord = Some(2);
        let mut second = row(base + MIN_MS, "1.2");
        second.last_log = Some(10);
        second.last_ord = Some(7);

        let rolled = rollup_bars(&[first, second], FIVE_MIN_MS);
        assert_eq!(1, rolled.len());
        assert_eq!(Some(10), rolled[0].last_log);
        assert_eq!(Some(7), rolled[0].last_ord);
    }
}

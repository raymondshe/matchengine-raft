//! Binance-style stream names, control messages and JSON payloads.

use serde::Deserialize;
use serde_json::json;

use matchengine_raft::market_data::BarRecord;
use matchengine_raft::market_data::BboRecord;
use matchengine_raft::market_data::LifecycleRecord;
use matchengine_raft::market_data::StatusRecord;
use matchengine_raft::market_data::TradeRecord;
use matchengine_raft::order_book::FeeAsset;
use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::OrderSide;

use crate::config::Config;
use crate::config::SymbolSpec;

/// Depth conflation speed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    Ms100,
    Ms1000,
}

impl Speed {
    pub fn millis(self) -> u64 {
        match self {
            Speed::Ms100 => 100,
            Speed::Ms1000 => 1_000,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Speed::Ms100 => "100ms",
            Speed::Ms1000 => "1000ms",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "100ms" => Some(Speed::Ms100),
            "1000ms" => Some(Speed::Ms1000),
            _ => None,
        }
    }
}

/// What a stream carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Trade,
    Sweep,
    BookTicker,
    DepthDiff(Speed),
    DepthPartial { levels: usize, speed: Speed },
    Kline { interval_ms: u64 },
    Status,
    Lifecycle,
}

/// A validated stream subscription.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamSpec {
    /// Canonical stream name, used as the registry key.
    pub key: String,
    /// Canonical symbol part of the name.
    pub symbol: String,
    pub kind: StreamKind,
}

impl StreamSpec {
    /// Book ticker and partial depth are latest-value streams; everything else
    /// is sequenced and must not lose a message silently.
    pub fn is_latest_value(&self) -> bool {
        matches!(
            self.kind,
            StreamKind::BookTicker | StreamKind::DepthPartial { .. }
        )
    }
}

/// A protocol-level rejection, rendered as a Binance-style error object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    pub code: u32,
    pub message: String,
}

impl ProtocolError {
    fn unknown_stream() -> Self {
        Self {
            code: 0,
            message: "unknown stream".to_string(),
        }
    }
}

/// Parses `<symbol>@<stream>` against the configured symbol.
pub fn parse_stream(config: &Config, name: &str) -> Result<StreamSpec, ProtocolError> {
    let (symbol, rest) = name
        .split_once('@')
        .ok_or_else(ProtocolError::unknown_stream)?;
    let spec = config
        .symbol(symbol)
        .ok_or_else(ProtocolError::unknown_stream)?;
    let kind = match rest {
        "trade" => StreamKind::Trade,
        "sweep" => StreamKind::Sweep,
        "bookTicker" => StreamKind::BookTicker,
        "status" => StreamKind::Status,
        "lifecycle" if config.lifecycle => StreamKind::Lifecycle,
        rest if rest.starts_with("kline_") => {
            let interval_ms = parse_kline_interval(&rest["kline_".len()..])
                .ok_or_else(ProtocolError::unknown_stream)?;
            StreamKind::Kline { interval_ms }
        }
        rest => parse_depth(rest).ok_or_else(ProtocolError::unknown_stream)?,
    };
    Ok(StreamSpec {
        // Canonical lowercase symbol, so `EURUSD@bookTicker` and
        // `eurusd@bookTicker` share one registry key.
        key: format!("{}@{}", spec.symbol, rest),
        symbol: spec.symbol.clone(),
        kind,
    })
}

fn parse_depth(rest: &str) -> Option<StreamKind> {
    let (head, speed) = rest.rsplit_once('@')?;
    let speed = Speed::parse(speed)?;
    if head == "depth" {
        return Some(StreamKind::DepthDiff(speed));
    }
    let levels = head.strip_prefix("depth")?.parse::<usize>().ok()?;
    if matches!(levels, 5 | 10 | 20) {
        Some(StreamKind::DepthPartial { levels, speed })
    } else {
        None
    }
}

/// Parses a Binance kline interval (`1m`) or an explicit `<ms>ms` value.
pub fn parse_kline_interval(label: &str) -> Option<u64> {
    let millis = match label {
        "1s" => 1_000,
        "1m" => 60_000,
        "3m" => 180_000,
        "5m" => 300_000,
        "15m" => 900_000,
        "30m" => 1_800_000,
        "1h" => 3_600_000,
        "2h" => 7_200_000,
        "4h" => 14_400_000,
        "6h" => 21_600_000,
        "8h" => 28_800_000,
        "12h" => 43_200_000,
        "1d" => 86_400_000,
        "3d" => 259_200_000,
        "1w" => 604_800_000,
        "1M" => 2_592_000_000,
        other => other.strip_suffix("ms")?.parse().ok()?,
    };
    Some(millis)
}

/// The Binance label for a millisecond interval.
pub fn kline_label(interval_ms: u64) -> String {
    match interval_ms {
        1_000 => "1s".to_string(),
        60_000 => "1m".to_string(),
        180_000 => "3m".to_string(),
        300_000 => "5m".to_string(),
        900_000 => "15m".to_string(),
        1_800_000 => "30m".to_string(),
        3_600_000 => "1h".to_string(),
        7_200_000 => "2h".to_string(),
        14_400_000 => "4h".to_string(),
        21_600_000 => "6h".to_string(),
        28_800_000 => "8h".to_string(),
        43_200_000 => "12h".to_string(),
        86_400_000 => "1d".to_string(),
        259_200_000 => "3d".to_string(),
        604_800_000 => "1w".to_string(),
        2_592_000_000 => "1M".to_string(),
        other => format!("{other}ms"),
    }
}

/// Binance-style control frames.
#[derive(Debug, Deserialize)]
#[serde(tag = "method")]
pub enum ControlMessage {
    #[serde(rename = "SUBSCRIBE")]
    Subscribe { params: Vec<String>, id: u64 },
    #[serde(rename = "UNSUBSCRIBE")]
    Unsubscribe { params: Vec<String>, id: u64 },
    #[serde(rename = "LIST_SUBSCRIPTIONS")]
    List { id: u64 },
}

/// Renders an error reply for a control message.
pub fn control_error(error: &ProtocolError, id: Option<u64>) -> Vec<u8> {
    match id {
        Some(id) => json!({"error": {"code": error.code, "msg": error.message}, "id": id})
            .to_string()
            .into_bytes(),
        None => json!({"error": {"code": error.code, "msg": error.message}})
            .to_string()
            .into_bytes(),
    }
}

/// Renders a successful `null` result.
pub fn control_result(id: u64) -> Vec<u8> {
    json!({"result": null, "id": id}).to_string().into_bytes()
}

/// Renders a list-subscription result.
pub fn control_list(id: u64, streams: &[String]) -> Vec<u8> {
    json!({"result": streams, "id": id})
        .to_string()
        .into_bytes()
}

fn fee_asset(symbol: &SymbolSpec, asset: Option<FeeAsset>) -> Option<&str> {
    asset.map(|asset| match asset {
        FeeAsset::Base => symbol.base_asset.as_str(),
        FeeAsset::Quote => symbol.quote_asset.as_str(),
    })
}

fn fee_fields_with_assets(symbol: &SymbolSpec, fill: &TradeRecord) -> serde_json::Value {
    json!({
        "tf": fill.taker_fee,
        "tfa": fee_asset(symbol, fill.taker_fee_asset),
        "mf": fill.maker_fee,
        "mfa": fee_asset(symbol, fill.maker_fee_asset),
        "err": fill.error.codes().map(|code| code.as_str()).collect::<Vec<_>>(),
    })
}

fn side_is_maker(fill: &TradeRecord) -> bool {
    fill.taker_side == OrderSide::Sell
}

fn fill_volume(fills: &[TradeRecord]) -> (Fixed, Fixed) {
    let mut volume = Fixed::ZERO;
    let mut quote = Fixed::ZERO;
    for fill in fills {
        volume = volume.saturating_add(fill.volume);
        quote = quote.saturating_add(fill.quote_volume);
    }
    (volume, quote)
}

/// One fill on `<symbol>@trade`.
pub fn trade_payload(symbol: &SymbolSpec, fill: &TradeRecord) -> Vec<u8> {
    let mut value = json!({
        "e": "trade",
        "E": fill.ts_ms,
        "s": symbol.symbol,
        "S": symbol.symbol_id,
        "t": fill.ordinal,
        "L": fill.log_index,
        "p": fill.price,
        "q": fill.volume,
        "qv": fill.quote_volume,
        "T": fill.ts_ms,
        "m": side_is_maker(fill),
        "ts": fill.taker_sequence,
        "ms": fill.maker_sequence,
    });
    let fees = fee_fields_with_assets(symbol, fill);
    value
        .as_object_mut()
        .unwrap()
        .extend(fees.as_object().unwrap().clone());
    value.to_string().into_bytes()
}

/// One taker's fills on `<symbol>@sweep`.
pub fn sweep_payload(
    symbol: &SymbolSpec,
    message: &matchengine_raft::market_data::MarketMessage,
) -> Vec<u8> {
    let fills = match &message.body {
        matchengine_raft::market_data::MessageBody::Trade(fills) => fills,
        _ => return Vec::new(),
    };
    let Some(first) = fills.first() else {
        return Vec::new();
    };
    let (volume, quote) = fill_volume(fills);
    let fills_json: Vec<serde_json::Value> = fills
        .iter()
        .map(|fill| {
            let mut value = json!({
                "t": fill.ordinal,
                "p": fill.price,
                "q": fill.volume,
                "qv": fill.quote_volume,
                "ms": fill.maker_sequence,
            });
            let fees = fee_fields_with_assets(symbol, fill);
            value
                .as_object_mut()
                .unwrap()
                .extend(fees.as_object().unwrap().clone());
            value
        })
        .collect();
    json!({
        "e": "sweep",
        "E": message.ts_ms,
        "s": symbol.symbol,
        "S": symbol.symbol_id,
        "u": message.market_seq + 1,
        "L": message.log_index,
        "T": first.ts_ms,
        "ts": first.taker_sequence,
        "m": side_is_maker(first),
        "fills": fills_json,
        "volume": volume,
        "quoteVolume": quote,
    })
    .to_string()
    .into_bytes()
}

/// Top of book on `<symbol>@bookTicker`.
pub fn book_ticker_payload(
    symbol: &SymbolSpec,
    bbo: &BboRecord,
    update_id: u64,
    ts_ms: u64,
) -> Vec<u8> {
    json!({
        "u": update_id,
        "s": symbol.symbol,
        "S": symbol.symbol_id,
        "E": ts_ms,
        "b": bbo.bid,
        "B": bbo.bid_volume,
        "a": bbo.ask,
        "A": bbo.ask_volume,
        "mid": bbo.mid,
        "spread": bbo.spread,
        "last": bbo.last_trade,
    })
    .to_string()
    .into_bytes()
}

/// Conflated diff depth on `<symbol>@depth@<speed>`.
pub fn depth_update_payload(
    symbol: &SymbolSpec,
    first_update_id: u64,
    last_update_id: u64,
    bids: &[(Fixed, Fixed)],
    asks: &[(Fixed, Fixed)],
    ts_ms: u64,
) -> Vec<u8> {
    json!({
        "e": "depthUpdate",
        "E": ts_ms,
        "s": symbol.symbol,
        "S": symbol.symbol_id,
        "U": first_update_id,
        "u": last_update_id,
        "b": bids.iter().map(|(price, volume)| [price.to_string(), volume.to_string()]).collect::<Vec<_>>(),
        "a": asks.iter().map(|(price, volume)| [price.to_string(), volume.to_string()]).collect::<Vec<_>>(),
    })
    .to_string()
    .into_bytes()
}

/// Subscribe-time full depth snapshot.
pub fn depth_snapshot_payload(
    symbol: &SymbolSpec,
    last_update_id: u64,
    bids: &[(Fixed, Fixed, u32)],
    asks: &[(Fixed, Fixed, u32)],
    ts_ms: u64,
) -> Vec<u8> {
    json!({
        "e": "depthSnapshot",
        "E": ts_ms,
        "s": symbol.symbol,
        "S": symbol.symbol_id,
        "lastUpdateId": last_update_id,
        "bids": bids.iter().map(|(price, volume, orders)| json!([price.to_string(), volume.to_string(), orders])).collect::<Vec<_>>(),
        "asks": asks.iter().map(|(price, volume, orders)| json!([price.to_string(), volume.to_string(), orders])).collect::<Vec<_>>(),
    })
    .to_string()
    .into_bytes()
}

/// Partial book depth on `<symbol>@depth<levels>@<speed>`.
///
/// Binance's partial-depth payload omits the symbol; we include `s`/`S` so a
/// raw `/ws` connection that holds streams for several symbols can attribute
/// each frame without the combined-stream envelope.
pub fn depth_partial_payload(
    symbol: &SymbolSpec,
    last_update_id: u64,
    bids: &[(Fixed, Fixed, u32)],
    asks: &[(Fixed, Fixed, u32)],
) -> Vec<u8> {
    json!({
        "s": symbol.symbol,
        "S": symbol.symbol_id,
        "lastUpdateId": last_update_id,
        "bids": bids.iter().map(|(price, volume, orders)| json!([price.to_string(), volume.to_string(), orders])).collect::<Vec<_>>(),
        "asks": asks.iter().map(|(price, volume, orders)| json!([price.to_string(), volume.to_string(), orders])).collect::<Vec<_>>(),
    })
    .to_string()
    .into_bytes()
}

/// One OHLCV bar on `<symbol>@kline_<interval>`.
pub fn kline_payload(
    symbol: &SymbolSpec,
    bar: &BarRecord,
    quote_volume: Fixed,
    ts_ms: u64,
    closed: bool,
) -> Vec<u8> {
    json!({
        "e": "kline",
        "E": ts_ms,
        "s": symbol.symbol,
        "S": symbol.symbol_id,
        "k": {
            "t": bar.window_start_ms,
            "T": bar.window_start_ms.saturating_add(bar.interval_ms).saturating_sub(1),
            "i": kline_label(bar.interval_ms),
            "o": bar.open,
            "c": bar.close,
            "h": bar.high,
            "l": bar.low,
            "v": bar.volume,
            "n": bar.trade_count,
            "x": closed,
            "q": quote_volume,
        }
    })
    .to_string()
    .into_bytes()
}

/// Diagnostic status on `<symbol>@status`.
pub fn status_payload(
    symbol: &SymbolSpec,
    status: &StatusRecord,
    last_update_id: u64,
    epoch: u64,
    leader_id: u64,
    ready: bool,
    ts_ms: u64,
) -> Vec<u8> {
    json!({
        "e": "status",
        "E": ts_ms,
        "s": symbol.symbol,
        "S": symbol.symbol_id,
        "lastUpdateId": last_update_id,
        "lastLogIndex": status.last_log_index,
        "lastOrdinal": status.last_ordinal,
        "bookSequence": status.book_sequence,
        "pendingStops": status.pending_stops,
        "epoch": epoch,
        "leaderId": leader_id,
        "ready": ready,
    })
    .to_string()
    .into_bytes()
}

/// Order-lifecycle records on `<symbol>@lifecycle`.
pub fn lifecycle_payload(symbol: &SymbolSpec, records: &[LifecycleRecord], ts_ms: u64) -> Vec<u8> {
    json!({
        "e": "lifecycle",
        "E": ts_ms,
        "s": symbol.symbol,
        "S": symbol.symbol_id,
        "events": records,
    })
    .to_string()
    .into_bytes()
}

/// A resync notice sent when depth state can no longer be trusted.
pub fn resync_payload(symbol: &SymbolSpec, reason: &str, dropped: u32) -> Vec<u8> {
    json!({
        "e": "resync",
        "s": symbol.symbol,
        "S": symbol.symbol_id,
        "reason": reason,
        "dropped": dropped,
    })
    .to_string()
    .into_bytes()
}

/// Convenience for tests: parses a payload back into JSON.
pub fn as_json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("valid JSON payload")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::for_test("127.0.0.1:0")
    }

    #[test]
    fn parses_the_stream_catalogue() {
        let config = config();
        for (name, kind) in [
            ("eurusd@trade", StreamKind::Trade),
            ("eurusd@sweep", StreamKind::Sweep),
            ("eurusd@bookTicker", StreamKind::BookTicker),
            ("eurusd@status", StreamKind::Status),
            ("eurusd@lifecycle", StreamKind::Lifecycle),
            ("eurusd@depth@100ms", StreamKind::DepthDiff(Speed::Ms100)),
            ("eurusd@depth@1000ms", StreamKind::DepthDiff(Speed::Ms1000)),
            (
                "eurusd@depth5@100ms",
                StreamKind::DepthPartial {
                    levels: 5,
                    speed: Speed::Ms100,
                },
            ),
            (
                "eurusd@depth20@1000ms",
                StreamKind::DepthPartial {
                    levels: 20,
                    speed: Speed::Ms1000,
                },
            ),
            (
                "eurusd@kline_1m",
                StreamKind::Kline {
                    interval_ms: 60_000,
                },
            ),
        ] {
            assert_eq!(kind, parse_stream(&config, name).unwrap().kind, "{name}");
        }
        assert_eq!(
            "eurusd@bookTicker",
            parse_stream(&config, "EURUSD@bookTicker").unwrap().key
        );
        assert!(parse_stream(&config, "btcusdt@trade").is_err());
        assert!(parse_stream(&config, "eurusd@depth7@100ms").is_err());
        assert!(parse_stream(&config, "eurusd@depth@250ms").is_err());
        assert!(parse_stream(&config, "eurusd@kline_2m").is_err());
        assert!(parse_stream(&config, "eurusd").is_err());
    }

    #[test]
    fn lifecycle_can_be_disabled() {
        let mut config = config();
        config.lifecycle = false;
        assert!(parse_stream(&config, "eurusd@lifecycle").is_err());
    }

    #[test]
    fn kline_labels_round_trip() {
        for label in [
            "1s", "1m", "3m", "5m", "15m", "30m", "1h", "2h", "4h", "6h", "8h", "12h", "1d", "3d",
            "1w", "1M",
        ] {
            let millis = parse_kline_interval(label).unwrap();
            assert_eq!(label, kline_label(millis), "{label}");
        }
        assert_eq!(Some(2_500), parse_kline_interval("2500ms"));
        assert_eq!("2500ms", kline_label(2_500));
    }

    #[test]
    fn depth_payloads_use_absolute_levels() {
        let config = config();
        let symbol = &config.symbols[0];
        let payload = depth_update_payload(
            symbol,
            3,
            4,
            &[(Fixed::from_units(100), Fixed::from_units(7))],
            &[],
            1_000,
        );
        let value = as_json(&payload);
        assert_eq!("depthUpdate", value["e"]);
        assert_eq!(3, value["U"]);
        assert_eq!(4, value["u"]);
        assert_eq!("100", value["b"][0][0]);
        assert_eq!("7", value["b"][0][1]);

        let snapshot = depth_snapshot_payload(
            symbol,
            2,
            &[(Fixed::from_units(100), Fixed::from_units(7), 2)],
            &[],
            1_000,
        );
        let value = as_json(&snapshot);
        assert_eq!(2, value["lastUpdateId"]);
        assert_eq!(2, value["bids"][0][2]);
    }

    #[test]
    fn control_replies_echo_the_id() {
        assert_eq!(br#"{"id":7,"result":null}"#.to_vec(), control_result(7));
        let value = as_json(&control_error(
            &ProtocolError {
                code: 0,
                message: "unknown stream".to_string(),
            },
            Some(9),
        ));
        assert_eq!(0, value["error"]["code"]);
        assert_eq!(9, value["id"]);
    }
}

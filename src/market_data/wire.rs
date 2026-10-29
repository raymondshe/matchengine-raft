//! Binary wire format for market-data messages.
//!
//! This is the on-the-wire encoding described in `docs/AERON_SOLUTION.md` §16:
//! a 64-byte little-endian header followed by a packed body. The encoder is
//! transport-independent; the Aeron adapter (phase 2) offers these bytes on
//! streams 2001–2007 without changing the schema.
//!
//! Layout is packed (no implicit padding): fields are written in the order
//! listed, and variable arrays start with a `u32` count, so a single taker's
//! uncapped fill list cannot be truncated. Version 5 encodes every price,
//! quantity, quote volume and fee as a scale-18 `i128` (16 bytes, little-endian),
//! so no financial value crosses the wire as `f64`, and carries the volume unit
//! and fee asset per record. A version bump is required for any change other
//! than adding a field at the end of `STATUS`.

use super::BarRecord;
use super::DepthSnapshotRecord;
use super::LifecycleRecord;
use super::MarketMessage;
use super::MessageBody;
use super::StatusRecord;
use super::TradeRecord;
use crate::match_result::MatchEventKind;
use crate::order_book::FeeAsset;
use crate::order_book::Fixed;
use crate::order_book::OrderSide;
use crate::order_book::RejectReason;
use crate::order_book::VolumeUnit;

/// Magic bytes at the start of every message.
pub const MAGIC: [u8; 4] = *b"AMKT";
/// Current schema version.
pub const VERSION: u8 = 5;
/// Header length in bytes.
pub const HEADER_LEN: usize = 64;

/// Message type codes used in the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    /// Executed fills.
    Trade = 1,
    /// Top of book.
    Bbo = 2,
    /// Full depth snapshot.
    DepthSnapshot = 3,
    /// Post-entry depth totals for touched levels.
    DepthDelta = 4,
    /// Order lifecycle.
    Lifecycle = 5,
    /// Deterministic status cadence.
    Status = 6,
    /// Dropped-frame marker.
    Gap = 7,
    /// OHLCV bar.
    Bar = 8,
}

impl MessageBody {
    /// Header type code for this body.
    pub fn message_type(&self) -> MessageType {
        match self {
            MessageBody::Trade(_) => MessageType::Trade,
            MessageBody::Bbo(_) => MessageType::Bbo,
            MessageBody::DepthSnapshot(_) => MessageType::DepthSnapshot,
            MessageBody::DepthDelta(_) => MessageType::DepthDelta,
            MessageBody::Lifecycle(_) => MessageType::Lifecycle,
            MessageBody::Status(_) => MessageType::Status,
            MessageBody::Gap(_) => MessageType::Gap,
            MessageBody::Bar(_) => MessageType::Bar,
        }
    }
}

/// Encodes one message into its wire representation.
pub fn encode(message: &MarketMessage) -> Vec<u8> {
    let mut body = Vec::with_capacity(128);
    encode_body(&message.body, &mut body);

    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(message.body.message_type() as u8);
    out.extend_from_slice(&message.flags.to_le_bytes());
    out.extend_from_slice(&message.market_seq.to_le_bytes());
    out.extend_from_slice(&message.log_index.to_le_bytes());
    out.extend_from_slice(&message.ordinal.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&message.epoch.to_le_bytes());
    out.extend_from_slice(&message.leader_id.to_le_bytes());
    out.extend_from_slice(&message.ts_ms.to_le_bytes());
    out.extend_from_slice(&message.symbol_id.to_le_bytes());
    out.extend_from_slice(&message.sub_seq.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&body);
    out
}

fn encode_body(body: &MessageBody, out: &mut Vec<u8>) {
    match body {
        MessageBody::Trade(records) => {
            out.extend_from_slice(&(records.len() as u32).to_le_bytes());
            for record in records {
                encode_trade(record, out);
            }
        }
        MessageBody::Bbo(bbo) => {
            put_option_fixed(bbo.last_trade, out);
            put_fixed(bbo.bid, out);
            put_fixed(bbo.bid_volume, out);
            put_fixed(bbo.ask, out);
            put_fixed(bbo.ask_volume, out);
            put_option_fixed(bbo.mid, out);
            put_option_fixed(bbo.spread, out);
        }
        MessageBody::DepthSnapshot(snapshot) => {
            encode_depth_snapshot(snapshot, out);
        }
        MessageBody::DepthDelta(updates) => {
            out.extend_from_slice(&(updates.len() as u32).to_le_bytes());
            for update in updates {
                encode_level(
                    update.side,
                    update.level.price,
                    update.level.volume,
                    update.level.orders,
                    out,
                );
            }
        }
        MessageBody::Lifecycle(records) => {
            out.extend_from_slice(&(records.len() as u32).to_le_bytes());
            for record in records {
                encode_lifecycle(record, out);
            }
        }
        MessageBody::Status(status) => {
            encode_status(status, out);
        }
        MessageBody::Gap(gap) => {
            out.extend_from_slice(&gap.last_good_log_index.to_le_bytes());
            out.extend_from_slice(&gap.last_good_ordinal.to_le_bytes());
            out.extend_from_slice(&gap.dropped.to_le_bytes());
        }
        MessageBody::Bar(bar) => {
            encode_bar(bar, out);
        }
    }
}

fn encode_trade(record: &TradeRecord, out: &mut Vec<u8>) {
    out.extend_from_slice(&record.log_index.to_le_bytes());
    out.extend_from_slice(&record.ordinal.to_le_bytes());
    out.extend_from_slice(&record.ts_ms.to_le_bytes());
    put_fixed(record.price, out);
    put_fixed(record.volume, out);
    put_fixed(record.quote_volume, out);
    put_fixed(record.taker_fee, out);
    out.push(fee_asset_code(record.taker_fee_asset));
    put_fixed(record.maker_fee, out);
    out.push(fee_asset_code(record.maker_fee_asset));
    out.push(u8::from(record.fee_overflow));
    out.push(side_code(record.taker_side));
    out.extend_from_slice(&record.taker_sequence.to_le_bytes());
    out.extend_from_slice(&record.maker_sequence.to_le_bytes());
}

fn encode_depth_snapshot(snapshot: &DepthSnapshotRecord, out: &mut Vec<u8>) {
    out.extend_from_slice(&(snapshot.bids.len() as u32).to_le_bytes());
    out.extend_from_slice(&(snapshot.asks.len() as u32).to_le_bytes());
    for level in &snapshot.bids {
        encode_level(OrderSide::Buy, level.price, level.volume, level.orders, out);
    }
    for level in &snapshot.asks {
        encode_level(
            OrderSide::Sell,
            level.price,
            level.volume,
            level.orders,
            out,
        );
    }
}

fn encode_level(side: OrderSide, price: Fixed, volume: Fixed, orders: u32, out: &mut Vec<u8>) {
    out.push(side_code(side));
    put_fixed(price, out);
    put_fixed(volume, out);
    out.extend_from_slice(&orders.to_le_bytes());
}

fn encode_lifecycle(record: &LifecycleRecord, out: &mut Vec<u8>) {
    out.extend_from_slice(&record.log_index.to_le_bytes());
    out.extend_from_slice(&record.ordinal.to_le_bytes());
    out.extend_from_slice(&record.ts_ms.to_le_bytes());
    out.push(kind_code(record.kind));
    out.extend_from_slice(&record.order_sequence.to_le_bytes());
    out.push(side_code(record.side));
    out.extend_from_slice(&record.counterparty_sequence.unwrap_or(0).to_le_bytes());
    put_fixed(record.price, out);
    put_fixed(record.volume, out);
    put_fixed(record.quote_volume, out);
    out.push(volume_unit_code(record.volume_unit));
    put_fixed(record.taker_fee, out);
    out.push(fee_asset_code(record.taker_fee_asset));
    put_fixed(record.maker_fee, out);
    out.push(fee_asset_code(record.maker_fee_asset));
    out.push(u8::from(record.fee_overflow));
    out.push(reason_code(record.reason));
}

fn encode_status(status: &StatusRecord, out: &mut Vec<u8>) {
    out.extend_from_slice(&status.last_market_seq.to_le_bytes());
    out.extend_from_slice(&status.last_sub_seq.to_le_bytes());
    out.extend_from_slice(&status.last_log_index.to_le_bytes());
    out.extend_from_slice(&status.last_ordinal.to_le_bytes());
    out.extend_from_slice(&status.applied_index.to_le_bytes());
    out.extend_from_slice(&status.book_sequence.to_le_bytes());
    out.extend_from_slice(&status.pending_stops.to_le_bytes());
}

fn encode_bar(bar: &BarRecord, out: &mut Vec<u8>) {
    out.extend_from_slice(&(bar.interval_ms as u32).to_le_bytes());
    out.extend_from_slice(&bar.window_start_ms.to_le_bytes());
    out.extend_from_slice(&bar.last_ts_ms.to_le_bytes());
    put_fixed(bar.open, out);
    put_fixed(bar.high, out);
    put_fixed(bar.low, out);
    put_fixed(bar.close, out);
    put_fixed(bar.volume, out);
    out.extend_from_slice(&bar.trade_count.to_le_bytes());
    out.extend_from_slice(&bar.bar_seq.to_le_bytes());
}

fn put_fixed(value: Fixed, out: &mut Vec<u8>) {
    out.extend_from_slice(&value.raw().to_le_bytes());
}

fn put_option_fixed(value: Option<Fixed>, out: &mut Vec<u8>) {
    match value {
        Some(value) => {
            out.push(1);
            put_fixed(value, out);
        }
        None => {
            out.push(0);
            put_fixed(Fixed::ZERO, out);
        }
    }
}

fn fee_asset_code(asset: Option<FeeAsset>) -> u8 {
    match asset {
        None => 0,
        Some(FeeAsset::Base) => 1,
        Some(FeeAsset::Quote) => 2,
    }
}

fn volume_unit_code(unit: VolumeUnit) -> u8 {
    match unit {
        VolumeUnit::Base => 0,
        VolumeUnit::Quote => 1,
    }
}

fn side_code(side: OrderSide) -> u8 {
    match side {
        OrderSide::Buy => 0,
        OrderSide::Sell => 1,
    }
}

fn kind_code(kind: MatchEventKind) -> u8 {
    match kind {
        MatchEventKind::Rested => 1,
        MatchEventKind::Fill => 2,
        MatchEventKind::Cancelled => 3,
        MatchEventKind::Triggered => 4,
        MatchEventKind::Expired => 5,
        MatchEventKind::Rejected => 6,
        MatchEventKind::Amended => 7,
    }
}

fn reason_code(reason: Option<RejectReason>) -> u8 {
    match reason {
        None => 0,
        Some(RejectReason::FokUnfillable) => 1,
        Some(RejectReason::PostOnlyWouldCross) => 2,
        Some(RejectReason::StopWouldTrigger) => 3,
        Some(RejectReason::GtdAlreadyExpired) => 4,
        Some(RejectReason::OrderNotFound) => 5,
        Some(RejectReason::UnsupportedOrder) => 6,
        Some(RejectReason::AmendWouldCross) => 7,
        Some(RejectReason::InvalidOrder) => 8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market_data::BboRecord;
    use crate::market_data::DepthSnapshotRecord;
    use crate::market_data::GapRecord;
    use crate::market_data::LevelUpdate;
    use crate::market_data::StatusRecord;
    use crate::market_data::flags;
    use crate::order_book::DepthLevel;

    fn fixed(value: &str) -> Fixed {
        value.parse().expect("valid test decimal")
    }

    struct Reader<'a> {
        bytes: &'a [u8],
        offset: usize,
    }

    impl<'a> Reader<'a> {
        fn new(bytes: &'a [u8], offset: usize) -> Self {
            Self { bytes, offset }
        }

        fn u8(&mut self) -> u8 {
            let value = self.bytes[self.offset];
            self.offset += 1;
            value
        }

        fn u16(&mut self) -> u16 {
            let value =
                u16::from_le_bytes(self.bytes[self.offset..self.offset + 2].try_into().unwrap());
            self.offset += 2;
            value
        }

        fn u32(&mut self) -> u32 {
            let value =
                u32::from_le_bytes(self.bytes[self.offset..self.offset + 4].try_into().unwrap());
            self.offset += 4;
            value
        }

        fn u64(&mut self) -> u64 {
            let value =
                u64::from_le_bytes(self.bytes[self.offset..self.offset + 8].try_into().unwrap());
            self.offset += 8;
            value
        }

        fn i128(&mut self) -> i128 {
            let value = i128::from_le_bytes(
                self.bytes[self.offset..self.offset + 16]
                    .try_into()
                    .unwrap(),
            );
            self.offset += 16;
            value
        }
    }

    fn message(body: MessageBody) -> MarketMessage {
        MarketMessage {
            stream: 0,
            flags: flags::END_OF_BATCH,
            market_seq: 7,
            sub_seq: 3,
            log_index: 42,
            ordinal: 3,
            epoch: 9,
            leader_id: 1,
            ts_ms: 1_700_000_000_000,
            symbol_id: 0,
            body,
        }
    }

    #[test]
    fn header_round_trips() {
        let bytes = encode(&message(MessageBody::Gap(GapRecord {
            last_good_log_index: 41,
            last_good_ordinal: 2,
            dropped: 1,
        })));

        assert_eq!(&bytes[0..4], &MAGIC);
        assert_eq!(VERSION, bytes[4]);
        assert_eq!(MessageType::Gap as u8, bytes[5]);
        let mut reader = Reader::new(&bytes, 6);
        assert_eq!(flags::END_OF_BATCH, reader.u16());
        assert_eq!(7, reader.u64());
        assert_eq!(42, reader.u64());
        assert_eq!(3, reader.u32());
        let body_len = reader.u32();
        assert_eq!((bytes.len() - HEADER_LEN) as u32, body_len);
        assert_eq!(9, reader.u64());
        assert_eq!(1, reader.u64());
        assert_eq!(1_700_000_000_000, reader.u64());
        assert_eq!(0, reader.u32());
        assert_eq!(3, reader.u16());
        assert_eq!(0, reader.u16());
        assert_eq!(HEADER_LEN, reader.offset);

        // Gap body: last good position and drop count.
        assert_eq!(41, reader.u64());
        assert_eq!(2, reader.u32());
        assert_eq!(1, reader.u32());
    }

    #[test]
    fn trade_batch_encodes_every_record() {
        let body = MessageBody::Trade(vec![
            TradeRecord {
                log_index: 10,
                ordinal: 0,
                ts_ms: 1_000,
                price: fixed("100.5"),
                volume: fixed("2"),
                quote_volume: fixed("201"),
                taker_fee: fixed("0.4"),
                taker_fee_asset: Some(FeeAsset::Base),
                maker_fee: fixed("0.2"),
                maker_fee_asset: Some(FeeAsset::Quote),
                fee_overflow: false,
                taker_side: OrderSide::Buy,
                taker_sequence: 4,
                maker_sequence: 2,
            },
            TradeRecord {
                log_index: 11,
                ordinal: 1,
                ts_ms: 1_001,
                price: fixed("101.000000000000000001"),
                volume: fixed("3"),
                quote_volume: fixed("303.000000000000000003"),
                taker_fee: fixed("0.1"),
                taker_fee_asset: Some(FeeAsset::Quote),
                maker_fee: fixed("0.05"),
                maker_fee_asset: Some(FeeAsset::Base),
                fee_overflow: false,
                taker_side: OrderSide::Sell,
                taker_sequence: 5,
                maker_sequence: 3,
            },
        ]);
        let bytes = encode(&message(body));
        let mut reader = Reader::new(&bytes, HEADER_LEN);

        assert_eq!(2, reader.u32());
        assert_eq!(10, reader.u64());
        assert_eq!(0, reader.u32());
        assert_eq!(1_000, reader.u64());
        assert_eq!(fixed("100.5").raw(), reader.i128());
        assert_eq!(fixed("2").raw(), reader.i128());
        assert_eq!(fixed("201").raw(), reader.i128());
        assert_eq!(fixed("0.4").raw(), reader.i128());
        assert_eq!(1, reader.u8());
        assert_eq!(fixed("0.2").raw(), reader.i128());
        assert_eq!(2, reader.u8());
        assert_eq!(0, reader.u8());
        assert_eq!(0, reader.u8());
        assert_eq!(4, reader.u64());
        assert_eq!(2, reader.u64());

        assert_eq!(11, reader.u64());
        assert_eq!(1, reader.u32());
        assert_eq!(1_001, reader.u64());
        assert_eq!(fixed("101.000000000000000001").raw(), reader.i128());
        assert_eq!(fixed("3").raw(), reader.i128());
        assert_eq!(fixed("303.000000000000000003").raw(), reader.i128());
        assert_eq!(fixed("0.1").raw(), reader.i128());
        assert_eq!(2, reader.u8());
        assert_eq!(fixed("0.05").raw(), reader.i128());
        assert_eq!(1, reader.u8());
        assert_eq!(0, reader.u8());
        assert_eq!(1, reader.u8());
        assert_eq!(5, reader.u64());
        assert_eq!(3, reader.u64());
    }

    #[test]
    fn depth_snapshot_and_delta_encode_levels() {
        let snapshot = MessageBody::DepthSnapshot(DepthSnapshotRecord {
            bids: vec![DepthLevel {
                price: fixed("100"),
                volume: fixed("5"),
                orders: 2,
            }],
            asks: vec![DepthLevel {
                price: fixed("101"),
                volume: fixed("1.5"),
                orders: 1,
            }],
        });
        let bytes = encode(&message(snapshot));
        let mut reader = Reader::new(&bytes, HEADER_LEN);
        assert_eq!(1, reader.u32());
        assert_eq!(1, reader.u32());
        assert_eq!(0, reader.u8());
        assert_eq!(fixed("100").raw(), reader.i128());
        assert_eq!(fixed("5").raw(), reader.i128());
        assert_eq!(2, reader.u32());
        assert_eq!(1, reader.u8());
        assert_eq!(fixed("101").raw(), reader.i128());
        assert_eq!(fixed("1.5").raw(), reader.i128());
        assert_eq!(1, reader.u32());

        let delta = MessageBody::DepthDelta(vec![LevelUpdate {
            side: OrderSide::Sell,
            level: DepthLevel {
                price: fixed("99"),
                volume: fixed("4"),
                orders: 1,
            },
        }]);
        let bytes = encode(&message(delta));
        let mut reader = Reader::new(&bytes, HEADER_LEN);
        assert_eq!(1, reader.u32());
        assert_eq!(1, reader.u8());
        assert_eq!(fixed("99").raw(), reader.i128());
        assert_eq!(fixed("4").raw(), reader.i128());
        assert_eq!(1, reader.u32());
    }

    #[test]
    fn bbo_encodes_presence_flags() {
        let body = MessageBody::Bbo(BboRecord {
            last_trade: Some(fixed("100")),
            bid: fixed("99.5"),
            bid_volume: fixed("3"),
            ask: fixed("100.5"),
            ask_volume: fixed("4"),
            mid: Some(fixed("100")),
            spread: Some(fixed("1")),
        });
        let bytes = encode(&message(body));
        let mut reader = Reader::new(&bytes, HEADER_LEN);
        assert_eq!(1, reader.u8());
        assert_eq!(fixed("100").raw(), reader.i128());
        assert_eq!(fixed("99.5").raw(), reader.i128());
        assert_eq!(fixed("3").raw(), reader.i128());
        assert_eq!(fixed("100.5").raw(), reader.i128());
        assert_eq!(fixed("4").raw(), reader.i128());
        assert_eq!(1, reader.u8());
        assert_eq!(fixed("100").raw(), reader.i128());
        assert_eq!(1, reader.u8());
        assert_eq!(fixed("1").raw(), reader.i128());
    }

    #[test]
    fn status_body_encodes() {
        let bytes = encode(&message(MessageBody::Status(StatusRecord {
            last_market_seq: 12,
            last_sub_seq: 1,
            last_log_index: 9,
            last_ordinal: 1,
            applied_index: 10,
            book_sequence: 4,
            pending_stops: 2,
        })));
        let mut reader = Reader::new(&bytes, HEADER_LEN);
        assert_eq!(12, reader.u64());
        assert_eq!(1, reader.u16());
        assert_eq!(9, reader.u64());
        assert_eq!(1, reader.u32());
        assert_eq!(10, reader.u64());
        assert_eq!(4, reader.u64());
        assert_eq!(2, reader.u32());
        assert_eq!(bytes.len(), HEADER_LEN + 42);
    }

    #[test]
    fn lifecycle_and_bar_encode() {
        let lifecycle = MessageBody::Lifecycle(vec![LifecycleRecord {
            log_index: 5,
            ordinal: 2,
            ts_ms: 1_000,
            kind: MatchEventKind::Fill,
            order_sequence: 8,
            side: OrderSide::Buy,
            counterparty_sequence: Some(3),
            price: fixed("100"),
            volume: fixed("2"),
            quote_volume: fixed("200"),
            volume_unit: VolumeUnit::Base,
            taker_fee: fixed("0.4"),
            taker_fee_asset: Some(FeeAsset::Base),
            maker_fee: fixed("0.2"),
            maker_fee_asset: Some(FeeAsset::Quote),
            fee_overflow: false,
            reason: None,
        }]);
        let bytes = encode(&message(lifecycle));
        let mut reader = Reader::new(&bytes, HEADER_LEN);
        assert_eq!(1, reader.u32());
        assert_eq!(5, reader.u64());
        assert_eq!(2, reader.u32());
        assert_eq!(1_000, reader.u64());
        assert_eq!(2, reader.u8()); // Fill
        assert_eq!(8, reader.u64());
        assert_eq!(0, reader.u8()); // Buy
        assert_eq!(3, reader.u64());
        assert_eq!(fixed("100").raw(), reader.i128());
        assert_eq!(fixed("2").raw(), reader.i128());
        assert_eq!(fixed("200").raw(), reader.i128());
        assert_eq!(0, reader.u8()); // base volume
        assert_eq!(fixed("0.4").raw(), reader.i128());
        assert_eq!(1, reader.u8()); // taker fee asset: base
        assert_eq!(fixed("0.2").raw(), reader.i128());
        assert_eq!(2, reader.u8()); // maker fee asset: quote
        assert_eq!(0, reader.u8()); // no overflow
        assert_eq!(0, reader.u8()); // no reason

        let bar = MessageBody::Bar(BarRecord {
            interval_ms: 1_000,
            window_start_ms: 1_000,
            last_ts_ms: 1_800,
            open: fixed("100"),
            high: fixed("101"),
            low: fixed("99"),
            close: fixed("100.5"),
            volume: fixed("7"),
            trade_count: 3,
            bar_seq: 4,
        });
        let bytes = encode(&message(bar));
        let mut reader = Reader::new(&bytes, HEADER_LEN);
        assert_eq!(1_000, reader.u32());
        assert_eq!(1_000, reader.u64());
        assert_eq!(1_800, reader.u64());
        assert_eq!(fixed("100").raw(), reader.i128());
        assert_eq!(fixed("101").raw(), reader.i128());
        assert_eq!(fixed("99").raw(), reader.i128());
        assert_eq!(fixed("100.5").raw(), reader.i128());
        assert_eq!(fixed("7").raw(), reader.i128());
        assert_eq!(3, reader.u32());
        assert_eq!(4, reader.u64());
    }

    #[test]
    fn trade_count_is_wide_enough_for_an_uncapped_sweep() {
        let fills: Vec<TradeRecord> = (0..70_000u64)
            .map(|i| TradeRecord {
                log_index: i,
                ordinal: 0,
                ts_ms: 1_000,
                price: fixed("100"),
                volume: fixed("1"),
                quote_volume: fixed("100"),
                taker_fee: Fixed::ZERO,
                taker_fee_asset: Some(FeeAsset::Base),
                maker_fee: Fixed::ZERO,
                maker_fee_asset: Some(FeeAsset::Quote),
                fee_overflow: false,
                taker_side: OrderSide::Buy,
                taker_sequence: 1,
                maker_sequence: i,
            })
            .collect();
        let bytes = encode(&message(MessageBody::Trade(fills)));
        let mut reader = Reader::new(&bytes, HEADER_LEN);
        assert_eq!(70_000, reader.u32());
        assert_eq!(
            HEADER_LEN + 4 + 70_000 * 120,
            bytes.len(),
            "every fill is encoded"
        );
    }

    #[test]
    fn wire_version_is_v5() {
        assert_eq!(5, VERSION);
        let bytes = encode(&message(MessageBody::Gap(GapRecord {
            last_good_log_index: 0,
            last_good_ordinal: 0,
            dropped: 0,
        })));
        assert_eq!(5, bytes[4]);
    }
}

//! Binary wire format for market-data messages.
//!
//! This is the on-the-wire encoding described in `docs/AERON_SOLUTION.md` §16:
//! a 64-byte little-endian header followed by a packed body. The encoder is
//! transport-independent; the Aeron adapter (phase 2) offers these bytes on
//! streams 2001–2007 without changing the schema.
//!
//! Layout is packed (no implicit padding): fields are written in the order
//! listed, and variable arrays start with a `u32` count, so a single taker's
//! uncapped fill list cannot be truncated. Version 6 encodes every price,
//! quantity, quote volume and fee as a scale-18 `i128` (16 bytes, little-endian),
//! so no financial value crosses the wire as `f64`, and carries the volume unit
//! and fee asset per record. A version bump is required for any change other
//! than adding a field at the end of `STATUS`.
//!
//! [`encode`] produces frames and [`decode`] is the reference consumer. The
//! decoder is strict and returns [`WireError`] instead of panicking on a
//! truncated, corrupt or unknown frame.

use std::fmt;

use super::BarRecord;
use super::BboRecord;
use super::DepthSnapshotRecord;
use super::GapRecord;
use super::LevelUpdate;
use super::LifecycleRecord;
use super::MarketMessage;
use super::MessageBody;
use super::STREAM_BAR;
use super::STREAM_BBO;
use super::STREAM_DEPTH_DELTA;
use super::STREAM_DEPTH_SNAPSHOT;
use super::STREAM_LIFECYCLE;
use super::STREAM_STATUS;
use super::STREAM_TRADE;
use super::StatusRecord;
use super::TradeRecord;
use crate::match_result::MatchEventKind;
use crate::order_book::DepthLevel;
use crate::order_book::FeeAsset;
use crate::order_book::Fixed;
use crate::order_book::MatchErrors;
use crate::order_book::OrderSide;
use crate::order_book::RejectReason;
use crate::order_book::VolumeUnit;

/// Magic bytes at the start of every message.
pub const MAGIC: [u8; 4] = *b"AMKT";
/// Current schema version.
pub const VERSION: u8 = 7;
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

impl MessageType {
    fn from_code(code: u8) -> Result<Self, WireError> {
        match code {
            1 => Ok(Self::Trade),
            2 => Ok(Self::Bbo),
            3 => Ok(Self::DepthSnapshot),
            4 => Ok(Self::DepthDelta),
            5 => Ok(Self::Lifecycle),
            6 => Ok(Self::Status),
            7 => Ok(Self::Gap),
            8 => Ok(Self::Bar),
            _ => Err(WireError::UnknownMessageType(code)),
        }
    }

    /// Stream a body travels on. `GAP` is emitted on the status stream.
    fn stream(self) -> u16 {
        match self {
            Self::Trade => STREAM_TRADE,
            Self::Bbo => STREAM_BBO,
            Self::DepthSnapshot => STREAM_DEPTH_SNAPSHOT,
            Self::DepthDelta => STREAM_DEPTH_DELTA,
            Self::Lifecycle => STREAM_LIFECYCLE,
            Self::Status | Self::Gap => STREAM_STATUS,
            Self::Bar => STREAM_BAR,
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
    out.push(record.error.bits());
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
    out.push(record.error.bits());
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
        Some(RejectReason::SelfTradePrevented) => 9,
        Some(RejectReason::PegWouldCross) => 10,
        Some(RejectReason::PegReferenceUnavailable) => 11,
    }
}

/// Error returned by [`decode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The frame is shorter than the fixed header.
    TooShort { len: usize, need: usize },
    /// The magic bytes are not `AMKT`.
    BadMagic([u8; 4]),
    /// The frame uses a schema version this decoder does not know.
    UnsupportedVersion(u8),
    /// The type code in the header is not defined.
    UnknownMessageType(u8),
    /// The header's body length does not match the bytes that follow it.
    BodyLengthMismatch { declared: u32, actual: usize },
    /// The body decoder finished before the declared body length.
    TrailingBytes { count: usize },
    /// A field ran past the end of the frame.
    Truncated { field: &'static str },
    /// An enum code has no defined value.
    InvalidCode { field: &'static str, code: u8 },
    /// A length prefix cannot fit in the remaining body; rejected before allocation.
    CountTooLarge {
        field: &'static str,
        count: usize,
        remaining: usize,
    },
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { len, need } => {
                write!(f, "frame is {len} bytes, need at least {need}")
            }
            Self::BadMagic(found) => write!(f, "bad magic: {found:02x?}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported wire version {version}")
            }
            Self::UnknownMessageType(code) => write!(f, "unknown message type {code}"),
            Self::BodyLengthMismatch { declared, actual } => {
                write!(
                    f,
                    "declared body length {declared} does not match {actual} bytes"
                )
            }
            Self::TrailingBytes { count } => write!(f, "{count} trailing body bytes"),
            Self::Truncated { field } => write!(f, "truncated while reading {field}"),
            Self::InvalidCode { field, code } => write!(f, "invalid {field} code {code}"),
            Self::CountTooLarge {
                field,
                count,
                remaining,
            } => write!(
                f,
                "{field} count {count} cannot fit in {remaining} remaining bytes"
            ),
        }
    }
}

impl std::error::Error for WireError {}

/// Fixed-size records, used to reject impossible length prefixes before allocation.
const TRADE_RECORD_LEN: usize = 120;
const LIFECYCLE_RECORD_LEN: usize = 123;
const DEPTH_LEVEL_LEN: usize = 37;

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn take(&mut self, len: usize, field: &'static str) -> Result<&'a [u8], WireError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(WireError::Truncated { field })?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(WireError::Truncated { field })?;
        self.offset = end;
        Ok(slice)
    }

    fn u8(&mut self, field: &'static str) -> Result<u8, WireError> {
        Ok(self.take(1, field)?[0])
    }

    fn u16(&mut self, field: &'static str) -> Result<u16, WireError> {
        let mut bytes = [0u8; 2];
        bytes.copy_from_slice(self.take(2, field)?);
        Ok(u16::from_le_bytes(bytes))
    }

    fn u32(&mut self, field: &'static str) -> Result<u32, WireError> {
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(self.take(4, field)?);
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self, field: &'static str) -> Result<u64, WireError> {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(self.take(8, field)?);
        Ok(u64::from_le_bytes(bytes))
    }

    fn i128(&mut self, field: &'static str) -> Result<i128, WireError> {
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(self.take(16, field)?);
        Ok(i128::from_le_bytes(bytes))
    }

    fn fixed(&mut self, field: &'static str) -> Result<Fixed, WireError> {
        Ok(Fixed::from_raw(self.i128(field)?))
    }

    fn count(&mut self, field: &'static str, record_len: usize) -> Result<usize, WireError> {
        let count = self.u32(field)? as usize;
        let remaining = self.remaining();
        if count > remaining / record_len {
            return Err(WireError::CountTooLarge {
                field,
                count,
                remaining,
            });
        }
        Ok(count)
    }
}

/// Decodes one frame produced by [`encode`].
///
/// The decoder is strict: it rejects a wrong magic, version or message type, a
/// body length that does not match the frame, trailing body bytes, invalid enum
/// codes and length prefixes that cannot fit the remaining body. It never
/// panics on malformed input, so consumers and fuzz targets can call it
/// directly.
///
/// `MarketMessage::stream` is not carried on the wire; it is derived from the
/// body type (`GAP` uses the `STATUS` stream).
///
/// The wire format encodes `counterparty_sequence = 0` for "no counterparty",
/// so a real maker sequence `0` decodes as `None`. Sequence `0` is valid for
/// the first order in an empty book; this is a known limitation.
pub fn decode(bytes: &[u8]) -> Result<MarketMessage, WireError> {
    if bytes.len() < HEADER_LEN {
        return Err(WireError::TooShort {
            len: bytes.len(),
            need: HEADER_LEN,
        });
    }

    let mut decoder = Decoder::new(bytes);
    let mut magic = [0u8; 4];
    magic.copy_from_slice(decoder.take(4, "magic")?);
    if magic != MAGIC {
        return Err(WireError::BadMagic(magic));
    }

    let version = decoder.u8("version")?;
    if version != VERSION {
        return Err(WireError::UnsupportedVersion(version));
    }
    let message_type = MessageType::from_code(decoder.u8("message type")?)?;
    let flags = decoder.u16("flags")?;
    let market_seq = decoder.u64("market_seq")?;
    let log_index = decoder.u64("log_index")?;
    let ordinal = decoder.u32("ordinal")?;
    let body_len = decoder.u32("body length")?;
    let epoch = decoder.u64("epoch")?;
    let leader_id = decoder.u64("leader_id")?;
    let ts_ms = decoder.u64("ts_ms")?;
    let symbol_id = decoder.u32("symbol_id")?;
    let sub_seq = decoder.u16("sub_seq")?;
    let _reserved = decoder.u16("reserved")?;

    let actual = bytes.len() - HEADER_LEN;
    if body_len as usize != actual {
        return Err(WireError::BodyLengthMismatch {
            declared: body_len,
            actual,
        });
    }

    let body = decode_body(message_type, &mut decoder)?;
    let trailing = decoder.remaining();
    if trailing != 0 {
        return Err(WireError::TrailingBytes { count: trailing });
    }

    Ok(MarketMessage {
        stream: message_type.stream(),
        flags,
        market_seq,
        sub_seq,
        log_index,
        ordinal,
        epoch,
        leader_id,
        ts_ms,
        symbol_id,
        body,
    })
}

fn decode_body(kind: MessageType, decoder: &mut Decoder<'_>) -> Result<MessageBody, WireError> {
    match kind {
        MessageType::Trade => {
            let count = decoder.count("trade count", TRADE_RECORD_LEN)?;
            let mut records = Vec::with_capacity(count);
            for _ in 0..count {
                records.push(decode_trade(decoder)?);
            }
            Ok(MessageBody::Trade(records))
        }
        MessageType::Bbo => Ok(MessageBody::Bbo(BboRecord {
            last_trade: decode_option_fixed(decoder, "bbo.last_trade")?,
            bid: decoder.fixed("bbo.bid")?,
            bid_volume: decoder.fixed("bbo.bid_volume")?,
            ask: decoder.fixed("bbo.ask")?,
            ask_volume: decoder.fixed("bbo.ask_volume")?,
            mid: decode_option_fixed(decoder, "bbo.mid")?,
            spread: decode_option_fixed(decoder, "bbo.spread")?,
        })),
        MessageType::DepthSnapshot => {
            Ok(MessageBody::DepthSnapshot(decode_depth_snapshot(decoder)?))
        }
        MessageType::DepthDelta => {
            let count = decoder.count("depth delta count", DEPTH_LEVEL_LEN)?;
            let mut updates = Vec::with_capacity(count);
            for _ in 0..count {
                let (side, level) = decode_level(decoder, "depth delta")?;
                updates.push(LevelUpdate { side, level });
            }
            Ok(MessageBody::DepthDelta(updates))
        }
        MessageType::Lifecycle => {
            let count = decoder.count("lifecycle count", LIFECYCLE_RECORD_LEN)?;
            let mut records = Vec::with_capacity(count);
            for _ in 0..count {
                records.push(decode_lifecycle(decoder)?);
            }
            Ok(MessageBody::Lifecycle(records))
        }
        MessageType::Status => Ok(MessageBody::Status(StatusRecord {
            last_market_seq: decoder.u64("status.last_market_seq")?,
            last_sub_seq: decoder.u16("status.last_sub_seq")?,
            last_log_index: decoder.u64("status.last_log_index")?,
            last_ordinal: decoder.u32("status.last_ordinal")?,
            applied_index: decoder.u64("status.applied_index")?,
            book_sequence: decoder.u64("status.book_sequence")?,
            pending_stops: decoder.u32("status.pending_stops")?,
        })),
        MessageType::Gap => Ok(MessageBody::Gap(GapRecord {
            last_good_log_index: decoder.u64("gap.last_good_log_index")?,
            last_good_ordinal: decoder.u32("gap.last_good_ordinal")?,
            dropped: decoder.u32("gap.dropped")?,
        })),
        MessageType::Bar => Ok(MessageBody::Bar(BarRecord {
            interval_ms: u64::from(decoder.u32("bar.interval_ms")?),
            window_start_ms: decoder.u64("bar.window_start_ms")?,
            last_ts_ms: decoder.u64("bar.last_ts_ms")?,
            open: decoder.fixed("bar.open")?,
            high: decoder.fixed("bar.high")?,
            low: decoder.fixed("bar.low")?,
            close: decoder.fixed("bar.close")?,
            volume: decoder.fixed("bar.volume")?,
            trade_count: decoder.u32("bar.trade_count")?,
            bar_seq: decoder.u64("bar.bar_seq")?,
        })),
    }
}

fn decode_trade(decoder: &mut Decoder<'_>) -> Result<TradeRecord, WireError> {
    Ok(TradeRecord {
        log_index: decoder.u64("trade.log_index")?,
        ordinal: decoder.u32("trade.ordinal")?,
        ts_ms: decoder.u64("trade.ts_ms")?,
        price: decoder.fixed("trade.price")?,
        volume: decoder.fixed("trade.volume")?,
        quote_volume: decoder.fixed("trade.quote_volume")?,
        taker_fee: decoder.fixed("trade.taker_fee")?,
        taker_fee_asset: decode_fee_asset(
            decoder.u8("trade.taker_fee_asset")?,
            "trade.taker_fee_asset",
        )?,
        maker_fee: decoder.fixed("trade.maker_fee")?,
        maker_fee_asset: decode_fee_asset(
            decoder.u8("trade.maker_fee_asset")?,
            "trade.maker_fee_asset",
        )?,
        error: {
            let bits = decoder.u8("trade.error")?;
            MatchErrors::from_bits(bits).ok_or(WireError::InvalidCode {
                field: "trade.error",
                code: bits,
            })?
        },
        taker_side: decode_side(decoder.u8("trade.taker_side")?, "trade.taker_side")?,
        taker_sequence: decoder.u64("trade.taker_sequence")?,
        maker_sequence: decoder.u64("trade.maker_sequence")?,
    })
}

fn decode_depth_snapshot(decoder: &mut Decoder<'_>) -> Result<DepthSnapshotRecord, WireError> {
    let bids = decoder.u32("depth.bids")? as usize;
    let asks = decoder.u32("depth.asks")? as usize;
    let total = bids.checked_add(asks).ok_or(WireError::CountTooLarge {
        field: "depth levels",
        count: usize::MAX,
        remaining: decoder.remaining(),
    })?;
    let remaining = decoder.remaining();
    if total > remaining / DEPTH_LEVEL_LEN {
        return Err(WireError::CountTooLarge {
            field: "depth levels",
            count: total,
            remaining,
        });
    }

    let mut bid_levels = Vec::with_capacity(bids);
    for _ in 0..bids {
        let (_, level) = decode_level(decoder, "depth.bid")?;
        bid_levels.push(level);
    }
    let mut ask_levels = Vec::with_capacity(asks);
    for _ in 0..asks {
        let (_, level) = decode_level(decoder, "depth.ask")?;
        ask_levels.push(level);
    }
    Ok(DepthSnapshotRecord {
        bids: bid_levels,
        asks: ask_levels,
    })
}

fn decode_level(
    decoder: &mut Decoder<'_>,
    field: &'static str,
) -> Result<(OrderSide, DepthLevel), WireError> {
    let side = decode_side(decoder.u8(field)?, field)?;
    let price = decoder.fixed(field)?;
    let volume = decoder.fixed(field)?;
    let orders = decoder.u32(field)?;
    Ok((
        side,
        DepthLevel {
            price,
            volume,
            orders,
        },
    ))
}

fn decode_lifecycle(decoder: &mut Decoder<'_>) -> Result<LifecycleRecord, WireError> {
    let log_index = decoder.u64("lifecycle.log_index")?;
    let ordinal = decoder.u32("lifecycle.ordinal")?;
    let ts_ms = decoder.u64("lifecycle.ts_ms")?;
    let kind = decode_kind(decoder.u8("lifecycle.kind")?)?;
    let order_sequence = decoder.u64("lifecycle.order_sequence")?;
    let side = decode_side(decoder.u8("lifecycle.side")?, "lifecycle.side")?;
    let counterparty = decoder.u64("lifecycle.counterparty_sequence")?;
    Ok(LifecycleRecord {
        log_index,
        ordinal,
        ts_ms,
        kind,
        order_sequence,
        side,
        counterparty_sequence: (counterparty != 0).then_some(counterparty),
        price: decoder.fixed("lifecycle.price")?,
        volume: decoder.fixed("lifecycle.volume")?,
        quote_volume: decoder.fixed("lifecycle.quote_volume")?,
        volume_unit: decode_volume_unit(
            decoder.u8("lifecycle.volume_unit")?,
            "lifecycle.volume_unit",
        )?,
        taker_fee: decoder.fixed("lifecycle.taker_fee")?,
        taker_fee_asset: decode_fee_asset(
            decoder.u8("lifecycle.taker_fee_asset")?,
            "lifecycle.taker_fee_asset",
        )?,
        maker_fee: decoder.fixed("lifecycle.maker_fee")?,
        maker_fee_asset: decode_fee_asset(
            decoder.u8("lifecycle.maker_fee_asset")?,
            "lifecycle.maker_fee_asset",
        )?,
        error: {
            let bits = decoder.u8("lifecycle.error")?;
            MatchErrors::from_bits(bits).ok_or(WireError::InvalidCode {
                field: "lifecycle.error",
                code: bits,
            })?
        },
        reason: decode_reason(decoder.u8("lifecycle.reason")?)?,
    })
}

fn decode_option_fixed(
    decoder: &mut Decoder<'_>,
    field: &'static str,
) -> Result<Option<Fixed>, WireError> {
    match decoder.u8(field)? {
        0 => {
            // The encoder writes a zero placeholder after the absence byte.
            decoder.take(16, field)?;
            Ok(None)
        }
        1 => Ok(Some(decoder.fixed(field)?)),
        code => Err(WireError::InvalidCode { field, code }),
    }
}

fn decode_fee_asset(code: u8, field: &'static str) -> Result<Option<FeeAsset>, WireError> {
    match code {
        0 => Ok(None),
        1 => Ok(Some(FeeAsset::Base)),
        2 => Ok(Some(FeeAsset::Quote)),
        _ => Err(WireError::InvalidCode { field, code }),
    }
}

fn decode_volume_unit(code: u8, field: &'static str) -> Result<VolumeUnit, WireError> {
    match code {
        0 => Ok(VolumeUnit::Base),
        1 => Ok(VolumeUnit::Quote),
        _ => Err(WireError::InvalidCode { field, code }),
    }
}

fn decode_side(code: u8, field: &'static str) -> Result<OrderSide, WireError> {
    match code {
        0 => Ok(OrderSide::Buy),
        1 => Ok(OrderSide::Sell),
        _ => Err(WireError::InvalidCode { field, code }),
    }
}

fn decode_kind(code: u8) -> Result<MatchEventKind, WireError> {
    match code {
        1 => Ok(MatchEventKind::Rested),
        2 => Ok(MatchEventKind::Fill),
        3 => Ok(MatchEventKind::Cancelled),
        4 => Ok(MatchEventKind::Triggered),
        5 => Ok(MatchEventKind::Expired),
        6 => Ok(MatchEventKind::Rejected),
        7 => Ok(MatchEventKind::Amended),
        _ => Err(WireError::InvalidCode {
            field: "lifecycle.kind",
            code,
        }),
    }
}

fn decode_reason(code: u8) -> Result<Option<RejectReason>, WireError> {
    match code {
        0 => Ok(None),
        1 => Ok(Some(RejectReason::FokUnfillable)),
        2 => Ok(Some(RejectReason::PostOnlyWouldCross)),
        3 => Ok(Some(RejectReason::StopWouldTrigger)),
        4 => Ok(Some(RejectReason::GtdAlreadyExpired)),
        5 => Ok(Some(RejectReason::OrderNotFound)),
        6 => Ok(Some(RejectReason::UnsupportedOrder)),
        7 => Ok(Some(RejectReason::AmendWouldCross)),
        8 => Ok(Some(RejectReason::InvalidOrder)),
        9 => Ok(Some(RejectReason::SelfTradePrevented)),
        10 => Ok(Some(RejectReason::PegWouldCross)),
        11 => Ok(Some(RejectReason::PegReferenceUnavailable)),
        _ => Err(WireError::InvalidCode {
            field: "lifecycle.reason",
            code,
        }),
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
        let stream = body.message_type().stream();
        MarketMessage {
            stream,
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
                error: MatchErrors::NONE,
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
                error: MatchErrors::NONE,
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
            error: MatchErrors::NONE,
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
                error: MatchErrors::NONE,
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
    fn wire_version_is_v7() {
        assert_eq!(7, VERSION);
        let bytes = encode(&message(MessageBody::Gap(GapRecord {
            last_good_log_index: 0,
            last_good_ordinal: 0,
            dropped: 0,
        })));
        assert_eq!(7, bytes[4]);
    }

    fn sample_trade() -> TradeRecord {
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
            error: MatchErrors::NOTIONAL_OVERFLOW | MatchErrors::MAKER_FEE_OVERFLOW,
            taker_side: OrderSide::Buy,
            taker_sequence: 4,
            maker_sequence: 2,
        }
    }

    fn sample_lifecycle() -> LifecycleRecord {
        LifecycleRecord {
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
            error: MatchErrors::TAKER_FEE_OVERFLOW,
            reason: None,
        }
    }

    fn sample_bar() -> BarRecord {
        BarRecord {
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
        }
    }

    fn assert_round_trip(body: MessageBody) {
        let original = message(body);
        let bytes = encode(&original);
        let decoded = decode(&bytes).expect("valid frame decodes");
        assert_eq!(original, decoded);
        assert_eq!(bytes, encode(&decoded));
    }

    #[test]
    fn decode_round_trips_every_body_type() {
        assert_round_trip(MessageBody::Trade(vec![sample_trade()]));
        assert_round_trip(MessageBody::Bbo(BboRecord {
            last_trade: Some(fixed("100")),
            bid: fixed("99.5"),
            bid_volume: fixed("3"),
            ask: fixed("100.5"),
            ask_volume: fixed("4"),
            mid: Some(fixed("100")),
            spread: Some(fixed("1")),
        }));
        assert_round_trip(MessageBody::Bbo(BboRecord {
            last_trade: None,
            bid: Fixed::ZERO,
            bid_volume: Fixed::ZERO,
            ask: Fixed::ZERO,
            ask_volume: Fixed::ZERO,
            mid: None,
            spread: None,
        }));
        assert_round_trip(MessageBody::DepthSnapshot(DepthSnapshotRecord {
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
        }));
        assert_round_trip(MessageBody::DepthDelta(vec![LevelUpdate {
            side: OrderSide::Sell,
            level: DepthLevel {
                price: fixed("99"),
                volume: fixed("4"),
                orders: 1,
            },
        }]));
        assert_round_trip(MessageBody::Lifecycle(vec![sample_lifecycle()]));
        assert_round_trip(MessageBody::Status(StatusRecord {
            last_market_seq: 12,
            last_sub_seq: 1,
            last_log_index: 9,
            last_ordinal: 1,
            applied_index: 10,
            book_sequence: 4,
            pending_stops: 2,
        }));
        assert_round_trip(MessageBody::Gap(GapRecord {
            last_good_log_index: 41,
            last_good_ordinal: 2,
            dropped: 1,
        }));
        assert_round_trip(MessageBody::Bar(sample_bar()));
    }

    const GOLDEN_GAP_HEX: &str = "414d4b5407070c0007000000000000002a000000000000000300000010000000090000000000000001000000000000000068e5cf8b010000000000000300000029000000000000000200000001000000";

    fn hex_bytes(hex: &str) -> Vec<u8> {
        assert_eq!(0, hex.len() % 2, "hex must have complete bytes");
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    #[test]
    fn golden_gap_frame_is_stable() {
        let bytes = hex_bytes(GOLDEN_GAP_HEX);
        assert_eq!(80, bytes.len());

        let decoded = decode(&bytes).expect("golden frame decodes");
        assert_eq!(STREAM_STATUS, decoded.stream);
        assert_eq!(flags::GAP | flags::END_OF_BATCH, decoded.flags);
        assert_eq!(7, decoded.market_seq);
        assert_eq!(42, decoded.log_index);
        assert_eq!(3, decoded.ordinal);
        assert_eq!(9, decoded.epoch);
        assert_eq!(1, decoded.leader_id);
        assert_eq!(1_700_000_000_000, decoded.ts_ms);
        assert_eq!(0, decoded.symbol_id);
        assert_eq!(3, decoded.sub_seq);
        assert_eq!(
            MessageBody::Gap(GapRecord {
                last_good_log_index: 41,
                last_good_ordinal: 2,
                dropped: 1,
            }),
            decoded.body
        );
        assert_eq!(bytes, encode(&decoded));
    }

    #[test]
    fn decode_rejects_malformed_frames() {
        let golden = hex_bytes(GOLDEN_GAP_HEX);

        assert!(matches!(
            decode(&[]),
            Err(WireError::TooShort {
                len: 0,
                need: HEADER_LEN
            })
        ));

        let mut bad = golden.clone();
        bad[0] = 0;
        assert!(matches!(decode(&bad), Err(WireError::BadMagic(_))));

        let mut bad = golden.clone();
        bad[4] = 4;
        assert!(matches!(
            decode(&bad),
            Err(WireError::UnsupportedVersion(4))
        ));

        let mut bad = golden.clone();
        bad[4] = 6;
        assert!(matches!(
            decode(&bad),
            Err(WireError::UnsupportedVersion(6))
        ));

        let mut bad = golden.clone();
        bad[5] = 99;
        assert!(matches!(
            decode(&bad),
            Err(WireError::UnknownMessageType(99))
        ));

        let mut bad = golden.clone();
        bad[28..32].copy_from_slice(&15u32.to_le_bytes());
        assert!(matches!(
            decode(&bad),
            Err(WireError::BodyLengthMismatch {
                declared: 15,
                actual: 16
            })
        ));

        let mut trailing = golden.clone();
        trailing.push(0);
        trailing[28..32].copy_from_slice(&17u32.to_le_bytes());
        assert!(matches!(
            decode(&trailing),
            Err(WireError::TrailingBytes { count: 1 })
        ));

        let mut truncated = encode(&message(MessageBody::Status(StatusRecord {
            last_market_seq: 12,
            last_sub_seq: 1,
            last_log_index: 9,
            last_ordinal: 1,
            applied_index: 10,
            book_sequence: 4,
            pending_stops: 2,
        })));
        truncated.pop();
        truncated[28..32].copy_from_slice(&41u32.to_le_bytes());
        assert!(matches!(
            decode(&truncated),
            Err(WireError::Truncated { .. })
        ));

        let mut bad_side = encode(&message(MessageBody::Trade(vec![sample_trade()])));
        bad_side[HEADER_LEN + 107] = 9;
        assert!(matches!(
            decode(&bad_side),
            Err(WireError::InvalidCode {
                field: "trade.taker_side",
                code: 9
            })
        ));

        let mut huge_count = encode(&message(MessageBody::Trade(vec![sample_trade()])));
        huge_count[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            decode(&huge_count),
            Err(WireError::CountTooLarge { .. })
        ));

        let mut bad_option = encode(&message(MessageBody::Bbo(BboRecord {
            last_trade: None,
            bid: Fixed::ZERO,
            bid_volume: Fixed::ZERO,
            ask: Fixed::ZERO,
            ask_volume: Fixed::ZERO,
            mid: None,
            spread: None,
        })));
        bad_option[HEADER_LEN] = 2;
        assert!(matches!(
            decode(&bad_option),
            Err(WireError::InvalidCode {
                field: "bbo.last_trade",
                code: 2
            })
        ));
    }
}

#[cfg(test)]
mod schema_v11_reason_tests {
    use super::*;

    #[test]
    fn new_reason_codes_round_trip() {
        for (reason, code) in [
            (RejectReason::SelfTradePrevented, 9),
            (RejectReason::PegWouldCross, 10),
            (RejectReason::PegReferenceUnavailable, 11),
        ] {
            assert_eq!(code, reason_code(Some(reason)));
            assert_eq!(Some(reason), decode_reason(code).unwrap());
        }
    }
}

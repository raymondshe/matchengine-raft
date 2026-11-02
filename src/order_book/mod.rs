use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;

#[cfg(test)]
mod conformance_tests;
pub mod fixed;
#[cfg(test)]
mod property_tests;
pub use fixed::AssetClass;
pub use fixed::AssetId;
pub use fixed::FeeAsset;
pub use fixed::FeeAssetPolicy;
pub use fixed::Fixed;
pub use fixed::MAX_FEE_RATE;
pub use fixed::MIN_FEE_RATE;
pub use fixed::SymbolConfig;
pub use fixed::VolumeUnit;

/// Specifies whether an order is a buy (bid) or sell (ask) order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderSide {
    /// Buy order - trader wants to purchase an asset.
    Buy,
    /// Sell order - trader wants to sell an asset.
    Sell,
}

/// Specifies the type of operation to perform on an order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderCmd {
    /// Place a new order in the order book.
    Place,
    /// Cancel an existing order from the order book.
    Cancel,
}

/// Specifies the type of order (execution style).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderType {
    /// Market order - executes immediately at the best available price,
    /// ignoring its own `price`. Whatever it cannot execute is cancelled, so a
    /// market order never rests.
    Market,
    /// Limit order - executes only at the specified price or better.
    Limit,
}

/// How long an order stays live: the classic time-in-force policies.
///
/// Variants and semantics mirror `pricelevel::TimeInForce` from
/// [OrderBook-rs](https://github.com/joaquinbejar/OrderBook-rs): `Ioc` and
/// `Fok` are immediate (whatever does not execute is cancelled, never
/// rested), `Gtd` carries its expiry as a Unix timestamp in milliseconds, and
/// `Day` expires at the trading-day close when the caller supplies one.
///
/// Expiry is never evaluated against a wall clock inside the state machine:
/// every mutating request carries the leader's intake timestamp through the
/// log and purges due GTD orders before applying, while `Day` waits for an
/// explicit [`crate::store::MatchEngineRequest::PurgeExpired`] with the trading
/// day's close. Every replica evicts exactly the same orders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TimeInForce {
    /// Good-'til-cancelled: rests until it fills or is cancelled (the default).
    #[default]
    #[serde(alias = "gtc")]
    Gtc,
    /// Immediate-or-cancel: execute what crosses now, cancel the remainder.
    #[serde(alias = "ioc")]
    Ioc,
    /// Fill-or-kill: execute everything now or cancel the whole order.
    #[serde(alias = "fok")]
    Fok,
    /// Good-'til-date: rests until `expiry` (Unix timestamp in milliseconds).
    #[serde(alias = "gtd")]
    Gtd(u64),
    /// Good for the day: rests until the trading-day close supplied with the
    /// expiry purge. Without a close it behaves like GTC.
    #[serde(alias = "day")]
    Day,
}

impl TimeInForce {
    /// `true` when the unfilled part of such an order is cancelled after the
    /// sweep instead of resting (IOC and FOK).
    pub fn is_immediate(&self) -> bool {
        matches!(self, Self::Ioc | Self::Fok)
    }

    /// `true` when the order can be removed by an expiry purge.
    pub fn has_expiry(&self) -> bool {
        matches!(self, Self::Gtd(_) | Self::Day)
    }

    /// Whether an order with this time in force has expired at `now_ms`.
    ///
    /// `Gtd` expires at or after its deadline. `Day` expires at the close, but
    /// only when the caller supplies one; `day_close_ms: None` keeps Day
    /// orders live (the same rule `pricelevel` uses).
    pub fn is_expired(&self, now_ms: u64, day_close_ms: Option<u64>) -> bool {
        match self {
            Self::Gtd(expiry) => now_ms >= *expiry,
            Self::Day => day_close_ms.is_some_and(|close| now_ms >= close),
            _ => false,
        }
    }
}

/// Why the engine refused an order outright: it never entered the book and
/// nothing executed for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// Fill-or-kill: the book could not fill the whole order at its price.
    FokUnfillable,
    /// Post-only: the order would have crossed the book, so it was refused
    /// instead of taking liquidity.
    PostOnlyWouldCross,
    /// Stop order: the last trade price already crossed the stop price, and a
    /// stop never turns into an order on entry (it only triggers on new
    /// trades).
    StopWouldTrigger,
    /// Good-'til-date: the expiry is not after the order's own timestamp.
    GtdAlreadyExpired,
    /// Amend: no resting order matches the supplied key.
    OrderNotFound,
    /// Amend: the order cannot be amended (a stop or a market order, which do
    /// not rest on the book).
    UnsupportedOrder,
    /// Amend: the new price would cross the opposite side. A resting book must
    /// never cross; cancel and place a new order to take liquidity.
    AmendWouldCross,
    /// Volume, price or an unsupported type combination is out of range.
    InvalidOrder,
}

/// Represents a single order in the order book.
///
/// An order contains all necessary information about a trader's request
/// to buy or sell an asset, including price, volume, and timing information
/// for matching and prioritization.
///
/// The plain fields (side/price/volume plus [`OrderType`]) are the core
/// order; the optional fields layer the additional types on top of the same
/// structure, mirroring how OrderBook-rs layers time in force, post-only,
/// icebergs and stops over one order record. Every new field has a serde
/// default, so payloads written before they existed still deserialize.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Order {
    /// The type of order (Limit or Market).
    pub live: OrderType,
    /// Whether this is a Buy or Sell order.
    pub side: OrderSide,
    /// Instrument id. It must match this process/cluster's configured
    /// [`crate::order_book::fixed::SymbolConfig`], which controls accepted
    /// price and quantity precision; values are stored at the common 18-decimal
    /// scale. Multi-symbol means a separate process/cluster per symbol.
    #[serde(default)]
    pub symbol_id: u32,
    /// The price level for this order (for limit orders), scale 18.
    pub price: Fixed,
    /// The quantity of the asset to trade.
    ///
    /// For a resting iceberg order this is the currently displayed tranche;
    /// [`Order::open_volume`] reports the tranche plus the hidden reserve.
    pub volume: Fixed,

    /// Gross size unit. `Base` means `volume` is a base quantity; `Quote`
    /// means it is a quote amount. Quote limit orders are normalized to base
    /// before resting; quote market orders use quote-budget matching.
    #[serde(default)]
    pub volume_unit: VolumeUnit,
    /// The client's gross amount in `requested_volume_unit`, retained for
    /// audit when a quote limit order is normalized to base. Zero means the
    /// original amount was `volume`.
    #[serde(default)]
    pub requested_volume: Fixed,
    /// The unit of `requested_volume`.
    #[serde(default)]
    pub requested_volume_unit: VolumeUnit,
    /// Maker fee rate as a signed fraction of notional, set from the user
    /// level at placement (`"0.001"` = 10 bps; negative = rebate).
    #[serde(default)]
    pub maker_fee_rate: Fixed,
    /// Taker fee rate as a signed fraction of notional, set from the user
    /// level at placement (`"0.001"` = 10 bps; negative = rebate).
    #[serde(default)]
    pub taker_fee_rate: Fixed,

    /// Unique identifier for the order.
    pub id: u64,
    /// Sequence number for ordering orders at the same price level.
    ///
    /// This ensures price-time priority - orders with lower sequence
    /// numbers are matched first at the same price level. Optional on input:
    /// `apply` assigns the real sequence and clients may omit it.
    #[serde(default)]
    pub sequence: u64,

    /// Time in force ([`TimeInForce`]); defaults to GTC.
    #[serde(default)]
    pub tif: TimeInForce,
    /// Post-only: never take liquidity. If the order would cross the book it
    /// is refused outright ([`RejectReason::PostOnlyWouldCross`]) instead of
    /// trading; otherwise it rests in full like a GTC limit.
    #[serde(default)]
    pub post_only: bool,
    /// Stop (trigger) price; `0.0` means the order is not a stop. When set,
    /// the order parks off the book until the last trade price elects it
    /// (sell: a trade at or below the stop; buy: a trade at or above it), then
    /// executes immediately as this order's `live` type — `Limit` makes it a
    /// stop-limit, `Market` a stop-market. The child never rests: whatever it
    /// cannot execute is cancelled.
    ///
    /// Prices are positive, so `0.0` doubles as "none" and keeps [`Order`]
    /// small enough to stay cheap to copy on the matching path.
    #[serde(default)]
    pub stop_price: Fixed,
    /// Iceberg display size (`0.0` means the order is not an iceberg): the
    /// visible tranche of a resting iceberg order. Whatever the sweep leaves
    /// over rests in [`Order::hidden_volume`], out of sight of depth and
    /// matching, and is drawn back into `volume` one tranche at a time as the
    /// visible part is consumed.
    #[serde(default)]
    pub display_volume: Fixed,
    /// Hidden reserve of a resting iceberg order. Invisible to depth and
    /// matching until the visible tranche is consumed.
    #[serde(default)]
    pub hidden_volume: Fixed,
    /// Intake timestamp (Unix ms), stamped by the receiving leader before the
    /// order is proposed (client values are overwritten). It is recorded in the
    /// log, validates GTD expiries, triggers the automatic GTD purge for every
    /// mutating request, and labels every market-data record derived from the
    /// order; `apply` itself never reads a clock.
    #[serde(default)]
    pub timestamp: u64,
}

impl Order {
    /// Creates a new GTC limit order with the specified parameters.
    ///
    /// # Arguments
    ///
    /// * `side` - Whether this is a Buy or Sell order
    /// * `price` - The price level for the limit order
    /// * `size` - The quantity to trade
    /// * `sequence` - Sequence number for price-time priority
    ///
    /// # Returns
    ///
    /// A new Order instance with `OrderType::Limit` and `TimeInForce::Gtc`.
    /// The identifier stays 0: never rebuild a resting order with this.
    pub fn new(
        side: OrderSide,
        price: impl Into<Fixed>,
        size: impl Into<Fixed>,
        sequence: u64,
    ) -> Self {
        Self {
            live: OrderType::Limit,
            side,
            symbol_id: 0,
            price: price.into(),
            volume: size.into(),
            volume_unit: VolumeUnit::Base,
            requested_volume: Fixed::ZERO,
            requested_volume_unit: VolumeUnit::Base,
            maker_fee_rate: Fixed::ZERO,
            taker_fee_rate: Fixed::ZERO,
            id: 0,
            sequence,
            tif: TimeInForce::Gtc,
            post_only: false,
            stop_price: Fixed::ZERO,
            display_volume: Fixed::ZERO,
            hidden_volume: Fixed::ZERO,
            timestamp: 0,
        }
    }

    /// Sets the instrument id without changing any other field.
    pub fn with_symbol(mut self, symbol_id: u32) -> Self {
        self.symbol_id = symbol_id;
        self
    }

    /// Sets the maker and taker fee rates, both fractions of notional.
    pub fn with_fee_rates(mut self, maker: impl Into<Fixed>, taker: impl Into<Fixed>) -> Self {
        self.maker_fee_rate = maker.into();
        self.taker_fee_rate = taker.into();
        self
    }

    /// Sets the client volume unit; `volume` remains the gross amount in it.
    pub fn with_volume_unit(mut self, unit: VolumeUnit) -> Self {
        self.volume_unit = unit;
        self
    }

    /// Total open volume: the visible part plus any iceberg reserve.
    ///
    /// This is what a cancel or expiry of this order removes, and what a FOK
    /// preflight may draw on.
    pub fn open_volume(&self) -> Fixed {
        self.volume + self.hidden_volume
    }

    /// `true` when the order is parked off the book until its stop price
    /// triggers.
    pub fn is_stop(&self) -> bool {
        self.stop_price > Fixed::ZERO
    }

    /// Whether a trade at `last_trade_price` elects this stop order: a sell
    /// stop fires at or below its stop price, a buy stop at or above it.
    fn stop_elected(&self, last_trade_price: Fixed) -> bool {
        match self.side {
            OrderSide::Buy => last_trade_price >= self.stop_price,
            OrderSide::Sell => last_trade_price <= self.stop_price,
        }
    }
}

/// Equality for orders compares the identifying fields and the current size.
///
/// `id`, `side`, `symbol_id` and `sequence` distinguish two orders even when
/// their price and volume happen to match; the price and volume are included
/// so a partial fill or amendment is observable.
impl PartialEq for Order {
    fn eq(&self, other: &Self) -> bool {
        (self.id == other.id)
            && (self.side == other.side)
            && (self.symbol_id == other.symbol_id)
            && (self.sequence == other.sequence)
            && (self.price == other.price)
            && (self.volume == other.volume)
    }
}

impl Eq for Order {}

/// Key used to order buy (bid) orders in the BTreeMap.
///
/// BidKey implements reverse price ordering (highest price first) and
/// FIFO ordering at the same price level (lowest sequence first).
/// This ensures the best bid price is always at the front of the book.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BidKey {
    /// The price level of this bid order.
    pub price: Fixed,
    /// Sequence number for FIFO ordering at the same price level.
    pub sequence: u64,
}

/// Equality for BidKey based solely on sequence number.
impl PartialEq for BidKey {
    fn eq(&self, other: &Self) -> bool {
        self.sequence == other.sequence
    }
}

impl Eq for BidKey {}

/// Ordering implementation for BidKey (buy orders).
///
/// Orders first by price descending (higher prices come first),
/// then by sequence ascending (earlier orders at same price come first).
impl Ord for BidKey {
    fn cmp(&self, other: &Self) -> Ordering {
        // Notice that the we flip the ordering on costs.
        // In case of a tie we compare positions - this step is necessary
        // to make implementations of `PartialEq` and `Ord` consistent.
        if self.price < other.price {
            Ordering::Greater
        } else if self.price > other.price {
            Ordering::Less
        } else {
            self.sequence.cmp(&other.sequence)
        }
    }
}

/// PartialOrd implementation delegating to Ord.
impl PartialOrd for BidKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Key used to order sell (ask) orders in the BTreeMap.
///
/// AskKey implements natural price ordering (lowest price first) and
/// FIFO ordering at the same price level (lowest sequence first).
/// This ensures the best ask price is always at the front of the book.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AskKey {
    /// The price level of this ask order.
    pub price: Fixed,
    /// Sequence number for FIFO ordering at the same price level.
    pub sequence: u64,
}

/// Equality for AskKey based solely on sequence number.
impl PartialEq for AskKey {
    fn eq(&self, other: &Self) -> bool {
        self.sequence == other.sequence
    }
}

impl Eq for AskKey {}

/// Ordering implementation for AskKey (sell orders).
///
/// Orders first by price ascending (lower prices come first),
/// then by sequence ascending (earlier orders at same price come first).
impl Ord for AskKey {
    fn cmp(&self, other: &Self) -> Ordering {
        // Notice that the we flip the ordering on costs.
        // In case of a tie we compare positions - this step is necessary
        // to make implementations of `PartialEq` and `Ord` consistent.
        if self.price < other.price {
            Ordering::Less
        } else if self.price > other.price {
            Ordering::Greater
        } else {
            self.sequence.cmp(&other.sequence)
        }
    }
}

/// PartialOrd implementation delegating to Ord.
impl PartialOrd for AskKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Serialization utilities for BTreeMap to vector conversion.
///
/// This module provides custom serialization for BTreeMaps to work around
/// serde's limitations with float keys. Maps are serialized as vectors
/// of key-value pairs.
pub mod vectorize {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::iter::FromIterator;

    /// Serializes a map-like structure as a vector of key-value pairs.
    ///
    /// This allows serialization of maps with non-standard key types
    /// (like floats wrapped in structs) that serde wouldn't otherwise handle.
    pub fn serialize<'a, T, K, V, S>(target: T, ser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: IntoIterator<Item = (&'a K, &'a V)>,
        K: Serialize + 'a,
        V: Serialize + 'a,
    {
        let container: Vec<_> = target.into_iter().collect();
        serde::Serialize::serialize(&container, ser)
    }

    /// Deserializes a vector of key-value pairs back into a map-like structure.
    ///
    /// Reverses the serialization performed by `serialize`.
    pub fn deserialize<'de, T, K, V, D>(des: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: FromIterator<(K, V)>,
        K: Deserialize<'de>,
        V: Deserialize<'de>,
    {
        let container: Vec<_> = serde::Deserialize::deserialize(des)?;
        Ok(T::from_iter(container))
    }
}

/// The main order book structure containing both bids and asks.
///
/// The order book maintains all active orders in two sorted collections:
/// - Bids (buy orders) sorted by price descending, then sequence ascending
/// - Asks (sell orders) sorted by price ascending, then sequence ascending
///
/// Stop orders are not in either map: while parked they are off the book,
/// invisible to matching and depth, and are held in `stops` in admission
/// order until a trade elects them.
///
/// This implements a standard price-time priority matching engine.
#[derive(Clone, Default, Serialize, Deserialize, Debug)]
pub struct OrderBook {
    /// Collection of buy orders (bids) sorted with best price first.
    #[serde(with = "vectorize")]
    pub bids: BTreeMap<BidKey, Order>,
    /// Collection of sell orders (asks) sorted with best price first.
    #[serde(with = "vectorize")]
    pub asks: BTreeMap<AskKey, Order>,
    /// Sequence counter for assigning order sequence numbers.
    pub sequence: u64,
    /// Parked stop orders, in admission (sequence) order. Elected stops are
    /// removed and executed as immediate takers; they never rest.
    #[serde(default)]
    pub stops: Vec<Order>,
    /// Price of the most recent trade, if there has been one. This is what
    /// elects parked stops, so it is part of the replicated state.
    #[serde(default)]
    pub last_trade_price: Option<Fixed>,
    /// Transient journal of visible-depth changes for the market-data tap.
    /// Never part of a snapshot; drained after each applied entry.
    #[serde(skip)]
    pub depth_journal: Vec<DepthChange>,
    /// Whether book mutations append to [`OrderBook::depth_journal`]. Enabled
    /// once when a market-data tap is installed, so the matching loop pays one
    /// predictable branch per mutation and nothing else when it is off.
    #[serde(skip)]
    pub record_depth: bool,
    /// Earliest GTD deadline among resting and parked orders, when known. The
    /// automatic purge checks this in O(1) and only scans the book when a
    /// deadline is actually due.
    #[serde(skip)]
    pub earliest_gtd_expiry: Option<u64>,
    /// Set when a GTD order left the book; the earliest deadline is recomputed
    /// lazily on the next mutation.
    #[serde(skip)]
    gtd_scan_dirty: bool,
}

/// Represents the result of a successful order match.
///
/// A MatchResult is created when a taker order crosses the spread and
/// matches with one or more maker orders already in the book.
#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub struct MatchResult {
    /// The incoming order that triggered the match.
    pub taker: Order,
    /// The resting order in the book that was matched against.
    pub maker: Order,
    /// The quantity that was matched between the two orders.
    pub matches: Fixed,
    /// Maker price × matched quantity, in quote units at scale 18. Zero when
    /// the notional does not fit the fixed-point range.
    pub notional: Fixed,
    /// Taker fee: positive = charge, negative = rebate. Zero when the fee
    /// cannot be represented exactly; see `fee_overflow`.
    pub taker_fee: Fixed,
    /// Asset in which the taker fee is charged or rebated.
    pub taker_fee_asset: FeeAsset,
    /// Maker fee: positive = charge, negative = rebate. Zero when the fee
    /// cannot be represented exactly; see `fee_overflow`.
    pub maker_fee: Fixed,
    /// Asset in which the maker fee is charged or rebated.
    pub maker_fee_asset: FeeAsset,
    /// True when the notional or either fee overflowed the scale-18 `i128`
    /// range. The fill itself is still valid; downstream must not assume the
    /// zero fee amounts are real.
    pub fee_overflow: bool,
}

/// Equality comparison for match results.
impl PartialEq for MatchResult {
    fn eq(&self, other: &Self) -> bool {
        (self.taker == other.taker)
            && (self.maker == other.maker)
            && (self.matches == other.matches)
            && (self.notional == other.notional)
            && (self.taker_fee == other.taker_fee)
            && (self.taker_fee_asset == other.taker_fee_asset)
            && (self.maker_fee == other.maker_fee)
            && (self.maker_fee_asset == other.maker_fee_asset)
            && (self.fee_overflow == other.fee_overflow)
    }
}

impl MatchResult {
    /// Creates a new MatchResult with the given orders and matched quantity.
    ///
    /// # Arguments
    ///
    /// * `taker` - The incoming order that triggered the match
    /// * `maker` - The resting order in the book
    /// * `matches` - The quantity that was matched
    pub fn new(taker: &Order, maker: &Order, matches: impl Into<Fixed>) -> Self {
        Self::new_with_policy(taker, maker, matches, FeeAssetPolicy::Received)
    }

    /// Creates a fill and computes fees according to a pair-level fee policy.
    pub fn new_with_policy(
        taker: &Order,
        maker: &Order,
        matches: impl Into<Fixed>,
        policy: FeeAssetPolicy,
    ) -> Self {
        let matches = matches.into();
        let notional = Fixed::notional(maker.price, matches);
        let taker_fee_asset = policy.fee_asset(taker.side);
        let maker_fee_asset = policy.fee_asset(maker.side);
        let fee = |asset: FeeAsset, rate: Fixed| match asset {
            FeeAsset::Base => matches.checked_mul(rate),
            FeeAsset::Quote => notional.and_then(|value| value.checked_mul(rate)),
        };
        let taker_fee = fee(taker_fee_asset, taker.taker_fee_rate);
        let maker_fee = fee(maker_fee_asset, maker.maker_fee_rate);
        Self {
            taker: *taker,
            maker: *maker,
            matches,
            notional: notional.unwrap_or(Fixed::ZERO),
            taker_fee: taker_fee.unwrap_or(Fixed::ZERO),
            taker_fee_asset,
            maker_fee: maker_fee.unwrap_or(Fixed::ZERO),
            maker_fee_asset,
            fee_overflow: notional.is_none() || taker_fee.is_none() || maker_fee.is_none(),
        }
    }
}

/// How the sweep tracks the taker's remaining size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweepMode {
    Base,
    BuyQuote,
    SellQuote,
}

/// Everything one submitted order did to the book: what
/// [`OrderBook::execute_order`] reports so the state machine can turn it into
/// events and a response.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// Replicated market/mutation sequence assigned to this submission. For a
    /// `Place` it is also the taker order's sequence.
    pub market_seq: u64,
    /// Resting orders evicted by the automatic GTD purge before this request
    /// was applied, in deterministic order.
    pub expired: Vec<Order>,
    /// Trades the submitted order executed, in execution order.
    pub fills: Vec<MatchResult>,
    /// Open volume the submission left resting in the book (the visible
    /// tranche for an iceberg); `None` when nothing rests.
    pub resting: Option<Fixed>,
    /// Volume of the submission that was cancelled without executing (an IOC
    /// remainder, a market remainder, or FOK rounding dust).
    pub cancelled: Option<Fixed>,
    /// The order was parked off the book as a stop order.
    pub parked: bool,
    /// The order was refused outright; it never entered the book.
    pub rejected: Option<RejectReason>,
    /// Parked stops the submission's trades elected, in execution order.
    pub triggers: Vec<StopExecution>,
}

/// One elected stop order: its election, its child execution, and what the
/// child could not execute.
#[derive(Debug, Clone)]
pub struct StopExecution {
    /// The stop as it was parked, with its open volume at election time.
    pub stop: Order,
    /// Last trade price that elected the stop.
    pub trigger_price: Fixed,
    /// Trades the stop's child executed (printed at maker prices).
    pub fills: Vec<MatchResult>,
    /// Child volume that could not execute; cancelled, never rested.
    pub cancelled: Option<Fixed>,
}

/// A successful amendment: the order as it now rests, plus where it came from.
#[derive(Debug, Clone)]
pub struct Amendment {
    /// Replicated market/mutation sequence assigned to this amendment.
    pub market_seq: u64,
    /// The resting order after the amendment. Its `sequence` is the identity to
    /// use from now on.
    pub order: Order,
    /// Sequence the order had before the amendment.
    pub previous_sequence: u64,
    /// Price the order rested at before the amendment.
    pub previous_price: Fixed,
    /// Open volume the order had before the amendment.
    pub previous_volume: Fixed,
}

/// Why an amendment was refused. Nothing changes on any of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmendError {
    /// No resting order matches the key (side + current price + sequence).
    NotFound,
    /// The order does not rest on the book (a parked stop or a market order).
    NotResting,
    /// Icebergs are not amendable: their hidden reserve has no well-defined
    /// meaning for a replacement volume.
    UnsupportedType,
    /// The new price is not a positive finite number.
    InvalidPrice,
    /// The new volume is not a positive finite number.
    InvalidVolume,
    /// The new price would cross the opposite side.
    WouldCross,
}

impl AmendError {
    /// Maps the error onto the reason published on a `rejected` event.
    pub fn reason(self) -> RejectReason {
        match self {
            AmendError::NotFound => RejectReason::OrderNotFound,
            AmendError::NotResting | AmendError::UnsupportedType => RejectReason::UnsupportedOrder,
            AmendError::InvalidPrice | AmendError::InvalidVolume => RejectReason::InvalidOrder,
            AmendError::WouldCross => RejectReason::AmendWouldCross,
        }
    }
}

/// One aggregated price level of the book, as returned by [`OrderBook::depth`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DepthLevel {
    /// Price of the level, scale 18.
    pub price: Fixed,
    /// Total visible volume resting at the level (hidden iceberg reserve is
    /// excluded, matching what the book itself can trade), scale 18.
    pub volume: Fixed,
    /// Number of orders at the level.
    pub orders: u32,
}

/// A net change to one visible price level, recorded while the book mutates.
///
/// The journal is a transient tap for market-data derivation: it carries only
/// values the book operation already knows, so recording it costs one push and
/// never walks the tree. It is disabled unless a market-data tap is attached.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DepthChange {
    /// Side of the book the level belongs to.
    pub side: OrderSide,
    /// Price of the level, scale 18.
    pub price: Fixed,
    /// Change in visible resting volume (hidden iceberg reserve is excluded),
    /// scale 18.
    pub delta_volume: Fixed,
    /// Change in the number of resting orders at the level.
    pub delta_orders: i32,
}

/// Shared mutable state threaded through one sweep of a resting side.
struct SweepContext<'a> {
    config: &'a SymbolConfig,
    record: bool,
    journal: &'a mut Vec<DepthChange>,
    track_gtd: bool,
    gtd_dirty: &'a mut bool,
}

impl OrderBook {
    /// Creates a new, empty order book.
    ///
    /// The order book starts with no bids or asks, no parked stops, and the
    /// sequence counter initialized to 0.
    pub fn new() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            sequence: 0,
            stops: Vec::new(),
            last_trade_price: None,
            depth_journal: Vec::new(),
            record_depth: false,
            earliest_gtd_expiry: None,
            gtd_scan_dirty: false,
        }
    }

    /// Takes the next replicated market/mutation sequence.
    ///
    /// Every mutating request consumes exactly one: `Place` uses it as the
    /// order sequence, `Cancel`/`PurgeExpired` as the event anchor, and `Amend`
    /// as the event anchor (and the re-inserted order's sequence when priority
    /// is lost). The counter is part of the replicated state and snapshots.
    pub fn next_market_sequence(&mut self) -> u64 {
        let sequence = self.sequence;
        self.sequence += 1;
        sequence
    }

    /// Recomputes the earliest GTD deadline after a GTD order left the book.
    fn refresh_earliest_gtd(&mut self) {
        if !self.gtd_scan_dirty {
            return;
        }
        self.gtd_scan_dirty = false;
        let mut earliest: Option<u64> = None;
        for order in self
            .bids
            .values()
            .chain(self.asks.values())
            .chain(self.stops.iter())
        {
            if let TimeInForce::Gtd(expiry) = order.tif {
                earliest = Some(earliest.map_or(expiry, |current| current.min(expiry)));
            }
        }
        self.earliest_gtd_expiry = earliest;
    }

    /// Rebuilds the derived indexes after a snapshot restore.
    pub fn rebuild_derived_indexes(&mut self) {
        self.gtd_scan_dirty = true;
        self.refresh_earliest_gtd();
    }

    /// Whether any order could be evicted by the automatic GTD purge.
    ///
    /// The hot path checks this before calling [`OrderBook::purge_due_gtd`], so
    /// a book without GTD orders pays one predictable branch per mutation.
    #[inline]
    pub fn has_gtd_deadlines(&self) -> bool {
        self.earliest_gtd_expiry.is_some() || self.gtd_scan_dirty
    }

    /// Purges GTD orders due at `now_ms`, doing no book scan when the earliest
    /// deadline is still in the future.
    #[inline]
    pub fn purge_due_gtd(&mut self, now_ms: u64) -> Vec<Order> {
        self.refresh_earliest_gtd();
        if self
            .earliest_gtd_expiry
            .is_some_and(|expiry| now_ms >= expiry)
        {
            self.purge_expired(now_ms, None)
        } else {
            Vec::new()
        }
    }

    /// Enables the transient depth journal (a market-data tap was installed).
    ///
    /// Idempotent; the journal keeps whatever it already holds so a caller can
    /// drain it with [`OrderBook::take_depth_journal`].
    pub fn enable_depth_journal(&mut self) {
        self.record_depth = true;
    }

    /// Drains the depth journal, leaving it empty.
    ///
    /// Returns an empty vector when the journal is disabled: the tap is the only
    /// consumer and it is either installed at start-up or not at all.
    pub fn take_depth_journal(&mut self) -> Vec<DepthChange> {
        std::mem::take(&mut self.depth_journal)
    }

    /// Appends one depth change when the journal is enabled.
    fn record_depth_change(
        &mut self,
        side: OrderSide,
        price: Fixed,
        delta_volume: Fixed,
        delta_orders: i32,
    ) {
        if self.record_depth {
            self.depth_journal.push(DepthChange {
                side,
                price,
                delta_volume,
                delta_orders,
            });
        }
    }

    /// Returns the current best (highest) bid price order.
    ///
    /// The best bid is the highest price any buyer is willing to pay.
    /// Returns `None` if there are no bid orders in the book.
    pub fn best_bid(&self) -> Option<&Order> {
        self.bids.first_key_value().map(|(_key, value)| value)
    }

    /// Returns the current best (lowest) ask price order.
    ///
    /// The best ask is the lowest price any seller is willing to accept.
    /// Returns `None` if there are no ask orders in the book.
    pub fn best_ask(&self) -> Option<&Order> {
        self.asks.first_key_value().map(|(_key, value)| value)
    }

    /// Best ask minus best bid, if both sides of the book are populated.
    ///
    /// A crossed book never exists after a successful submission, so the
    /// spread is non-negative for a live book. Returns `None` if the
    /// subtraction overflows the fixed-point range.
    pub fn spread(&self) -> Option<Fixed> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => ask.price.checked_sub(bid.price),
            _ => None,
        }
    }

    /// Midpoint between the best bid and the best ask, if both exist.
    ///
    /// Returns `None` if either side is empty or the integer sum overflows.
    pub fn mid_price(&self) -> Option<Fixed> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => bid.price.midpoint(ask.price),
            _ => None,
        }
    }

    /// Aggregated visible depth for one side, best price first, up to `levels`
    /// distinct prices.
    ///
    /// Volume counts the visible tranche of each order; an iceberg's hidden
    /// reserve is invisible to the book, so it is invisible here too. The
    /// returned levels are a snapshot: later mutations are not reflected.
    pub fn depth(&self, side: OrderSide, levels: usize) -> Vec<DepthLevel> {
        let mut depth: Vec<DepthLevel> = Vec::with_capacity(levels.min(64));
        if levels == 0 {
            return depth;
        }

        let orders: Box<dyn Iterator<Item = &Order>> = match side {
            OrderSide::Buy => Box::new(self.bids.values()),
            OrderSide::Sell => Box::new(self.asks.values()),
        };

        for order in orders {
            let same_level = depth.last().is_some_and(|level| level.price == order.price);
            if same_level {
                let level = depth.last_mut().expect("checked above");
                level.volume += order.volume;
                level.orders += 1;
            } else if depth.len() < levels {
                depth.push(DepthLevel {
                    price: order.price,
                    volume: order.volume,
                    orders: 1,
                });
            } else {
                // A new price level beyond the requested depth ends the walk.
                break;
            }
        }

        depth
    }

    /// Inserts an order directly into the book without attempting to match it.
    ///
    /// This is a low-level operation used primarily when rebuilding the
    /// order book from a snapshot. For normal order processing, use
    /// `place_order` instead which will attempt to match the order first.
    ///
    /// # Arguments
    ///
    /// * `order` - The order to insert into the book
    pub fn insert_order(&mut self, order: &Order) {
        match order.side {
            OrderSide::Buy => {
                self.bids.insert(
                    BidKey {
                        price: order.price,
                        sequence: order.sequence,
                    },
                    *order,
                );
            }
            OrderSide::Sell => {
                self.asks.insert(
                    AskKey {
                        price: order.price,
                        sequence: order.sequence,
                    },
                    *order,
                );
            }
        }
    }

    /// Places an order into the book, attempting to match it with existing orders first.
    ///
    /// This is the main entry point for order processing. The order is assigned
    /// the next sequence number and then:
    /// 1. For buy orders: matches against asks starting from the best price
    /// 2. For sell orders: matches against bids starting from the best price
    /// 3. Any remaining quantity is added to the book
    ///
    /// Only the trades are returned; see [`OrderBook::execute_order`] for the
    /// rest of the outcome (rested volume, cancellations, stop elections and
    /// rejections), which is what the state machine needs to emit events.
    ///
    /// # Arguments
    ///
    /// * `order` - The order to place (will have its sequence number set)
    ///
    /// # Returns
    ///
    /// A vector of `MatchResult` representing all trades that occurred,
    /// or an empty vector if no matches were made.
    pub fn place_order(&mut self, order: &mut Order) -> Vec<MatchResult> {
        self.execute_order(order).fills
    }

    /// Runs one submitted order through its whole lifecycle: validation, stop
    /// parking, the sweep, the rest or cancellation of the remainder, and the
    /// stop cascade the trades may elect.
    ///
    /// This is the deterministic core `apply` runs: everything it decides comes
    /// from the order (as recorded in the log) and the book itself. No clock is
    /// read — the order's intake timestamp drives the automatic GTD purge, `Day`
    /// waits for an explicit `PurgeExpired`, and stop elections follow the last
    /// trade price, part of the replicated state.
    ///
    /// # Arguments
    ///
    /// * `order` - The order to process (will have its sequence number set)
    pub fn execute_order(&mut self, order: &mut Order) -> Outcome {
        self.execute_order_inner(order, SymbolConfig::default())
    }

    /// Executes an order with the configured symbol's precision configuration.
    ///
    /// The configuration is authoritative at apply time: it rejects an
    /// unsupported `symbol_id` and any price/quantity with more fractional
    /// digits than the symbol declares. [`OrderBook::execute_order`] keeps the
    /// legacy unchecked path for unit tests and callers that already validated.
    pub fn execute_order_with_symbol(
        &mut self,
        order: &mut Order,
        config: SymbolConfig,
    ) -> Outcome {
        self.execute_order_inner(order, config)
    }

    /// Consumes the next sequence and returns a rejected outcome without
    /// touching the book. Used when the symbol is unknown or differs from this
    /// process's configured symbol.
    pub fn reject_order(&mut self, order: &mut Order, reason: RejectReason) -> Outcome {
        order.sequence = self.next_market_sequence();
        Outcome {
            market_seq: order.sequence,
            rejected: Some(reason),
            ..Outcome::default()
        }
    }

    fn execute_order_inner(&mut self, order: &mut Order, config: SymbolConfig) -> Outcome {
        order.sequence = self.next_market_sequence();

        let mut outcome = Outcome {
            market_seq: order.sequence,
            ..Outcome::default()
        };
        if order.requested_volume == Fixed::ZERO {
            order.requested_volume = order.volume;
            order.requested_volume_unit = order.volume_unit;
        }
        if order.volume_unit == VolumeUnit::Quote {
            // Quote volume currently supports plain limit/market orders. Mixed
            // unit normalization for iceberg tranches/FOK is not defined yet.
            if order.display_volume > Fixed::ZERO
                || order.hidden_volume > Fixed::ZERO
                || order.tif == TimeInForce::Fok
                || !order
                    .volume
                    .has_at_most_decimals(config.quote_asset.decimals)
                || !order
                    .requested_volume
                    .has_at_most_decimals(config.quote_asset.decimals)
            {
                outcome.rejected = Some(RejectReason::InvalidOrder);
                return outcome;
            }
        }
        if order.volume_unit == VolumeUnit::Quote && order.live == OrderType::Limit {
            // A quote limit order becomes a base-denominated resting order at
            // its limit price. The original quote amount stays on the order.
            // `checked_div` rejects a zero price and a result that does not fit
            // the fixed-point range before any book state changes.
            let Some(base) = order.volume.checked_div(order.price) else {
                outcome.rejected = Some(RejectReason::InvalidOrder);
                return outcome;
            };
            let base = base.floor_to_multiple(config.base_lot);
            if base <= Fixed::ZERO {
                outcome.rejected = Some(RejectReason::InvalidOrder);
                return outcome;
            }
            order.volume = base;
            order.volume_unit = VolumeUnit::Base;
        }
        if let Some(reason) =
            validate_order(order).or_else(|| validate_order_for_symbol(order, &config))
        {
            outcome.rejected = Some(reason);
            return outcome;
        }

        // Every admissible mutating request advances the GTD clock: the taker's
        // intake timestamp is the trigger time and its sequence anchors the
        // expiry events. `Day` needs a market close, so only the explicit
        // `PurgeExpired` request evicts it.
        if self.has_gtd_deadlines() {
            outcome.expired = self.purge_due_gtd(order.timestamp);
        }

        // A stop order parks off the book; it is only ever elected by a *new*
        // trade, so one the last trade already crosses is refused on entry.
        if order.is_stop() {
            if self
                .last_trade_price
                .is_some_and(|last| order.stop_elected(last))
            {
                outcome.rejected = Some(RejectReason::StopWouldTrigger);
            } else {
                if let TimeInForce::Gtd(expiry) = order.tif {
                    self.earliest_gtd_expiry = Some(
                        self.earliest_gtd_expiry
                            .map_or(expiry, |current| current.min(expiry)),
                    );
                }
                self.stops.push(*order);
                outcome.parked = true;
            }
            return outcome;
        }

        // Post-only never takes liquidity: rest it whole or refuse it.
        if order.post_only {
            if self.would_cross(order) {
                outcome.rejected = Some(RejectReason::PostOnlyWouldCross);
            } else {
                outcome.resting = Some(self.rest(order, order.volume));
            }
            return outcome;
        }

        // Fill-or-kill: the whole quantity must be there, or nothing happens.
        if order.tif == TimeInForce::Fok && self.fillable_volume(order) < order.volume {
            outcome.rejected = Some(RejectReason::FokUnfillable);
            return outcome;
        }

        // A market order sweeps every level; a limit order stops at its price.
        let limit = match order.live {
            OrderType::Market => None,
            OrderType::Limit => Some(order.price),
        };
        let (fills, remainder) = self.sweep(order, limit, &config);
        outcome.fills = fills;

        if remainder > Fixed::ZERO {
            if order.volume_unit == VolumeUnit::Quote
                || order.tif.is_immediate()
                || order.live == OrderType::Market
            {
                // Quote-market leftovers, IOC/FOK remainders and market
                // remainders are cancelled; they never rest.
                outcome.cancelled = Some(remainder);
            } else {
                outcome.resting = Some(self.rest(order, remainder));
            }
        }

        // Any trade moves the last trade price, which may elect parked stops.
        if let Some(print) = outcome.fills.last() {
            self.last_trade_price = Some(print.maker.price);
        }
        self.run_stops(&mut outcome, &config);

        outcome
    }

    /// Cancels an existing order from the book or from the parked stops.
    ///
    /// # Arguments
    ///
    /// * `order` - The order to cancel (must have the correct price and sequence;
    ///   a parked stop is matched by side and sequence, since its price is a
    ///   trigger rather than a book key)
    ///
    /// # Returns
    ///
    /// The cancelled order if found, or `None` if no matching order was in the book.
    pub fn cancel(&mut self, order: &Order) -> Option<Order> {
        // A parked stop is off the book: its sequence identifies it.
        let parked = self
            .stops
            .iter()
            .position(|stop| stop.side == order.side && stop.sequence == order.sequence);
        if let Some(index) = parked {
            let stop = self.stops.remove(index);
            if matches!(stop.tif, TimeInForce::Gtd(_)) {
                self.gtd_scan_dirty = true;
            }
            return Some(stop);
        }

        let removed = match order.side {
            OrderSide::Buy => self.bids.remove(&BidKey {
                price: order.price,
                sequence: order.sequence,
            }),
            OrderSide::Sell => self.asks.remove(&AskKey {
                price: order.price,
                sequence: order.sequence,
            }),
        };
        if let Some(cancelled) = removed {
            if matches!(cancelled.tif, TimeInForce::Gtd(_)) {
                self.gtd_scan_dirty = true;
            }
            self.record_depth_change(cancelled.side, cancelled.price, -cancelled.volume, -1);
        }
        removed
    }

    /// Amends the price and/or open volume of a resting limit order.
    ///
    /// The key identifies the order exactly like [`OrderBook::cancel`]: side,
    /// current price and sequence. `price` and `volume` are the new values;
    /// `None` leaves a field unchanged.
    ///
    /// Priority follows the usual exchange rule:
    ///
    /// - same price and a volume that does not grow: the order keeps its
    ///   sequence, so it keeps time priority and is updated in place;
    /// - a new price or a larger volume: the order is re-inserted with a new
    ///   sequence, so it loses time priority. The returned [`Amendment`]
    ///   carries the identity to use from now on.
    ///
    /// An amendment whose new price would cross the opposite side is refused
    /// ([`AmendError::WouldCross`]): a resting book must never cross, so taking
    /// liquidity is cancel + place. Icebergs, parked stops and market orders are
    /// not amendable ([`AmendError::UnsupportedType`] /
    /// [`AmendError::NotResting`]).
    pub fn amend(
        &mut self,
        key: &Order,
        price: Option<Fixed>,
        volume: Option<Fixed>,
    ) -> Result<Amendment, AmendError> {
        let market_seq = self.next_market_sequence();
        self.amend_with_sequence(key, price, volume, market_seq)
    }

    /// [`OrderBook::amend`] with the request's market sequence already assigned.
    ///
    /// The caller (`apply`) takes the sequence and runs the automatic purge
    /// before calling this, so the amendment and the expiries it may trigger
    /// share one anchor.
    pub fn amend_with_sequence(
        &mut self,
        key: &Order,
        price: Option<Fixed>,
        volume: Option<Fixed>,
        market_seq: u64,
    ) -> Result<Amendment, AmendError> {
        if let Some(price) = price
            && !(price.is_finite() && price > Fixed::ZERO)
        {
            return Err(AmendError::InvalidPrice);
        }
        if let Some(volume) = volume
            && !(volume.is_finite() && volume > Fixed::ZERO)
        {
            return Err(AmendError::InvalidVolume);
        }
        if key.is_stop() {
            return Err(AmendError::NotResting);
        }

        let stored = match key.side {
            OrderSide::Buy => *self
                .bids
                .get(&BidKey {
                    price: key.price,
                    sequence: key.sequence,
                })
                .ok_or(AmendError::NotFound)?,
            OrderSide::Sell => *self
                .asks
                .get(&AskKey {
                    price: key.price,
                    sequence: key.sequence,
                })
                .ok_or(AmendError::NotFound)?,
        };

        if stored.live != OrderType::Limit {
            return Err(AmendError::NotResting);
        }
        if stored.display_volume > Fixed::ZERO {
            return Err(AmendError::UnsupportedType);
        }

        let new_price = price.unwrap_or(stored.price);
        let new_volume = volume.unwrap_or(stored.volume);

        if new_price != stored.price {
            let would_cross = match key.side {
                OrderSide::Buy => self.best_ask().is_some_and(|ask| ask.price <= new_price),
                OrderSide::Sell => self.best_bid().is_some_and(|bid| bid.price >= new_price),
            };
            if would_cross {
                return Err(AmendError::WouldCross);
            }
        }

        let (amendment, reinserted) = match key.side {
            OrderSide::Buy => amend_in_map(
                &mut self.bids,
                BidKey {
                    price: key.price,
                    sequence: key.sequence,
                },
                stored,
                new_price,
                new_volume,
                market_seq,
                |price, sequence| BidKey { price, sequence },
            ),
            OrderSide::Sell => amend_in_map(
                &mut self.asks,
                AskKey {
                    price: key.price,
                    sequence: key.sequence,
                },
                stored,
                new_price,
                new_volume,
                market_seq,
                |price, sequence| AskKey { price, sequence },
            ),
        };
        if reinserted {
            self.record_depth_change(
                key.side,
                amendment.previous_price,
                -amendment.previous_volume,
                -1,
            );
            self.record_depth_change(key.side, amendment.order.price, amendment.order.volume, 1);
        } else if amendment.order.volume != amendment.previous_volume {
            self.record_depth_change(
                key.side,
                amendment.order.price,
                amendment.order.volume - amendment.previous_volume,
                0,
            );
        }

        Ok(amendment)
    }

    /// Volume of `order` that is still resting after [`OrderBook::place_order`], if any.
    ///
    /// `place_order` does not write the unfilled remainder back into the caller's order, so this
    /// looks the order up by the key it would have been stored under. `None` means the order was
    /// either fully filled or never rested. The value is exactly the volume in the book, which
    /// matters when emitting events: deriving it as `submitted - filled` can differ in the last
    /// floating-point bit.
    pub fn resting_volume(&self, order: &Order) -> Option<Fixed> {
        match order.side {
            OrderSide::Buy => self
                .bids
                .get(&BidKey {
                    price: order.price,
                    sequence: order.sequence,
                })
                .map(|resting| resting.volume),
            OrderSide::Sell => self
                .asks
                .get(&AskKey {
                    price: order.price,
                    sequence: order.sequence,
                })
                .map(|resting| resting.volume),
        }
    }

    /// Rests `total` of unfilled volume for `order` on its own side of the book.
    ///
    /// A plain order rests whole; an iceberg rests one display tranche and moves
    /// the remainder into its hidden reserve, which is invisible to depth and
    /// to [`OrderBook::resting_volume`] until the sweep consumes the tranche and
    /// draws the next one out of it.
    /// Returns the visible tranche that went into the book (all of `total`
    /// for a plain order), so callers do not have to look it up again.
    fn rest(&mut self, order: &Order, total: Fixed) -> Fixed {
        let visible = if order.display_volume > Fixed::ZERO {
            order.display_volume.min(total)
        } else {
            total
        };
        let mut resting = *order;
        resting.volume = visible;
        resting.hidden_volume = total - visible;

        match order.side {
            OrderSide::Buy => {
                self.bids.insert(
                    BidKey {
                        price: order.price,
                        sequence: order.sequence,
                    },
                    resting,
                );
            }
            OrderSide::Sell => {
                self.asks.insert(
                    AskKey {
                        price: order.price,
                        sequence: order.sequence,
                    },
                    resting,
                );
            }
        }
        if let TimeInForce::Gtd(expiry) = order.tif {
            self.earliest_gtd_expiry = Some(
                self.earliest_gtd_expiry
                    .map_or(expiry, |current| current.min(expiry)),
            );
        }
        self.record_depth_change(order.side, order.price, visible, 1);

        visible
    }

    /// Sweeps the opposite side of the book, returning the trades it executed
    /// and the volume that could not execute.
    ///
    /// `limit` bounds the execution price: `None` (a market order) sweeps every
    /// resting level, `Some(price)` stops as soon as the best level is away from
    /// it. Only the resting side is touched; resting or cancelling the remainder
    /// is the caller's decision.
    ///
    /// A fully consumed iceberg draws its next tranche out of its hidden reserve
    /// instead of leaving the book and the taker keeps matching it, exactly as
    /// OrderBook-rs replenishes an iceberg tranche: to the taker it looks like
    /// several orders queued at that price.
    fn sweep(
        &mut self,
        order: &Order,
        limit: Option<Fixed>,
        config: &SymbolConfig,
    ) -> (Vec<MatchResult>, Fixed) {
        // The journal is a separate field, so the resting side and the journal
        // can be borrowed together without a second pass over the map.
        let record = self.record_depth;
        // Books without GTD orders skip the per-fill expiry check entirely.
        let mut gtd_dirty = self.gtd_scan_dirty;
        let track_gtd = self.has_gtd_deadlines();
        let result = match order.side {
            OrderSide::Buy => Self::sweep_side(
                &mut self.asks,
                order,
                limit,
                SweepContext {
                    config,
                    record,
                    journal: &mut self.depth_journal,
                    track_gtd,
                    gtd_dirty: &mut gtd_dirty,
                },
            ),
            OrderSide::Sell => Self::sweep_side(
                &mut self.bids,
                order,
                limit,
                SweepContext {
                    config,
                    record,
                    journal: &mut self.depth_journal,
                    track_gtd,
                    gtd_dirty: &mut gtd_dirty,
                },
            ),
        };
        self.gtd_scan_dirty = gtd_dirty;
        result
    }

    /// Shared sweep loop for one resting side; see [`OrderBook::sweep`].
    ///
    /// Generic over the key type so bids and asks run the same loop: a sweep
    /// never rebuilds a key, it only walks `first_entry`.
    fn sweep_side<K: Ord>(
        resting: &mut BTreeMap<K, Order>,
        order: &Order,
        limit: Option<Fixed>,
        context: SweepContext<'_>,
    ) -> (Vec<MatchResult>, Fixed) {
        let mut results = Vec::new();
        let mut remaining = order.volume;
        let mode = if order.volume_unit == VolumeUnit::Quote {
            match order.side {
                OrderSide::Buy => SweepMode::BuyQuote,
                OrderSide::Sell => SweepMode::SellQuote,
            }
        } else {
            SweepMode::Base
        };

        // Stop as soon as the taker is exhausted: a zero-volume taker must not
        // "match" the next maker and rewrite it.
        while remaining > Fixed::ZERO {
            // `first_entry` walks the tree once and keeps the position, so
            // removing a fully filled maker, shrinking a partially filled one
            // or refilling an iceberg below does not descend a second time.
            let Some(mut best) = resting.first_entry() else {
                break;
            };

            let maker = *best.get();
            let crosses = match (limit, order.side) {
                (Some(limit), OrderSide::Buy) => maker.price <= limit,
                (Some(limit), OrderSide::Sell) => maker.price >= limit,
                (None, _) => true,
            };
            if !crosses {
                // The best level is away from the taker's limit: it cannot
                // trade any further.
                break;
            }

            let by_base = remaining.min(maker.volume);
            let match_volume = match mode {
                SweepMode::Base => by_base,
                SweepMode::BuyQuote => (remaining / maker.price)
                    .floor_to_multiple(context.config.base_lot)
                    .min(maker.volume),
                SweepMode::SellQuote => (remaining / maker.price)
                    .ceil_to_multiple(context.config.base_lot)
                    .min(maker.volume),
            };
            if match_volume <= Fixed::ZERO {
                break;
            }
            let notional = Fixed::notional(maker.price, match_volume);
            if mode != SweepMode::Base && notional.is_none() {
                // The quote budget cannot be represented; stop rather than
                // spend an amount we cannot track.
                break;
            }
            let notional = notional.unwrap_or(Fixed::ZERO);
            let taker = Order {
                volume: if mode == SweepMode::Base {
                    remaining
                } else {
                    match_volume
                },
                ..*order
            };
            results.push(MatchResult::new_with_policy(
                &taker,
                &maker,
                match_volume,
                context.config.fee_asset_policy,
            ));

            let maker_remaining = maker.volume - match_volume;
            if maker_remaining > Fixed::ZERO {
                // Partially filled: shrink in place so the maker keeps its
                // identity (id, side, live and sequence).
                best.get_mut().volume = maker_remaining;
                record_depth_change(
                    context.journal,
                    context.record,
                    maker.side,
                    maker.price,
                    -match_volume,
                    0,
                );
                if mode == SweepMode::Base {
                    remaining = Fixed::ZERO;
                } else {
                    remaining = remaining.saturating_sub(notional);
                }
                break;
            }

            // The maker is fully consumed. Update the taker remainder before
            // the iceberg refill so the loop can continue on the same order.
            match mode {
                SweepMode::Base => remaining -= match_volume,
                SweepMode::BuyQuote | SweepMode::SellQuote => {
                    remaining = remaining.saturating_sub(notional);
                }
            }

            if maker.hidden_volume > Fixed::ZERO {
                // Iceberg: refill one tranche from the hidden reserve and keep
                // matching this same order.
                let draw = if maker.display_volume > Fixed::ZERO {
                    maker.display_volume.min(maker.hidden_volume)
                } else {
                    maker.hidden_volume
                };
                let maker_slot = best.get_mut();
                maker_slot.volume = draw;
                maker_slot.hidden_volume = maker.hidden_volume - draw;
                record_depth_change(
                    context.journal,
                    context.record,
                    maker.side,
                    maker.price,
                    draw - match_volume,
                    0,
                );
            } else {
                best.remove();
                if context.track_gtd && matches!(maker.tif, TimeInForce::Gtd(_)) {
                    *context.gtd_dirty = true;
                }
                record_depth_change(
                    context.journal,
                    context.record,
                    maker.side,
                    maker.price,
                    -match_volume,
                    -1,
                );
            }
            if remaining <= Fixed::ZERO {
                break;
            }
        }

        (results, remaining)
    }

    /// Volume the opposite side could execute for `order` right now: every
    /// visible tranche plus every iceberg reserve at or inside its limit.
    ///
    /// Read-only. A fill-or-kill preflight compares this with the order's
    /// volume before anything moves; the sweep then allocates greedily per
    /// level, so an accepted FOK executes its whole quantity and rests nothing.
    fn fillable_volume(&self, order: &Order) -> Fixed {
        let limit = match order.live {
            OrderType::Market => None,
            OrderType::Limit => Some(order.price),
        };
        match order.side {
            OrderSide::Buy => self
                .asks
                .values()
                .take_while(|maker| limit.is_none_or(|limit| maker.price <= limit))
                .map(Order::open_volume)
                .fold(Fixed::ZERO, Fixed::saturating_add),
            OrderSide::Sell => self
                .bids
                .values()
                .take_while(|maker| limit.is_none_or(|limit| maker.price >= limit))
                .map(Order::open_volume)
                .fold(Fixed::ZERO, Fixed::saturating_add),
        }
    }

    /// Whether `order` at its own price would trade against the book right now.
    fn would_cross(&self, order: &Order) -> bool {
        match order.side {
            OrderSide::Buy => self.best_ask().is_some_and(|ask| ask.price <= order.price),
            OrderSide::Sell => self.best_bid().is_some_and(|bid| bid.price >= order.price),
        }
    }

    /// Elects and executes parked stops while the last trade price crosses one.
    ///
    /// Stops are considered in admission (sequence) order. An elected stop runs
    /// immediately — a market stop sweeps every level, a limit stop keeps to its
    /// limit price — and whatever its child cannot execute is cancelled, never
    /// rested. Each child's prints move the last trade price again, so one trade
    /// can cascade through several stops; every turn removes a stop, so the
    /// cascade terminates.
    fn run_stops(&mut self, outcome: &mut Outcome, config: &SymbolConfig) {
        loop {
            let Some(last_trade_price) = self.last_trade_price else {
                return;
            };
            let Some(index) = self
                .stops
                .iter()
                .position(|stop| stop.stop_elected(last_trade_price))
            else {
                return;
            };

            let stop = self.stops.remove(index);
            if matches!(stop.tif, TimeInForce::Gtd(_)) {
                self.gtd_scan_dirty = true;
            }
            let trigger_price = last_trade_price;
            let limit = match stop.live {
                OrderType::Market => None,
                OrderType::Limit => Some(stop.price),
            };

            let (fills, remainder) = self.sweep(&stop, limit, config);
            if let Some(print) = fills.last() {
                self.last_trade_price = Some(print.maker.price);
            }

            outcome.triggers.push(StopExecution {
                stop,
                trigger_price,
                fills,
                cancelled: (remainder > Fixed::ZERO).then_some(remainder),
            });
        }
    }

    /// Removes every resting and parked order whose time in force has expired,
    /// returning them in a deterministic order: bids by sequence, then asks by
    /// sequence, then parked stops by admission.
    ///
    /// `now_ms` and `day_close_ms` travel in the request and therefore through
    /// the Raft log, so every replica evicts exactly the same orders while the
    /// state machine itself never reads a clock. A `Day` order only expires when
    /// a close is supplied; `GTD` expires at or after its deadline.
    pub fn purge_expired(&mut self, now_ms: u64, day_close_ms: Option<u64>) -> Vec<Order> {
        let has_expired = |order: &Order| order.tif.is_expired(now_ms, day_close_ms);
        let mut removed = Vec::new();

        let bids: Vec<BidKey> = self.bids.keys().copied().collect();
        for key in bids {
            if has_expired(&self.bids[&key]) {
                let order = self.bids.remove(&key).expect("key came from the map");
                self.record_depth_change(OrderSide::Buy, order.price, -order.volume, -1);
                removed.push(order);
            }
        }
        let asks: Vec<AskKey> = self.asks.keys().copied().collect();
        for key in asks {
            if has_expired(&self.asks[&key]) {
                let order = self.asks.remove(&key).expect("key came from the map");
                self.record_depth_change(OrderSide::Sell, order.price, -order.volume, -1);
                removed.push(order);
            }
        }
        self.stops.retain(|stop| {
            if has_expired(stop) {
                removed.push(*stop);
                false
            } else {
                true
            }
        });
        if !removed.is_empty() {
            self.gtd_scan_dirty = true;
        }

        removed
    }
}

/// Appends a depth change to the sweep journal when recording is enabled.
fn record_depth_change(
    journal: &mut Vec<DepthChange>,
    record: bool,
    side: OrderSide,
    price: Fixed,
    delta_volume: Fixed,
    delta_orders: i32,
) {
    if record {
        journal.push(DepthChange {
            side,
            price,
            delta_volume,
            delta_orders,
        });
    }
}

/// Applies a validated amendment to one side of the book.
///
/// Returns the amendment and whether the order was re-inserted (which costs a
/// new sequence, so the caller must advance the counter).
fn amend_in_map<K: Ord>(
    map: &mut BTreeMap<K, Order>,
    key: K,
    stored: Order,
    new_price: Fixed,
    new_volume: Fixed,
    market_seq: u64,
    make_key: impl Fn(Fixed, u64) -> K,
) -> (Amendment, bool) {
    let previous_sequence = stored.sequence;
    let previous_price = stored.price;
    let previous_volume = stored.volume;

    if new_price == stored.price && new_volume <= stored.volume {
        // A same-price reduction keeps time priority, so the key survives.
        let entry = map.get_mut(&key).expect("the caller looked the key up");
        entry.volume = new_volume;
        return (
            Amendment {
                market_seq,
                order: *entry,
                previous_sequence,
                previous_price,
                previous_volume,
            },
            false,
        );
    }

    // Price changes and size increases lose time priority: re-insert the order
    // under a fresh sequence so it queues behind everyone already at the level.
    map.remove(&key).expect("the caller looked the key up");
    let mut updated = stored;
    updated.price = new_price;
    updated.volume = new_volume;
    updated.sequence = market_seq;
    map.insert(make_key(new_price, market_seq), updated);

    (
        Amendment {
            market_seq,
            order: updated,
            previous_sequence,
            previous_price,
            previous_volume,
        },
        true,
    )
}

/// Checks an order against a symbol's declared decimal precision.
///
/// The order must belong to the configured symbol; otherwise it is refused as
/// an unsupported order. Price fields use `price_decimals`, quantity fields
/// use `quantity_decimals`.
fn validate_order_for_symbol(order: &Order, config: &SymbolConfig) -> Option<RejectReason> {
    if order.symbol_id != config.symbol_id {
        return Some(RejectReason::UnsupportedOrder);
    }
    let price_ok = order.price.has_at_most_decimals(config.price_decimals)
        && order.stop_price.has_at_most_decimals(config.price_decimals)
        && order.price.is_multiple_of(config.price_tick)
        && order.stop_price.is_multiple_of(config.price_tick);
    if !price_ok {
        return Some(RejectReason::InvalidOrder);
    }
    match order.volume_unit {
        VolumeUnit::Base => {
            let quantity_ok = order
                .volume
                .has_at_most_decimals(config.base_asset.decimals)
                && order
                    .display_volume
                    .has_at_most_decimals(config.base_asset.decimals)
                && order
                    .hidden_volume
                    .has_at_most_decimals(config.base_asset.decimals)
                && order.volume.is_multiple_of(config.base_lot)
                && order.display_volume.is_multiple_of(config.base_lot)
                && order.hidden_volume.is_multiple_of(config.base_lot);
            if !quantity_ok {
                return Some(RejectReason::InvalidOrder);
            }
        }
        VolumeUnit::Quote => {
            // Quote volume is only supported for plain market orders. Iceberg
            // tranches and FOK semantics need base normalization that is not
            // defined yet.
            if order.display_volume > Fixed::ZERO
                || order.hidden_volume > Fixed::ZERO
                || order.tif == TimeInForce::Fok
            {
                return Some(RejectReason::InvalidOrder);
            }
            if !order
                .volume
                .has_at_most_decimals(config.quote_asset.decimals)
            {
                return Some(RejectReason::InvalidOrder);
            }
        }
    }
    None
}

/// Checks an order's shape and whether its type combination is supported;
/// `None` means the order is admissible.
fn validate_order(order: &Order) -> Option<RejectReason> {
    if !(order.volume.is_finite() && order.volume > Fixed::ZERO) {
        return Some(RejectReason::InvalidOrder);
    }
    if order.maker_fee_rate < MIN_FEE_RATE
        || order.maker_fee_rate > MAX_FEE_RATE
        || order.taker_fee_rate < MIN_FEE_RATE
        || order.taker_fee_rate > MAX_FEE_RATE
    {
        return Some(RejectReason::InvalidOrder);
    }
    // A market order ignores its own price; a limit order (including a
    // stop-limit child) needs a positive one.
    if order.live == OrderType::Limit && !(order.price.is_finite() && order.price > Fixed::ZERO) {
        return Some(RejectReason::InvalidOrder);
    }
    // A stop price is a price, not a size: any non-negative finite value is
    // fine (`0.0` means "no stop").
    if !order.stop_price.is_finite() || order.stop_price < Fixed::ZERO {
        return Some(RejectReason::InvalidOrder);
    }
    // An iceberg displays whole tranches: anything outside (0, volume] is nonsense.
    if !order.display_volume.is_finite()
        || order.display_volume < Fixed::ZERO
        || order.display_volume > order.volume
    {
        return Some(RejectReason::InvalidOrder);
    }

    // Combinations this engine does not define: post-only and stops are their
    // own admission paths, and neither can also be an iceberg or immediate.
    if order.post_only
        && (order.live != OrderType::Limit || order.is_stop() || order.display_volume > Fixed::ZERO)
    {
        return Some(RejectReason::InvalidOrder);
    }
    if order.is_stop()
        && (order.post_only || order.display_volume > Fixed::ZERO || order.tif.is_immediate())
    {
        return Some(RejectReason::InvalidOrder);
    }

    // A GTD deadline at or before the order's own timestamp is already dead.
    if let TimeInForce::Gtd(expiry) = order.tif
        && expiry <= order.timestamp
    {
        return Some(RejectReason::GtdAlreadyExpired);
    }

    None
}

#[cfg(test)]
mod tests;

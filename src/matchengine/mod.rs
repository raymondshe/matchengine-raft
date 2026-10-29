use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;

pub mod fixed;
pub use fixed::Fixed;
pub use fixed::SymbolConfig;

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
    /// Instrument id (0 until the engine is multi-symbol). The per-symbol
    /// [`crate::matchengine::fixed::SymbolConfig`] controls accepted price and
    /// quantity precision; values are stored at the common 18-decimal scale.
    #[serde(default)]
    pub symbol_id: u32,
    /// The price level for this order (for limit orders), scale 18.
    pub price: Fixed,
    /// The quantity of the asset to trade.
    ///
    /// For a resting iceberg order this is the currently displayed tranche;
    /// [`Order::open_volume`] reports the tranche plus the hidden reserve.
    pub volume: Fixed,

    /// Unique identifier for the order.
    pub id: u64,
    /// Sequence number for ordering orders at the same price level.
    ///
    /// This ensures price-time priority - orders with lower sequence
    /// numbers are matched first at the same price level.
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

/// Equality comparison for orders based on sequence, price, and volume.
impl PartialEq for Order {
    fn eq(&self, other: &Self) -> bool {
        (self.sequence == other.sequence)
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
}

/// Equality comparison for match results.
impl PartialEq for MatchResult {
    fn eq(&self, other: &Self) -> bool {
        (self.taker == other.taker)
            && (self.maker == other.maker)
            && (self.matches == other.matches)
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
        Self {
            taker: *taker,
            maker: *maker,
            matches: matches.into(),
        }
    }
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
        order.sequence = self.next_market_sequence();

        let mut outcome = Outcome {
            market_seq: order.sequence,
            ..Outcome::default()
        };
        if let Some(reason) = validate_order(order) {
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
        let (fills, remainder) = self.sweep(order, limit);
        outcome.fills = fills;

        if remainder > Fixed::ZERO {
            if order.tif.is_immediate() || order.live == OrderType::Market {
                // IOC/FOK remainders and market remainders are cancelled: a
                // market or immediate order never rests.
                outcome.cancelled = Some(remainder);
            } else {
                outcome.resting = Some(self.rest(order, remainder));
            }
        }

        // Any trade moves the last trade price, which may elect parked stops.
        if let Some(print) = outcome.fills.last() {
            self.last_trade_price = Some(print.maker.price);
        }
        self.run_stops(&mut outcome);

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
    fn sweep(&mut self, order: &Order, limit: Option<Fixed>) -> (Vec<MatchResult>, Fixed) {
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
                record,
                &mut self.depth_journal,
                track_gtd,
                &mut gtd_dirty,
            ),
            OrderSide::Sell => Self::sweep_side(
                &mut self.bids,
                order,
                limit,
                record,
                &mut self.depth_journal,
                track_gtd,
                &mut gtd_dirty,
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
        record: bool,
        journal: &mut Vec<DepthChange>,
        track_gtd: bool,
        gtd_dirty: &mut bool,
    ) -> (Vec<MatchResult>, Fixed) {
        let mut results = Vec::new();
        let mut remaining_volume = order.volume;

        // Stop as soon as the taker is exhausted: a zero-volume taker must not
        // "match" the next maker and rewrite it.
        while remaining_volume > Fixed::ZERO {
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

            let match_volume = remaining_volume.min(maker.volume);
            results.push(MatchResult {
                taker: Order {
                    volume: remaining_volume,
                    ..*order
                },
                maker,
                matches: match_volume,
            });

            if maker.volume > remaining_volume {
                // Partially filled: shrink the maker in place so it keeps
                // its identity (id, side, live and sequence).
                best.get_mut().volume = maker.volume - match_volume;
                record_depth_change(journal, record, maker.side, maker.price, -match_volume, 0);
                // Taker fully matched
                remaining_volume = Fixed::ZERO;
                break;
            }

            // The visible tranche is gone.
            remaining_volume -= match_volume;
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
                    journal,
                    record,
                    maker.side,
                    maker.price,
                    draw - match_volume,
                    0,
                );
            } else {
                best.remove();
                if track_gtd && matches!(maker.tif, TimeInForce::Gtd(_)) {
                    *gtd_dirty = true;
                }
                record_depth_change(journal, record, maker.side, maker.price, -match_volume, -1);
            }
        }

        (results, remaining_volume)
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
                .fold(Fixed::ZERO, |total, volume| total + volume),
            OrderSide::Sell => self
                .bids
                .values()
                .take_while(|maker| limit.is_none_or(|limit| maker.price >= limit))
                .map(Order::open_volume)
                .fold(Fixed::ZERO, |total, volume| total + volume),
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
    fn run_stops(&mut self, outcome: &mut Outcome) {
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

            let (fills, remainder) = self.sweep(&stop, limit);
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

/// Checks an order's shape and whether its type combination is supported;
/// `None` means the order is admissible.
fn validate_order(order: &Order) -> Option<RejectReason> {
    if !(order.volume.is_finite() && order.volume > Fixed::ZERO) {
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
mod tests {
    use super::*;

    /// Tests that the best bid (highest price) is correctly maintained.
    ///
    /// Places multiple buy orders at different prices and verifies that
    /// the highest price is always returned as the best bid.
    #[test]
    fn basic_best_bid() {
        let mut ob = OrderBook::new();
        let mut o1 = Order::new(OrderSide::Buy, 100.22, 20.0, 1);
        let mut o2 = Order::new(OrderSide::Buy, 100.22, 20.0, 2);
        let mut o3 = Order::new(OrderSide::Buy, 100.24, 20.0, 3);
        let mut o4 = Order::new(OrderSide::Buy, 100.20, 20.0, 4);

        ob.place_order(&mut o1);
        assert_eq!(ob.best_bid(), Some(&o1));
        ob.place_order(&mut o2);
        assert_eq!(ob.best_bid(), Some(&o1));
        ob.place_order(&mut o3);
        assert_eq!(ob.best_bid(), Some(&o3));
        ob.place_order(&mut o4);
        assert_eq!(ob.best_bid(), Some(&o3));
    }

    /// Tests that the best ask (lowest price) is correctly maintained.
    ///
    /// Places multiple sell orders at different prices and verifies that
    /// the lowest price is always returned as the best ask.
    #[test]
    fn basic_best_ask() {
        let mut ob = OrderBook::new();
        let mut o1 = Order::new(OrderSide::Sell, 100.22, 20.0, 1);
        let mut o2 = Order::new(OrderSide::Sell, 100.22, 20.0, 2);
        let mut o3 = Order::new(OrderSide::Sell, 100.24, 20.0, 3);
        let mut o4 = Order::new(OrderSide::Sell, 100.20, 20.0, 4);

        ob.place_order(&mut o1);
        assert_eq!(ob.best_ask(), Some(&o1));
        ob.place_order(&mut o2);
        assert_eq!(ob.best_ask(), Some(&o1));
        ob.place_order(&mut o3);
        assert_eq!(ob.best_ask(), Some(&o1));
        ob.place_order(&mut o4);
        assert_eq!(ob.best_ask(), Some(&o4));
    }

    /// Builds a resting limit order with an explicit identity, so tests can
    /// assert that matching never rewrites it.
    fn resting_order(side: OrderSide, price: f64, volume: f64, id: u64) -> Order {
        let mut order = Order::new(side, price, volume, 0);
        order.id = id;
        order
    }

    /// A resting order with every optional type switched off apart from the
    /// ones the caller sets, so tests read as "this order, but IOC".
    fn order_with(
        side: OrderSide,
        price: f64,
        volume: f64,
        id: u64,
        apply: impl FnOnce(&mut Order),
    ) -> Order {
        let mut order = resting_order(side, price, volume, id);
        apply(&mut order);
        order
    }

    /// Regression test for issue #1.
    ///
    /// A sell that exactly consumes the best bid must produce exactly one fill
    /// and must leave the next bid untouched. Before the fix the matcher did an
    /// extra loop turn, emitted a zero-quantity `MatchResult` against bid #2 and
    /// re-stored it through `Order::new`, which reset its `id` to 0.
    #[test]
    fn exact_fill_does_not_emit_phantom_or_corrupt_bystander_bid() {
        let mut ob = OrderBook::new();
        let mut b1 = resting_order(OrderSide::Buy, 100.0, 10.0, 1);
        let mut b2 = resting_order(OrderSide::Buy, 100.0, 10.0, 2);
        ob.place_order(&mut b1);
        ob.place_order(&mut b2);

        let mut s = resting_order(OrderSide::Sell, 100.0, 10.0, 3);
        let trades = ob.place_order(&mut s);

        assert_eq!(trades.len(), 1, "only bid #1 should fill, got {:?}", trades);
        assert_eq!(trades[0].maker.id, 1);
        assert_eq!(trades[0].taker.id, 3);
        assert_eq!(trades[0].matches, 10.0);
        assert!(
            trades.iter().all(|t| t.matches > 0.0),
            "phantom fill: {:?}",
            trades
        );

        let bystander = ob.best_bid().expect("bid #2 must still rest");
        assert_eq!(bystander.id, 2, "bystander id must not be reset");
        assert_eq!(bystander.volume, 10.0);
        assert_eq!(bystander.sequence, 1);
        assert!(matches!(bystander.side, OrderSide::Buy));
        assert!(matches!(bystander.live, OrderType::Limit));
        assert_eq!(ob.bids.len(), 1);
    }

    /// Mirror of the bid case for a buy that exactly consumes the best ask.
    #[test]
    fn exact_fill_does_not_emit_phantom_or_corrupt_bystander_ask() {
        let mut ob = OrderBook::new();
        let mut a1 = resting_order(OrderSide::Sell, 100.0, 10.0, 1);
        let mut a2 = resting_order(OrderSide::Sell, 100.0, 10.0, 2);
        ob.place_order(&mut a1);
        ob.place_order(&mut a2);

        let mut b = resting_order(OrderSide::Buy, 100.0, 10.0, 3);
        let trades = ob.place_order(&mut b);

        assert_eq!(trades.len(), 1, "only ask #1 should fill, got {:?}", trades);
        assert_eq!(trades[0].maker.id, 1);
        assert_eq!(trades[0].taker.id, 3);
        assert_eq!(trades[0].matches, 10.0);
        assert!(
            trades.iter().all(|t| t.matches > 0.0),
            "phantom fill: {:?}",
            trades
        );

        let bystander = ob.best_ask().expect("ask #2 must still rest");
        assert_eq!(bystander.id, 2, "bystander id must not be reset");
        assert_eq!(bystander.volume, 10.0);
        assert_eq!(bystander.sequence, 1);
        assert!(matches!(bystander.side, OrderSide::Sell));
        assert!(matches!(bystander.live, OrderType::Limit));
        assert_eq!(ob.asks.len(), 1);
    }

    /// A genuine partial fill must only shrink the maker's volume. The old code
    /// rebuilt the maker with `Order::new`, which discarded its `id`.
    #[test]
    fn partial_fill_preserves_maker_identity() {
        let mut ob = OrderBook::new();
        let mut maker = resting_order(OrderSide::Sell, 100.22, 22.0, 7);
        ob.place_order(&mut maker);

        let mut taker = resting_order(OrderSide::Buy, 100.22, 10.0, 8);
        let trades = ob.place_order(&mut taker);

        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].maker.id, 7);
        assert_eq!(trades[0].matches, 10.0);

        let rest = ob.best_ask().expect("the remainder must rest as an ask");
        assert_eq!(rest.id, 7, "partial fill must keep the maker id");
        assert_eq!(rest.volume, 12.0);
        assert_eq!(rest.sequence, 0);
        assert_eq!(rest.price, 100.22);
        assert!(matches!(rest.side, OrderSide::Sell));
        assert!(matches!(rest.live, OrderType::Limit));
    }

    /// `resting_volume` is how event emitters learn the unfilled remainder without recomputing it
    /// from floating-point subtraction.
    #[test]
    fn resting_volume_reports_the_stored_remainder() {
        let mut ob = OrderBook::new();
        let mut maker = resting_order(OrderSide::Sell, 100.0, 5.0, 1);
        ob.place_order(&mut maker);

        // Buys 10 against an ask of 5: 5 trades, 5 rests as a bid.
        let mut taker = resting_order(OrderSide::Buy, 100.0, 10.0, 2);
        ob.place_order(&mut taker);

        assert_eq!(Some(5.0), ob.resting_volume(&taker).map(Fixed::to_f64));

        // A fully filled taker leaves nothing behind.
        let mut filled = resting_order(OrderSide::Sell, 100.0, 5.0, 3);
        ob.place_order(&mut filled);
        assert_eq!(None, ob.resting_volume(&filled));

        // So does an order that was never placed.
        let unplaced = resting_order(OrderSide::Buy, 100.0, 1.0, 99);
        assert_eq!(None, ob.resting_volume(&unplaced));
    }

    /// An exact fill must stop matching even when deeper price levels would
    /// still cross the taker. The larger order behind must stay untouched.
    #[test]
    fn exact_fill_stops_before_deeper_levels() {
        // Sell taker fills bid #1 at 100 but is priced through bid #2 at 99.
        let mut ob = OrderBook::new();
        let mut b1 = resting_order(OrderSide::Buy, 100.0, 10.0, 1);
        let mut b2 = resting_order(OrderSide::Buy, 99.0, 20.0, 2);
        ob.place_order(&mut b1);
        ob.place_order(&mut b2);

        let mut s = resting_order(OrderSide::Sell, 99.0, 10.0, 3);
        let trades = ob.place_order(&mut s);

        assert_eq!(
            trades.len(),
            1,
            "deeper bid must not be touched: {:?}",
            trades
        );
        assert_eq!((trades[0].maker.id, trades[0].matches.to_f64()), (1, 10.0));

        let deeper = ob.best_bid().expect("bid #2 must still rest");
        assert_eq!(deeper.id, 2, "deeper bid id must not be reset");
        assert_eq!(deeper.volume, 20.0);
        assert_eq!(deeper.sequence, 1);
        assert_eq!(ob.bids.len(), 1);

        // Buy taker fills ask #1 at 100 but is priced through ask #2 at 101.
        let mut ob = OrderBook::new();
        let mut a1 = resting_order(OrderSide::Sell, 100.0, 10.0, 1);
        let mut a2 = resting_order(OrderSide::Sell, 101.0, 20.0, 2);
        ob.place_order(&mut a1);
        ob.place_order(&mut a2);

        let mut b = resting_order(OrderSide::Buy, 101.0, 10.0, 3);
        let trades = ob.place_order(&mut b);

        assert_eq!(
            trades.len(),
            1,
            "deeper ask must not be touched: {:?}",
            trades
        );
        assert_eq!((trades[0].maker.id, trades[0].matches.to_f64()), (1, 10.0));

        let deeper = ob.best_ask().expect("ask #2 must still rest");
        assert_eq!(deeper.id, 2, "deeper ask id must not be reset");
        assert_eq!(deeper.volume, 20.0);
        assert_eq!(deeper.sequence, 1);
        assert_eq!(ob.asks.len(), 1);
    }

    /// A taker that consumes every level exactly empties the book without
    /// emitting a trailing zero-quantity fill.
    #[test]
    fn full_sweep_leaves_book_empty_without_phantom_trades() {
        let mut ob = OrderBook::new();
        let mut b1 = resting_order(OrderSide::Buy, 100.0, 10.0, 1);
        let mut b2 = resting_order(OrderSide::Buy, 99.0, 5.0, 2);
        ob.place_order(&mut b1);
        ob.place_order(&mut b2);

        let mut s = resting_order(OrderSide::Sell, 99.0, 15.0, 3);
        let trades = ob.place_order(&mut s);

        assert_eq!(
            trades
                .iter()
                .map(|t| (t.maker.id, t.matches.to_f64()))
                .collect::<Vec<_>>(),
            vec![(1, 10.0), (2, 5.0)]
        );
        assert!(ob.best_bid().is_none(), "fully swept book must be empty");
        assert_eq!(ob.bids.len(), 0);

        let mut ob = OrderBook::new();
        let mut a1 = resting_order(OrderSide::Sell, 100.0, 10.0, 1);
        let mut a2 = resting_order(OrderSide::Sell, 101.0, 5.0, 2);
        ob.place_order(&mut a1);
        ob.place_order(&mut a2);

        let mut b = resting_order(OrderSide::Buy, 101.0, 15.0, 3);
        let trades = ob.place_order(&mut b);

        assert_eq!(
            trades
                .iter()
                .map(|t| (t.maker.id, t.matches.to_f64()))
                .collect::<Vec<_>>(),
            vec![(1, 10.0), (2, 5.0)]
        );
        assert!(ob.best_ask().is_none(), "fully swept book must be empty");
        assert_eq!(ob.asks.len(), 0);
    }

    /// Tests that a partially filled order has its remaining quantity updated in the book.
    ///
    /// Places a sell order, then matches a buy order for part of it.
    /// Verifies that the remaining quantity is correctly maintained.
    #[test]
    fn order_replace() {
        let mut ob = OrderBook::new();
        let mut o1 = Order::new(OrderSide::Sell, 100.22, 22.0, 1);
        let mut o2 = Order::new(OrderSide::Buy, 100.22, 10.0, 1);

        let o3 = Order::new(OrderSide::Sell, 100.22, 12.0, 0);

        ob.place_order(&mut o1);
        assert_eq!(ob.best_ask(), Some(&o1));
        ob.place_order(&mut o2);
        assert_eq!(ob.best_ask(), Some(&o3));
        /*
        let serialized = serde_json::to_string(&ob).unwrap();
        println!("serialized = {}", serialized);

        let deserialized: OrderBook = serde_json::from_str(&serialized).unwrap();
        println!("deserialized = {:?}", deserialized);
        */
    }

    // --- Time in force

    #[test]
    fn ioc_cancels_its_remainder_instead_of_resting() {
        let mut ob = OrderBook::new();
        let mut maker = resting_order(OrderSide::Sell, 100.0, 6.0, 1);
        ob.place_order(&mut maker);

        let mut ioc = order_with(OrderSide::Buy, 100.0, 10.0, 2, |o| o.tif = TimeInForce::Ioc);
        let outcome = ob.execute_order(&mut ioc);

        assert_eq!(
            vec![6.0],
            outcome
                .fills
                .iter()
                .map(|f| f.matches.to_f64())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            Some(4.0),
            outcome.cancelled.map(Fixed::to_f64),
            "the remainder must be cancelled"
        );
        assert_eq!(None, outcome.resting, "an IOC order never rests");
        assert!(ob.bids.is_empty());
        assert!(ob.asks.is_empty(), "the maker was consumed");
    }

    #[test]
    fn ioc_without_liquidity_executes_nothing() {
        let mut ob = OrderBook::new();
        let mut ioc = order_with(OrderSide::Buy, 100.0, 5.0, 1, |o| o.tif = TimeInForce::Ioc);

        let outcome = ob.execute_order(&mut ioc);

        assert!(outcome.fills.is_empty());
        assert_eq!(Some(5.0), outcome.cancelled.map(Fixed::to_f64));
        assert_eq!(None, outcome.resting);
        assert!(ob.bids.is_empty());
    }

    #[test]
    fn fok_sweeps_every_level_it_needs() {
        let mut ob = OrderBook::new();
        let mut a1 = resting_order(OrderSide::Sell, 100.0, 6.0, 1);
        let mut a2 = resting_order(OrderSide::Sell, 101.0, 5.0, 2);
        ob.place_order(&mut a1);
        ob.place_order(&mut a2);

        let mut fok = order_with(OrderSide::Buy, 101.0, 10.0, 3, |o| o.tif = TimeInForce::Fok);
        let outcome = ob.execute_order(&mut fok);

        assert_eq!(
            vec![6.0, 4.0],
            outcome
                .fills
                .iter()
                .map(|f| f.matches.to_f64())
                .collect::<Vec<_>>()
        );
        assert!(outcome.rejected.is_none());
        assert_eq!(None, outcome.cancelled);
        assert_eq!(None, outcome.resting);
        assert_eq!(
            1.0,
            ob.best_ask().expect("1 unit of ask #2 must remain").volume
        );
    }

    #[test]
    fn fok_is_refused_when_the_whole_order_cannot_fill() {
        let build = || {
            let mut ob = OrderBook::new();
            let mut a1 = resting_order(OrderSide::Sell, 100.0, 6.0, 1);
            let mut a2 = resting_order(OrderSide::Sell, 101.0, 5.0, 2);
            ob.place_order(&mut a1);
            ob.place_order(&mut a2);
            ob
        };

        // 10 at a limit of 100 reaches only the 6 resting there.
        let mut ob = build();
        let mut fok = order_with(OrderSide::Buy, 100.0, 10.0, 3, |o| o.tif = TimeInForce::Fok);
        let outcome = ob.execute_order(&mut fok);
        assert_eq!(Some(RejectReason::FokUnfillable), outcome.rejected);
        assert!(outcome.fills.is_empty());
        assert_eq!(2, ob.asks.len(), "a rejected FOK must not move the book");
        assert_eq!(6.0, ob.best_ask().unwrap().volume);

        // 12 is more than the 11 the book holds, even within the limit.
        let mut ob = build();
        let mut fok = order_with(OrderSide::Buy, 101.0, 12.0, 3, |o| o.tif = TimeInForce::Fok);
        let outcome = ob.execute_order(&mut fok);
        assert_eq!(Some(RejectReason::FokUnfillable), outcome.rejected);
        assert_eq!(2, ob.asks.len());
    }

    #[test]
    fn market_order_sweeps_every_level_and_never_rests() {
        let mut ob = OrderBook::new();
        let mut a1 = resting_order(OrderSide::Sell, 100.0, 6.0, 1);
        let mut a2 = resting_order(OrderSide::Sell, 101.0, 5.0, 2);
        ob.place_order(&mut a1);
        ob.place_order(&mut a2);

        // A market buy ignores its own price entirely (0.0 would never cross).
        let mut market = order_with(OrderSide::Buy, 0.0, 20.0, 3, |o| o.live = OrderType::Market);
        let outcome = ob.execute_order(&mut market);

        assert_eq!(
            vec![6.0, 5.0],
            outcome
                .fills
                .iter()
                .map(|f| f.matches.to_f64())
                .collect::<Vec<_>>()
        );
        assert_eq!(Some(9.0), outcome.cancelled.map(Fixed::to_f64));
        assert_eq!(None, outcome.resting, "a market order never rests");
        assert!(ob.asks.is_empty());
        assert!(ob.bids.is_empty());

        // The sell side behaves the same way.
        let mut b1 = resting_order(OrderSide::Buy, 100.0, 6.0, 4);
        let mut b2 = resting_order(OrderSide::Buy, 99.0, 5.0, 5);
        ob.place_order(&mut b1);
        ob.place_order(&mut b2);
        let mut market = order_with(OrderSide::Sell, 0.0, 20.0, 6, |o| {
            o.live = OrderType::Market
        });
        let outcome = ob.execute_order(&mut market);
        assert_eq!(
            vec![6.0, 5.0],
            outcome
                .fills
                .iter()
                .map(|f| f.matches.to_f64())
                .collect::<Vec<_>>()
        );
        assert_eq!(Some(9.0), outcome.cancelled.map(Fixed::to_f64));
        assert!(ob.bids.is_empty());
    }

    #[test]
    fn post_only_rests_when_it_would_not_cross() {
        let mut ob = OrderBook::new();
        let mut ask = resting_order(OrderSide::Sell, 101.0, 5.0, 1);
        ob.place_order(&mut ask);

        let mut post = order_with(OrderSide::Buy, 100.0, 4.0, 2, |o| o.post_only = true);
        let outcome = ob.execute_order(&mut post);

        assert!(outcome.fills.is_empty());
        assert_eq!(Some(4.0), outcome.resting.map(Fixed::to_f64));
        assert_eq!(
            4.0,
            ob.best_bid().expect("the post-only order rests").volume
        );
    }

    #[test]
    fn post_only_is_refused_when_it_would_cross() {
        let mut ob = OrderBook::new();
        let mut ask = resting_order(OrderSide::Sell, 100.0, 5.0, 1);
        ob.place_order(&mut ask);

        let mut post = order_with(OrderSide::Buy, 100.0, 4.0, 2, |o| o.post_only = true);
        let outcome = ob.execute_order(&mut post);

        assert_eq!(Some(RejectReason::PostOnlyWouldCross), outcome.rejected);
        assert!(outcome.fills.is_empty());
        assert!(ob.bids.is_empty());
        assert_eq!(5.0, ob.best_ask().unwrap().volume, "the book must not move");
    }

    // --- Iceberg

    #[test]
    fn iceberg_rests_only_its_display_tranche() {
        let mut ob = OrderBook::new();
        let mut iceberg = order_with(OrderSide::Sell, 100.0, 10.0, 1, |o| {
            o.display_volume = 4.0.into()
        });

        let outcome = ob.execute_order(&mut iceberg);

        assert_eq!(
            Some(4.0),
            outcome.resting.map(Fixed::to_f64),
            "only the tranche is visible"
        );
        let resting = ob.best_ask().expect("the tranche rests");
        assert_eq!(4.0, resting.volume);
        assert_eq!(6.0, resting.hidden_volume);
        assert_eq!(10.0, resting.open_volume(), "the order still owns 10");
    }

    #[test]
    fn iceberg_replenishes_from_its_hidden_reserve() {
        let mut ob = OrderBook::new();
        let mut iceberg = order_with(OrderSide::Sell, 100.0, 10.0, 1, |o| {
            o.display_volume = 4.0.into()
        });
        ob.place_order(&mut iceberg);

        // 4 consumes the whole tranche; the order refills another 4 from hidden.
        let mut taker = resting_order(OrderSide::Buy, 100.0, 4.0, 2);
        let trades = ob.place_order(&mut taker);
        assert_eq!(1, trades.len(), "the taker is done after one tranche");
        assert_eq!(4.0, trades[0].matches);
        assert_eq!(4.0, ob.best_ask().unwrap().volume, "refilled from hidden");
        assert_eq!(2.0, ob.best_ask().unwrap().hidden_volume);

        // 6 takes the refilled tranche and then the last two units.
        let mut taker = resting_order(OrderSide::Buy, 100.0, 6.0, 3);
        let trades = ob.place_order(&mut taker);
        assert_eq!(
            vec![4.0, 2.0],
            trades.iter().map(|t| t.matches).collect::<Vec<_>>()
        );
        assert!(ob.asks.is_empty(), "an exhausted iceberg leaves the book");
    }

    #[test]
    fn iceberg_reserves_count_as_reachable_depth_for_fill_or_kill() {
        let mut ob = OrderBook::new();
        let mut iceberg = order_with(OrderSide::Sell, 100.0, 10.0, 1, |o| {
            o.display_volume = 4.0.into()
        });
        ob.place_order(&mut iceberg);

        let mut fok = order_with(OrderSide::Buy, 100.0, 10.0, 2, |o| o.tif = TimeInForce::Fok);
        let outcome = ob.execute_order(&mut fok);

        assert!(
            outcome.rejected.is_none(),
            "the hidden reserve is real depth"
        );
        assert_eq!(
            vec![4.0, 4.0, 2.0],
            outcome
                .fills
                .iter()
                .map(|f| f.matches.to_f64())
                .collect::<Vec<_>>()
        );
        assert!(outcome.resting.is_none());
        assert!(ob.asks.is_empty());
    }

    // --- Stops

    #[test]
    fn stop_market_parks_and_is_elected_by_a_trade() {
        let mut ob = OrderBook::new();
        let mut b1 = resting_order(OrderSide::Buy, 95.0, 10.0, 1);
        let mut b2 = resting_order(OrderSide::Buy, 100.0, 10.0, 2);
        ob.place_order(&mut b1);
        ob.place_order(&mut b2);

        // Nothing has traded yet, so a sell stop at 95 can park.
        let mut stop = order_with(OrderSide::Sell, 0.0, 5.0, 3, |o| {
            o.live = OrderType::Market;
            o.stop_price = 95.0.into();
        });
        let outcome = ob.execute_order(&mut stop);
        assert!(outcome.parked);
        assert_eq!(1, ob.stops.len());
        assert!(outcome.fills.is_empty());
        assert!(!ob.bids.is_empty(), "the book is untouched by parking");

        // A print at 100 is above the stop: nothing is elected.
        let mut taker = resting_order(OrderSide::Sell, 100.0, 10.0, 4);
        let outcome = ob.execute_order(&mut taker);
        assert!(outcome.triggers.is_empty());
        assert_eq!(Some(100.0), ob.last_trade_price.map(Fixed::to_f64));

        // A print at 95 elects the stop: the market child sweeps 5 off the bid.
        let mut taker = resting_order(OrderSide::Sell, 95.0, 3.0, 5);
        let outcome = ob.execute_order(&mut taker);
        assert_eq!(
            vec![3.0],
            outcome
                .fills
                .iter()
                .map(|f| f.matches.to_f64())
                .collect::<Vec<_>>()
        );
        assert_eq!(1, outcome.triggers.len(), "the stop must fire");
        let trigger = &outcome.triggers[0];
        assert_eq!(95.0, trigger.trigger_price);
        assert_eq!(2, trigger.stop.sequence, "the stop keeps its identity");
        assert_eq!(
            vec![5.0],
            trigger
                .fills
                .iter()
                .map(|f| f.matches.to_f64())
                .collect::<Vec<_>>()
        );
        assert!(trigger.cancelled.is_none());
        assert!(ob.stops.is_empty(), "an elected stop leaves the book");
        assert_eq!(2.0, ob.best_bid().expect("10 - 3 - 5 = 2").volume);
        assert_eq!(Some(95.0), ob.last_trade_price.map(Fixed::to_f64));
    }

    #[test]
    fn stop_limit_child_keeps_to_its_limit_and_never_rests() {
        let mut ob = OrderBook::new();
        let mut a1 = resting_order(OrderSide::Sell, 101.0, 5.0, 1);
        let mut a2 = resting_order(OrderSide::Sell, 102.0, 9.0, 2);
        let mut bid = resting_order(OrderSide::Buy, 100.0, 10.0, 3);
        ob.place_order(&mut a1);
        ob.place_order(&mut a2);
        ob.place_order(&mut bid);

        // Buy stop at 100 that sweeps at most up to 101.
        let mut stop = order_with(OrderSide::Buy, 101.0, 20.0, 4, |o| {
            o.stop_price = 100.0.into()
        });
        assert!(ob.execute_order(&mut stop).parked);

        // A print at 100 (the ask trades against the resting bid) elects it.
        let mut taker = resting_order(OrderSide::Sell, 100.0, 1.0, 5);
        let outcome = ob.execute_order(&mut taker);

        assert_eq!(1, outcome.triggers.len());
        let trigger = &outcome.triggers[0];
        assert_eq!(100.0, trigger.trigger_price);
        assert_eq!(
            vec![5.0],
            trigger
                .fills
                .iter()
                .map(|f| f.matches.to_f64())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            Some(15.0),
            trigger.cancelled.map(Fixed::to_f64),
            "the child never rests"
        );
        assert_eq!(
            9.0,
            ob.best_ask().expect("only the 102 level survives").volume
        );
        assert_eq!(9.0, ob.best_bid().expect("10 - 1 bid remains").volume);
    }

    #[test]
    fn exact_last_trade_price_does_not_round_stop_election() {
        let mut ob = OrderBook::new();
        let stop_price: Fixed = "100.000000000000000001".parse().unwrap();
        let trade_price: Fixed = "100.000000000000000002".parse().unwrap();

        let mut bid = Order::new(OrderSide::Buy, trade_price, 1, 1);
        ob.place_order(&mut bid);
        let mut stop = Order::new(OrderSide::Sell, Fixed::ZERO, 1, 2);
        stop.live = OrderType::Market;
        stop.stop_price = stop_price;
        ob.execute_order(&mut stop);
        assert!(ob.stops.len() == 1, "the stop parks");

        // A print above a sell stop must not elect it. Both values collapse to
        // the same f64, so a conversion here would incorrectly trigger it.
        let mut taker = Order::new(OrderSide::Sell, trade_price, 1, 3);
        let outcome = ob.execute_order(&mut taker);
        assert_eq!(Some(trade_price), ob.last_trade_price);
        assert!(outcome.triggers.is_empty(), "100...002 is above 100...001");
        assert_eq!(1, ob.stops.len());

        // A print exactly at the stop price does elect it.
        let mut bid = Order::new(OrderSide::Buy, stop_price, 1, 4);
        ob.place_order(&mut bid);
        let mut taker = Order::new(OrderSide::Sell, stop_price, 1, 5);
        let outcome = ob.execute_order(&mut taker);
        assert_eq!(1, outcome.triggers.len());
        assert_eq!(stop_price, outcome.triggers[0].trigger_price);
        assert!(ob.stops.is_empty());
    }

    #[test]
    fn spread_and_midpoint_keep_18_decimals() {
        let mut ob = OrderBook::new();
        let bid_price: Fixed = "100.000000000000000001".parse().unwrap();
        let ask_price: Fixed = "100.000000000000000003".parse().unwrap();
        let mut bid = Order::new(OrderSide::Buy, bid_price, 1, 1);
        let mut ask = Order::new(OrderSide::Sell, ask_price, 1, 2);
        ob.place_order(&mut bid);
        ob.place_order(&mut ask);

        assert_eq!(
            Some("0.000000000000000002".parse::<Fixed>().unwrap()),
            ob.spread()
        );
        assert_eq!(
            Some("100.000000000000000002".parse::<Fixed>().unwrap()),
            ob.mid_price()
        );
    }

    #[test]
    fn stop_that_the_last_trade_already_crossed_is_refused() {
        let mut ob = OrderBook::new();
        let mut bid = resting_order(OrderSide::Buy, 100.0, 10.0, 1);
        ob.place_order(&mut bid);
        let mut taker = resting_order(OrderSide::Sell, 100.0, 5.0, 2);
        ob.place_order(&mut taker);
        assert_eq!(Some(100.0), ob.last_trade_price.map(Fixed::to_f64));

        // A sell stop at or above the last trade would fire immediately, so a
        // stop never turns into a market order on entry.
        let mut stop = order_with(OrderSide::Sell, 0.0, 5.0, 3, |o| {
            o.live = OrderType::Market;
            o.stop_price = 101.0.into();
        });
        let outcome = ob.execute_order(&mut stop);

        assert_eq!(Some(RejectReason::StopWouldTrigger), outcome.rejected);
        assert!(ob.stops.is_empty());
        assert!(outcome.fills.is_empty());
    }

    // --- Expiry

    #[test]
    fn expiry_purge_removes_gtd_and_day_orders() {
        let mut ob = OrderBook::new();
        let mut gtd = order_with(OrderSide::Buy, 100.0, 10.0, 1, |o| {
            o.tif = TimeInForce::Gtd(2_000);
            o.timestamp = 1_000;
        });
        let mut day = order_with(OrderSide::Buy, 99.0, 10.0, 2, |o| o.tif = TimeInForce::Day);
        let mut gtc = order_with(OrderSide::Buy, 98.0, 10.0, 3, |_| {});
        let mut gtd_stop = order_with(OrderSide::Sell, 0.0, 5.0, 4, |o| {
            o.live = OrderType::Market;
            o.stop_price = 95.0.into();
            o.tif = TimeInForce::Gtd(2_000);
            o.timestamp = 1_000;
        });
        ob.execute_order(&mut gtd);
        ob.execute_order(&mut day);
        ob.execute_order(&mut gtc);
        ob.execute_order(&mut gtd_stop);
        assert_eq!(1, ob.stops.len());

        // Before either deadline nothing expires; Day without a close never does.
        assert!(ob.purge_expired(1_999, None).is_empty());
        assert_eq!(3, ob.bids.len());

        // At the GTD deadline both GTD orders (resting and parked) go.
        let expired = ob.purge_expired(2_000, None);
        assert_eq!(2, expired.len());
        assert_eq!(
            vec![0, 3],
            expired
                .iter()
                .map(|o| o.sequence as u32)
                .collect::<Vec<_>>()
        );
        assert!(ob.stops.is_empty());
        assert_eq!(2, ob.bids.len(), "Day and GTC stay without a close");

        // Day expires once a close at or before `now` is supplied.
        let expired = ob.purge_expired(1_000, Some(900));
        assert_eq!(1, expired.len());
        assert_eq!(1, expired[0].sequence);
        assert_eq!(1, ob.bids.len(), "the GTC order survives everything");
        assert_eq!(4, ob.sequence, "purging never rewinds the sequence");
    }

    // --- Validation

    #[test]
    fn invalid_orders_are_refused_without_touching_the_book() {
        let mut ob = OrderBook::new();

        let mut empty = resting_order(OrderSide::Buy, 100.0, 0.0, 1);
        assert_eq!(
            Some(RejectReason::InvalidOrder),
            ob.execute_order(&mut empty).rejected
        );

        let mut post_market = order_with(OrderSide::Buy, 0.0, 1.0, 2, |o| {
            o.live = OrderType::Market;
            o.post_only = true;
        });
        assert_eq!(
            Some(RejectReason::InvalidOrder),
            ob.execute_order(&mut post_market).rejected
        );

        let mut stop_iceberg = order_with(OrderSide::Sell, 100.0, 10.0, 3, |o| {
            o.display_volume = 4.0.into();
            o.stop_price = 95.0.into();
        });
        assert_eq!(
            Some(RejectReason::InvalidOrder),
            ob.execute_order(&mut stop_iceberg).rejected
        );

        let mut dead_gtd = order_with(OrderSide::Buy, 100.0, 1.0, 4, |o| {
            o.tif = TimeInForce::Gtd(500);
            o.timestamp = 500;
        });
        assert_eq!(
            Some(RejectReason::GtdAlreadyExpired),
            ob.execute_order(&mut dead_gtd).rejected
        );

        assert!(ob.bids.is_empty() && ob.asks.is_empty() && ob.stops.is_empty());
        assert_eq!(
            4, ob.sequence,
            "every submission consumes a sequence, even a refused one"
        );
    }

    #[test]
    fn stop_cascade_runs_each_elected_stop_in_admission_order() {
        let mut ob = OrderBook::new();
        let mut b1 = resting_order(OrderSide::Buy, 100.0, 6.0, 1);
        let mut b2 = resting_order(OrderSide::Buy, 95.0, 4.0, 2);
        let mut b3 = resting_order(OrderSide::Buy, 90.0, 5.0, 3);
        ob.place_order(&mut b1);
        ob.place_order(&mut b2);
        ob.place_order(&mut b3);

        let mut stop_95 = order_with(OrderSide::Sell, 0.0, 5.0, 4, |o| {
            o.live = OrderType::Market;
            o.stop_price = 95.0.into();
        });
        let mut stop_90 = order_with(OrderSide::Sell, 0.0, 3.0, 5, |o| {
            o.live = OrderType::Market;
            o.stop_price = 90.0.into();
        });
        ob.execute_order(&mut stop_95);
        ob.execute_order(&mut stop_90);
        assert_eq!(2, ob.stops.len());

        // Print 100: above both stops, nothing fires.
        let mut taker = resting_order(OrderSide::Sell, 100.0, 6.0, 6);
        assert!(ob.execute_order(&mut taker).triggers.is_empty());

        // Print 95 elects the first stop; its child sweeps down to 90, which
        // elects the second stop inside the same submission.
        let mut taker = resting_order(OrderSide::Sell, 95.0, 4.0, 7);
        let outcome = ob.execute_order(&mut taker);

        assert_eq!(2, outcome.triggers.len(), "the cascade must run both stops");
        assert_eq!(
            vec![3, 4],
            outcome
                .triggers
                .iter()
                .map(|t| t.stop.sequence)
                .collect::<Vec<_>>()
        );
        assert_eq!(95.0, outcome.triggers[0].trigger_price);
        assert_eq!(
            vec![5.0],
            outcome.triggers[0]
                .fills
                .iter()
                .map(|f| f.matches.to_f64())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            90.0, outcome.triggers[1].trigger_price,
            "the first child moved the price"
        );
        assert!(
            outcome.triggers[1].fills.is_empty(),
            "the book is empty by now"
        );
        assert_eq!(Some(3.0), outcome.triggers[1].cancelled.map(Fixed::to_f64));
        assert!(ob.stops.is_empty());
        assert!(ob.bids.is_empty());
        assert_eq!(Some(90.0), ob.last_trade_price.map(Fixed::to_f64));
    }

    #[test]
    fn parked_stop_is_cancelled_by_side_and_sequence() {
        let mut ob = OrderBook::new();
        let mut stop = order_with(OrderSide::Sell, 0.0, 5.0, 1, |o| {
            o.live = OrderType::Market;
            o.stop_price = 95.0.into();
        });
        ob.execute_order(&mut stop);
        assert_eq!(1, ob.stops.len());

        // The cancel request identifies the stop by side and sequence; its price
        // is a trigger target, not a book key.
        let mut request = Order::new(OrderSide::Sell, 0.0, 5.0, 0);
        request.stop_price = 95.0.into();
        request.sequence = stop.sequence;

        let cancelled = ob.cancel(&request).expect("the parked stop is cancellable");
        assert_eq!(stop.sequence, cancelled.sequence);
        assert!(ob.stops.is_empty());
        assert!(
            ob.cancel(&request).is_none(),
            "a second cancel finds nothing"
        );
    }

    // --- Amendment

    #[test]
    fn amend_quantity_down_keeps_time_priority() {
        let mut ob = OrderBook::new();
        let mut first = resting_order(OrderSide::Buy, 100.0, 10.0, 1);
        let mut second = resting_order(OrderSide::Buy, 100.0, 10.0, 2);
        ob.place_order(&mut first);
        ob.place_order(&mut second);

        let amended = ob.amend(&first, None, Some(4.0.into())).expect("amendable");

        assert_eq!(
            first.sequence, amended.order.sequence,
            "a reduction keeps the sequence"
        );
        assert_eq!(2, amended.market_seq, "the amendment consumes one sequence");
        assert_eq!(first.sequence, amended.previous_sequence);
        assert_eq!(4.0, amended.order.volume);
        let (key, best) = ob.bids.first_key_value().expect("the level survives");
        assert_eq!(first.sequence, key.sequence, "still first at the level");
        assert_eq!(4.0, best.volume);
        assert_eq!(
            Some(4.0),
            ob.resting_volume(&amended.order).map(Fixed::to_f64)
        );
    }

    #[test]
    fn amend_size_up_loses_priority() {
        let mut ob = OrderBook::new();
        let mut first = resting_order(OrderSide::Buy, 100.0, 10.0, 1);
        let mut second = resting_order(OrderSide::Buy, 100.0, 10.0, 2);
        ob.place_order(&mut first);
        ob.place_order(&mut second);
        let old_sequence = first.sequence;

        let amended = ob
            .amend(&first, None, Some(15.0.into()))
            .expect("amendable");

        assert_eq!(old_sequence, amended.previous_sequence);
        assert_eq!(2, amended.market_seq);
        assert_eq!(
            2, amended.order.sequence,
            "the amendment takes a fresh sequence"
        );
        let (key, _) = ob.bids.first_key_value().expect("both orders remain");
        assert_eq!(
            second.sequence, key.sequence,
            "the untouched order now leads"
        );
    }

    #[test]
    fn amend_price_moves_the_order_and_keeps_its_fields() {
        let mut ob = OrderBook::new();
        let mut ask = resting_order(OrderSide::Sell, 101.0, 10.0, 1);
        ask.tif = TimeInForce::Gtd(9_000);
        ask.timestamp = 1_000;
        ob.place_order(&mut ask);
        let old_sequence = ask.sequence;

        let amended = ob.amend(&ask, Some(102.0.into()), None).expect("amendable");

        assert_eq!(1, amended.market_seq);
        assert_eq!(102.0, amended.order.price);
        assert_eq!(
            TimeInForce::Gtd(9_000),
            amended.order.tif,
            "the rest of the order survives"
        );
        assert_eq!(1_000, amended.order.timestamp);
        assert_eq!(old_sequence, amended.previous_sequence);
        assert_ne!(old_sequence, amended.order.sequence);
        assert_eq!(
            None,
            ob.asks.get(&AskKey {
                price: 101.0.into(),
                sequence: old_sequence
            })
        );
        assert_eq!(Some(102.0), ob.best_ask().map(|order| order.price.to_f64()));
    }

    #[test]
    fn amend_refuses_unknown_crossing_and_unsupported_orders() {
        let mut ob = OrderBook::new();
        let mut ask = resting_order(OrderSide::Sell, 100.0, 5.0, 1);
        ob.place_order(&mut ask);
        let mut bid = resting_order(OrderSide::Buy, 99.0, 5.0, 2);
        ob.place_order(&mut bid);

        let unknown = resting_order(OrderSide::Buy, 99.0, 5.0, 99);
        assert_eq!(
            Err(AmendError::NotFound),
            ob.amend(&unknown, Some(98.0.into()), None).map(|_| ())
        );

        // Moving the bid through the ask would cross the book.
        assert_eq!(
            Err(AmendError::WouldCross),
            ob.amend(&bid, Some(101.0.into()), None).map(|_| ())
        );

        assert_eq!(
            Err(AmendError::InvalidVolume),
            ob.amend(&bid, None, Some(0.0.into())).map(|_| ())
        );
        assert_eq!(
            Err(AmendError::InvalidPrice),
            ob.amend(&bid, Some(Fixed::from(-1.0)), None).map(|_| ())
        );

        // Stops park off the book and cannot be amended.
        let mut stop = order_with(OrderSide::Sell, 0.0, 5.0, 3, |o| {
            o.live = OrderType::Market;
            o.stop_price = 95.0.into();
        });
        ob.execute_order(&mut stop);
        assert_eq!(
            Err(AmendError::NotResting),
            ob.amend(&stop, None, Some(1.0.into())).map(|_| ())
        );

        // Icebergs have no well-defined replacement volume here.
        let mut iceberg = order_with(OrderSide::Sell, 103.0, 10.0, 4, |o| {
            o.display_volume = 4.0.into()
        });
        ob.place_order(&mut iceberg);
        assert_eq!(
            Err(AmendError::UnsupportedType),
            ob.amend(&iceberg, None, Some(9.0.into())).map(|_| ())
        );
    }

    // --- Depth and spread

    #[test]
    fn depth_aggregates_levels_and_reports_spread() {
        let mut ob = OrderBook::new();
        for (price, volume) in [(100.0, 1.0), (100.0, 2.0), (99.0, 3.0), (98.0, 4.0)] {
            let mut bid = resting_order(OrderSide::Buy, price, volume, 0);
            ob.place_order(&mut bid);
        }
        let mut ask1 = resting_order(OrderSide::Sell, 101.0, 5.0, 0);
        let mut ask2 = resting_order(OrderSide::Sell, 102.0, 6.0, 0);
        ob.place_order(&mut ask1);
        ob.place_order(&mut ask2);

        let bids = ob.depth(OrderSide::Buy, 2);
        assert_eq!(
            vec![
                DepthLevel {
                    price: 100.0.into(),
                    volume: 3.0.into(),
                    orders: 2
                },
                DepthLevel {
                    price: 99.0.into(),
                    volume: 3.0.into(),
                    orders: 1
                },
            ],
            bids
        );

        let asks = ob.depth(OrderSide::Sell, 10);
        assert_eq!(2, asks.len());
        assert_eq!(101.0, asks[0].price);

        assert_eq!(Some(1.0), ob.spread().map(Fixed::to_f64));
        assert_eq!(Some(100.5), ob.mid_price().map(Fixed::to_f64));
        assert!(ob.depth(OrderSide::Buy, 0).is_empty());
    }

    #[test]
    fn depth_ignores_hidden_iceberg_reserve() {
        let mut ob = OrderBook::new();
        let mut iceberg = order_with(OrderSide::Sell, 100.0, 10.0, 1, |o| {
            o.display_volume = 4.0.into()
        });
        ob.place_order(&mut iceberg);

        let asks = ob.depth(OrderSide::Sell, 5);
        assert_eq!(1, asks.len());
        assert_eq!(4.0, asks[0].volume, "only the visible tranche is depth");
        assert_eq!(1, asks[0].orders);
    }

    #[test]
    fn an_empty_book_has_no_spread() {
        let ob = OrderBook::new();
        assert_eq!(None, ob.spread());
        assert_eq!(None, ob.mid_price());
        assert!(ob.depth(OrderSide::Buy, 10).is_empty());
        assert!(ob.depth(OrderSide::Sell, 10).is_empty());
    }

    // --- Depth journal

    fn sum_depth(journal: &[DepthChange], side: OrderSide, price: f64) -> (f64, i32) {
        let price = Fixed::from(price);
        let (volume, orders) = journal
            .iter()
            .filter(|change| change.side == side && change.price == price)
            .fold((Fixed::ZERO, 0), |(volume, orders), change| {
                (volume + change.delta_volume, orders + change.delta_orders)
            });
        (volume.to_f64(), orders)
    }

    #[test]
    fn depth_journal_records_rest_fill_and_cancel() {
        let mut ob = OrderBook::new();
        ob.enable_depth_journal();

        let mut maker = resting_order(OrderSide::Sell, 100.0, 10.0, 1);
        ob.place_order(&mut maker);
        assert_eq!(
            vec![DepthChange {
                side: OrderSide::Sell,
                price: 100.0.into(),
                delta_volume: 10.0.into(),
                delta_orders: 1,
            }],
            ob.take_depth_journal()
        );

        let mut taker = resting_order(OrderSide::Buy, 100.0, 4.0, 2);
        ob.place_order(&mut taker);
        assert_eq!(
            vec![DepthChange {
                side: OrderSide::Sell,
                price: 100.0.into(),
                delta_volume: (-4.0).into(),
                delta_orders: 0,
            }],
            ob.take_depth_journal()
        );

        let cancelled = ob.cancel(&maker).expect("the remainder rests");
        assert_eq!(6.0, cancelled.volume);
        assert_eq!(
            vec![DepthChange {
                side: OrderSide::Sell,
                price: 100.0.into(),
                delta_volume: (-6.0).into(),
                delta_orders: -1,
            }],
            ob.take_depth_journal()
        );
    }

    #[test]
    fn depth_journal_handles_iceberg_refill() {
        let mut ob = OrderBook::new();
        ob.enable_depth_journal();

        let mut iceberg = order_with(OrderSide::Sell, 100.0, 10.0, 1, |o| {
            o.display_volume = 4.0.into()
        });
        ob.place_order(&mut iceberg);
        assert_eq!(
            vec![DepthChange {
                side: OrderSide::Sell,
                price: 100.0.into(),
                delta_volume: 4.0.into(),
                delta_orders: 1,
            }],
            ob.take_depth_journal()
        );

        // 4 fills the tranche, the order refills 4, then 2 fill the next tranche:
        // the level went from 4 to 2, so the net recorded change is -2.
        let mut taker = resting_order(OrderSide::Buy, 100.0, 6.0, 2);
        ob.place_order(&mut taker);
        assert_eq!(
            (-2.0, 0),
            sum_depth(&ob.take_depth_journal(), OrderSide::Sell, 100.0)
        );
        assert_eq!(2.0, ob.best_ask().expect("the iceberg survives").volume);
    }

    #[test]
    fn depth_journal_records_amend_and_expiry() {
        let mut ob = OrderBook::new();
        ob.enable_depth_journal();

        let mut bid = resting_order(OrderSide::Buy, 100.0, 10.0, 1);
        ob.place_order(&mut bid);
        ob.take_depth_journal();

        // Same-price reduction: one in-place delta.
        ob.amend(&bid, None, Some(4.0.into())).expect("amendable");
        assert_eq!(
            vec![DepthChange {
                side: OrderSide::Buy,
                price: 100.0.into(),
                delta_volume: (-6.0).into(),
                delta_orders: 0,
            }],
            ob.take_depth_journal()
        );

        // Price change: the level is left and re-created under the new price.
        let amended = ob.amend(&bid, Some(99.0.into()), None).expect("amendable");
        let journal = ob.take_depth_journal();
        assert_eq!((-4.0, -1), sum_depth(&journal, OrderSide::Buy, 100.0));
        assert_eq!((4.0, 1), sum_depth(&journal, OrderSide::Buy, 99.0));

        // Expiry removes the level.
        let mut gtd = order_with(OrderSide::Buy, 98.0, 3.0, 0, |o| {
            o.tif = TimeInForce::Gtd(2_000);
            o.timestamp = 1_000;
        });
        ob.execute_order(&mut gtd);
        ob.take_depth_journal();
        assert_eq!(1, ob.purge_expired(2_000, None).len());
        assert_eq!(
            (-3.0, -1),
            sum_depth(&ob.take_depth_journal(), OrderSide::Buy, 98.0)
        );
        assert_eq!(99.0, amended.order.price);
    }

    #[test]
    fn depth_journal_is_off_by_default() {
        let mut ob = OrderBook::new();
        let mut bid = resting_order(OrderSide::Buy, 100.0, 1.0, 1);
        ob.place_order(&mut bid);
        assert!(ob.take_depth_journal().is_empty());
    }

    #[test]
    fn a_place_purges_due_gtd_before_matching() {
        let mut ob = OrderBook::new();
        let mut gtd = order_with(OrderSide::Buy, 100.0, 5.0, 0, |o| {
            o.tif = TimeInForce::Gtd(2_000);
            o.timestamp = 1_000;
        });
        assert!(ob.execute_order(&mut gtd).expired.is_empty());

        // The taker's intake timestamp is the purge trigger time.
        let mut taker = resting_order(OrderSide::Sell, 100.0, 2.0, 0);
        taker.timestamp = 2_000;
        let outcome = ob.execute_order(&mut taker);

        assert_eq!(1, outcome.expired.len(), "the GTD bid expires first");
        assert_eq!(gtd.sequence, outcome.expired[0].sequence);
        assert!(outcome.fills.is_empty(), "expired liquidity must not fill");
        assert_eq!(1, ob.asks.len(), "the sell rests on the ask side");
        assert!(ob.bids.is_empty());
        assert_eq!(taker.sequence, outcome.market_seq);
    }

    #[test]
    fn day_orders_need_an_explicit_purge() {
        let mut ob = OrderBook::new();
        let mut day = order_with(OrderSide::Buy, 100.0, 5.0, 0, |o| o.tif = TimeInForce::Day);
        ob.execute_order(&mut day);

        let mut taker = resting_order(OrderSide::Sell, 101.0, 1.0, 0);
        taker.timestamp = u64::MAX / 2;
        let outcome = ob.execute_order(&mut taker);
        assert!(outcome.expired.is_empty(), "Day waits for a market close");

        assert_eq!(1, ob.purge_expired(1_001, Some(1_001)).len());
        assert!(ob.bids.is_empty());
    }
}

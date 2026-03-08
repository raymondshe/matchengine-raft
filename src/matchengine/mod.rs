use std::cmp::Ordering;
use std::collections::BTreeMap;
use serde::{Serialize, Deserialize};

/// Specifies whether an order is a buy (bid) or sell (ask) order.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum OrderType {
    /// Market order - executes immediately at the best available price.
    Market,
    /// Limit order - executes only at the specified price or better.
    Limit,
}

/// Represents a single order in the order book.
///
/// An order contains all necessary information about a trader's request
/// to buy or sell an asset, including price, volume, and timing information
/// for matching and prioritization.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Order {
    /// The type of order (Limit or Market).
    pub live: OrderType,
    /// Whether this is a Buy or Sell order.
    pub side: OrderSide,
    /// The price level for this order (for limit orders).
    pub price: f64,
    /// The quantity of the asset to trade.
    pub volume: f64,

    /// Unique identifier for the order.
    pub id: u64,
    /// Sequence number for ordering orders at the same price level.
    ///
    /// This ensures price-time priority - orders with lower sequence
    /// numbers are matched first at the same price level.
    pub sequence: u64,
}

impl Order {
    /// Creates a new limit order with the specified parameters.
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
    /// A new Order instance with OrderType::Limit.
    pub fn new(side: OrderSide, price: f64, size: f64, sequence: u64) -> Self {
        Self {
            id: 0,
            side: side,
            live: OrderType::Limit, // TODO: More types
            price: price,
            volume: size,
            sequence: sequence,
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
#[derive(Debug, Clone, Copy,Serialize, Deserialize)]
pub struct BidKey {
    /// The price level of this bid order.
    pub price: f64,
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
            if self.sequence > other.sequence {
                Ordering::Greater
            } else if self.sequence < other.sequence {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        }
    }
}

/// PartialOrd implementation delegating to Ord.
impl PartialOrd for BidKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(&other))
    }
}

/// Key used to order sell (ask) orders in the BTreeMap.
///
/// AskKey implements natural price ordering (lowest price first) and
/// FIFO ordering at the same price level (lowest sequence first).
/// This ensures the best ask price is always at the front of the book.
#[derive(Debug, Clone, Copy,Serialize, Deserialize)]
pub struct AskKey {
    /// The price level of this ask order.
    pub price: f64,
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
            if self.sequence > other.sequence {
                Ordering::Greater
            } else if self.sequence < other.sequence {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        }
    }
}

/// PartialOrd implementation delegating to Ord.
impl PartialOrd for AskKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(&other))
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
        Ok(T::from_iter(container.into_iter()))
    }
}

/// The main order book structure containing both bids and asks.
///
/// The order book maintains all active orders in two sorted collections:
/// - Bids (buy orders) sorted by price descending, then sequence ascending
/// - Asks (sell orders) sorted by price ascending, then sequence ascending
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
    pub matches: f64,
}

/// Equality comparison for match results.
impl PartialEq for MatchResult {
    fn eq(&self, other: &Self) -> bool {
        (self.taker == other.taker) && (self.maker == other.maker) && (self.matches == other.matches)
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
    pub fn new(taker: &Order, maker: &Order, matches: f64) -> Self {
        Self {
            taker: taker.clone(),
            maker: maker.clone(),
            matches: matches,
        }
    }
}

impl OrderBook {
    /// Creates a new, empty order book.
    ///
    /// The order book starts with no bids or asks, and the sequence
    /// counter initialized to 0.
    pub fn new() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            sequence: 0,
        }
    }

    /// Returns the current best (highest) bid price order.
    ///
    /// The best bid is the highest price any buyer is willing to pay.
    /// Returns `None` if there are no bid orders in the book.
    pub fn best_bid(&self) -> Option<&Order> {
        match self.bids.iter().next() {
            None => None,
            Some((_key,value)) => Some(value),
        }
    }

    /// Returns the current best (lowest) ask price order.
    ///
    /// The best ask is the lowest price any seller is willing to accept.
    /// Returns `None` if there are no ask orders in the book.
    pub fn best_ask(&self) -> Option<&Order> {
        match self.asks.iter().next() {
            None => None,
            Some((_key,value)) => Some(value),
        }
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
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn insert_order(&mut self, order: &Order) {
        //tracing::debug!("insert_order: {:?})", order);
        match order.side {
            OrderSide::Buy => {
                self.bids.insert(BidKey{price: order.price, sequence: order.sequence}, order.clone());
            }
            OrderSide::Sell => {
                self.asks.insert(AskKey{price: order.price, sequence: order.sequence}, order.clone());
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
    /// # Arguments
    ///
    /// * `order` - The order to place (will have its sequence number set)
    ///
    /// # Returns
    ///
    /// A vector of `MatchResult` representing all trades that occurred,
    /// or an empty vector if no matches were made.
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn place_order(&mut self, order: &mut Order) -> Vec<MatchResult> {
        //tracing::debug!("place_order: [{:?}, +oo)", order);
        order.sequence = self.sequence;
        self.sequence += 1;

        match order.side {
            OrderSide::Buy => {
                return self.match_bid_order(&order);
            }
            OrderSide::Sell => {
                return self.match_ask_order(&order);
            }
        }
    }

    /// Cancels an existing order from the book.
    ///
    /// # Arguments
    ///
    /// * `order` - The order to cancel (must have the correct price and sequence)
    ///
    /// # Returns
    ///
    /// The cancelled order if found, or `None` if no matching order was in the book.
    pub fn cancel(&mut self, order: &Order) -> Option<Order>{
        match order.side {
            OrderSide::Buy => {
                let key = BidKey{price: order.price, sequence: order.sequence};
                return self.bids.remove(&key);
            }
            OrderSide::Sell => {
                let key = AskKey{price: order.price, sequence: order.sequence};
                return self.asks.remove(&key);
            }
        }
    }

    /// Attempts to match a buy (bid) order against the ask side of the book.
    ///
    /// The matching algorithm:
    /// 1. Starts with the best (lowest priced) ask
    /// 2. Matches as much as possible at each price level
    /// 3. If the ask is completely filled, removes it and continues
    /// 4. If the ask has remaining volume, updates it and stops
    /// 5. If the order can't be fully matched, adds the remainder to the bid side
    ///
    /// # Arguments
    ///
    /// * `order` - The buy order to match
    ///
    /// # Returns
    ///
    /// A vector of MatchResult for each trade executed
    fn match_bid_order(&mut self, order: &Order) -> Vec<MatchResult> {
        let mut results = Vec::new();
        let mut remaining_volume = order.volume;

        loop {
            match self.asks.iter().next() {
                None => {
                    // No more asks to match, add remaining to bids
                    if remaining_volume > 0.0 {
                        let mut remaining_order = order.clone();
                        remaining_order.volume = remaining_volume;
                        self.bids.insert(
                            BidKey{price: order.price, sequence: order.sequence},
                            remaining_order
                        );
                    }
                    break;
                }
                Some((key, best_ask)) => {
                    if best_ask.price > order.price {
                        // Best ask price is too high, add remaining to bids
                        if remaining_volume > 0.0 {
                            let mut remaining_order = order.clone();
                            remaining_order.volume = remaining_volume;
                            self.bids.insert(
                                BidKey{ price: order.price, sequence: order.sequence },
                                remaining_order
                            );
                        }
                        break;
                    }

                    // Can match with best ask
                    let match_volume = remaining_volume.min(best_ask.volume);
                    let mut temp_order = order.clone();
                    temp_order.volume = remaining_volume;
                    results.push(MatchResult::new(&temp_order, &best_ask, match_volume));

                    if best_ask.volume > remaining_volume {
                        // Update the best ask with remaining volume
                        let new_best_ask = Order::new(
                            OrderSide::Sell,
                            best_ask.price,
                            best_ask.volume - match_volume,
                            best_ask.sequence,
                        );
                        self.asks.insert(
                            AskKey{ price: new_best_ask.price, sequence: new_best_ask.sequence},
                            new_best_ask
                        );
                        // Fully matched
                        break;
                    } else {
                        // Remove the fully matched ask
                        let akey = AskKey{price: key.price, sequence: key.sequence};
                        self.asks.remove(&akey);
                        remaining_volume -= match_volume;
                        // Continue matching with next ask
                    }
                }
            }
        }

        results
    }

    /// Attempts to match a sell (ask) order against the bid side of the book.
    ///
    /// The matching algorithm:
    /// 1. Starts with the best (highest priced) bid
    /// 2. Matches as much as possible at each price level
    /// 3. If the bid is completely filled, removes it and continues
    /// 4. If the bid has remaining volume, updates it and stops
    /// 5. If the order can't be fully matched, adds the remainder to the ask side
    ///
    /// # Arguments
    ///
    /// * `order` - The sell order to match
    ///
    /// # Returns
    ///
    /// A vector of MatchResult for each trade executed
    fn match_ask_order(&mut self, order: &Order) -> Vec<MatchResult> {
        let mut results = Vec::new();
        let mut remaining_volume = order.volume;

        loop {
            match self.bids.iter().next() {
                None => {
                    // No more bids to match, add remaining to asks
                    if remaining_volume > 0.0 {
                        let mut remaining_order = order.clone();
                        remaining_order.volume = remaining_volume;
                        self.asks.insert(
                            AskKey{price: order.price, sequence: order.sequence},
                            remaining_order
                        );
                    }
                    break;
                }
                Some((key, best_bid)) => {
                    if best_bid.price < order.price {
                        // Best bid price is too low, add remaining to asks
                        if remaining_volume > 0.0 {
                            let mut remaining_order = order.clone();
                            remaining_order.volume = remaining_volume;
                            self.asks.insert(
                                AskKey{price: order.price, sequence: order.sequence},
                                remaining_order
                            );
                        }
                        break;
                    }

                    // Can match with best bid
                    let match_volume = remaining_volume.min(best_bid.volume);
                    let mut temp_order = order.clone();
                    temp_order.volume = remaining_volume;
                    results.push(MatchResult::new(&temp_order, &best_bid, match_volume));

                    if best_bid.volume > remaining_volume {
                        // Update the best bid with remaining volume
                        let new_best_bid = Order::new(
                            OrderSide::Buy,
                            best_bid.price,
                            best_bid.volume - match_volume,
                            best_bid.sequence,
                        );
                        self.bids.insert(
                            BidKey{price: new_best_bid.price, sequence: new_best_bid.sequence},
                            new_best_bid
                        );
                        // Fully matched
                        break;
                    } else {
                        // Remove the fully matched bid
                        let akey = BidKey{price: key.price, sequence: key.sequence};
                        self.bids.remove(&akey);
                        remaining_volume -= match_volume;
                        // Continue matching with next bid
                    }
                }
            }
        }

        results
    }
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
        let mut o1 = Order::new(OrderSide::Sell,100.22, 20.0, 1);
        let mut o2 = Order::new(OrderSide::Sell,100.22, 20.0, 2);
        let mut o3 = Order::new(OrderSide::Sell,100.24, 20.0, 3);
        let mut o4 = Order::new(OrderSide::Sell,100.20, 20.0, 4);

        ob.place_order(&mut o1);
        assert_eq!(ob.best_ask(), Some(&o1));
        ob.place_order(&mut o2);
        assert_eq!(ob.best_ask(), Some(&o1));
        ob.place_order(&mut o3);
        assert_eq!(ob.best_ask(), Some(&o1));
        ob.place_order(&mut o4);
        assert_eq!(ob.best_ask(), Some(&o4));
    }

    /// Tests that a partially filled order has its remaining quantity updated in the book.
    ///
    /// Places a sell order, then matches a buy order for part of it.
    /// Verifies that the remaining quantity is correctly maintained.
    #[test]
    fn order_replace() {
        let mut ob = OrderBook::new();
        let mut o1 = Order::new(OrderSide::Sell, 100.22, 22.0, 1);
        let o2 = Order::new(OrderSide::Buy, 100.22, 10.0, 1);

        let o3 = Order::new(OrderSide::Sell, 100.22, 12.0, 0);

        ob.place_order(&mut o1);
        assert_eq!(ob.best_ask(), Some(&o1));
        ob.match_bid_order(&o2);
        assert_eq!(ob.best_ask(), Some(&o3));
        /*
        let serialized = serde_json::to_string(&ob).unwrap();
        println!("serialized = {}", serialized);

        let deserialized: OrderBook = serde_json::from_str(&serialized).unwrap();
        println!("deserialized = {:?}", deserialized);
        */
    }

}

use std::cmp::Ordering;
use std::collections::BTreeMap;
use serde::{Serialize, Deserialize};


#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderCmd {
    Place,
    Cancel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum OrderType {
    Market,
    Limit,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Order {
    pub live: OrderType,
    pub side: OrderSide,
    pub price: f64,
    pub volume: f64,

    // ** For unique order
    pub id: u64,
    pub sequance: u64,
}

impl Order {
    pub fn new(side: OrderSide, price: f64, size: f64,sequance: u64) -> Self {
        Self {
            id: 0,
            side: side,
            live: OrderType::Limit, // TODO: More types
            price: price,
            volume: size,
            sequance: sequance,
        }
    }
}

impl PartialEq for Order {
    fn eq(&self, other: &Self) -> bool {
        (self.sequance == other.sequance)
            && (self.price == other.price)
            && (self.volume == self.volume)
    }
}
impl Eq for Order {}

#[derive(Debug, Clone, Copy,Serialize, Deserialize)]
pub struct BidKey {
    pub price: f64,
    pub sequance: u64,
}

impl PartialEq for BidKey {
    fn eq(&self, other: &Self) -> bool {
        self.sequance == other.sequance
    }
}

impl Eq for BidKey {}

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
            if self.sequance > other.sequance {
                Ordering::Greater
            } else if self.sequance < other.sequance {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        }
    }
}

// `PartialOrd` needs to be implemented as well.
impl PartialOrd for BidKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(&other))
    }
}

#[derive(Debug, Clone, Copy,Serialize, Deserialize)]
pub struct AskKey {
    pub price : f64,
    pub sequance : u64,
}

impl PartialEq for AskKey {
    fn eq(&self, other: &Self) -> bool {
        self.sequance == other.sequance
    }
}

impl Eq for AskKey {}

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
            if self.sequance > other.sequance {
                Ordering::Greater
            } else if self.sequance < other.sequance {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        }
    }
}

// `PartialOrd` needs to be implemented as well.
impl PartialOrd for AskKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(&other))
    }
}


pub mod vectorize {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::iter::FromIterator;

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

/// main OrderBook structure
#[derive(Clone, Default, Serialize, Deserialize, Debug)]
pub struct OrderBook {
    #[serde(with = "vectorize")]
    pub bids: BTreeMap<BidKey, Order>,
    #[serde(with = "vectorize")]
    pub asks: BTreeMap<AskKey, Order>,
    pub sequance: u64,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub struct MatchResult {
    pub taker: Order,
    pub maker: Order,
    pub matches: f64,
}

impl PartialEq for MatchResult {
    fn eq(&self, other: &Self) -> bool {
        (self.taker == other.taker) && (self.maker == other.maker) && (self.matches == other.matches)
    }
}


impl MatchResult {
    /// creates new matchresult
    pub fn new(taker: &Order, maker: &Order, matches: f64) -> Self {
        Self {
            taker: taker.clone(),
            maker: maker.clone(),
            matches: matches,
        }
    }
}

impl OrderBook {
    /// creates new orderbook
    pub fn new() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            sequance: 0,
        }
    }

    /// get current bid
    pub fn best_bid(&self) -> Option<&Order> {
        match self.bids.iter().next() {
            None => None,
            Some((_key,value)) => Some(value),
        }
    }

    /// get current ask
    pub fn best_ask(&self) -> Option<&Order> {
        match self.asks.iter().next() {
            None => None,
            Some((_key,value)) => Some(value),
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub fn insert_order(&mut self, order: &Order) {
        //tracing::debug!("insert_order: {:?})", order);
        match order.side {
            OrderSide::Buy => {
                self.bids.insert(BidKey{price: order.price, sequance: order.sequance}, order.clone());
            }
            OrderSide::Sell => {
                self.asks.insert(AskKey{price: order.price, sequance: order.sequance}, order.clone());
            }
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub fn place_order(&mut self, order: &mut Order) -> Vec<MatchResult> {
        //tracing::debug!("place_order: [{:?}, +oo)", order);
        order.sequance = self.sequance;
        self.sequance += 1;

        match order.side {
            OrderSide::Buy => {
                return self.match_bid_order(&order);
            }
            OrderSide::Sell => {
                return self.match_ask_order(&order);
            }
        }
    }

    pub fn cancle(&mut self, order: &Order) -> Option<Order>{
        match order.side {
            OrderSide::Buy => {
                let key = BidKey{price: order.price,sequance: order.sequance};
                return self.bids.remove(&key);
            }
            OrderSide::Sell => {
                let key = AskKey{price: order.price,sequance: order.sequance};
                return self.asks.remove(&key);
            }
        }      
    }

    fn match_bid_order(&mut self, order: &Order) -> Vec<MatchResult> {
        match self.asks.iter().next() {
            None => {
                self.bids.insert( 
                    BidKey{price: order.price,sequance:order.sequance}, 
                    order.clone() );
                return vec![];
            }
            Some((key, best_ask)) => {
                if best_ask.price > order.price {
                    self.bids.insert(
                        BidKey{ price: order.price, sequance: order.sequance },
                        order.clone()
                    );
                    return vec![];
                } else {
                    // Take order
                    if best_ask.volume >= order.volume {
                        let matches = order.volume;
                        let mr = MatchResult::new(order, &best_ask, matches);

                        if best_ask.volume > order.volume {
                            let new_best_ask = Order::new(
                                OrderSide::Buy,
                                best_ask.price,
                                best_ask.volume - matches,
                                best_ask.sequance,
                            );
                            self.asks.insert(
                                AskKey{ price: new_best_ask.price, sequance: new_best_ask.sequance},
                                new_best_ask
                            );
                        } else {
                            let akey = AskKey{price:key.price, sequance:key.sequance};
                            self.asks.remove(&akey);
                        }
                        return vec![mr];
                    } else {
                        let mut mrs = vec![MatchResult::new(order, &best_ask, best_ask.volume)];
                        let new_order = Order::new(
                                order.side,
                                order.price, 
                                order.volume - best_ask.volume, 
                                order.sequance);
                        let akey = AskKey{price:key.price, sequance:key.sequance};
                        self.asks.remove(&akey);
                        // Recursion
                        let mut new_mrs = self.match_bid_order(&new_order);
                        mrs.append(&mut new_mrs);
                        return mrs;
                    }
                }
            }
        }
    }

    fn match_ask_order(&mut self, order: &Order) -> Vec<MatchResult> {
        match self.bids.iter().next() {
            None => {
                self.asks.insert(
                    AskKey{price: order.price, sequance: order.sequance},
                    order.clone()
                );
                return vec![];
            }
            Some((key, best_bid)) => {
                if best_bid.price < order.price {
                    self.asks.insert(
                        AskKey{price: order.price, sequance: order.sequance},
                        order.clone()
                    );
                    return vec![];
                } else {
                    // Take order
                    if best_bid.volume >= order.volume {
                        let matches = order.volume;
                        let mr = MatchResult::new(order, &best_bid, matches);

                        if best_bid.volume > order.volume {
                            let new_best_bid = Order::new(
                                order.side,
                                best_bid.price,
                                best_bid.volume - matches,
                                best_bid.sequance,
                            );
                            self.bids.insert(BidKey{price: new_best_bid.price, sequance: new_best_bid.sequance},
                                new_best_bid
                            );
                        } else {
                            let akey = BidKey{price:key.price, sequance:key.sequance};
                            self.bids.remove(&akey);
                        }
                        return vec![mr];
                    } else {
                        let mut mrs = vec![MatchResult::new(order, &best_bid, best_bid.volume)];
                        let new_order = Order::new(
                            order.side,
                            order.price, 
                            order.volume - best_bid.volume, 
                            order.sequance);
                        let akey = BidKey{price:key.price, sequance:key.sequance};
                        self.bids.remove(&akey);
                        let mut new_mrs = self.match_ask_order(&new_order);
                        mrs.append(&mut new_mrs);
                        return mrs;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

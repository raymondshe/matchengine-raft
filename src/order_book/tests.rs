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

#[test]
fn order_equality_uses_identity_and_sequence_defaults() {
    let base = Order::new(OrderSide::Buy, 100.0, 1.0, 7);
    let same = base;
    assert_eq!(base, same);

    let mut different_id = base;
    different_id.id = 1;
    assert_ne!(base, different_id);

    let mut different_side = base;
    different_side.side = OrderSide::Sell;
    assert_ne!(base, different_side);

    let mut different_symbol = base;
    different_symbol.symbol_id = 2;
    assert_ne!(base, different_symbol);

    // `sequence` is optional on the wire; apply overwrites it anyway.
    let mut json = serde_json::to_value(base).unwrap();
    let _ = json.as_object_mut().unwrap().remove("sequence");
    let decoded: Order = serde_json::from_value(json).unwrap();
    assert_eq!(0, decoded.sequence);
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
fn fok_preflight_saturates_instead_of_overflowing() {
    let huge = Fixed::from_raw(i128::MAX / 2);
    let mut ob = OrderBook::new();
    let mut a1 = Order::new(OrderSide::Sell, 100, huge, 1);
    let mut a2 = Order::new(OrderSide::Sell, 100, huge, 2);
    ob.place_order(&mut a1);
    ob.place_order(&mut a2);

    let mut fok = Order::new(OrderSide::Buy, 100, 1, 3);
    fok.tif = TimeInForce::Fok;
    let outcome = ob.execute_order(&mut fok);

    assert_eq!(None, outcome.rejected);
    assert_eq!(1, outcome.fills.len());
    assert_eq!(Fixed::from(1), outcome.fills[0].matches);
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
fn fill_fees_use_taker_and_maker_rates() {
    let rate = |value: &str| value.parse::<Fixed>().unwrap();
    let mut ob = OrderBook::new();
    let mut maker =
        Order::new(OrderSide::Sell, 100, 2, 1).with_fee_rates(rate("0.001"), rate("0.002"));
    ob.place_order(&mut maker);
    let mut taker =
        Order::new(OrderSide::Buy, 100, 1, 2).with_fee_rates(rate("0.003"), rate("0.004"));

    let outcome = ob.execute_order(&mut taker);
    assert_eq!(1, outcome.fills.len());
    let fill = outcome.fills[0];
    assert_eq!(rate("100"), fill.notional);
    assert_eq!(
        rate("0.004"),
        fill.taker_fee,
        "taker buys, so the taker fee is in base"
    );
    assert_eq!(FeeAsset::Base, fill.taker_fee_asset);
    assert_eq!(
        rate("0.1"),
        fill.maker_fee,
        "the maker sells, so its fee is in quote"
    );
    assert_eq!(FeeAsset::Quote, fill.maker_fee_asset);
    assert!(fill.error.is_empty());

    let remaining = ob.best_ask().expect("the maker still has volume");
    assert_eq!(rate("0.001"), remaining.maker_fee_rate);
    assert_eq!(rate("0.002"), remaining.taker_fee_rate);
}

#[test]
fn maker_rebate_and_rested_taker_use_the_maker_rate() {
    let rate = |value: &str| value.parse::<Fixed>().unwrap();
    let mut ob = OrderBook::new();
    let mut resting =
        Order::new(OrderSide::Buy, 100, 2, 1).with_fee_rates(rate("-0.0005"), rate("0.01"));
    ob.place_order(&mut resting);
    let mut taker = Order::new(OrderSide::Sell, 100, 1, 2).with_fee_rates(rate("0"), rate("0"));

    let outcome = ob.execute_order(&mut taker);
    let fill = outcome.fills[0];
    assert_eq!(rate("-0.0005"), fill.maker_fee);
    assert_eq!(FeeAsset::Base, fill.maker_fee_asset);
    assert_eq!(Fixed::ZERO, fill.taker_fee);
    assert_eq!(FeeAsset::Quote, fill.taker_fee_asset);
}

#[test]
fn quote_limit_order_normalizes_to_base_at_the_limit_price() {
    let config = SymbolConfig::new(0, 2, 4).unwrap();
    let mut ob = OrderBook::new();
    let mut order = Order::new(OrderSide::Buy, 100, 1000, 1).with_volume_unit(VolumeUnit::Quote);
    let outcome = ob.execute_order_with_symbol(&mut order, config);

    assert_eq!(None, outcome.rejected);
    assert_eq!(Fixed::from(10), order.volume);
    assert_eq!(VolumeUnit::Base, order.volume_unit);
    assert_eq!(Fixed::from(1000), order.requested_volume);
    assert_eq!(VolumeUnit::Quote, order.requested_volume_unit);
    assert_eq!(1, ob.bids.len());
    assert_eq!(Fixed::from(10), ob.best_bid().unwrap().volume);

    // The configured base lot can make a quote amount too small to place.
    let config = SymbolConfig::new(0, 2, 0)
        .unwrap()
        .with_tick_and_lot("0.01".parse().unwrap(), "1".parse().unwrap());
    let mut too_small = Order::new(OrderSide::Buy, 100, 50, 2).with_volume_unit(VolumeUnit::Quote);
    assert_eq!(
        Some(RejectReason::InvalidOrder),
        ob.execute_order_with_symbol(&mut too_small, config)
            .rejected
    );
}

#[test]
fn quote_limit_with_zero_or_overflowing_price_is_rejected() {
    let mut ob = OrderBook::new();

    // A quote limit normalizes volume / price before shape validation; a
    // zero price must be rejected, not panic in the division.
    let mut zero = Order::new(OrderSide::Buy, 0, 100, 1).with_volume_unit(VolumeUnit::Quote);
    assert_eq!(
        Some(RejectReason::InvalidOrder),
        ob.execute_order(&mut zero).rejected
    );
    assert!(ob.bids.is_empty());

    // A result that does not fit the scale-18 range is also rejected.
    let mut overflow = Order::new(
        OrderSide::Buy,
        Fixed::from_raw(1),
        Fixed::from_raw(i128::MAX),
        2,
    )
    .with_volume_unit(VolumeUnit::Quote);
    assert_eq!(
        Some(RejectReason::InvalidOrder),
        ob.execute_order(&mut overflow).rejected
    );
    assert!(ob.bids.is_empty());
}

#[test]
fn quote_market_buy_spends_the_budget_and_cancels_dust() {
    let config = SymbolConfig::new(0, 2, 4).unwrap();
    let mut ob = OrderBook::new();
    let mut maker = Order::new(OrderSide::Sell, 100, 2, 1);
    ob.place_order(&mut maker);

    let mut taker = Order::new(OrderSide::Buy, 0, 150, 2).with_volume_unit(VolumeUnit::Quote);
    taker.live = OrderType::Market;
    let outcome = ob.execute_order_with_symbol(&mut taker, config.clone());

    assert_eq!(1, outcome.fills.len());
    assert_eq!(outcome.fills[0].matches, "1.5".parse::<Fixed>().unwrap());
    assert_eq!(outcome.fills[0].notional, Fixed::from(150));
    assert_eq!(None, outcome.cancelled);
    assert_eq!(VolumeUnit::Quote, taker.volume_unit);
    assert_eq!(
        "0.5".parse::<Fixed>().unwrap(),
        ob.best_ask().unwrap().volume
    );

    let mut dust_book = OrderBook::new();
    let mut maker = Order::new(OrderSide::Sell, 100, 2, 3);
    dust_book.place_order(&mut maker);
    let mut dusty = Order::new(OrderSide::Buy, 0, "150.005".parse::<Fixed>().unwrap(), 4)
        .with_volume_unit(VolumeUnit::Quote);
    dusty.live = OrderType::Market;
    let outcome = dust_book.execute_order_with_symbol(&mut dusty, config);
    assert_eq!(outcome.fills[0].matches, "1.5".parse::<Fixed>().unwrap());
    assert_eq!(Some("0.005".parse::<Fixed>().unwrap()), outcome.cancelled);
}

#[test]
fn quote_market_sell_targets_gross_proceeds_and_reports_overshoot() {
    let config = SymbolConfig::new(0, 2, 4).unwrap();
    let mut ob = OrderBook::new();
    let mut maker = Order::new(OrderSide::Buy, 100, 2, 1);
    ob.place_order(&mut maker);

    let mut taker = Order::new(OrderSide::Sell, 0, "150.005".parse::<Fixed>().unwrap(), 2)
        .with_volume_unit(VolumeUnit::Quote);
    taker.live = OrderType::Market;
    let outcome = ob.execute_order_with_symbol(&mut taker, config);
    assert_eq!(1, outcome.fills.len());
    assert_eq!(outcome.fills[0].matches, "1.5001".parse::<Fixed>().unwrap());
    assert_eq!(
        outcome.fills[0].notional,
        "150.01".parse::<Fixed>().unwrap()
    );
    assert_eq!(None, outcome.cancelled);
}

#[test]
fn tick_and_lot_increments_are_enforced() {
    let config = SymbolConfig::new(0, 2, 4).unwrap();
    let mut ob = OrderBook::new();
    let mut bad_price = Order::new(OrderSide::Buy, "100.001".parse::<Fixed>().unwrap(), 1, 1);
    assert_eq!(
        Some(RejectReason::InvalidOrder),
        ob.execute_order_with_symbol(&mut bad_price, config.clone())
            .rejected
    );
    let mut bad_lot = Order::new(OrderSide::Buy, 100, "0.00001".parse::<Fixed>().unwrap(), 2);
    assert_eq!(
        Some(RejectReason::InvalidOrder),
        ob.execute_order_with_symbol(&mut bad_lot, config).rejected
    );
}

#[test]
fn fee_asset_policies_charge_the_expected_asset() {
    let rate = |value: &str| value.parse::<Fixed>().unwrap();
    for (policy, taker_asset, maker_asset, taker_fee, maker_fee) in [
        (
            FeeAssetPolicy::Received,
            FeeAsset::Base,
            FeeAsset::Quote,
            rate("0.004"),
            rate("0.1"),
        ),
        (
            FeeAssetPolicy::Quote,
            FeeAsset::Quote,
            FeeAsset::Quote,
            rate("0.4"),
            rate("0.1"),
        ),
        (
            FeeAssetPolicy::Base,
            FeeAsset::Base,
            FeeAsset::Base,
            rate("0.004"),
            rate("0.001"),
        ),
    ] {
        let config = SymbolConfig::new(0, 2, 8)
            .unwrap()
            .with_fee_asset_policy(policy);
        let mut ob = OrderBook::new();
        let mut maker =
            Order::new(OrderSide::Sell, 100, 2, 1).with_fee_rates(rate("0.001"), rate("0"));
        ob.place_order(&mut maker);
        let mut taker =
            Order::new(OrderSide::Buy, 100, 1, 2).with_fee_rates(rate("0"), rate("0.004"));
        let outcome = ob.execute_order_with_symbol(&mut taker, config);
        let fill = outcome.fills[0];
        assert_eq!(taker_asset, fill.taker_fee_asset, "policy {policy:?}");
        assert_eq!(maker_asset, fill.maker_fee_asset, "policy {policy:?}");
        assert_eq!(taker_fee, fill.taker_fee, "policy {policy:?}");
        assert_eq!(maker_fee, fill.maker_fee, "policy {policy:?}");
    }
}

#[test]
fn notional_overflow_is_reported_explicitly() {
    let huge = Fixed::from_units(10_000_000_000_000_000_000);
    let maker = Order::new(OrderSide::Sell, huge, 1, 1);
    let taker = Order::new(OrderSide::Buy, huge, 1, 2);
    let fill = MatchResult::new(&taker, &maker, huge);
    assert!(fill.error.contains(MatchErrorCode::NotionalOverflow));
    assert!(fill.error.contains(MatchErrorCode::MakerFeeOverflow));
    assert!(!fill.error.contains(MatchErrorCode::TakerFeeOverflow));
    assert_eq!(Fixed::ZERO, fill.notional);
    assert_eq!(Fixed::ZERO, fill.taker_fee);
    assert_eq!(Fixed::ZERO, fill.maker_fee);
}

#[test]
fn notional_overflow_keeps_base_fees_that_fit() {
    let rate = |value: &str| value.parse::<Fixed>().unwrap();
    let price = Fixed::from_units(100_000_000_000);
    let volume = Fixed::from_units(10_000_000_000);
    let maker =
        Order::new(OrderSide::Sell, price, volume, 1).with_fee_rates(rate("0.001"), Fixed::ZERO);
    let taker =
        Order::new(OrderSide::Buy, price, volume, 2).with_fee_rates(Fixed::ZERO, rate("0.002"));
    let fill = MatchResult::new_with_policy(&taker, &maker, volume, FeeAssetPolicy::Base);

    // The quote notional overflows, but base fees depend only on volume.
    assert_eq!(MatchErrors::NOTIONAL_OVERFLOW, fill.error);
    assert_eq!(Fixed::ZERO, fill.notional);
    assert_eq!(Fixed::from_units(10_000_000), fill.maker_fee);
    assert_eq!(Fixed::from_units(20_000_000), fill.taker_fee);
    assert_eq!(FeeAsset::Base, fill.maker_fee_asset);
    assert_eq!(FeeAsset::Base, fill.taker_fee_asset);
}

#[test]
fn match_errors_serialize_as_named_codes() {
    let mut errors = MatchErrors::NONE;
    errors.insert(MatchErrorCode::NotionalOverflow);
    errors.insert(MatchErrorCode::MakerFeeOverflow);
    assert_eq!(
        r#"["notional_overflow","maker_fee_overflow"]"#,
        serde_json::to_string(&errors).unwrap()
    );
    assert_eq!(
        MatchErrors::NOTIONAL_OVERFLOW,
        serde_json::from_str::<MatchErrors>("\"notional_overflow\"").unwrap()
    );
    assert_eq!(
        MatchErrors::TAKER_FEE_OVERFLOW,
        serde_json::from_str::<MatchErrors>(r#"["taker_fee_overflow"]"#).unwrap()
    );
    assert_eq!(
        MatchErrors::NONE,
        serde_json::from_str::<MatchErrors>("[]").unwrap()
    );
    assert!(MatchErrors::from_bits(0b1000).is_none());
}

#[test]
fn fee_rates_outside_the_supported_range_are_rejected() {
    let rate = |value: &str| value.parse::<Fixed>().unwrap();
    let mut ob = OrderBook::new();
    let mut bad = Order::new(OrderSide::Buy, 100, 1, 1)
        .with_fee_rates(rate("1.000000000000000001"), Fixed::ZERO);
    assert_eq!(
        Some(RejectReason::InvalidOrder),
        ob.execute_order(&mut bad).rejected
    );
    let mut bad = Order::new(OrderSide::Buy, 100, 1, 2)
        .with_fee_rates(Fixed::ZERO, rate("-1.000000000000000001"));
    assert_eq!(
        Some(RejectReason::InvalidOrder),
        ob.execute_order(&mut bad).rejected
    );
    assert!(ob.bids.is_empty() && ob.asks.is_empty());
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

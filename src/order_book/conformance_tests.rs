//! Focused conformance cases for order-type combinations the property tests
//! exercise statistically but that deserve an explicit expected outcome.

use super::*;

fn order_with(side: OrderSide, price: f64, volume: f64, apply: impl FnOnce(&mut Order)) -> Order {
    let mut order = Order::new(side, price, volume, 0);
    apply(&mut order);
    order
}

fn rate(value: &str) -> Fixed {
    value.parse().expect("valid fixed-point rate")
}

fn asks() -> OrderBook {
    let mut book = OrderBook::new();
    let mut first = Order::new(OrderSide::Sell, 100.0, 6.0, 0);
    let mut second = Order::new(OrderSide::Sell, 101.0, 5.0, 0);
    book.execute_order(&mut first);
    book.execute_order(&mut second);
    book
}

#[test]
fn market_fok_is_all_or_nothing_and_never_rests() {
    // The whole 11 available within the market sweep fills and leaves nothing.
    let mut book = asks();
    let mut fok = order_with(OrderSide::Buy, 0.0, 11.0, |order| {
        order.live = OrderType::Market;
        order.tif = TimeInForce::Fok;
    });
    let outcome = book.execute_order(&mut fok);

    assert_eq!(None, outcome.rejected);
    assert_eq!(
        vec![6.0, 5.0],
        outcome
            .fills
            .iter()
            .map(|fill| fill.matches.to_f64())
            .collect::<Vec<_>>()
    );
    assert_eq!(None, outcome.resting);
    assert_eq!(None, outcome.cancelled);
    assert!(book.asks.is_empty());

    // One unit more is unfillable, so a market FOK rejects without touching the
    // book. In particular it must not degrade into a market order that takes
    // everything and cancels the rest.
    let mut book = asks();
    let mut fok = order_with(OrderSide::Buy, 0.0, 12.0, |order| {
        order.live = OrderType::Market;
        order.tif = TimeInForce::Fok;
    });
    let outcome = book.execute_order(&mut fok);

    assert_eq!(Some(RejectReason::FokUnfillable), outcome.rejected);
    assert!(outcome.fills.is_empty());
    assert_eq!(None, outcome.resting);
    assert_eq!(None, outcome.cancelled);
    assert_eq!(2, book.asks.len());
}

#[test]
fn market_without_liquidity_cancels_the_whole_order() {
    let mut book = OrderBook::new();
    let mut market = order_with(OrderSide::Buy, 0.0, 5.0, |order| {
        order.live = OrderType::Market;
    });
    let outcome = book.execute_order(&mut market);

    assert_eq!(None, outcome.rejected);
    assert!(outcome.fills.is_empty());
    assert_eq!(Some(Fixed::from(5)), outcome.cancelled);
    assert_eq!(None, outcome.resting);
    assert!(book.bids.is_empty() && book.asks.is_empty());
}

#[test]
fn cancel_and_expiry_keep_hidden_iceberg_volume() {
    let mut book = OrderBook::new();
    let mut iceberg = order_with(OrderSide::Sell, 100.0, 10.0, |order| {
        order.display_volume = Fixed::from(4);
    });
    assert_eq!(
        Some(Fixed::from(4)),
        book.execute_order(&mut iceberg).resting
    );

    let removed = book.cancel(&iceberg).expect("the iceberg is resting");
    assert_eq!(Fixed::from(4), removed.volume);
    assert_eq!(Fixed::from(6), removed.hidden_volume);
    assert_eq!(Fixed::from(10), removed.open_volume());
    assert!(book.asks.is_empty());

    let mut gtd = order_with(OrderSide::Sell, 101.0, 10.0, |order| {
        order.display_volume = Fixed::from(4);
        order.tif = TimeInForce::Gtd(2_000);
        order.timestamp = 1_000;
    });
    assert_eq!(Some(Fixed::from(4)), book.execute_order(&mut gtd).resting);
    let expired = book.purge_expired(2_000, None);

    assert_eq!(1, expired.len());
    assert_eq!(Fixed::from(4), expired[0].volume);
    assert_eq!(Fixed::from(6), expired[0].hidden_volume);
    assert_eq!(Fixed::from(10), expired[0].open_volume());
    assert!(book.asks.is_empty());
}

#[test]
fn gtd_and_day_parked_stops_expire_before_triggering() {
    let mut book = OrderBook::new();
    let mut gtd_stop = order_with(OrderSide::Sell, 0.0, 5.0, |order| {
        order.live = OrderType::Market;
        order.stop_price = Fixed::from(95);
        order.tif = TimeInForce::Gtd(2_000);
        order.timestamp = 1_000;
    });
    assert!(book.execute_order(&mut gtd_stop).parked);
    assert_eq!(1, book.purge_expired(2_000, None).len());
    assert!(book.stops.is_empty());

    let mut day_stop = order_with(OrderSide::Sell, 0.0, 5.0, |order| {
        order.live = OrderType::Market;
        order.stop_price = Fixed::from(95);
        order.tif = TimeInForce::Day;
    });
    assert!(book.execute_order(&mut day_stop).parked);
    assert_eq!(1, book.purge_expired(1_001, Some(1_001)).len());
    assert!(book.stops.is_empty());
}

#[test]
fn fees_are_reported_on_market_ioc_and_stop_child_fills() {
    let mut book = OrderBook::new();
    let mut maker =
        Order::new(OrderSide::Sell, 100.0, 2.0, 0).with_fee_rates(rate("0.001"), rate("0.002"));
    book.execute_order(&mut maker);

    let mut market = order_with(OrderSide::Buy, 0.0, 1.0, |order| {
        order.live = OrderType::Market;
        order.maker_fee_rate = rate("0.003");
        order.taker_fee_rate = rate("0.004");
    });
    let market_fill = book.execute_order(&mut market).fills[0];
    assert_eq!(Fixed::from(100), market_fill.notional);
    assert_eq!(FeeAsset::Base, market_fill.taker_fee_asset);
    assert_eq!(rate("0.004"), market_fill.taker_fee);
    assert_eq!(FeeAsset::Quote, market_fill.maker_fee_asset);
    assert_eq!(rate("0.1"), market_fill.maker_fee);

    let mut ioc = order_with(OrderSide::Buy, 100.0, 1.0, |order| {
        order.tif = TimeInForce::Ioc;
        order.maker_fee_rate = rate("0.003");
        order.taker_fee_rate = rate("0.004");
    });
    let ioc_fill = book.execute_order(&mut ioc).fills[0];
    assert_eq!(Fixed::from(100), ioc_fill.notional);
    assert_eq!(rate("0.004"), ioc_fill.taker_fee);
    assert_eq!(rate("0.1"), ioc_fill.maker_fee);

    // A market stop child pays the stop owner's taker rate. The top-level
    // trade is only the election trigger; the child fills against the bid.
    let mut book = OrderBook::new();
    let mut bid =
        Order::new(OrderSide::Buy, 95.0, 3.0, 0).with_fee_rates(rate("0.001"), rate("0.002"));
    book.execute_order(&mut bid);

    let mut stop = order_with(OrderSide::Sell, 0.0, 5.0, |order| {
        order.live = OrderType::Market;
        order.stop_price = Fixed::from(95);
        order.maker_fee_rate = rate("0.003");
        order.taker_fee_rate = rate("0.004");
    });
    assert!(book.execute_order(&mut stop).parked);

    let mut trigger = Order::new(OrderSide::Sell, 95.0, 1.0, 0);
    let outcome = book.execute_order(&mut trigger);
    let child = outcome.triggers[0]
        .fills
        .first()
        .expect("the stop child fills against the remaining bid");
    assert_eq!(Fixed::from(2), child.matches);
    assert_eq!(Fixed::from(190), child.notional);
    assert_eq!(FeeAsset::Quote, child.taker_fee_asset);
    assert_eq!(rate("0.76"), child.taker_fee);
    assert_eq!(FeeAsset::Base, child.maker_fee_asset);
    assert_eq!(rate("0.002"), child.maker_fee);
    assert_eq!(Some(Fixed::from(3)), outcome.triggers[0].cancelled);
}

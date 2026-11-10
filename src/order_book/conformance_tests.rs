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

fn bid_sequences(book: &OrderBook) -> Vec<u64> {
    book.bids.values().map(|order| order.sequence).collect()
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
fn rejected_fok_is_atomic_but_consumes_one_sequence() {
    let mut book = OrderBook::new();
    let mut bid = Order::new(OrderSide::Buy, 99.0, 4.0, 0);
    let mut ask = Order::new(OrderSide::Sell, 100.0, 6.0, 0);
    let mut deeper_ask = Order::new(OrderSide::Sell, 101.0, 5.0, 0);
    let mut stop = order_with(OrderSide::Sell, 0.0, 2.0, |order| {
        order.live = OrderType::Market;
        order.stop_price = Fixed::from(90);
    });
    book.execute_order(&mut bid);
    book.execute_order(&mut ask);
    book.execute_order(&mut deeper_ask);
    assert!(book.execute_order(&mut stop).parked);
    assert_eq!(4, book.sequence, "four submissions consumed four sequences");

    let bids = book.bids.values().copied().collect::<Vec<_>>();
    let asks = book.asks.values().copied().collect::<Vec<_>>();
    let stops = book.stops.clone();
    let last_trade_price = book.last_trade_price;

    // Only 6 are reachable at 100, so a 7-lot FOK must not execute anything.
    let mut fok = order_with(OrderSide::Buy, 100.0, 7.0, |order| {
        order.tif = TimeInForce::Fok
    });
    let outcome = book.execute_order(&mut fok);

    assert_eq!(Some(RejectReason::FokUnfillable), outcome.rejected);
    assert_eq!(4, outcome.market_seq);
    assert_eq!(4, fok.sequence, "the rejected taker still gets a sequence");
    assert_eq!(
        5, book.sequence,
        "a rejection consumes exactly one sequence"
    );
    assert!(outcome.fills.is_empty());
    assert_eq!(None, outcome.resting);
    assert_eq!(None, outcome.cancelled);
    assert!(outcome.expired.is_empty());
    assert!(!outcome.parked);
    assert!(outcome.triggers.is_empty());
    assert_eq!(bids, book.bids.values().copied().collect::<Vec<_>>());
    assert_eq!(asks, book.asks.values().copied().collect::<Vec<_>>());
    assert_eq!(stops, book.stops);
    assert_eq!(last_trade_price, book.last_trade_price);
}

#[test]
fn iceberg_partial_fill_and_cancel_conserve_volume() {
    let mut book = OrderBook::new();
    let mut iceberg = order_with(OrderSide::Sell, 100.0, 10.0, |order| {
        order.display_volume = Fixed::from(4);
    });
    assert_eq!(
        Some(Fixed::from(4)),
        book.execute_order(&mut iceberg).resting
    );
    let iceberg_sequence = iceberg.sequence;

    let mut taker = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
    let outcome = book.execute_order(&mut taker);
    assert_eq!(1, outcome.fills.len());
    assert_eq!(Fixed::from(1), outcome.fills[0].matches);
    assert_eq!(iceberg_sequence, outcome.fills[0].maker.sequence);

    // A partial fill shrinks only the visible tranche; it does not draw from
    // the reserve until the tranche is fully consumed.
    let resting = book.best_ask().expect("the iceberg still rests");
    assert_eq!(iceberg_sequence, resting.sequence);
    assert_eq!(Fixed::from(3), resting.volume);
    assert_eq!(Fixed::from(6), resting.hidden_volume);
    assert_eq!(Fixed::from(9), resting.open_volume());

    let removed = book.cancel(&iceberg).expect("the iceberg is resting");
    assert_eq!(Fixed::from(3), removed.volume);
    assert_eq!(Fixed::from(6), removed.hidden_volume);
    assert_eq!(Fixed::from(9), removed.open_volume());
    assert_eq!(
        Fixed::from(10),
        outcome.fills[0].matches + removed.open_volume(),
        "the fill plus the cancellation must account for the submission"
    );
    assert!(book.asks.is_empty());
}

#[test]
fn iceberg_refill_keeps_identity_and_queue_priority() {
    let mut book = OrderBook::new();
    let mut first = order_with(OrderSide::Sell, 100.0, 10.0, |order| {
        order.display_volume = Fixed::from(4);
    });
    let mut second = order_with(OrderSide::Sell, 100.0, 4.0, |order| {
        order.display_volume = Fixed::from(4);
    });
    book.execute_order(&mut first);
    book.execute_order(&mut second);
    let first_sequence = first.sequence;
    let second_sequence = second.sequence;

    // The taker consumes A's first tranche, A refills, and the taker takes two
    // more from A. B never trades because A keeps its original queue position.
    let mut taker = Order::new(OrderSide::Buy, 100.0, 6.0, 0);
    let outcome = book.execute_order(&mut taker);
    assert_eq!(
        vec![
            (Fixed::from(4), first_sequence),
            (Fixed::from(2), first_sequence)
        ],
        outcome
            .fills
            .iter()
            .map(|fill| (fill.matches, fill.maker.sequence))
            .collect::<Vec<_>>()
    );

    let (best_key, best) = book.asks.first_key_value().expect("two icebergs rest");
    assert_eq!(first_sequence, best_key.sequence);
    assert_eq!(Fixed::from(2), best.volume);
    assert_eq!(Fixed::from(2), best.hidden_volume);
    assert_eq!(2, book.asks.len());
    assert_eq!(Some(Fixed::from(4)), book.resting_volume(&second));

    // The next taker finishes A (visible, refill, visible) before B is reached.
    let mut taker = Order::new(OrderSide::Buy, 100.0, 4.0, 0);
    let outcome = book.execute_order(&mut taker);
    assert_eq!(
        vec![
            (Fixed::from(2), first_sequence),
            (Fixed::from(2), first_sequence)
        ],
        outcome
            .fills
            .iter()
            .map(|fill| (fill.matches, fill.maker.sequence))
            .collect::<Vec<_>>()
    );
    assert_eq!(1, book.asks.len());
    let (best_key, best) = book.asks.first_key_value().expect("B still rests");
    assert_eq!(second_sequence, best_key.sequence);
    assert_eq!(Fixed::from(4), best.volume);
    assert_eq!(Fixed::ZERO, best.hidden_volume);
    assert_eq!(Some(Fixed::from(4)), book.resting_volume(&second));
}

#[test]
fn stop_cascade_conserves_each_child_and_never_rests_one() {
    let mut book = OrderBook::new();
    let mut first_bid = Order::new(OrderSide::Buy, 100.0, 4.0, 0);
    let mut second_bid = Order::new(OrderSide::Buy, 90.0, 3.0, 0);
    let mut third_bid = Order::new(OrderSide::Buy, 80.0, 2.0, 0);
    book.execute_order(&mut first_bid);
    book.execute_order(&mut second_bid);
    book.execute_order(&mut third_bid);

    // Sell stop-limit at 80 (triggered at 100) and sell stop-market at 90.
    let mut stop_limit = order_with(OrderSide::Sell, 80.0, 6.0, |order| {
        order.stop_price = Fixed::from(100);
    });
    let mut stop_market = order_with(OrderSide::Sell, 0.0, 10.0, |order| {
        order.live = OrderType::Market;
        order.stop_price = Fixed::from(90);
    });
    assert!(book.execute_order(&mut stop_limit).parked);
    assert!(book.execute_order(&mut stop_market).parked);
    let stop_limit_sequence = stop_limit.sequence;
    let stop_market_sequence = stop_market.sequence;
    assert_eq!(2, book.stops.len());

    let mut trigger = Order::new(OrderSide::Sell, 100.0, 4.0, 0);
    let outcome = book.execute_order(&mut trigger);
    assert_eq!(
        vec![Fixed::from(4)],
        outcome
            .fills
            .iter()
            .map(|fill| fill.matches)
            .collect::<Vec<_>>()
    );
    assert_eq!(2, outcome.triggers.len(), "the cascade must run both stops");

    let first = &outcome.triggers[0];
    assert_eq!(stop_limit_sequence, first.stop.sequence);
    assert_eq!(Fixed::from(100), first.trigger_price);
    assert_eq!(
        vec![
            (Fixed::from(3), Fixed::from(90)),
            (Fixed::from(2), Fixed::from(80))
        ],
        first
            .fills
            .iter()
            .map(|fill| (fill.matches, fill.maker.price))
            .collect::<Vec<_>>()
    );
    assert_eq!(Some(Fixed::from(1)), first.cancelled);
    assert_eq!(
        first.stop.open_volume(),
        first
            .fills
            .iter()
            .map(|fill| fill.matches)
            .fold(Fixed::ZERO, |total, volume| total + volume)
            + first.cancelled.unwrap_or(Fixed::ZERO),
        "the first child must fill or cancel every unit"
    );

    let second = &outcome.triggers[1];
    assert_eq!(stop_market_sequence, second.stop.sequence);
    assert_eq!(
        Fixed::from(80),
        second.trigger_price,
        "the first child moved the last trade price"
    );
    assert!(second.fills.is_empty(), "the book is empty by now");
    assert_eq!(Some(Fixed::from(10)), second.cancelled);
    assert_eq!(
        second.stop.open_volume(),
        second.cancelled.unwrap_or(Fixed::ZERO)
    );

    assert!(book.bids.is_empty());
    assert!(book.asks.is_empty());
    assert!(book.stops.is_empty(), "no stop may survive its election");
    assert_eq!(Some(Fixed::from(80)), book.last_trade_price);
}

#[test]
fn post_only_amend_keeps_its_flag_and_refuses_crossing() {
    let mut book = OrderBook::new();
    let mut ask = Order::new(OrderSide::Sell, 101.0, 5.0, 0);
    book.execute_order(&mut ask);
    let mut post_only = order_with(OrderSide::Buy, 100.0, 10.0, |order| {
        order.post_only = true;
    });
    assert_eq!(
        Some(Fixed::from(10)),
        book.execute_order(&mut post_only).resting
    );
    let original_sequence = post_only.sequence;
    assert_eq!(1, original_sequence);

    // A same-price reduction keeps time priority and the post-only flag.
    let amended = book
        .amend(&post_only, None, Some(Fixed::from(4)))
        .expect("same-price reduction");
    assert_eq!(2, amended.market_seq);
    assert_eq!(original_sequence, amended.order.sequence);
    assert!(amended.order.post_only);
    let best = book.best_bid().expect("the order still rests");
    assert_eq!(original_sequence, best.sequence);
    assert_eq!(Fixed::from(4), best.volume);

    // Moving onto the resting ask is still refused for a post-only order.
    assert_eq!(
        AmendError::WouldCross,
        book.amend(&post_only, Some(Fixed::from(101)), None)
            .unwrap_err()
    );
    let best = book.best_bid().expect("a refused amend leaves the order");
    assert_eq!(original_sequence, best.sequence);
    assert_eq!(Fixed::from(100), best.price);
    assert_eq!(Fixed::from(4), best.volume);
    assert!(best.post_only);
    assert_eq!(
        4, book.sequence,
        "the refused crossing amendment consumed one sequence"
    );

    // Once the level is gone, the price move re-inserts under a new sequence
    // and the post-only flag survives. The refused crossing amendment consumed
    // one sequence, so this is sequence 4, not 3.
    book.cancel(&ask).expect("the ask is resting");
    let amended = book
        .amend(&post_only, Some(Fixed::from(101)), None)
        .expect("no longer crosses");
    assert_eq!(4, amended.market_seq);
    assert_eq!(amended.market_seq, amended.order.sequence);
    assert!(amended.order.post_only);
    let best = book.best_bid().expect("the amended order moved");
    assert_eq!(Fixed::from(101), best.price);
    assert_eq!(Fixed::from(4), best.volume);
    assert!(best.post_only);
}

#[test]
fn amend_priority_matrix_matches_price_time_rules() {
    fn build() -> (OrderBook, Order, Order) {
        let mut book = OrderBook::new();
        let mut first = Order::new(OrderSide::Buy, 100.0, 10.0, 0);
        let mut second = Order::new(OrderSide::Buy, 100.0, 10.0, 0);
        book.execute_order(&mut first);
        book.execute_order(&mut second);
        (book, first, second)
    }

    fn sequences(book: &OrderBook) -> Vec<u64> {
        book.bids.keys().map(|key| key.sequence).collect()
    }

    // Same price, smaller volume: keep the sequence and time priority.
    let (mut book, first, second) = build();
    let amended = book
        .amend(&first, None, Some(Fixed::from(4)))
        .expect("same-price reduction");
    assert_eq!(2, amended.market_seq);
    assert_eq!(first.sequence, amended.order.sequence);
    assert_eq!(first.sequence, amended.previous_sequence);
    assert_eq!(Fixed::from(10), amended.previous_volume);
    assert_eq!(vec![first.sequence, second.sequence], sequences(&book));
    assert_eq!(Fixed::from(4), book.best_bid().unwrap().volume);

    // Same price, larger volume: re-insert behind the other order.
    let (mut book, first, second) = build();
    let amended = book
        .amend(&first, None, Some(Fixed::from(12)))
        .expect("same-price increase");
    assert_eq!(2, amended.market_seq);
    assert_eq!(2, amended.order.sequence);
    assert_eq!(first.sequence, amended.previous_sequence);
    assert_eq!(
        vec![second.sequence, amended.order.sequence],
        sequences(&book)
    );
    assert_eq!(Fixed::from(10), book.best_bid().unwrap().volume);
    assert_eq!(Some(Fixed::from(12)), book.resting_volume(&amended.order));

    // Price up: new sequence, best price, old level behind it.
    let (mut book, first, second) = build();
    let amended = book
        .amend(&first, Some(Fixed::from(101)), None)
        .expect("non-crossing price move up");
    assert_eq!(2, amended.market_seq);
    assert_eq!(2, amended.order.sequence);
    assert_eq!(
        vec![amended.order.sequence, second.sequence],
        sequences(&book)
    );
    let best = book.best_bid().expect("the amended bid is best");
    assert_eq!(Fixed::from(101), best.price);
    assert_eq!(Fixed::from(10), best.volume);

    // Price down: new sequence and now behind the untouched better bid.
    let (mut book, first, second) = build();
    let amended = book
        .amend(&first, Some(Fixed::from(99)), None)
        .expect("non-crossing price move down");
    assert_eq!(2, amended.market_seq);
    assert_eq!(2, amended.order.sequence);
    assert_eq!(
        vec![second.sequence, amended.order.sequence],
        sequences(&book)
    );
    let best = book.best_bid().expect("the untouched bid is best");
    assert_eq!(Fixed::from(100), best.price);
    assert_eq!(second.sequence, best.sequence);
}

#[test]
fn amend_parked_stop_updates_trigger_limit_and_volume() {
    let mut book = OrderBook::new();
    let mut stop = order_with(OrderSide::Sell, 94.0, 5.0, |order| {
        order.stop_price = Fixed::from(95);
    });
    assert!(book.execute_order(&mut stop).parked);
    let sequence = stop.sequence;

    // The key carries the new trigger; `price` and `volume` are the new child
    // limit and open volume. The stop keeps its sequence and stays parked.
    let mut key = stop;
    key.stop_price = Fixed::from(96);
    let amended = book
        .amend(&key, Some(Fixed::from(93)), Some(Fixed::from(7)))
        .expect("parked stop is amendable");

    assert_eq!(sequence, amended.order.sequence);
    assert_eq!(sequence, amended.previous_sequence);
    assert_eq!(Fixed::from(94), amended.previous_price);
    assert_eq!(Fixed::from(5), amended.previous_volume);
    assert_eq!(Fixed::from(96), amended.order.stop_price);
    assert_eq!(Fixed::from(93), amended.order.price);
    assert_eq!(Fixed::from(7), amended.order.volume);
    assert_eq!(1, book.stops.len());
    assert_eq!(Fixed::from(96), book.stops[0].stop_price);
    assert_eq!(Fixed::from(93), book.stops[0].price);
    assert_eq!(Fixed::from(7), book.stops[0].volume);
    assert!(book.bids.is_empty() && book.asks.is_empty());
}

#[test]
fn amend_parked_stop_rejects_an_already_crossed_trigger() {
    let mut book = OrderBook::new();
    let mut bid = Order::new(OrderSide::Buy, 95.0, 1.0, 0);
    let mut ask = Order::new(OrderSide::Sell, 95.0, 1.0, 0);
    book.execute_order(&mut bid);
    book.execute_order(&mut ask);
    assert_eq!(Some(Fixed::from(95)), book.last_trade_price);

    let mut stop = order_with(OrderSide::Sell, 0.0, 5.0, |order| {
        order.live = OrderType::Market;
        order.stop_price = Fixed::from(90);
    });
    assert!(book.execute_order(&mut stop).parked);
    let original = book.stops[0];

    let mut key = stop;
    key.stop_price = Fixed::from(96);
    assert_eq!(
        AmendError::StopWouldTrigger,
        book.amend(&key, None, Some(Fixed::from(2))).unwrap_err()
    );
    assert_eq!(
        original.stop_price, book.stops[0].stop_price,
        "a refused amend must not move the stop"
    );
    assert_eq!(original.volume, book.stops[0].volume);

    // Zero in the key's stop_price means "keep the trigger", so a volume-only
    // amend still succeeds.
    let amended = book
        .amend(&stop, None, Some(Fixed::from(2)))
        .expect("volume-only stop amend");
    assert_eq!(Fixed::from(90), amended.order.stop_price);
    assert_eq!(Fixed::from(2), amended.order.volume);
}

#[test]
fn amend_stop_market_rejects_a_limit_price() {
    let mut book = OrderBook::new();
    let mut stop = order_with(OrderSide::Sell, 0.0, 5.0, |order| {
        order.live = OrderType::Market;
        order.stop_price = Fixed::from(95);
    });
    assert!(book.execute_order(&mut stop).parked);

    assert_eq!(
        AmendError::UnsupportedType,
        book.amend(&stop, Some(Fixed::from(94)), None).unwrap_err()
    );
    assert_eq!(Fixed::from(5), book.stops[0].volume);
}

#[test]
fn amend_iceberg_reduction_keeps_priority_and_reserve() {
    let mut book = OrderBook::new();
    let mut first = order_with(OrderSide::Sell, 100.0, 10.0, |order| {
        order.display_volume = Fixed::from(4);
    });
    let mut second = Order::new(OrderSide::Sell, 100.0, 5.0, 0);
    book.execute_order(&mut first);
    book.execute_order(&mut second);
    let sequence = first.sequence;

    let amended = book
        .amend(&first, None, Some(Fixed::from(6)))
        .expect("iceberg reduction");
    assert_eq!(sequence, amended.order.sequence);
    assert_eq!(Fixed::from(4), amended.order.volume);
    assert_eq!(Fixed::from(2), amended.order.hidden_volume);
    assert_eq!(Fixed::from(10), amended.previous_volume);
    let (best_key, best) = book.asks.first_key_value().expect("two asks");
    assert_eq!(
        sequence, best_key.sequence,
        "the iceberg kept time priority"
    );
    assert_eq!(Fixed::from(4), best.volume);
    assert_eq!(Fixed::from(2), best.hidden_volume);

    // Shrinking below the display tranche moves the reserve into the visible
    // part without changing the sequence.
    let amended = book
        .amend(&first, None, Some(Fixed::from(2)))
        .expect("iceberg shrink below display");
    assert_eq!(sequence, amended.order.sequence);
    assert_eq!(Fixed::from(2), amended.order.volume);
    assert_eq!(Fixed::ZERO, amended.order.hidden_volume);
    assert_eq!(Fixed::from(6), amended.previous_volume);
}

#[test]
fn amend_iceberg_increase_reinserts_with_a_new_sequence() {
    let mut book = OrderBook::new();
    let mut first = order_with(OrderSide::Sell, 100.0, 10.0, |order| {
        order.display_volume = Fixed::from(4);
    });
    let mut second = Order::new(OrderSide::Sell, 100.0, 5.0, 0);
    book.execute_order(&mut first);
    book.execute_order(&mut second);
    let old_sequence = first.sequence;

    let amended = book
        .amend(&first, None, Some(Fixed::from(12)))
        .expect("iceberg increase");
    assert_ne!(old_sequence, amended.order.sequence);
    assert_eq!(old_sequence, amended.previous_sequence);
    assert_eq!(Fixed::from(10), amended.previous_volume);
    assert_eq!(Fixed::from(4), amended.order.volume);
    assert_eq!(Fixed::from(8), amended.order.hidden_volume);
    let sequences = book.asks.keys().map(|key| key.sequence).collect::<Vec<_>>();
    assert_eq!(vec![second.sequence, amended.order.sequence], sequences);

    // A price move re-inserts the iceberg again and preserves the display.
    let amended = book
        .amend(&amended.order, Some(Fixed::from(99)), None)
        .expect("iceberg price move");
    assert_eq!(2, book.asks.len());
    let (best_key, best) = book.asks.first_key_value().expect("two asks");
    assert_eq!(amended.order.sequence, best_key.sequence);
    assert_eq!(Fixed::from(99), best.price);
    assert_eq!(Fixed::from(4), best.volume);
    assert_eq!(Fixed::from(8), best.hidden_volume);
}

#[test]
fn amend_plain_order_rejects_a_stop_trigger() {
    let mut book = OrderBook::new();
    let mut bid = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
    book.execute_order(&mut bid);
    let mut key = bid;
    key.stop_price = Fixed::from(95);
    assert_eq!(
        AmendError::UnsupportedType,
        book.amend(&key, None, Some(Fixed::from(1))).unwrap_err()
    );
    assert_eq!(Fixed::from(5), book.best_bid().unwrap().volume);
}

#[test]
fn quote_iceberg_and_fok_are_rejected_at_admission() {
    let config = SymbolConfig::new(0, 2, 8).unwrap();
    let mut book = OrderBook::new();

    let mut iceberg =
        Order::new(OrderSide::Buy, 100.0, 100.0, 0).with_volume_unit(VolumeUnit::Quote);
    iceberg.display_volume = Fixed::from(1);
    assert_eq!(
        Some(RejectReason::InvalidOrder),
        book.execute_order_with_symbol(&mut iceberg, config.clone())
            .rejected
    );

    let mut fok = Order::new(OrderSide::Buy, 100.0, 100.0, 0).with_volume_unit(VolumeUnit::Quote);
    fok.tif = TimeInForce::Fok;
    assert_eq!(
        Some(RejectReason::InvalidOrder),
        book.execute_order_with_symbol(&mut fok, config.clone())
            .rejected
    );

    // A plain quote limit is the supported shape and still rests.
    let mut plain = Order::new(OrderSide::Buy, 100.0, 100.0, 0).with_volume_unit(VolumeUnit::Quote);
    assert_eq!(
        None,
        book.execute_order_with_symbol(&mut plain, config).rejected
    );
    assert_eq!(Fixed::from(1), book.best_bid().unwrap().volume);
    assert_eq!(
        3, book.sequence,
        "each rejected/ accepted submission consumes one sequence"
    );
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

#[test]
fn fills_print_at_the_maker_price_in_both_directions() {
    // A buy lifting a cheaper ask pays the resting ask's price.
    let mut book = OrderBook::new();
    let mut ask = Order::new(OrderSide::Sell, 100.0, 2.0, 0);
    book.execute_order(&mut ask);
    let mut bid = Order::new(OrderSide::Buy, 101.0, 2.0, 0);
    let outcome = book.execute_order(&mut bid);
    assert_eq!(1, outcome.fills.len());
    assert_eq!(Fixed::from(100), outcome.fills[0].maker.price);
    assert_eq!(Fixed::from(2), outcome.fills[0].matches);

    // A sell hitting a higher bid pays the resting bid's price.
    let mut book = OrderBook::new();
    let mut bid = Order::new(OrderSide::Buy, 100.0, 2.0, 0);
    book.execute_order(&mut bid);
    let mut ask = Order::new(OrderSide::Sell, 99.0, 2.0, 0);
    let outcome = book.execute_order(&mut ask);
    assert_eq!(1, outcome.fills.len());
    assert_eq!(Fixed::from(100), outcome.fills[0].maker.price);
    assert_eq!(Fixed::from(2), outcome.fills[0].matches);
}

#[test]
fn cancel_head_middle_and_scattered_orders_preserve_fifo() {
    let mut book = OrderBook::new();
    let mut a = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
    let mut b = Order::new(OrderSide::Buy, 100.0, 2.0, 0);
    let mut c = Order::new(OrderSide::Buy, 100.0, 3.0, 0);
    let mut d = Order::new(OrderSide::Buy, 100.0, 4.0, 0);
    for order in [&mut a, &mut b, &mut c, &mut d] {
        book.execute_order(order);
    }
    let seq = |order: &Order| order.sequence;
    assert_eq!(
        vec![seq(&a), seq(&b), seq(&c), seq(&d)],
        bid_sequences(&book)
    );

    // Cancel the head, then a middle sibling: the untouched orders keep their
    // arrival order.
    assert!(book.cancel(&a).is_some());
    assert_eq!(vec![seq(&b), seq(&c), seq(&d)], bid_sequences(&book));
    assert!(book.cancel(&c).is_some());
    assert_eq!(vec![seq(&b), seq(&d)], bid_sequences(&book));

    // Scatter cancels across two price levels and both queue positions.
    let mut e = Order::new(OrderSide::Buy, 99.0, 5.0, 0);
    let mut f = Order::new(OrderSide::Buy, 98.0, 6.0, 0);
    let mut g = Order::new(OrderSide::Buy, 99.0, 7.0, 0);
    for order in [&mut e, &mut f, &mut g] {
        book.execute_order(order);
    }
    assert_eq!(
        vec![seq(&b), seq(&d), seq(&e), seq(&g), seq(&f)],
        bid_sequences(&book)
    );
    assert!(book.cancel(&f).is_some(), "deepest level");
    assert!(book.cancel(&e).is_some(), "head of the second level");
    assert_eq!(vec![seq(&b), seq(&d), seq(&g)], bid_sequences(&book));
    assert!(book.cancel(&d).is_some(), "middle of the best level");
    assert_eq!(vec![seq(&b), seq(&g)], bid_sequences(&book));
}

#[test]
fn cancel_best_level_updates_bbo_without_a_follow_on_sweep() {
    let mut book = OrderBook::new();
    let mut bid_100 = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
    let mut bid_99 = Order::new(OrderSide::Buy, 99.0, 2.0, 0);
    let mut ask_101 = Order::new(OrderSide::Sell, 101.0, 3.0, 0);
    let mut ask_102 = Order::new(OrderSide::Sell, 102.0, 4.0, 0);
    for order in [&mut bid_100, &mut bid_99, &mut ask_101, &mut ask_102] {
        book.execute_order(order);
    }

    assert_eq!(Some(Fixed::from(100)), book.best_bid().map(|o| o.price));
    assert_eq!(Some(Fixed::from(101)), book.best_ask().map(|o| o.price));

    // Removing the best level must reseat the BBO immediately, with no trade
    // or sweep to make a stale cache self-heal.
    assert!(book.cancel(&bid_100).is_some());
    assert_eq!(Some(Fixed::from(99)), book.best_bid().map(|o| o.price));
    assert_eq!(
        vec![DepthLevel {
            price: Fixed::from(99),
            volume: Fixed::from(2),
            orders: 1,
        }],
        book.depth(OrderSide::Buy, 2)
    );

    assert!(book.cancel(&ask_101).is_some());
    assert_eq!(Some(Fixed::from(102)), book.best_ask().map(|o| o.price));
    assert_eq!(
        vec![DepthLevel {
            price: Fixed::from(102),
            volume: Fixed::from(4),
            orders: 1,
        }],
        book.depth(OrderSide::Sell, 2)
    );

    assert!(book.cancel(&bid_99).is_some());
    assert!(book.best_bid().is_none());
    assert!(book.depth(OrderSide::Buy, 2).is_empty());
    assert!(book.cancel(&ask_102).is_some());
    assert!(book.best_ask().is_none());
}

#[test]
fn multi_level_ioc_fills_best_price_first_and_cancels_remainder() {
    let mut book = OrderBook::new();
    for (price, volume) in [(100.0, 2.0), (101.0, 3.0), (102.0, 4.0)] {
        let mut ask = Order::new(OrderSide::Sell, price, volume, 0);
        book.execute_order(&mut ask);
    }

    let mut ioc = Order::new(OrderSide::Buy, 102.0, 10.0, 0);
    ioc.tif = TimeInForce::Ioc;
    let outcome = book.execute_order(&mut ioc);

    assert_eq!(None, outcome.rejected);
    assert_eq!(
        vec![Fixed::from(2), Fixed::from(3), Fixed::from(4)],
        outcome
            .fills
            .iter()
            .map(|fill| fill.matches)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        vec![Fixed::from(100), Fixed::from(101), Fixed::from(102)],
        outcome
            .fills
            .iter()
            .map(|fill| fill.maker.price)
            .collect::<Vec<_>>()
    );
    assert_eq!(Some(Fixed::from(1)), outcome.cancelled);
    assert_eq!(None, outcome.resting);
    assert!(book.asks.is_empty());
    assert!(book.bids.is_empty());
}

#[test]
fn marketable_limit_residual_rests_at_its_limit() {
    let mut book = OrderBook::new();
    for (price, volume) in [(100.0, 2.0), (101.0, 3.0), (102.0, 4.0)] {
        let mut ask = Order::new(OrderSide::Sell, price, volume, 0);
        book.execute_order(&mut ask);
    }

    let mut buy = Order::new(OrderSide::Buy, 101.0, 6.0, 0);
    let outcome = book.execute_order(&mut buy);

    assert_eq!(None, outcome.rejected);
    assert_eq!(
        vec![Fixed::from(2), Fixed::from(3)],
        outcome
            .fills
            .iter()
            .map(|fill| fill.matches)
            .collect::<Vec<_>>()
    );
    assert_eq!(Some(Fixed::from(1)), outcome.resting);
    assert_eq!(None, outcome.cancelled);
    let best = book.best_bid().expect("the residual rests");
    assert_eq!(Fixed::from(101), best.price);
    assert_eq!(Fixed::from(1), best.volume);
    assert_eq!(
        vec![DepthLevel {
            price: Fixed::from(102),
            volume: Fixed::from(4),
            orders: 1,
        }],
        book.depth(OrderSide::Sell, 2),
        "the residual must not walk through its own limit"
    );
}

#[test]
fn deep_same_price_sweep_conserves_every_fill() {
    let mut book = OrderBook::new();
    for _ in 0..5_000 {
        let mut ask = Order::new(OrderSide::Sell, 100.0, 1.0, 0);
        let outcome = book.execute_order(&mut ask);
        assert_eq!(None, outcome.rejected);
        assert_eq!(Some(Fixed::from(1)), outcome.resting);
    }

    let mut buy = Order::new(OrderSide::Buy, 100.0, 5_000.0, 0);
    let outcome = book.execute_order(&mut buy);

    assert_eq!(None, outcome.rejected);
    assert_eq!(5_000, outcome.fills.len());
    assert!(
        outcome
            .fills
            .iter()
            .all(|fill| fill.matches == Fixed::from(1))
    );
    assert_eq!(
        Fixed::from(5_000),
        outcome
            .fills
            .iter()
            .fold(Fixed::ZERO, |total, fill| total + fill.matches)
    );
    assert_eq!(None, outcome.resting);
    assert_eq!(None, outcome.cancelled);
    assert!(book.asks.is_empty());
    assert_eq!(5_001, book.sequence, "5,000 asks plus the taker");
}

#[test]
fn near_limit_prices_rest_and_cancel() {
    let max_price = Fixed::from_raw(i128::MAX);
    let mut book = OrderBook::new();
    let mut bid = Order::new(OrderSide::Buy, max_price, 1.0, 0);
    let outcome = book.execute_order(&mut bid);
    assert_eq!(None, outcome.rejected);
    assert_eq!(Some(max_price), book.best_bid().map(|order| order.price));
    assert_eq!(
        vec![DepthLevel {
            price: max_price,
            volume: Fixed::from(1),
            orders: 1,
        }],
        book.depth(OrderSide::Buy, 1)
    );
    assert!(book.cancel(&bid).is_some());
    assert!(book.best_bid().is_none());

    let min_price = Fixed::from_raw(1);
    let mut ask = Order::new(OrderSide::Sell, min_price, 1.0, 0);
    let outcome = book.execute_order(&mut ask);
    assert_eq!(None, outcome.rejected);
    assert_eq!(Some(min_price), book.best_ask().map(|order| order.price));
    assert_eq!(
        vec![DepthLevel {
            price: min_price,
            volume: Fixed::from(1),
            orders: 1,
        }],
        book.depth(OrderSide::Sell, 1)
    );
    assert!(book.cancel(&ask).is_some());
    assert!(book.best_ask().is_none());
}

#[test]
fn cancel_and_amend_of_a_filled_order_are_rejected() {
    let mut book = OrderBook::new();
    let mut ask = Order::new(OrderSide::Sell, 100.0, 1.0, 0);
    book.execute_order(&mut ask);
    let mut bid = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
    let outcome = book.execute_order(&mut bid);
    assert_eq!(1, outcome.fills.len());
    assert!(book.asks.is_empty());

    assert_eq!(None, book.cancel(&ask));
    assert!(matches!(
        book.amend(&ask, Some(Fixed::from(99)), None),
        Err(AmendError::NotFound)
    ));
    assert_eq!(None, book.cancel(&bid));
    assert!(matches!(
        book.amend(&bid, None, Some(Fixed::from(2))),
        Err(AmendError::NotFound)
    ));
}

#[test]
fn order_id_is_client_metadata_not_a_book_key() {
    let mut book = OrderBook::new();
    let mut first = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
    first.id = 7;
    let mut second = Order::new(OrderSide::Buy, 100.0, 2.0, 0);
    second.id = 7;
    book.execute_order(&mut first);
    book.execute_order(&mut second);

    assert_eq!(
        2,
        book.bids.len(),
        "duplicate client ids do not collide: sequence is the book key"
    );
    assert_ne!(first.sequence, second.sequence);

    // Cancelling by sequence removes only the intended order.
    assert!(book.cancel(&first).is_some());
    let remaining = book.best_bid().expect("the second order remains");
    assert_eq!(7, remaining.id);
    assert_eq!(second.sequence, remaining.sequence);
}

#[test]
fn market_to_limit_rests_the_residual_at_the_last_fill() {
    let mut book = OrderBook::new();
    let mut first = Order::new(OrderSide::Sell, 100.0, 2.0, 0);
    let mut second = Order::new(OrderSide::Sell, 101.0, 3.0, 0);
    book.execute_order(&mut first);
    book.execute_order(&mut second);

    let mut mtl = Order::new(OrderSide::Buy, 0.0, 6.0, 0);
    mtl.live = OrderType::Market;
    mtl.market_to_limit = true;
    let outcome = book.execute_order(&mut mtl);

    assert_eq!(None, outcome.rejected);
    assert_eq!(2, outcome.fills.len());
    assert_eq!(Some(Fixed::from(1)), outcome.resting);
    assert_eq!(None, outcome.cancelled);
    assert_eq!(Fixed::from(101), mtl.price);
    assert_eq!(OrderType::Limit, mtl.live);
    assert!(!mtl.market_to_limit);
    let best = book.best_bid().expect("the MTL residual rests");
    assert_eq!(Fixed::from(101), best.price);
    assert_eq!(Fixed::from(1), best.volume);

    // No liquidity means no reference price, so the whole order is cancelled.
    let mut empty = OrderBook::new();
    let mut mtl = Order::new(OrderSide::Buy, 0.0, 2.0, 0);
    mtl.live = OrderType::Market;
    mtl.market_to_limit = true;
    let outcome = empty.execute_order(&mut mtl);
    assert_eq!(None, outcome.rejected);
    assert_eq!(Some(Fixed::from(2)), outcome.cancelled);
    assert_eq!(None, outcome.resting);

    // Immediate types have no defined resting residual.
    let mut invalid = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
    invalid.live = OrderType::Market;
    invalid.market_to_limit = true;
    invalid.tif = TimeInForce::Ioc;
    assert_eq!(
        Some(RejectReason::InvalidOrder),
        OrderBook::new().execute_order(&mut invalid).rejected
    );
}

#[test]
fn trailing_stop_trails_the_print_and_elects_on_a_later_trade() {
    let mut book = OrderBook::new();
    let mut ask = Order::new(OrderSide::Sell, 100.0, 1.0, 0);
    book.execute_order(&mut ask);
    let mut buy = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
    buy.live = OrderType::Market;
    book.execute_order(&mut buy);
    assert_eq!(Some(Fixed::from(100)), book.last_trade_price);

    let mut stop = Order::new(OrderSide::Sell, 99.0, 1.0, 0);
    stop.trail_offset = Fixed::from(1);
    let outcome = book.execute_order(&mut stop);
    assert!(outcome.parked);
    let parked = book.stops.first().expect("the trailing stop is parked");
    assert_eq!(Fixed::from(100), parked.trail_reference);
    assert_eq!(Fixed::from(99), parked.stop_price);

    // A higher print lifts the reference and trigger but does not elect the stop.
    let mut ask = Order::new(OrderSide::Sell, 105.0, 1.0, 0);
    book.execute_order(&mut ask);
    let mut buy = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
    buy.live = OrderType::Market;
    let outcome = book.execute_order(&mut buy);
    assert!(outcome.triggers.is_empty());
    let parked = book.stops.first().expect("still parked");
    assert_eq!(Fixed::from(105), parked.trail_reference);
    assert_eq!(Fixed::from(104), parked.stop_price);

    // A retrace to the trailing trigger elects it; the child has no bid to hit
    // and cancels rather than resting.
    let mut ask = Order::new(OrderSide::Sell, 104.0, 1.0, 0);
    book.execute_order(&mut ask);
    let mut buy = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
    buy.live = OrderType::Market;
    let outcome = book.execute_order(&mut buy);
    assert_eq!(1, outcome.triggers.len());
    let trigger = &outcome.triggers[0];
    assert_eq!(Fixed::from(104), trigger.trigger_price);
    assert_eq!(Fixed::from(104), trigger.stop.stop_price);
    assert_eq!(Some(Fixed::from(1)), trigger.cancelled);
    assert!(book.stops.is_empty());
}

#[test]
fn pegged_order_reprices_against_non_pegged_liquidity() {
    let mut book = OrderBook::new();
    let mut bid = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
    let mut ask = Order::new(OrderSide::Sell, 102.0, 1.0, 0);
    book.execute_order(&mut bid);
    book.execute_order(&mut ask);

    let mut peg = Order::new(OrderSide::Buy, 0.0, 2.0, 0);
    peg.peg = Some(PegReference::Primary);
    let outcome = book.execute_order(&mut peg);
    assert_eq!(None, outcome.rejected);
    assert_eq!(Some(Fixed::from(2)), outcome.resting);
    assert_eq!(Fixed::from(100), peg.price);
    let original_sequence = peg.sequence;

    // A better non-pegged bid moves the primary peg up, with a new sequence.
    let mut better = Order::new(OrderSide::Buy, 101.0, 1.0, 0);
    book.execute_order(&mut better);
    let updates = book.reprice_pegged();
    assert_eq!(1, updates.len());
    match &updates[0] {
        PegUpdate::Amended(amendment) => {
            assert_eq!(Fixed::from(101), amendment.order.price);
            assert_eq!(original_sequence, amendment.previous_sequence);
            assert_ne!(original_sequence, amendment.order.sequence);
        }
        other => panic!("expected an amendment, got {other:?}"),
    }
    assert_eq!(
        1,
        book.bids
            .values()
            .filter(|order| order.is_pegged() && order.price == Fixed::from(101))
            .count()
    );

    // Removing the better non-pegged bid moves the peg back to the earlier
    // non-pegged reference.
    assert!(book.cancel(&better).is_some());
    let updates = book.reprice_pegged();
    assert_eq!(1, updates.len());
    match &updates[0] {
        PegUpdate::Amended(amendment) => {
            assert_eq!(Fixed::from(100), amendment.order.price);
        }
        other => panic!("expected an amendment, got {other:?}"),
    }

    // No non-pegged reference is a rejection, not an orphaned peg.
    let mut empty = OrderBook::new();
    let mut orphan = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
    orphan.peg = Some(PegReference::Midpoint);
    assert_eq!(
        Some(RejectReason::PegReferenceUnavailable),
        empty.execute_order(&mut orphan).rejected
    );
}

#[test]
fn pegged_order_rejects_a_crossing_target() {
    let mut book = OrderBook::new();
    let mut bid = Order::new(OrderSide::Buy, 99.0, 1.0, 0);
    let mut ask = Order::new(OrderSide::Sell, 100.0, 1.0, 0);
    book.execute_order(&mut bid);
    book.execute_order(&mut ask);

    let mut peg = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
    peg.peg = Some(PegReference::Primary);
    peg.peg_offset = Fixed::from(2);
    assert_eq!(
        Some(RejectReason::PegWouldCross),
        book.execute_order(&mut peg).rejected
    );
}

fn stp_book(config: SymbolConfig) -> (OrderBook, Order, Order) {
    let mut book = OrderBook::new();
    let mut same = Order::new(OrderSide::Sell, 100.0, 1.0, 0);
    same.owner_id = Some(1);
    let mut other = Order::new(OrderSide::Sell, 100.0, 1.0, 0);
    other.owner_id = Some(2);
    book.execute_order_with_symbol(&mut same, config.clone());
    book.execute_order_with_symbol(&mut other, config);
    (book, same, other)
}

#[test]
fn self_trade_prevention_modes_cancel_the_right_order() {
    // CancelResting: remove the same-owner maker and keep matching.
    let config = SymbolConfig::new(0, 2, 8)
        .unwrap()
        .with_stp(StpMode::CancelResting);
    let (mut book, same, _) = stp_book(config.clone());
    let mut taker = Order::new(OrderSide::Buy, 100.0, 2.0, 0);
    taker.owner_id = Some(1);
    let outcome = book.execute_order_with_symbol(&mut taker, config);
    assert_eq!(1, outcome.stp_cancelled.len());
    assert_eq!(same.sequence, outcome.stp_cancelled[0].order.sequence);
    assert_eq!(1, outcome.fills.len());
    assert_eq!(Some(Fixed::from(1)), outcome.resting);
    assert_eq!(None, outcome.cancel_reason);

    // CancelAggressor: the maker stays, the taker remainder is cancelled.
    let config = SymbolConfig::new(0, 2, 8)
        .unwrap()
        .with_stp(StpMode::CancelAggressor);
    let (mut book, same, other) = stp_book(config.clone());
    let mut taker = Order::new(OrderSide::Buy, 100.0, 2.0, 0);
    taker.owner_id = Some(1);
    let outcome = book.execute_order_with_symbol(&mut taker, config);
    assert!(outcome.stp_cancelled.is_empty());
    assert!(outcome.fills.is_empty());
    assert_eq!(Some(Fixed::from(2)), outcome.cancelled);
    assert_eq!(
        Some(RejectReason::SelfTradePrevented),
        outcome.cancel_reason
    );
    assert!(book.bids.is_empty());
    assert_eq!(2, book.asks.len());
    assert_eq!(same.sequence, book.asks.values().next().unwrap().sequence);
    assert!(
        book.asks
            .values()
            .any(|order| order.sequence == other.sequence)
    );

    // CancelBoth: remove the maker and cancel the taker.
    let config = SymbolConfig::new(0, 2, 8)
        .unwrap()
        .with_stp(StpMode::CancelBoth);
    let (mut book, _, other) = stp_book(config.clone());
    let mut taker = Order::new(OrderSide::Buy, 100.0, 2.0, 0);
    taker.owner_id = Some(1);
    let outcome = book.execute_order_with_symbol(&mut taker, config);
    assert_eq!(1, outcome.stp_cancelled.len());
    assert_eq!(Some(Fixed::from(2)), outcome.cancelled);
    assert_eq!(
        Some(RejectReason::SelfTradePrevented),
        outcome.cancel_reason
    );
    assert_eq!(1, book.asks.len());
    assert_eq!(other.sequence, book.asks.values().next().unwrap().sequence);

    // Off: the owners are irrelevant and both makers fill.
    let config = SymbolConfig::new(0, 2, 8).unwrap();
    let (mut book, _, _) = stp_book(config.clone());
    let mut taker = Order::new(OrderSide::Buy, 100.0, 2.0, 0);
    taker.owner_id = Some(1);
    let outcome = book.execute_order_with_symbol(&mut taker, config);
    assert_eq!(2, outcome.fills.len());
    assert!(outcome.stp_cancelled.is_empty());
    assert_eq!(None, outcome.cancelled);
}

#[test]
fn self_trade_prevention_fok_preflight_uses_the_same_policy() {
    let config = SymbolConfig::new(0, 2, 8)
        .unwrap()
        .with_stp(StpMode::CancelResting);
    let (mut book, _, _) = stp_book(config.clone());
    let mut taker = Order::new(OrderSide::Buy, 100.0, 2.0, 0);
    taker.owner_id = Some(1);
    taker.tif = TimeInForce::Fok;
    assert_eq!(
        Some(RejectReason::FokUnfillable),
        book.execute_order_with_symbol(&mut taker, config).rejected
    );
    assert_eq!(2, book.asks.len(), "a rejected FOK touches nothing");

    let config = SymbolConfig::new(0, 2, 8)
        .unwrap()
        .with_stp(StpMode::CancelAggressor);
    let (mut book, _, _) = stp_book(config.clone());
    let mut taker = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
    taker.owner_id = Some(1);
    taker.tif = TimeInForce::Fok;
    assert_eq!(
        Some(RejectReason::FokUnfillable),
        book.execute_order_with_symbol(&mut taker, config).rejected
    );
}

#[test]
fn pegged_target_cannot_cross_another_pegged_order() {
    let mut book = OrderBook::new();
    let mut bid = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
    let mut ask = Order::new(OrderSide::Sell, 102.0, 1.0, 0);
    book.execute_order(&mut bid);
    book.execute_order(&mut ask);

    let mut buy_peg = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
    buy_peg.peg = Some(PegReference::Primary);
    assert_eq!(None, book.execute_order(&mut buy_peg).rejected);

    book.last_trade_price = Some(Fixed::from(100));
    let mut sell_peg = Order::new(OrderSide::Sell, 0.0, 1.0, 0);
    sell_peg.peg = Some(PegReference::LastTrade);
    sell_peg.peg_offset = Fixed::from(-1);
    assert_eq!(
        Some(RejectReason::PegWouldCross),
        book.execute_order(&mut sell_peg).rejected,
        "the full opposite BBO, including pegs, must be checked"
    );
}

#[test]
fn midpoint_and_last_trade_pegs_use_their_reference() {
    let mut book = OrderBook::new();
    let mut bid = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
    let mut ask = Order::new(OrderSide::Sell, 102.0, 1.0, 0);
    book.execute_order(&mut bid);
    book.execute_order(&mut ask);

    let mut midpoint = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
    midpoint.peg = Some(PegReference::Midpoint);
    let outcome = book.execute_order(&mut midpoint);
    assert_eq!(None, outcome.rejected);
    assert_eq!(Fixed::from(101), midpoint.price);

    book.last_trade_price = Some(Fixed::from(100));
    let mut last = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
    last.peg = Some(PegReference::LastTrade);
    let outcome = book.execute_order(&mut last);
    assert_eq!(None, outcome.rejected);
    assert_eq!(Fixed::from(100), last.price);
}

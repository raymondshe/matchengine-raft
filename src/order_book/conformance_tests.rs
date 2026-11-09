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

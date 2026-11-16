use super::*;
use crate::MatchEngineTypeConfig;
use openraft::LogId;
use openraft::alias::CommittedLeaderIdOf;
use openraft::vote::RaftLeaderId;

fn log_id(term: u64, index: u64) -> LogIdOf<MatchEngineTypeConfig> {
    LogId::new(
        CommittedLeaderIdOf::<MatchEngineTypeConfig>::new(term, 1),
        index,
    )
}

#[test]
fn symbol_config_controls_precision_and_rejects_other_symbols() {
    use crate::order_book::SymbolConfig;

    let mut inner = StateMachineStoreInner::<MatchEngineTypeConfig>::default();
    assert!(
        inner
            .configure_symbol(SymbolConfig {
                price_decimals: 19,
                ..SymbolConfig::default()
            })
            .is_err(),
        "more than 18 decimals is invalid"
    );
    inner
        .configure_symbol(SymbolConfig::new(0, 2, 2).unwrap())
        .unwrap();

    let mut too_precise = Order::new(
        crate::order_book::OrderSide::Buy,
        "100.255".parse::<Fixed>().unwrap(),
        "1.5".parse::<Fixed>().unwrap(),
        0,
    );
    let outcome = inner
        .orderbook
        .execute_order_with_symbol(&mut too_precise, inner.symbol_config());
    assert_eq!(Some(RejectReason::InvalidOrder), outcome.rejected);

    let mut valid = Order::new(
        crate::order_book::OrderSide::Buy,
        "100.25".parse::<Fixed>().unwrap(),
        "1.5".parse::<Fixed>().unwrap(),
        0,
    );
    let outcome = inner
        .orderbook
        .execute_order_with_symbol(&mut valid, inner.symbol_config());
    assert_eq!(None, outcome.rejected);
    assert_eq!(1, inner.orderbook.bids.len());

    // The configured symbol precision cannot change after trading starts.
    assert!(
        inner
            .configure_symbol(SymbolConfig::new(0, 3, 3).unwrap())
            .is_err()
    );

    // A process configured for symbol 0 rejects an order routed to symbol 1.
    let mut other = Order::new(
        crate::order_book::OrderSide::Buy,
        "100.25".parse::<Fixed>().unwrap(),
        "1.5".parse::<Fixed>().unwrap(),
        0,
    )
    .with_symbol(1);
    let outcome = inner
        .orderbook
        .execute_order_with_symbol(&mut other, inner.symbol_config());
    assert_eq!(Some(RejectReason::UnsupportedOrder), outcome.rejected);

    // A fresh process can be provisioned for a different symbol before trading.
    let mut other_process = StateMachineStoreInner::<MatchEngineTypeConfig>::default();
    other_process
        .configure_symbol(SymbolConfig::new(7, 2, 2).unwrap())
        .unwrap();
    let mut symbol_seven = Order::new(
        crate::order_book::OrderSide::Buy,
        "100.25".parse::<Fixed>().unwrap(),
        "1.5".parse::<Fixed>().unwrap(),
        0,
    )
    .with_symbol(7);
    let outcome = other_process
        .orderbook
        .execute_order_with_symbol(&mut symbol_seven, other_process.symbol_config());
    assert_eq!(None, outcome.rejected);
    assert_eq!(7, other_process.symbol_config().symbol_id);
}

#[tokio::test]
async fn symbol_configuration_resets_a_publishing_feed() {
    use crate::order_book::SymbolConfig;
    use std::sync::atomic::AtomicBool;

    let dir = tempdir();
    let store = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
    let leader = Arc::new(AtomicBool::new(true));
    let resetting = Arc::new(AtomicBool::new(false));
    let pending = Arc::new(std::sync::Mutex::new(None));
    let control = MarketDataControl::new(
        leader,
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        true,
        resetting,
        pending,
        Arc::new(tokio::sync::Notify::new()),
    );
    store.set_market_data_control(control.clone(), 5);

    let mut inner = store.inner().lock().await;
    store
        .configure_symbol_locked(&mut inner, SymbolConfig::new(7, 2, 2).unwrap())
        .unwrap();
    let snapshot = control.take_pending().expect("a symbol reset was queued");
    assert_eq!(7, snapshot.symbol_id);
}

#[tokio::test]
async fn market_data_snapshot_uses_configured_symbol() {
    use crate::order_book::SymbolConfig;

    let dir = tempdir();
    let store = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
    {
        let mut inner = store.inner().lock().await;
        inner
            .configure_symbol(SymbolConfig::new(7, 2, 2).unwrap())
            .unwrap();
    }
    let snapshot = store.capture_market_data_snapshot(5).await;
    assert_eq!(7, snapshot.symbol_id);
}

#[tokio::test]
async fn snapshot_survives_reopen() {
    let dir = tempdir();

    let store = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
    {
        let mut inner = store.inner().lock().await;
        inner.orderbook.place_order(&mut Order::new(
            crate::order_book::OrderSide::Buy,
            10.0,
            2.0,
            0,
        ));
        inner.last_applied_log = Some(log_id(1, 3));
        inner.last_membership = StoredMembership::new(Some(log_id(1, 1)), Default::default());
    }
    store.persist_snapshot().await.unwrap();

    let reopened = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
    let content = reopened.to_content().await;
    assert_eq!(1, content.orders.len());
    assert_eq!(1, content.sequence);
}

#[tokio::test]
async fn corrupt_snapshot_is_an_error() {
    let dir = tempdir();
    std::fs::write(dir.path().join(SNAPSHOT_FILE), b"not a snapshot").unwrap();

    let err = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap_err();
    assert_eq!(io::ErrorKind::InvalidData, err.kind());
}

#[tokio::test]
async fn a_leftover_snapshot_temp_file_is_ignored() {
    let dir = tempdir();
    // A crash between writing the temp file and renaming it leaves no visible
    // snapshot; startup must ignore the partial file instead of failing.
    std::fs::write(
        dir.path().join(format!("{SNAPSHOT_FILE}.tmp")),
        b"partial snapshot",
    )
    .unwrap();

    let store = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
    let content = store.to_content().await;
    assert!(content.orders.is_empty());
    assert_eq!(0, content.sequence);
}

#[tokio::test]
async fn a_partial_snapshot_write_does_not_replace_the_committed_one() {
    let dir = tempdir();
    let store = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
    {
        let mut inner = store.inner().lock().await;
        inner.orderbook.place_order(&mut Order::new(
            crate::order_book::OrderSide::Buy,
            10.0,
            2.0,
            0,
        ));
        inner.last_applied_log = Some(log_id(1, 3));
    }
    store.persist_snapshot().await.unwrap();

    // Simulate a killed process mid-write: the partial file is newer than the
    // committed snapshot but was never renamed into place.
    std::fs::write(
        dir.path().join(format!("{SNAPSHOT_FILE}.tmp")),
        b"partial snapshot",
    )
    .unwrap();

    let reopened = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
    let content = reopened.to_content().await;
    assert_eq!(1, content.orders.len());
    assert_eq!(1, content.sequence);
}

/// Applies real entries so the sink wiring is covered, not just event construction.
#[tokio::test]
async fn apply_publishes_rests_fills_and_cancels_to_the_sink() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use openraft::alias::EntryOf;
    use openraft::alias::EntryPayloadOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
            .unwrap();

    // A sell of 5 rests, then a buy of 8 crosses it: 5 fills and 3 rest as a bid.
    let entries = futures::stream::iter(vec![
        Ok((
            entry(
                1,
                EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                    OrderSide::Sell,
                    100.0,
                    5.0,
                    0,
                ))),
            ),
            None,
        )),
        Ok((
            entry(
                2,
                EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    100.0,
                    8.0,
                    0,
                ))),
            ),
            None,
        )),
    ]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    assert_eq!(3, events.len(), "expected rested, fill, rested: {events:?}");

    assert_eq!(MatchEventKind::Rested, events[0].kind);
    assert_eq!(0, events[0].order_sequence);
    assert_eq!(5.0, events[0].volume);
    assert_eq!(1, events[0].event_id.log_index);

    assert_eq!(MatchEventKind::Fill, events[1].kind);
    assert_eq!(1, events[1].order_sequence);
    assert_eq!(Some(0), events[1].counterparty_sequence);
    assert_eq!(100.0, events[1].price);
    assert_eq!(5.0, events[1].volume);
    assert_eq!(2, events[1].event_id.log_index);
    assert_eq!(0, events[1].event_id.ordinal);

    assert_eq!(MatchEventKind::Rested, events[2].kind);
    assert_eq!(1, events[2].order_sequence);
    assert_eq!(3.0, events[2].volume);
    assert_eq!(2, events[2].event_id.log_index);
    assert_eq!(1, events[2].event_id.ordinal);

    // Cancelling the 3 that rested reports the remaining volume from the book.
    let entries = futures::stream::iter(vec![Ok((
        entry(
            3,
            EntryPayload::Normal(MatchEngineRequest::Cancel {
                order: Order::new(OrderSide::Buy, 100.0, 3.0, 1),
            }),
        ),
        None,
    ))]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    assert_eq!(4, events.len());
    let cancelled = &events[3];
    assert_eq!(MatchEventKind::Cancelled, cancelled.kind);
    assert_eq!(1, cancelled.order_sequence);
    assert_eq!(3.0, cancelled.volume);
    assert_eq!(3, cancelled.event_id.log_index);

    // A cancel of something that is not in the book is an explicit rejection,
    // not a silent acknowledgement.
    let entries = futures::stream::iter(vec![Ok((
        entry(
            4,
            EntryPayload::Normal(MatchEngineRequest::Cancel {
                order: Order::new(OrderSide::Buy, 100.0, 3.0, 1),
            }),
        ),
        None,
    ))]);
    store.apply(entries).await.unwrap();
    let events = sink.events();
    assert_eq!(5, events.len());
    let rejected = &events[4];
    assert_eq!(MatchEventKind::Rejected, rejected.kind);
    assert_eq!(Some(RejectReason::OrderNotFound), rejected.reason);
    assert_eq!(1, rejected.order_sequence);
    assert_eq!(4, rejected.event_id.log_index);
}

/// A cancel or amend of a fully filled order is rejected, not silently
/// acknowledged, and each rejection still consumes one market sequence.
#[tokio::test]
async fn apply_rejects_cancel_and_amend_of_a_fully_filled_order() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use crate::order_book::RejectReason;
    use openraft::alias::EntryOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
            .unwrap();

    let bid = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
    let ask = Order::new(OrderSide::Sell, 100.0, 1.0, 0);
    let entries = futures::stream::iter(vec![
        Ok((
            entry(1, EntryPayload::Normal(MatchEngineRequest::place(bid))),
            None,
        )),
        Ok((
            entry(2, EntryPayload::Normal(MatchEngineRequest::place(ask))),
            None,
        )),
    ]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    assert_eq!(2, events.len(), "rest + fill: {events:?}");
    assert_eq!(MatchEventKind::Rested, events[0].kind);
    assert_eq!(MatchEventKind::Fill, events[1].kind);

    let entries = futures::stream::iter(vec![
        Ok((
            entry(
                3,
                EntryPayload::Normal(MatchEngineRequest::Cancel { order: bid }),
            ),
            None,
        )),
        Ok((
            entry(
                4,
                EntryPayload::Normal(MatchEngineRequest::Amend {
                    order: bid,
                    price: Some(99.0.into()),
                    volume: None,
                }),
            ),
            None,
        )),
    ]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    assert_eq!(4, events.len(), "unexpected events: {events:?}");
    for event in &events[2..] {
        assert_eq!(MatchEventKind::Rejected, event.kind);
        assert_eq!(Some(RejectReason::OrderNotFound), event.reason);
        assert_eq!(0, event.order_sequence);
    }

    let inner = store.inner().lock().await;
    assert_eq!(
        4, inner.orderbook.sequence,
        "both rejected requests consumed one sequence"
    );
}

/// A batch applies every sub-request in one entry, with continuous event
/// ordinals and one response per item.
#[tokio::test]
async fn apply_batch_preserves_sequences_and_event_ordinals() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use openraft::alias::EntryOf;
    use openraft::alias::EntryPayloadOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
            .unwrap();
    let batch = MatchEngineRequest::Batch {
        requests: vec![
            MatchEngineRequest::place(Order::new(OrderSide::Buy, 100.0, 1.0, 0)),
            MatchEngineRequest::place(Order::new(OrderSide::Buy, 99.0, 2.0, 0)),
        ],
    };
    let entries = futures::stream::iter(vec![Ok((entry(1, EntryPayload::Normal(batch)), None))]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    assert_eq!(2, events.len(), "one event per resting order: {events:?}");
    assert_eq!(MatchEventKind::Rested, events[0].kind);
    assert_eq!(0, events[0].event_id.ordinal);
    assert_eq!(MatchEventKind::Rested, events[1].kind);
    assert_eq!(1, events[1].event_id.ordinal);
    assert_eq!(events[0].event_id.log_index, events[1].event_id.log_index);

    let content = store.to_content().await;
    assert_eq!(2, content.orders.len());
    assert_eq!(2, content.sequence);
    assert_eq!(2, content.events.len());
}

/// Event ordinals continue across mixed sub-requests in one batch entry.
#[tokio::test]
async fn batch_ordinals_continue_across_mixed_requests() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use openraft::alias::EntryOf;
    use openraft::alias::EntryPayloadOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
            .unwrap();
    let batch = MatchEngineRequest::Batch {
        requests: vec![
            MatchEngineRequest::place(Order::new(OrderSide::Buy, 100.0, 1.0, 0)),
            MatchEngineRequest::Cancel {
                order: Order::new(OrderSide::Buy, 100.0, 1.0, 0),
            },
            MatchEngineRequest::Amend {
                order: Order::new(OrderSide::Buy, 100.0, 1.0, 99),
                price: Some(101.0.into()),
                volume: None,
            },
        ],
    };
    let entries = futures::stream::iter(vec![Ok((entry(1, EntryPayload::Normal(batch)), None))]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    assert_eq!(
        3,
        events.len(),
        "rest, cancel and rejected amend: {events:?}"
    );
    assert_eq!(MatchEventKind::Rested, events[0].kind);
    assert_eq!(0, events[0].event_id.ordinal);
    assert_eq!(MatchEventKind::Cancelled, events[1].kind);
    assert_eq!(1, events[1].event_id.ordinal);
    assert_eq!(MatchEventKind::Rejected, events[2].kind);
    assert_eq!(2, events[2].event_id.ordinal);
    assert!(
        events.iter().all(|event| event.event_id.log_index == 1),
        "one entry, one log index"
    );

    let content = store.to_content().await;
    assert_eq!(3, content.sequence, "place + cancel + amend market seqs");
    assert!(content.orders.is_empty(), "the cancel removed the order");
    assert_eq!(3, content.events.len());
}

/// A retried batch item recovers only its own events, even though every
/// sub-request in the batch shares one log index.
#[tokio::test]
async fn batch_duplicate_returns_only_its_own_events() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use openraft::alias::EntryOf;
    use openraft::alias::EntryPayloadOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
            .unwrap();
    let batch = MatchEngineRequest::Batch {
        requests: vec![
            MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Buy, 100.0, 1.0, 0),
                "first",
            ),
            MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Buy, 99.0, 1.0, 0),
                "second",
            ),
        ],
    };
    let entries = futures::stream::iter(vec![Ok((entry(1, EntryPayload::Normal(batch)), None))]);
    store.apply(entries).await.unwrap();
    assert_eq!(2, sink.events().len());
    assert_eq!(MatchEventKind::Rested, sink.events()[1].kind);

    let inner = store.inner().lock().await;
    let second = inner
        .submissions
        .get("second")
        .expect("second submission recorded")
        .clone();
    let duplicate = inner.duplicate_response(&second);
    assert_eq!(Some(1), duplicate.sequence);
    assert_eq!(1, duplicate.events.len(), "only the second place's event");
    assert_eq!(1, duplicate.events[0].event_id.ordinal);
    assert_eq!(1, duplicate.events[0].order_sequence);
    assert!(!duplicate.events_truncated);

    let first = inner
        .submissions
        .get("first")
        .expect("first submission recorded")
        .clone();
    let duplicate = inner.duplicate_response(&first);
    assert_eq!(Some(0), duplicate.sequence);
    assert_eq!(1, duplicate.events.len(), "only the first place's event");
    assert_eq!(0, duplicate.events[0].event_id.ordinal);
}

/// A retry whose events were evicted still returns the original sequence,
/// with an empty, explicitly truncated event list.
#[tokio::test]
async fn duplicate_response_reports_evicted_events() {
    use crate::match_result::MemorySink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use openraft::alias::EntryOf;
    use openraft::alias::EntryPayloadOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_limits(dir.path(), sink, None, 1, 5)
            .await
            .unwrap();
    let entries = futures::stream::iter(vec![
        Ok((
            entry(
                1,
                EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
                    Order::new(OrderSide::Buy, 100.0, 1.0, 0),
                    "a",
                )),
            ),
            None,
        )),
        Ok((
            entry(
                2,
                EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
                    Order::new(OrderSide::Buy, 99.0, 1.0, 0),
                    "b",
                )),
            ),
            None,
        )),
        Ok((
            entry(
                3,
                EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
                    Order::new(OrderSide::Buy, 98.0, 1.0, 0),
                    "c",
                )),
            ),
            None,
        )),
    ]);
    store.apply(entries).await.unwrap();

    let inner = store.inner().lock().await;
    let record = inner
        .submissions
        .get("a")
        .expect("the submission is still retained")
        .clone();
    let duplicate = inner.duplicate_response(&record);
    assert_eq!(Some(0), duplicate.sequence);
    assert!(duplicate.events.is_empty(), "the event window dropped it");
    assert!(duplicate.events_truncated);
}

/// A failing sink must not fail `apply` or change the resulting state.
#[tokio::test]
async fn sink_failure_does_not_fail_apply() {
    use crate::match_result::MatchResultSink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use openraft::alias::EntryOf;

    #[derive(Debug)]
    struct FailingSink;

    impl MatchResultSink for FailingSink {
        fn name(&self) -> &'static str {
            "failing"
        }

        fn publish(&self, _events: &[crate::match_result::MatchEvent]) -> io::Result<()> {
            Err(io::Error::other("sink is down"))
        }
    }

    let dir = tempdir();
    let mut store = StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(
        dir.path(),
        Arc::new(FailingSink),
    )
    .unwrap();

    let entry = EntryOf::<MatchEngineTypeConfig> {
        log_id: log_id(1, 1),
        payload: EntryPayload::Normal(MatchEngineRequest::place(Order::new(
            OrderSide::Buy,
            100.0,
            1.0,
            0,
        ))),
    };
    let entries = futures::stream::iter(vec![Ok((entry, None))]);

    store.apply(entries).await.unwrap();

    let inner = store.inner().lock().await;
    assert_eq!(1.0, inner.orderbook.best_bid().unwrap().volume);
    assert_eq!(Some(log_id(1, 1)), inner.last_applied_log);
}

/// The time-in-force and stop paths publish their own event kinds, and one
/// entry's events (a stop cascade) keep increasing ordinals.
#[tokio::test]
async fn apply_publishes_ioc_stop_and_expiry_events() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use crate::order_book::OrderType;
    use crate::order_book::TimeInForce;
    use openraft::alias::EntryOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
            .unwrap();

    let mut gtd = Order::new(OrderSide::Buy, 90.0, 1.0, 0);
    gtd.tif = TimeInForce::Gtd(2_000);
    gtd.timestamp = 1_000;

    let mut stop = Order::new(OrderSide::Sell, 0.0, 5.0, 0);
    stop.live = OrderType::Market;
    stop.stop_price = 95.0.into();

    let entries = futures::stream::iter(vec![
        Ok((
            entry(1, EntryPayload::Normal(MatchEngineRequest::place(gtd))),
            None,
        )),
        Ok((
            entry(
                2,
                EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    100.0,
                    10.0,
                    0,
                ))),
            ),
            None,
        )),
        Ok((
            entry(
                3,
                EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                    OrderSide::Buy,
                    95.0,
                    10.0,
                    0,
                ))),
            ),
            None,
        )),
        Ok((
            entry(4, EntryPayload::Normal(MatchEngineRequest::place(stop))),
            None,
        )),
        Ok((
            entry(
                5,
                EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                    OrderSide::Sell,
                    100.0,
                    10.0,
                    0,
                ))),
            ),
            None,
        )),
        Ok((
            entry(
                6,
                EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                    OrderSide::Sell,
                    95.0,
                    3.0,
                    0,
                ))),
            ),
            None,
        )),
        Ok((
            entry(
                7,
                EntryPayload::Normal(MatchEngineRequest::PurgeExpired {
                    now_ms: 2_000,
                    day_close_ms: None,
                }),
            ),
            None,
        )),
    ]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    // Rests for the GTD order and the two bids; a fill for entry 5; then the
    // elected stop's trigger + child fill; finally the expiry.
    assert_eq!(8, events.len(), "unexpected events: {events:?}");

    assert_eq!(MatchEventKind::Rested, events[0].kind);
    assert_eq!(MatchEventKind::Fill, events[3].kind);
    assert_eq!(10.0, events[3].volume);

    let triggered = &events[5];
    assert_eq!(MatchEventKind::Triggered, triggered.kind);
    assert_eq!(3, triggered.order_sequence, "the stop's sequence");
    assert_eq!(95.0, triggered.price, "the trade that elected it");
    assert_eq!(5.0, triggered.volume);
    assert_eq!(
        [6, 1],
        [
            triggered.event_id.log_index,
            u64::from(triggered.event_id.ordinal)
        ]
    );

    let child_fill = &events[6];
    assert_eq!(MatchEventKind::Fill, child_fill.kind);
    assert_eq!(
        3, child_fill.order_sequence,
        "child fills belong to the stop"
    );
    assert_eq!(5.0, child_fill.volume);
    assert_eq!(2, child_fill.event_id.ordinal, "same entry, next ordinal");

    let expired = &events[7];
    assert_eq!(MatchEventKind::Expired, expired.kind);
    assert_eq!(0, expired.order_sequence, "the GTD order");
    assert_eq!(1.0, expired.volume);
    assert_eq!(7, expired.event_id.log_index);
}

/// Market, IOC, FOK, post-only and iceberg paths all emit the same
/// match-result fields as plain limit orders.
#[tokio::test]
async fn apply_publishes_market_ioc_fok_post_only_and_iceberg_events() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::FeeAsset;
    use crate::order_book::Fixed;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use crate::order_book::OrderType;
    use crate::order_book::RejectReason;
    use crate::order_book::TimeInForce;
    use openraft::alias::EntryOf;
    use openraft::alias::EntryPayloadOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    fn rate(value: &str) -> Fixed {
        value.parse().expect("valid fixed-point rate")
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
            .unwrap();

    let maker =
        Order::new(OrderSide::Sell, 100.0, 5.0, 0).with_fee_rates(rate("0.001"), rate("0.002"));
    let mut market =
        Order::new(OrderSide::Buy, 0.0, 8.0, 0).with_fee_rates(rate("0.003"), rate("0.004"));
    market.live = OrderType::Market;
    let mut ioc = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
    ioc.tif = TimeInForce::Ioc;
    let mut fok = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
    fok.tif = TimeInForce::Fok;
    let ask = Order::new(OrderSide::Sell, 100.0, 5.0, 0);
    let mut post_cross = Order::new(OrderSide::Buy, 100.0, 4.0, 0);
    post_cross.post_only = true;
    let mut post_rest = Order::new(OrderSide::Buy, 99.0, 4.0, 0);
    post_rest.post_only = true;
    let mut iceberg =
        Order::new(OrderSide::Sell, 101.0, 10.0, 0).with_fee_rates(rate("0.001"), rate("0.002"));
    iceberg.display_volume = rate("4");
    let mut sweep =
        Order::new(OrderSide::Buy, 0.0, 6.0, 0).with_fee_rates(rate("0.003"), rate("0.004"));
    sweep.live = OrderType::Market;

    let entries = futures::stream::iter(vec![
        Ok((
            entry(1, EntryPayload::Normal(MatchEngineRequest::place(maker))),
            None,
        )),
        Ok((
            entry(2, EntryPayload::Normal(MatchEngineRequest::place(market))),
            None,
        )),
        Ok((
            entry(3, EntryPayload::Normal(MatchEngineRequest::place(ioc))),
            None,
        )),
        Ok((
            entry(4, EntryPayload::Normal(MatchEngineRequest::place(fok))),
            None,
        )),
        Ok((
            entry(5, EntryPayload::Normal(MatchEngineRequest::place(ask))),
            None,
        )),
        Ok((
            entry(
                6,
                EntryPayload::Normal(MatchEngineRequest::place(post_cross)),
            ),
            None,
        )),
        Ok((
            entry(
                7,
                EntryPayload::Normal(MatchEngineRequest::Cancel {
                    order: Order::new(OrderSide::Sell, 100.0, 5.0, 4),
                }),
            ),
            None,
        )),
        Ok((
            entry(
                8,
                EntryPayload::Normal(MatchEngineRequest::place(post_rest)),
            ),
            None,
        )),
        Ok((
            entry(9, EntryPayload::Normal(MatchEngineRequest::place(iceberg))),
            None,
        )),
        Ok((
            entry(10, EntryPayload::Normal(MatchEngineRequest::place(sweep))),
            None,
        )),
    ]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    assert_eq!(12, events.len(), "unexpected events: {events:?}");

    assert_eq!(MatchEventKind::Rested, events[0].kind);
    assert_eq!(0, events[0].order_sequence);

    let market_fill = &events[1];
    assert_eq!(MatchEventKind::Fill, market_fill.kind);
    assert_eq!(1, market_fill.order_sequence);
    assert_eq!(Some(0), market_fill.counterparty_sequence);
    assert_eq!(Fixed::from(5), market_fill.volume);
    assert_eq!(Fixed::from(500), market_fill.quote_volume);
    assert_eq!(Fixed::from(0.02), market_fill.taker_fee);
    assert_eq!(Some(FeeAsset::Base), market_fill.taker_fee_asset);
    assert_eq!(Fixed::from(0.5), market_fill.maker_fee);
    assert_eq!(Some(FeeAsset::Quote), market_fill.maker_fee_asset);

    assert_eq!(MatchEventKind::Cancelled, events[2].kind);
    assert_eq!(1, events[2].order_sequence);
    assert_eq!(Fixed::from(3), events[2].volume);

    assert_eq!(MatchEventKind::Cancelled, events[3].kind);
    assert_eq!(2, events[3].order_sequence);
    assert_eq!(Fixed::from(5), events[3].volume);

    assert_eq!(MatchEventKind::Rejected, events[4].kind);
    assert_eq!(3, events[4].order_sequence);
    assert_eq!(Some(RejectReason::FokUnfillable), events[4].reason);

    assert_eq!(MatchEventKind::Rested, events[5].kind);
    assert_eq!(4, events[5].order_sequence);

    assert_eq!(MatchEventKind::Rejected, events[6].kind);
    assert_eq!(5, events[6].order_sequence);
    assert_eq!(Some(RejectReason::PostOnlyWouldCross), events[6].reason);

    assert_eq!(MatchEventKind::Cancelled, events[7].kind);
    assert_eq!(4, events[7].order_sequence);
    assert_eq!(Fixed::from(5), events[7].volume);

    assert_eq!(MatchEventKind::Rested, events[8].kind);
    assert_eq!(7, events[8].order_sequence);
    assert_eq!(Fixed::from(4), events[8].volume);

    assert_eq!(MatchEventKind::Rested, events[9].kind);
    assert_eq!(8, events[9].order_sequence);
    assert_eq!(Fixed::from(4), events[9].volume, "only the visible tranche");

    let first_iceberg_fill = &events[10];
    assert_eq!(MatchEventKind::Fill, first_iceberg_fill.kind);
    assert_eq!(9, first_iceberg_fill.order_sequence);
    assert_eq!(Some(8), first_iceberg_fill.counterparty_sequence);
    assert_eq!(Fixed::from(4), first_iceberg_fill.volume);
    assert_eq!(Fixed::from(404), first_iceberg_fill.quote_volume);
    assert_eq!(Fixed::from(0.016), first_iceberg_fill.taker_fee);
    assert_eq!(Fixed::from(0.404), first_iceberg_fill.maker_fee);
    assert_eq!(0, first_iceberg_fill.event_id.ordinal);

    let second_iceberg_fill = &events[11];
    assert_eq!(MatchEventKind::Fill, second_iceberg_fill.kind);
    assert_eq!(Some(8), second_iceberg_fill.counterparty_sequence);
    assert_eq!(Fixed::from(2), second_iceberg_fill.volume);
    assert_eq!(Fixed::from(202), second_iceberg_fill.quote_volume);
    assert_eq!(Fixed::from(0.008), second_iceberg_fill.taker_fee);
    assert_eq!(Fixed::from(0.202), second_iceberg_fill.maker_fee);
    assert_eq!(1, second_iceberg_fill.event_id.ordinal);
}

/// A stop child that cannot fully execute emits the trigger, its fills and
/// the cancelled remainder in one entry, all tied to the stop's identity.
#[tokio::test]
async fn apply_publishes_stop_trigger_with_cancelled_child() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::FeeAsset;
    use crate::order_book::Fixed;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use crate::order_book::OrderType;
    use openraft::alias::EntryOf;
    use openraft::alias::EntryPayloadOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    fn rate(value: &str) -> Fixed {
        value.parse().expect("valid fixed-point rate")
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
            .unwrap();

    let bid = Order::new(OrderSide::Buy, 95.0, 3.0, 0).with_fee_rates(rate("0.001"), rate("0.002"));
    let mut stop =
        Order::new(OrderSide::Sell, 0.0, 5.0, 0).with_fee_rates(rate("0.003"), rate("0.004"));
    stop.live = OrderType::Market;
    stop.stop_price = rate("95");
    let trigger =
        Order::new(OrderSide::Sell, 95.0, 1.0, 0).with_fee_rates(rate("0.003"), rate("0.004"));

    let entries = futures::stream::iter(vec![
        Ok((
            entry(1, EntryPayload::Normal(MatchEngineRequest::place(bid))),
            None,
        )),
        Ok((
            entry(2, EntryPayload::Normal(MatchEngineRequest::place(stop))),
            None,
        )),
        Ok((
            entry(3, EntryPayload::Normal(MatchEngineRequest::place(trigger))),
            None,
        )),
    ]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    assert_eq!(5, events.len(), "unexpected events: {events:?}");
    assert_eq!(MatchEventKind::Rested, events[0].kind);
    assert_eq!(MatchEventKind::Fill, events[1].kind);
    assert_eq!(Fixed::from(1), events[1].volume);
    assert_eq!(Fixed::from(0.38), events[1].taker_fee);
    assert_eq!(Some(FeeAsset::Quote), events[1].taker_fee_asset);
    assert_eq!(Fixed::from(0.001), events[1].maker_fee);
    assert_eq!(Some(FeeAsset::Base), events[1].maker_fee_asset);

    assert_eq!(MatchEventKind::Triggered, events[2].kind);
    assert_eq!(1, events[2].order_sequence);
    assert_eq!(Fixed::from(95), events[2].price);
    assert_eq!(Fixed::from(5), events[2].volume);

    assert_eq!(MatchEventKind::Fill, events[3].kind);
    assert_eq!(1, events[3].order_sequence);
    assert_eq!(Fixed::from(2), events[3].volume);
    assert_eq!(Fixed::from(190), events[3].quote_volume);
    assert_eq!(Fixed::from(0.76), events[3].taker_fee);
    assert_eq!(Some(FeeAsset::Quote), events[3].taker_fee_asset);
    assert_eq!(Fixed::from(0.002), events[3].maker_fee);
    assert_eq!(Some(FeeAsset::Base), events[3].maker_fee_asset);

    assert_eq!(MatchEventKind::Cancelled, events[4].kind);
    assert_eq!(1, events[4].order_sequence);
    assert_eq!(Fixed::from(3), events[4].volume);
    assert_eq!(3, events[4].event_id.ordinal);
}

/// A later request's intake timestamp purges an expired GTD order before
/// matching; the expiry is published before the new order's events.
#[tokio::test]
async fn apply_publishes_automatic_gtd_expiry_before_matching() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::Fixed;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use crate::order_book::TimeInForce;
    use openraft::alias::EntryOf;
    use openraft::alias::EntryPayloadOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
            .unwrap();

    let mut gtd = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
    gtd.tif = TimeInForce::Gtd(2_000);
    gtd.timestamp = 1_000;
    let mut taker = Order::new(OrderSide::Sell, 100.0, 1.0, 0);
    taker.timestamp = 2_000;

    let entries = futures::stream::iter(vec![
        Ok((
            entry(1, EntryPayload::Normal(MatchEngineRequest::place(gtd))),
            None,
        )),
        Ok((
            entry(2, EntryPayload::Normal(MatchEngineRequest::place(taker))),
            None,
        )),
    ]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    assert_eq!(3, events.len(), "unexpected events: {events:?}");
    assert_eq!(MatchEventKind::Rested, events[0].kind);
    assert_eq!(MatchEventKind::Expired, events[1].kind);
    assert_eq!(0, events[1].order_sequence);
    assert_eq!(Fixed::from(5), events[1].volume);
    assert_eq!(MatchEventKind::Rested, events[2].kind);
    assert_eq!(1, events[2].order_sequence);
    assert_eq!(Fixed::from(1), events[2].volume);
}

/// Explicit purges are idempotent, and an automatic purge before a place keeps
/// the place's sequence and the entry's event ordinals.
#[tokio::test]
async fn gtd_purge_is_idempotent_and_auto_expiry_keeps_place_ordinals() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::Fixed;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use crate::order_book::TimeInForce;
    use openraft::alias::EntryOf;
    use openraft::alias::EntryPayloadOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
            .unwrap();

    let mut first_gtd = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
    first_gtd.tif = TimeInForce::Gtd(2_000);
    first_gtd.timestamp = 1_000;

    let mut second_gtd = Order::new(OrderSide::Buy, 99.0, 2.0, 0);
    second_gtd.tif = TimeInForce::Gtd(3_000);
    second_gtd.timestamp = 1_000;

    let mut taker = Order::new(OrderSide::Sell, 99.0, 1.0, 0);
    taker.timestamp = 3_000;

    let entries = futures::stream::iter(vec![
        Ok((
            entry(
                1,
                EntryPayload::Normal(MatchEngineRequest::place(first_gtd)),
            ),
            None,
        )),
        Ok((
            entry(
                2,
                EntryPayload::Normal(MatchEngineRequest::PurgeExpired {
                    now_ms: 2_000,
                    day_close_ms: None,
                }),
            ),
            None,
        )),
        // The same purge at the same instant is a no-op: no duplicate event.
        Ok((
            entry(
                3,
                EntryPayload::Normal(MatchEngineRequest::PurgeExpired {
                    now_ms: 2_000,
                    day_close_ms: None,
                }),
            ),
            None,
        )),
        Ok((
            entry(
                4,
                EntryPayload::Normal(MatchEngineRequest::place(second_gtd)),
            ),
            None,
        )),
        // The taker's intake timestamp purges the second GTD before matching,
        // then the taker itself is applied in the same entry.
        Ok((
            entry(5, EntryPayload::Normal(MatchEngineRequest::place(taker))),
            None,
        )),
        // A later explicit purge finds nothing.
        Ok((
            entry(
                6,
                EntryPayload::Normal(MatchEngineRequest::PurgeExpired {
                    now_ms: 3_000,
                    day_close_ms: None,
                }),
            ),
            None,
        )),
    ]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    assert_eq!(5, events.len(), "unexpected events: {events:?}");
    assert_eq!(
        vec![
            MatchEventKind::Rested,
            MatchEventKind::Expired,
            MatchEventKind::Rested,
            MatchEventKind::Expired,
            MatchEventKind::Rested,
        ],
        events.iter().map(|event| event.kind).collect::<Vec<_>>()
    );
    assert_eq!(
        vec![0, 0, 3, 3, 4],
        events
            .iter()
            .map(|event| event.order_sequence)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        vec![1, 2, 4, 5, 5],
        events
            .iter()
            .map(|event| event.event_id.log_index)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        vec![0, 0, 0, 0, 1],
        events
            .iter()
            .map(|event| event.event_id.ordinal)
            .collect::<Vec<_>>(),
        "automatic expiry and the place share one entry and keep ordinal order"
    );
    assert_eq!(Fixed::from(5), events[1].volume);
    assert_eq!(Fixed::from(2), events[3].volume);
    assert_eq!(Fixed::from(1), events[4].volume);
}

#[tokio::test]
async fn apply_amends_parked_stop_and_iceberg() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::Fixed;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use openraft::alias::EntryOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
            .unwrap();

    let mut stop = Order::new(OrderSide::Sell, 94.0, 5.0, 0);
    stop.stop_price = Fixed::from(95);
    let mut stop_key = stop;
    stop_key.stop_price = Fixed::from(96);

    let mut iceberg = Order::new(OrderSide::Sell, 100.0, 10.0, 0);
    iceberg.display_volume = Fixed::from(4);
    let mut iceberg_key = iceberg;
    iceberg_key.sequence = 2; // assigned by the place applied above

    let entries = futures::stream::iter(vec![
        Ok((
            entry(1, EntryPayload::Normal(MatchEngineRequest::place(stop))),
            None,
        )),
        Ok((
            entry(
                2,
                EntryPayload::Normal(MatchEngineRequest::Amend {
                    order: stop_key,
                    price: Some(93.0.into()),
                    volume: Some(7.0.into()),
                }),
            ),
            None,
        )),
        Ok((
            entry(3, EntryPayload::Normal(MatchEngineRequest::place(iceberg))),
            None,
        )),
        Ok((
            entry(
                4,
                EntryPayload::Normal(MatchEngineRequest::Amend {
                    order: iceberg_key,
                    price: None,
                    volume: Some(12.0.into()),
                }),
            ),
            None,
        )),
    ]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    assert_eq!(3, events.len(), "unexpected events: {events:?}");
    assert_eq!(MatchEventKind::Amended, events[0].kind);
    assert_eq!(0, events[0].order_sequence);
    assert_eq!(Some(0), events[0].previous_sequence);
    assert_eq!(Fixed::from(93), events[0].price);
    assert_eq!(Fixed::from(7), events[0].volume);

    assert_eq!(MatchEventKind::Rested, events[1].kind);
    assert_eq!(Fixed::from(4), events[1].volume);
    assert_eq!(MatchEventKind::Amended, events[2].kind);
    assert_eq!(3, events[2].order_sequence, "unexpected events: {events:?}");
    assert_eq!(Some(2), events[2].previous_sequence);
    assert_eq!(
        Fixed::from(12),
        events[2].volume,
        "the event reports the iceberg's new open volume"
    );

    let inner = store.inner().lock().await;
    assert_eq!(1, inner.orderbook.stops.len());
    assert_eq!(Fixed::from(96), inner.orderbook.stops[0].stop_price);
    assert_eq!(Fixed::from(93), inner.orderbook.stops[0].price);
    assert_eq!(Fixed::from(7), inner.orderbook.stops[0].volume);
    let best = inner.orderbook.best_ask().expect("the iceberg rests");
    assert_eq!(Fixed::from(4), best.volume);
    assert_eq!(Fixed::from(8), best.hidden_volume);
}

/// Amendment goes through `apply`: the event carries both identities and the
/// book keeps the new size; a failed amend publishes a rejection.
#[tokio::test]
async fn apply_publishes_amended_and_rejected_events() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use crate::order_book::RejectReason;
    use openraft::alias::EntryOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
            .unwrap();

    // The key of an order uses side + price + sequence; `Order::new` sets the
    // sequence, and the taker flow assigns the same one on apply.
    let bid = Order::new(OrderSide::Buy, 100.0, 10.0, 0);
    let entries = futures::stream::iter(vec![
        Ok((
            entry(1, EntryPayload::Normal(MatchEngineRequest::place(bid))),
            None,
        )),
        Ok((
            entry(
                2,
                EntryPayload::Normal(MatchEngineRequest::Amend {
                    order: bid,
                    price: None,
                    volume: Some(12.0.into()),
                }),
            ),
            None,
        )),
        Ok((
            entry(
                3,
                EntryPayload::Normal(MatchEngineRequest::Amend {
                    order: Order::new(OrderSide::Buy, 100.0, 1.0, 42),
                    price: Some(99.0.into()),
                    volume: None,
                }),
            ),
            None,
        )),
    ]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    assert_eq!(3, events.len(), "unexpected events: {events:?}");

    assert_eq!(MatchEventKind::Rested, events[0].kind);

    let amended = &events[1];
    assert_eq!(MatchEventKind::Amended, amended.kind);
    assert_eq!(
        1, amended.order_sequence,
        "the amendment takes a new sequence"
    );
    assert_eq!(Some(0), amended.previous_sequence);
    assert_eq!(100.0, amended.price);
    assert_eq!(12.0, amended.volume);

    let rejected = &events[2];
    assert_eq!(MatchEventKind::Rejected, rejected.kind);
    assert_eq!(Some(RejectReason::OrderNotFound), rejected.reason);

    let inner = store.inner().lock().await;
    let best = inner.orderbook.best_bid().expect("the bid remains");
    assert_eq!(12.0, best.volume);
    assert_eq!(1, best.sequence);
    assert_eq!(
        3, inner.orderbook.sequence,
        "the failed amend consumed a sequence"
    );
}

/// `apply` hands events and the depth journal to the market-data tap with
/// the request's intake timestamp; the snapshot capture reads the book.
#[tokio::test]
async fn apply_publishes_market_data_frames_and_captures_snapshots() {
    use crate::market_data::Ingest;
    use crate::market_data::MarketDataTap;
    use crate::match_result::MatchEventKind;
    use crate::match_result::NoopSink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use openraft::alias::EntryOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    async fn next_frame(
        receiver: &mut tokio::sync::mpsc::Receiver<Ingest>,
    ) -> crate::market_data::TapFrame {
        match receiver.recv().await {
            Some(Ingest::Frame(frame)) => frame,
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    let dir = tempdir();
    let (tap, mut channels) = MarketDataTap::channel(16);
    let mut store = StateMachineStore::<MatchEngineTypeConfig>::open_with_sinks(
        dir.path(),
        Arc::new(NoopSink),
        Some(tap),
    )
    .unwrap();

    let mut bid = Order::new(OrderSide::Buy, 100.0, 10.0, 0);
    bid.timestamp = 1_234;
    let entries = futures::stream::iter(vec![Ok((
        entry(1, EntryPayload::Normal(MatchEngineRequest::place(bid))),
        None,
    ))]);
    store.apply(entries).await.unwrap();

    let frame = next_frame(&mut channels.receiver).await;
    assert_eq!(1, frame.log_index);
    assert_eq!(0, frame.market_seq, "the bid's order sequence");
    assert_eq!(1_234, frame.ts_ms);
    assert_eq!(0, frame.symbol_id);
    assert_eq!(1, frame.events.len());
    assert_eq!(MatchEventKind::Rested, frame.events[0].kind);
    assert_eq!(1, frame.depth.len());
    assert_eq!(10.0, frame.depth[0].delta_volume);
    assert_eq!(1, frame.depth[0].delta_orders);

    // A fill carries both its event and the depth change.
    let mut ask = Order::new(OrderSide::Sell, 100.0, 4.0, 0);
    ask.timestamp = 2_000;
    let entries = futures::stream::iter(vec![Ok((
        entry(2, EntryPayload::Normal(MatchEngineRequest::place(ask))),
        None,
    ))]);
    store.apply(entries).await.unwrap();

    let frame = next_frame(&mut channels.receiver).await;
    assert_eq!(2, frame.log_index);
    assert_eq!(1, frame.market_seq, "the ask's order sequence");
    assert_eq!(2_000, frame.ts_ms);
    assert!(
        frame
            .events
            .iter()
            .any(|event| event.kind == MatchEventKind::Fill)
    );
    assert!(frame.depth.iter().any(|change| change.delta_volume < 0.0));
    assert_eq!(
        2,
        channels
            .stats
            .frames
            .load(std::sync::atomic::Ordering::Relaxed)
    );

    let snapshot = store.capture_market_data_snapshot(5).await;
    assert_eq!(2, snapshot.log_index);
    assert_eq!(2, snapshot.market_seq, "the next sequence is the boundary");
    assert_eq!(1, snapshot.bids.len(), "the bid remainder is captured");
    assert_eq!(6.0, snapshot.bids[0].volume);
    assert_eq!(Some(100.0.into()), snapshot.last_trade);

    let status = store
        .inner()
        .lock()
        .await
        .market_data
        .as_ref()
        .expect("tap installed")
        .status_json();
    let status: serde_json::Value = serde_json::from_str(&status).expect("status json");
    assert_eq!(2, status["frames"]);
    assert_eq!(0, status["dropped_frames"]);
}

/// A takeover snapshots and enqueues first, and only then opens the tap
/// gate; a follower's frames never queue ahead of the reset.
#[tokio::test]
async fn market_data_feed_opens_only_after_the_snapshot_is_queued() {
    use crate::market_data::Ingest;
    use crate::market_data::MarketDataControl;
    use crate::market_data::MarketDataTap;
    use crate::match_result::NoopSink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use openraft::alias::EntryOf;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let (tap, mut channels) = MarketDataTap::channel(16);
    // A follower: the tap gate is closed, so this entry never queues a frame.
    channels.enabled.store(false, Ordering::Relaxed);
    let mut store = StateMachineStore::<MatchEngineTypeConfig>::open_with_sinks(
        dir.path(),
        Arc::new(NoopSink),
        Some(tap.clone()),
    )
    .unwrap();

    let mut bid = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
    bid.timestamp = 1_000;
    let entries = futures::stream::iter(vec![Ok((
        entry(1, EntryPayload::Normal(MatchEngineRequest::place(bid))),
        None,
    ))]);
    store.apply(entries).await.unwrap();

    let control = MarketDataControl::new(
        channels.enabled.clone(),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        true,
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        Arc::new(Mutex::new(None)),
        Arc::new(tokio::sync::Notify::new()),
    );
    store.start_market_data_feed(&control, 5).await;
    assert!(channels.enabled.load(Ordering::Relaxed), "gate opened");

    let snapshot = control.take_pending().expect("snapshot queued");
    assert_eq!(1, snapshot.log_index, "snapshot covers the follower entry");
    assert_eq!(
        1, snapshot.market_seq,
        "the next market sequence after the bid"
    );
    assert_eq!(1, snapshot.bids.len());
    assert_eq!(5.0, snapshot.bids[0].volume);

    let mut ask = Order::new(OrderSide::Sell, 101.0, 1.0, 0);
    ask.timestamp = 2_000;
    let entries = futures::stream::iter(vec![Ok((
        entry(2, EntryPayload::Normal(MatchEngineRequest::place(ask))),
        None,
    ))]);
    store.apply(entries).await.unwrap();

    match channels.receiver.recv().await.expect("post-snapshot frame") {
        Ingest::Frame(frame) => {
            assert_eq!(2, frame.log_index);
            assert_eq!(1, frame.market_seq);
        }
    }
}

/// Cancel/Amend consume a market sequence and run the automatic GTD purge,
/// so an order due at the request's intake time cannot be cancelled or filled.
#[tokio::test]
async fn cancel_and_amend_consume_sequences_and_purge_due_gtd() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use crate::order_book::TimeInForce;
    use openraft::alias::EntryOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
            .unwrap();

    let mut gtd = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
    gtd.tif = TimeInForce::Gtd(2_000);
    gtd.timestamp = 1_000;
    let mut cancel_key = Order::new(OrderSide::Buy, 100.0, 5.0, 0);
    cancel_key.timestamp = 2_000;
    let entries = futures::stream::iter(vec![
        Ok((
            entry(1, EntryPayload::Normal(MatchEngineRequest::place(gtd))),
            None,
        )),
        Ok((
            entry(
                2,
                EntryPayload::Normal(MatchEngineRequest::Cancel { order: cancel_key }),
            ),
            None,
        )),
    ]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    assert_eq!(
        3,
        events.len(),
        "rest, expiry, then rejected cancel: {events:?}"
    );
    assert_eq!(MatchEventKind::Rested, events[0].kind);
    assert_eq!(MatchEventKind::Expired, events[1].kind);
    assert_eq!(5.0, events[1].volume);
    assert_eq!(MatchEventKind::Rejected, events[2].kind);
    assert_eq!(Some(RejectReason::OrderNotFound), events[2].reason);
    assert!(
        store.inner().lock().await.orderbook.bids.is_empty(),
        "the expired bid is gone and the cancel found nothing"
    );
}

struct TempDir(PathBuf);

impl TempDir {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> TempDir {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let unique = format!(
        "matchengine-sm-test-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let path = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&path).unwrap();
    TempDir(path)
}

/// Events are replicated, bounded, paged and deduplicated by client id.
#[tokio::test]
async fn event_log_retains_pages_and_dedupes_submissions() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use openraft::alias::EntryOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    fn place(
        index: u64,
        side: OrderSide,
        price: f64,
        client_id: &str,
    ) -> EntryResponder<MatchEngineTypeConfig> {
        (
            entry(
                index,
                EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
                    Order::new(side, price, 1.0, 0),
                    client_id,
                )),
            ),
            None,
        )
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store = StateMachineStore::<MatchEngineTypeConfig>::open_with_limits(
        dir.path(),
        sink.clone(),
        None,
        3,
        3,
    )
    .await
    .unwrap();

    let entries = futures::stream::iter(vec![
        Ok(place(1, OrderSide::Buy, 100.0, "a")),
        Ok(place(2, OrderSide::Buy, 99.0, "b")),
        // Same id, different order: must not place a second order.
        Ok(place(3, OrderSide::Sell, 50.0, "a")),
    ]);
    store.apply(entries).await.unwrap();

    let content = store.to_content().await;
    assert_eq!(2, content.orders.len(), "the duplicate did not place");
    assert_eq!(
        2, content.sequence,
        "the duplicate did not consume a sequence"
    );
    assert_eq!(2, content.submissions.len());
    assert_eq!(2, content.events.len());
    assert_eq!(2, sink.events().len(), "the duplicate produced no events");

    // Two more entries push the oldest events and submission out of the window.
    let entries = futures::stream::iter(vec![
        Ok(place(4, OrderSide::Buy, 98.0, "c")),
        Ok(place(5, OrderSide::Buy, 97.0, "d")),
    ]);
    store.apply(entries).await.unwrap();

    let content = store.to_content().await;
    assert_eq!(3, content.events.len(), "window is capped");
    assert_eq!(1, content.events_evicted);
    assert_eq!(3, content.submissions.len(), "submission map is capped");
    assert_eq!(1, content.submissions_evicted);

    let inner = store.inner().lock().await;
    assert_eq!(
        Some(EventId {
            log_index: 2,
            ordinal: 0
        }),
        inner.oldest_event_id()
    );
    assert_eq!(
        Some(EventId {
            log_index: 5,
            ordinal: 0
        }),
        inner.last_event_id()
    );
    let (page, next, has_more) = inner.events_since(None, 2);
    assert_eq!(2, page.len());
    assert!(has_more);
    assert_eq!(Some(page[1].event_id), next);
    let (page2, _, has_more2) = inner.events_since(next, 2);
    assert_eq!(1, page2.len());
    assert!(!has_more2);
    assert_eq!(MatchEventKind::Rested, page2[0].kind);

    // An evicted client id is no longer deduplicated; it is a new submission.
    drop(inner);
    let entries = futures::stream::iter(vec![Ok(place(6, OrderSide::Buy, 96.0, "a"))]);
    store.apply(entries).await.unwrap();
    assert_eq!(5, store.to_content().await.orders.len());
}

#[tokio::test]
async fn event_log_and_dedupe_survive_a_snapshot() {
    use crate::match_result::NoopSink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use openraft::alias::EntryOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let mut store = StateMachineStore::<MatchEngineTypeConfig>::open_with_sinks_and_retention(
        dir.path(),
        Arc::new(NoopSink),
        None,
        8,
    )
    .unwrap();

    let entries = futures::stream::iter(vec![Ok((
        entry(
            1,
            EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Buy, 100.0, 5.0, 0),
                "snapshot-id",
            )),
        ),
        None,
    ))]);
    store.apply(entries).await.unwrap();
    store.persist_snapshot().await.unwrap();

    let reopened = StateMachineStore::<MatchEngineTypeConfig>::open_with_sinks_and_retention(
        dir.path(),
        Arc::new(NoopSink),
        None,
        8,
    )
    .unwrap();
    let content = reopened.to_content().await;
    assert_eq!(1, content.orders.len());
    assert_eq!(1, content.sequence);
    assert_eq!(1, content.events.len());
    assert_eq!(1, content.submissions.len());

    // The retry is still a duplicate after restarting from the snapshot.
    let mut store = reopened;
    let entries = futures::stream::iter(vec![Ok((
        entry(
            2,
            EntryPayload::Normal(MatchEngineRequest::place_with_client_id(
                Order::new(OrderSide::Sell, 50.0, 1.0, 0),
                "snapshot-id",
            )),
        ),
        None,
    ))]);
    store.apply(entries).await.unwrap();
    let content = store.to_content().await;
    assert_eq!(1, content.orders.len());
    assert_eq!(1, content.sequence);
}

/// Long-poll readers sleep on the event watermark; only appends may wake them.
#[tokio::test]
async fn apply_wakes_event_waiters_only_when_events_are_appended() {
    use openraft::alias::EntryOf;
    use openraft::alias::EntryPayloadOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let mut store = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
    let mut changed = store.subscribe_events();
    assert_eq!(&None, &*changed.borrow_and_update());

    // A blank entry applies but appends no events.
    store
        .apply(futures::stream::iter(vec![Ok((
            entry(1, EntryPayload::Blank),
            None,
        ))]))
        .await
        .unwrap();
    assert!(
        !changed.has_changed().unwrap(),
        "a blank entry woke waiters"
    );

    // A resting order appends one event and advances the watermark.
    store
        .apply(futures::stream::iter(vec![Ok((
            entry(
                2,
                EntryPayload::Normal(MatchEngineRequest::place(Order::new(
                    OrderSide::Sell,
                    100.0,
                    1.0,
                    0,
                ))),
            ),
            None,
        ))]))
        .await
        .unwrap();
    assert!(
        changed.has_changed().unwrap(),
        "an appended event did not wake waiters"
    );
    let watermark = *changed.borrow_and_update();
    assert_eq!(
        Some(EventId {
            log_index: 2,
            ordinal: 0,
        }),
        watermark
    );

    // Another no-event entry leaves the watermark where it was.
    store
        .apply(futures::stream::iter(vec![Ok((
            entry(3, EntryPayload::Blank),
            None,
        ))]))
        .await
        .unwrap();
    assert!(!changed.has_changed().unwrap());
    assert_eq!(watermark, *changed.borrow_and_update());
}

/// STP maker cancellations are interleaved with fills, and pegged orders are
/// repriced after the primary request's events.
#[tokio::test]
async fn apply_publishes_stp_and_pegged_events() {
    use crate::match_result::MatchEventKind;
    use crate::match_result::MemorySink;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use crate::order_book::PegReference;
    use crate::order_book::RejectReason;
    use crate::order_book::StpMode;
    use crate::order_book::SymbolConfig;
    use openraft::alias::EntryOf;

    fn entry(
        index: u64,
        payload: EntryPayloadOf<MatchEngineTypeConfig>,
    ) -> EntryOf<MatchEngineTypeConfig> {
        Entry {
            log_id: log_id(1, index),
            payload,
        }
    }

    let dir = tempdir();
    let sink = Arc::new(MemorySink::new());
    let mut store =
        StateMachineStore::<MatchEngineTypeConfig>::open_with_sink(dir.path(), sink.clone())
            .unwrap();

    let config = SymbolConfig::new(0, 2, 8)
        .unwrap()
        .with_stp(StpMode::CancelResting);
    let mut same = Order::new(OrderSide::Sell, 100.0, 1.0, 0);
    same.owner_id = Some(1);
    let mut other = Order::new(OrderSide::Sell, 100.0, 1.0, 0);
    other.owner_id = Some(2);
    let mut taker = Order::new(OrderSide::Buy, 100.0, 2.0, 0);
    taker.owner_id = Some(1);
    let entries = futures::stream::iter(vec![
        Ok((
            entry(
                1,
                EntryPayload::Normal(MatchEngineRequest::ConfigureSymbol { config }),
            ),
            None,
        )),
        Ok((
            entry(2, EntryPayload::Normal(MatchEngineRequest::place(same))),
            None,
        )),
        Ok((
            entry(3, EntryPayload::Normal(MatchEngineRequest::place(other))),
            None,
        )),
        Ok((
            entry(4, EntryPayload::Normal(MatchEngineRequest::place(taker))),
            None,
        )),
    ]);
    store.apply(entries).await.unwrap();

    let events = sink.events();
    assert_eq!(
        vec![
            MatchEventKind::Rested,
            MatchEventKind::Rested,
            MatchEventKind::Cancelled,
            MatchEventKind::Fill,
            MatchEventKind::Rested,
        ],
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        "events: {events:?}"
    );
    assert_eq!(Some(RejectReason::SelfTradePrevented), events[2].reason);
    assert_eq!(same.sequence, events[2].order_sequence);

    // A better non-pegged bid reprices the primary peg after the rested event.
    let bid = Order::new(OrderSide::Buy, 101.0, 1.0, 0);
    let peg = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
    let mut peg = peg;
    peg.peg = Some(PegReference::Primary);
    let entries = futures::stream::iter(vec![
        Ok((
            entry(5, EntryPayload::Normal(MatchEngineRequest::place(bid))),
            None,
        )),
        Ok((
            entry(6, EntryPayload::Normal(MatchEngineRequest::place(peg))),
            None,
        )),
    ]);
    store.apply(entries).await.unwrap();

    // The peg is placed after the better bid, so it targets 101 immediately.
    let events = sink.events();
    let peg_events = &events[events.len() - 2..];
    assert_eq!(
        vec![MatchEventKind::Rested, MatchEventKind::Rested],
        peg_events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>()
    );
    let peg_sequence = peg_events[1].order_sequence;

    let better = Order::new(OrderSide::Buy, 102.0, 1.0, 0);
    let entries = futures::stream::iter(vec![Ok((
        entry(7, EntryPayload::Normal(MatchEngineRequest::place(better))),
        None,
    ))]);
    store.apply(entries).await.unwrap();
    let events = sink.events();
    let tail = &events[events.len() - 2..];
    assert_eq!(
        vec![MatchEventKind::Rested, MatchEventKind::Amended],
        tail.iter().map(|event| event.kind).collect::<Vec<_>>()
    );
    let amended = &tail[1];
    assert_eq!(Fixed::from(102), amended.price);
    assert_eq!(Some(peg_sequence), amended.previous_sequence);
}

#[tokio::test]
async fn snapshot_preserves_v11_order_types() {
    use crate::order_book::Fixed;
    use crate::order_book::Order;
    use crate::order_book::OrderSide;
    use crate::order_book::PegReference;
    use crate::order_book::StpMode;
    use crate::order_book::SymbolConfig;

    let dir = tempdir();
    let store = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
    {
        let mut inner = store.inner().lock().await;
        inner
            .configure_symbol(
                SymbolConfig::new(0, 2, 8)
                    .unwrap()
                    .with_stp(StpMode::CancelBoth),
            )
            .unwrap();
        inner.orderbook.last_trade_price = Some(Fixed::from(100));

        let mut bid = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
        bid.owner_id = Some(7);
        inner.orderbook.place_order(&mut bid);
        let mut ask = Order::new(OrderSide::Sell, 102.0, 1.0, 0);
        inner.orderbook.place_order(&mut ask);

        let mut peg = Order::new(OrderSide::Buy, 0.0, 1.0, 0);
        peg.peg = Some(PegReference::Primary);
        inner.orderbook.place_order(&mut peg);

        let mut trail = Order::new(OrderSide::Sell, 99.0, 1.0, 0);
        trail.trail_offset = Fixed::from(1);
        inner.orderbook.place_order(&mut trail);

        inner.last_applied_log = Some(log_id(1, 3));
    }
    store.persist_snapshot().await.unwrap();

    let reopened = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
    let content = reopened.to_content().await;
    assert_eq!(StpMode::CancelBoth, content.symbol_config.stp);
    assert!(
        content.orders.iter().any(|order| order.owner_id == Some(7)),
        "owner identity is persisted"
    );
    let peg = content
        .orders
        .iter()
        .find(|order| order.is_pegged())
        .expect("the peg is persisted");
    assert_eq!(Some(PegReference::Primary), peg.peg);
    assert_eq!(Fixed::from(100), peg.price);

    let trailing = content
        .stops
        .iter()
        .find(|order| order.is_trailing_stop())
        .expect("the trailing stop is persisted");
    assert_eq!(Fixed::from(100), trailing.trail_reference);
    assert_eq!(Fixed::from(99), trailing.stop_price);
}

#[tokio::test]
async fn old_snapshot_version_is_rejected() {
    let dir = tempdir();
    let mut bytes = Vec::from(SNAPSHOT_MAGIC.as_slice());
    bytes.extend_from_slice(&10u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    std::fs::write(dir.path().join(SNAPSHOT_FILE), bytes).unwrap();

    let err = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap_err();
    assert_eq!(io::ErrorKind::InvalidData, err.kind());
    assert!(
        err.to_string().contains("unsupported snapshot version 10"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn staged_snapshot_installs_and_removes_its_staging_file() {
    use crate::store::state_machine::STAGING_FILE_PREFIX;

    let source_dir = tempdir();
    let target_dir = tempdir();

    let snapshot = {
        let mut store =
            StateMachineStore::<MatchEngineTypeConfig>::open(source_dir.path()).unwrap();
        {
            let mut inner = store.inner().lock().await;
            inner.orderbook.place_order(&mut Order::new(
                crate::order_book::OrderSide::Buy,
                10.0,
                2.0,
                0,
            ));
            inner.last_applied_log = Some(log_id(1, 3));
        }
        store.build_snapshot().await.unwrap()
    };
    let bytes = snapshot.snapshot.clone().into_bytes().unwrap();
    let mut target = StateMachineStore::<MatchEngineTypeConfig>::open(target_dir.path()).unwrap();
    let staging_path = target_dir
        .path()
        .join(format!("{STAGING_FILE_PREFIX}test.tmp"));
    std::fs::write(&staging_path, &bytes).unwrap();
    let staged = StagedSnapshot::new(&staging_path, bytes.len() as u64, crc32fast::hash(&bytes));

    target
        .install_snapshot(&snapshot.meta, SnapshotData::Staged(Arc::new(staged)))
        .await
        .unwrap();

    let content = target.to_content().await;
    assert_eq!(1, content.orders.len());
    let (applied, _) = target.clone().applied_state().await.unwrap();
    assert_eq!(Some(log_id(1, 3)), applied);
    assert!(
        !staging_path.exists(),
        "a completed install removes its staging file"
    );
}

#[tokio::test]
async fn staged_snapshot_with_a_bad_checksum_is_rejected() {
    use crate::store::state_machine::STAGING_FILE_PREFIX;

    let dir = tempdir();
    let mut store = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
    {
        let mut inner = store.inner().lock().await;
        inner.orderbook.place_order(&mut Order::new(
            crate::order_book::OrderSide::Buy,
            10.0,
            2.0,
            0,
        ));
        inner.last_applied_log = Some(log_id(1, 3));
    }
    store.persist_snapshot().await.unwrap();

    let staging_path = dir.path().join(format!("{STAGING_FILE_PREFIX}bad.tmp"));
    std::fs::write(&staging_path, b"corrupt staged body").unwrap();
    let staged = StagedSnapshot::new(&staging_path, 19, 0);
    let meta = SnapshotMetaOf::<MatchEngineTypeConfig> {
        last_log_id: Some(log_id(2, 99)),
        last_membership: Default::default(),
    };

    let err = store
        .install_snapshot(&meta, SnapshotData::Staged(Arc::new(staged)))
        .await
        .unwrap_err();
    assert_eq!(io::ErrorKind::InvalidData, err.kind());
    assert_eq!(1, store.to_content().await.orders.len());
    assert!(
        !staging_path.exists(),
        "a rejected install still removes its staging file"
    );
}

#[tokio::test]
async fn stale_staging_files_are_removed_on_open() {
    use crate::store::state_machine::STAGING_FILE_PREFIX;

    let dir = tempdir();
    let stale = dir.path().join(format!("{STAGING_FILE_PREFIX}stale.tmp"));
    std::fs::write(&stale, b"left by a killed process").unwrap();

    let _store = StateMachineStore::<MatchEngineTypeConfig>::open(dir.path()).unwrap();
    assert!(!stale.exists(), "startup clears transport staging files");
}

use super::*;
use crate::order_book::MatchResult;
use crate::order_book::Order;
use crate::order_book::fixed::SCALE;

#[derive(Clone, Default)]
struct CollectSink(Arc<Mutex<Vec<MarketMessage>>>);

impl MarketDataSink for CollectSink {
    fn name(&self) -> &'static str {
        "collect"
    }

    fn publish(&mut self, message: &MarketMessage) -> io::Result<()> {
        self.0.lock().unwrap().push(message.clone());
        Ok(())
    }
}

struct Harness {
    processor: MarketDataProcessor,
    messages: Arc<Mutex<Vec<MarketMessage>>>,
    stats: Arc<MarketDataStats>,
    leader: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
    leader_id: Arc<AtomicU64>,
    resetting: Arc<AtomicBool>,
    gap_pending: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
}

fn harness(config: MarketDataConfig) -> Harness {
    let messages = Arc::new(Mutex::new(Vec::new()));
    let stats = Arc::new(MarketDataStats::default());
    let leader = Arc::new(AtomicBool::new(true));
    let epoch = Arc::new(AtomicU64::new(0));
    let leader_id = Arc::new(AtomicU64::new(0));
    let resetting = Arc::new(AtomicBool::new(false));
    let pending = Arc::new(Mutex::new(None));
    let dropped = Arc::new(AtomicU64::new(0));
    let gap_pending = Arc::new(AtomicBool::new(false));
    let processor = MarketDataProcessor::new(
        config,
        Box::new(CollectSink(messages.clone())),
        stats.clone(),
        leader.clone(),
        epoch.clone(),
        leader_id.clone(),
        resetting.clone(),
        pending,
        dropped.clone(),
        gap_pending.clone(),
    );
    Harness {
        processor,
        messages,
        stats,
        leader,
        epoch,
        leader_id,
        resetting,
        gap_pending,
        dropped,
    }
}

impl Harness {
    fn set_epoch(&self, epoch: u64, leader_id: u64) {
        self.epoch.store(epoch, Ordering::Relaxed);
        self.leader_id.store(leader_id, Ordering::Relaxed);
    }

    fn messages(&self) -> Vec<MarketMessage> {
        self.messages.lock().unwrap().clone()
    }

    fn bodies(&self) -> Vec<MessageBody> {
        self.messages()
            .into_iter()
            .map(|message| message.body)
            .collect()
    }

    fn clear(&self) {
        self.messages.lock().unwrap().clear();
    }

    fn assert_unique_keys(&self) {
        let messages = self.messages();
        let mut keys: Vec<(u16, u64, u16)> = messages
            .iter()
            .map(|message| (message.stream, message.market_seq, message.sub_seq))
            .collect();
        let total = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(total, keys.len(), "duplicate message keys: {messages:?}");
    }
}

fn frame(
    market_seq: u64,
    ts_ms: u64,
    depth: Vec<DepthChange>,
    events: Vec<MatchEvent>,
) -> TapFrame {
    TapFrame {
        log_index: market_seq + 1,
        market_seq,
        ts_ms,
        symbol_id: 0,
        events,
        depth,
    }
}

fn fill_event(
    market_seq: u64,
    side: OrderSide,
    price: f64,
    volume: f64,
    taker: u64,
    maker: u64,
) -> MatchEvent {
    let taker_order = Order::new(side, price, volume, taker);
    let maker_order = Order::new(
        match side {
            OrderSide::Buy => OrderSide::Sell,
            OrderSide::Sell => OrderSide::Buy,
        },
        price,
        volume,
        maker,
    );
    MatchEvent::for_fills(
        market_seq + 1,
        0,
        side,
        taker,
        &[MatchResult::new(&taker_order, &maker_order, volume)],
    )
    .remove(0)
}

fn depth(side: OrderSide, price: f64, delta_volume: f64, delta_orders: i32) -> DepthChange {
    DepthChange {
        side,
        price: price.into(),
        delta_volume: delta_volume.into(),
        delta_orders,
    }
}

fn snapshot(market_seq: u64) -> SnapshotFrame {
    SnapshotFrame {
        log_index: market_seq + 1,
        market_seq,
        ts_ms: 5_000,
        symbol_id: 0,
        bids: vec![DepthLevel {
            price: 100.0.into(),
            volume: 5.0.into(),
            orders: 1,
        }],
        asks: vec![DepthLevel {
            price: 101.0.into(),
            volume: 4.0.into(),
            orders: 1,
        }],
        last_trade: Some(100.5.into()),
        book_sequence: market_seq + 1,
        pending_stops: 1,
    }
}

#[test]
fn messages_carry_the_current_epoch_and_leader() {
    let mut h = harness(MarketDataConfig::default());
    h.set_epoch(7, 3);
    h.processor.ingest(frame(
        1,
        1_000,
        vec![depth(OrderSide::Sell, 100.0, 5.0, 1)],
        vec![fill_event(1, OrderSide::Buy, 100.0, 1.0, 1, 2)],
    ));

    let messages = h.messages();
    assert!(!messages.is_empty());
    assert!(
        messages
            .iter()
            .all(|message| message.epoch == 7 && message.leader_id == 3),
        "every header must carry the current epoch/leader: {messages:?}"
    );
}

#[test]
fn publishes_all_products_for_one_frame() {
    let mut h = harness(MarketDataConfig::default());
    h.processor.ingest(frame(
        7,
        1_000,
        vec![
            depth(OrderSide::Sell, 100.5, 5.0, 1),
            depth(OrderSide::Buy, 99.5, 3.0, 1),
        ],
        vec![fill_event(7, OrderSide::Buy, 100.5, 2.0, 10, 1)],
    ));

    let messages = h.messages();
    assert!(
        messages
            .iter()
            .all(|message| message.market_seq == 7 && message.sub_seq == 0),
        "every message is anchored to the frame: {messages:?}"
    );

    let trade = messages
        .iter()
        .find_map(|message| match &message.body {
            MessageBody::Trade(records) => Some(records),
            _ => None,
        })
        .expect("trade message");
    assert_eq!(1, trade.len());
    assert_eq!(OrderSide::Buy, trade[0].taker_side);
    assert_eq!(100.5, trade[0].price);
    assert_eq!(2.0, trade[0].volume);

    let bbo = messages
        .iter()
        .find_map(|message| match &message.body {
            MessageBody::Bbo(bbo) => Some(bbo),
            _ => None,
        })
        .expect("bbo message");
    assert_eq!(Some(100.5.into()), bbo.last_trade);
    assert_eq!((99.5.into(), 3.0.into()), (bbo.bid, bbo.bid_volume));
    assert_eq!((100.5.into(), 5.0.into()), (bbo.ask, bbo.ask_volume));

    let depth = messages
        .iter()
        .find_map(|message| match &message.body {
            MessageBody::DepthDelta(updates) => Some(updates),
            _ => None,
        })
        .expect("depth message");
    assert_eq!(2, depth.len());

    assert!(
        messages
            .iter()
            .any(|message| matches!(message.body, MessageBody::Lifecycle(_))),
        "lifecycle message"
    );
    h.assert_unique_keys();
}

#[test]
fn one_trade_message_carries_every_fill_of_the_taker() {
    let mut h = harness(MarketDataConfig::default());
    let events: Vec<MatchEvent> = (0..500u64)
        .map(|i| fill_event(9, OrderSide::Sell, 100.0, 1.0, 100, i))
        .collect();
    h.processor.ingest(frame(9, 1_000, vec![], events));

    let trades: Vec<_> = h
        .bodies()
        .into_iter()
        .filter_map(|body| match body {
            MessageBody::Trade(records) => Some(records),
            _ => None,
        })
        .collect();
    assert_eq!(1, trades.len(), "one message per taker");
    assert_eq!(500, trades[0].len(), "no chunking");
    assert_eq!(500, h.stats.trades.load(Ordering::Relaxed));
}

#[test]
fn exact_accessors_resolve_feed_overflow_sentinels() {
    let trade = TradeRecord {
        log_index: 1,
        ordinal: 0,
        ts_ms: 1_000,
        price: 100.0.into(),
        volume: 5.0.into(),
        quote_volume: Fixed::ZERO,
        taker_fee: Fixed::from(1),
        taker_fee_asset: Some(FeeAsset::Base),
        maker_fee: Fixed::ZERO,
        maker_fee_asset: Some(FeeAsset::Quote),
        error: MatchErrors::NOTIONAL_OVERFLOW | MatchErrors::MAKER_FEE_OVERFLOW,
        taker_side: OrderSide::Buy,
        taker_sequence: 2,
        maker_sequence: 1,
    };
    assert_eq!(None, trade.exact_quote_volume());
    assert_eq!(Some(Fixed::from(1)), trade.exact_taker_fee());
    assert_eq!(None, trade.exact_maker_fee());

    let lifecycle = LifecycleRecord {
        log_index: 2,
        ordinal: 1,
        ts_ms: 1_001,
        kind: MatchEventKind::Fill,
        order_sequence: 3,
        side: OrderSide::Sell,
        counterparty_sequence: Some(4),
        price: 101.0.into(),
        volume: 2.0.into(),
        quote_volume: 202.0.into(),
        volume_unit: VolumeUnit::Base,
        taker_fee: Fixed::ZERO,
        taker_fee_asset: Some(FeeAsset::Quote),
        maker_fee: Fixed::from(2),
        maker_fee_asset: Some(FeeAsset::Base),
        error: MatchErrors::TAKER_FEE_OVERFLOW,
        reason: None,
    };
    assert_eq!(Some(Fixed::from(202)), lifecycle.exact_quote_volume());
    assert_eq!(None, lifecycle.exact_taker_fee());
    assert_eq!(Some(Fixed::from(2)), lifecycle.exact_maker_fee());

    let rested = LifecycleRecord {
        kind: MatchEventKind::Rested,
        error: MatchErrors::NONE,
        ..lifecycle
    };
    assert_eq!(None, rested.exact_quote_volume());
    assert_eq!(None, rested.exact_taker_fee());
    assert_eq!(None, rested.exact_maker_fee());
}

#[test]
fn each_frame_emits_its_own_post_batch_depth_total() {
    let mut h = harness(MarketDataConfig::default());
    h.processor.ingest(frame(
        1,
        1_000,
        vec![depth(OrderSide::Sell, 100.0, 5.0, 1)],
        vec![],
    ));
    h.processor.ingest(frame(
        2,
        1_001,
        vec![depth(OrderSide::Sell, 100.0, -2.0, 0)],
        vec![],
    ));

    let levels: Vec<u64> = h
        .messages()
        .iter()
        .filter_map(|message| match &message.body {
            MessageBody::DepthDelta(updates) => {
                Some(updates[0].level.volume.raw() as u64 / (SCALE / 10) as u64)
            }
            _ => None,
        })
        .collect();
    assert_eq!(vec![50, 30], levels);
    assert_eq!(
        vec![1u64, 2u64],
        h.messages()
            .iter()
            .filter(|m| matches!(m.body, MessageBody::DepthDelta(_)))
            .map(|m| m.market_seq)
            .collect::<Vec<_>>()
    );
}

#[test]
fn removed_depth_level_is_published_as_a_zero_volume_level() {
    let mut h = harness(MarketDataConfig::default());
    h.processor.ingest(frame(
        1,
        1_000,
        vec![depth(OrderSide::Sell, 100.0, 5.0, 1)],
        vec![],
    ));
    // Filling or cancelling the last order at the level removes it from the
    // engine's shadow book. Diff-depth consumers need the zero-volume level to
    // delete their own copy.
    h.processor.ingest(frame(
        2,
        1_001,
        vec![depth(OrderSide::Sell, 100.0, -5.0, -1)],
        vec![],
    ));

    let deltas: Vec<Vec<LevelUpdate>> = h
        .messages()
        .into_iter()
        .filter_map(|message| match message.body {
            MessageBody::DepthDelta(updates) => Some(updates),
            _ => None,
        })
        .collect();
    assert_eq!(2, deltas.len());
    assert_eq!(1, deltas[1].len());
    let removal = &deltas[1][0];
    assert_eq!(OrderSide::Sell, removal.side);
    assert_eq!(100.0, removal.level.price);
    assert_eq!(Fixed::ZERO, removal.level.volume);
    assert_eq!(0, removal.level.orders);
}

#[test]
fn bars_close_from_event_time_and_are_anchored() {
    let config = MarketDataConfig {
        bar_intervals_ms: vec![1_000],
        ..MarketDataConfig::default()
    };
    let mut h = harness(config);

    h.processor.ingest(frame(
        1,
        1_000,
        vec![],
        vec![fill_event(1, OrderSide::Buy, 100.0, 1.0, 1, 2)],
    ));
    h.processor.ingest(frame(
        2,
        1_100,
        vec![],
        vec![fill_event(2, OrderSide::Buy, 102.0, 2.0, 3, 4)],
    ));
    h.processor.ingest(frame(
        3,
        2_100,
        vec![],
        vec![fill_event(3, OrderSide::Buy, 99.0, 1.0, 5, 6)],
    ));

    let bars: Vec<(u64, BarRecord)> = h
        .messages()
        .into_iter()
        .filter_map(|message| match message.body {
            MessageBody::Bar(bar) => Some((message.market_seq, bar)),
            _ => None,
        })
        .collect();
    let (anchor, closed) = bars
        .iter()
        .find(|(_, bar)| bar.window_start_ms == 1_000 && bar.trade_count > 0)
        .expect("closed bar");
    assert_eq!(3, *anchor, "the frame that rolled the window");
    assert_eq!(100.0, closed.open);
    assert_eq!(102.0, closed.high);
    assert_eq!(100.0, closed.low);
    assert_eq!(102.0, closed.close);
    assert_eq!(3.0, closed.volume);
    assert_eq!(2, closed.trade_count);
}

#[test]
fn lifecycle_sheds_rested_before_critical_events() {
    let config = MarketDataConfig {
        lifecycle_batch_max: 2,
        ..MarketDataConfig::default()
    };
    let mut h = harness(config);

    let mut events = Vec::new();
    for _ in 0..3 {
        let order = Order::new(OrderSide::Buy, 100.0, 1.0, 0);
        events.extend(MatchEvent::for_place_at(
            1,
            0,
            &order,
            &[],
            Some(1.0.into()),
        ));
    }
    for sequence in 0..2 {
        let mut order = Order::new(OrderSide::Buy, 100.0, 1.0, sequence);
        order.sequence = sequence;
        events.push(MatchEvent::for_cancel_at(2, 0, &order));
    }
    h.processor.ingest(frame(3, 3_000, vec![], events));

    let lifecycle = h
        .bodies()
        .into_iter()
        .find_map(|body| match body {
            MessageBody::Lifecycle(records) => Some(records),
            _ => None,
        })
        .expect("lifecycle batch");
    assert_eq!(2, lifecycle.len());
    assert!(
        lifecycle
            .iter()
            .all(|record| record.kind == MatchEventKind::Cancelled)
    );
    assert_eq!(3, h.stats.lifecycle_shed.load(Ordering::Relaxed));
}

#[test]
fn snapshot_resets_and_suppresses_older_frames() {
    let mut h = harness(MarketDataConfig::default());
    h.processor.apply_snapshot(snapshot(10));

    let messages = h.messages();
    let reset = messages
        .iter()
        .find(|message| matches!(message.body, MessageBody::DepthSnapshot(_)))
        .expect("snapshot message");
    assert_eq!(10, reset.market_seq);
    assert_eq!(flags::SNAPSHOT | flags::RESET, reset.flags);
    assert!(
        messages
            .iter()
            .any(|message| matches!(message.body, MessageBody::Bbo(_)))
    );

    h.clear();
    h.processor.ingest(frame(
        9,
        4_000,
        vec![depth(OrderSide::Buy, 99.0, 1.0, 1)],
        vec![fill_event(9, OrderSide::Buy, 99.0, 1.0, 7, 8)],
    ));
    assert!(h.messages().is_empty(), "pre-snapshot frame suppressed");

    h.processor.ingest(frame(
        11,
        6_000,
        vec![depth(OrderSide::Buy, 99.0, 1.0, 1)],
        vec![fill_event(11, OrderSide::Buy, 99.0, 1.0, 9, 10)],
    ));
    assert!(h.messages().iter().all(|message| message.market_seq == 11));
}

#[test]
fn leader_and_reset_gates_suppress_emission() {
    let mut h = harness(MarketDataConfig::default());
    h.leader.store(false, Ordering::Relaxed);
    h.processor.ingest(frame(
        1,
        1_000,
        vec![depth(OrderSide::Buy, 100.0, 1.0, 1)],
        vec![fill_event(1, OrderSide::Buy, 100.0, 1.0, 1, 2)],
    ));
    assert!(h.messages().is_empty());

    h.leader.store(true, Ordering::Relaxed);
    h.resetting.store(true, Ordering::Relaxed);
    h.processor.ingest(frame(
        2,
        1_100,
        vec![],
        vec![fill_event(2, OrderSide::Buy, 100.0, 1.0, 3, 4)],
    ));
    assert!(h.messages().is_empty(), "resetting suppresses");

    h.processor.apply_snapshot(snapshot(2));
    h.clear();
    h.processor.ingest(frame(
        3,
        1_200,
        vec![],
        vec![fill_event(3, OrderSide::Buy, 100.0, 1.0, 5, 6)],
    ));
    assert!(!h.messages().is_empty());

    h.leader.store(false, Ordering::Relaxed);
    h.clear();
    h.processor.ingest(frame(
        4,
        1_300,
        vec![],
        vec![fill_event(4, OrderSide::Buy, 100.0, 1.0, 7, 8)],
    ));
    assert!(h.messages().is_empty(), "loss closes the gate");
}

#[test]
fn gap_marker_is_anchored_to_the_next_frame() {
    let mut h = harness(MarketDataConfig::default());
    h.dropped.store(3, Ordering::Relaxed);
    h.gap_pending.store(true, Ordering::Relaxed);
    h.processor.ingest(frame(5, 1_000, vec![], vec![]));

    let gap = h
        .bodies()
        .into_iter()
        .find_map(|body| match body {
            MessageBody::Gap(gap) => Some(gap),
            _ => None,
        })
        .expect("gap marker");
    assert_eq!(3, gap.dropped);
    assert_eq!(1, h.stats.gaps.load(Ordering::Relaxed));
}

#[test]
fn status_uses_a_deterministic_cadence() {
    let config = MarketDataConfig {
        status_every: 100,
        ..MarketDataConfig::default()
    };
    let mut h = harness(config);

    h.processor.ingest(frame(100, 1_000, vec![], vec![]));
    h.processor.ingest(frame(101, 1_001, vec![], vec![]));

    let statuses: Vec<_> = h
        .messages()
        .into_iter()
        .filter_map(|message| match message.body {
            MessageBody::Status(status) => Some((message.market_seq, status)),
            _ => None,
        })
        .collect();
    assert_eq!(1, statuses.len());
    assert_eq!(100, statuses[0].0);
    assert_eq!(100, statuses[0].1.last_market_seq);
    assert_eq!(0, statuses[0].1.last_sub_seq);
}

#[test]
fn tap_drops_and_requests_a_gap_when_the_queue_is_full() {
    let (tap, channels) = MarketDataTap::channel(1);
    tap.publish(frame(1, 1_000, vec![], vec![]));
    tap.publish(frame(2, 1_001, vec![], vec![]));

    assert_eq!(1, channels.dropped.load(Ordering::Relaxed));
    assert!(channels.gap_pending.load(Ordering::Relaxed));
    assert_eq!(1, channels.stats.dropped_frames.load(Ordering::Relaxed));
    assert_eq!(2, channels.stats.frames.load(Ordering::Relaxed));
}

#[test]
fn two_replicas_fed_the_same_frames_emit_identical_messages() {
    let frames = vec![
        frame(
            1,
            1_000,
            vec![depth(OrderSide::Sell, 100.5, 5.0, 1)],
            vec![fill_event(1, OrderSide::Buy, 100.5, 2.0, 1, 2)],
        ),
        frame(2, 1_100, vec![depth(OrderSide::Buy, 99.5, 3.0, 1)], vec![]),
        frame(
            3,
            2_100,
            vec![depth(OrderSide::Sell, 100.5, -2.0, 0)],
            vec![fill_event(3, OrderSide::Buy, 100.5, 1.0, 3, 2)],
        ),
    ];
    let mut a = harness(MarketDataConfig::default());
    let mut b = harness(MarketDataConfig::default());
    for frame in &frames {
        a.processor.ingest(frame.clone());
        b.processor.ingest(frame.clone());
    }

    let json = |h: &Harness| serde_json::to_string(&h.messages()).unwrap();
    assert_eq!(json(&a), json(&b), "same frames, same messages");
    assert!(a.messages().iter().all(|message| message.market_seq > 0));
}

#[test]
fn a_takeover_snapshot_continues_the_same_keys() {
    let frames: Vec<TapFrame> = (1..=6)
        .map(|sequence| {
            frame(
                sequence,
                1_000 * sequence,
                vec![depth(OrderSide::Sell, 100.5, 1.0, 1)],
                vec![fill_event(sequence, OrderSide::Buy, 100.5, 1.0, 10, 20)],
            )
        })
        .collect();

    let mut a = harness(MarketDataConfig::default());
    for frame in &frames {
        a.processor.ingest(frame.clone());
    }

    let mut b = harness(MarketDataConfig::default());
    b.processor.apply_snapshot(snapshot(4));
    for frame in frames.iter().filter(|frame| frame.market_seq >= 4) {
        b.processor.ingest(frame.clone());
    }

    let trades = |h: &Harness, from: u64| -> Vec<(u64, Vec<TradeRecord>)> {
        h.messages()
            .into_iter()
            .filter(|message| message.market_seq >= from)
            .filter_map(|message| match message.body {
                MessageBody::Trade(records) => Some((message.market_seq, records)),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        trades(&a, 4),
        trades(&b, 4),
        "the takeover continues the same keys"
    );
    assert!(trades(&b, 4).iter().all(|(sequence, _)| *sequence >= 4));
}

#[test]
fn frames_after_the_snapshot_capture_are_replayed() {
    let mut h = harness(MarketDataConfig::default());
    h.resetting.store(true, Ordering::Relaxed);
    h.processor.ingest(frame(
        5,
        5_000,
        vec![depth(OrderSide::Buy, 99.0, 1.0, 1)],
        vec![fill_event(5, OrderSide::Buy, 99.0, 1.0, 7, 8)],
    ));
    assert!(h.messages().is_empty(), "buffered while resetting");

    h.processor.apply_snapshot(snapshot(5));

    let messages = h.messages();
    assert!(
        messages
            .iter()
            .any(|message| matches!(message.body, MessageBody::DepthSnapshot(_)))
    );
    assert!(
            messages
                .iter()
                .any(|message| message.market_seq == 5
                    && matches!(message.body, MessageBody::Trade(_))),
            "the post-capture frame is replayed after the snapshot"
        );
    h.assert_unique_keys();
}

#[test]
fn gap_and_status_share_the_stream_without_key_collisions() {
    let config = MarketDataConfig {
        status_every: 100,
        ..MarketDataConfig::default()
    };
    let mut h = harness(config);
    h.dropped.store(2, Ordering::Relaxed);
    h.gap_pending.store(true, Ordering::Relaxed);

    h.processor.ingest(frame(100, 1_000, vec![], vec![]));

    let statuses: Vec<_> = h
        .messages()
        .into_iter()
        .filter(|message| message.stream == STREAM_STATUS)
        .collect();
    assert_eq!(2, statuses.len(), "one GAP, one cadence status");
    let mut sub_seqs: Vec<u16> = statuses.iter().map(|message| message.sub_seq).collect();
    sub_seqs.sort_unstable();
    assert_eq!(
        vec![0, GAP_SUB_SEQ],
        sub_seqs,
        "the anchor's keys stay unique"
    );
    assert_eq!(
        GAP_SUB_SEQ,
        statuses
            .iter()
            .find(|message| message.flags == flags::GAP)
            .expect("gap marker")
            .sub_seq
    );
    let status = statuses
        .iter()
        .find_map(|message| match &message.body {
            MessageBody::Status(status) => Some(status),
            _ => None,
        })
        .expect("cadence status");
    assert_eq!(0, status.last_sub_seq);
}

#[test]
fn idle_heartbeat_uses_a_reserved_sub_seq_and_watermark() {
    let config = MarketDataConfig {
        heartbeat_ms: 1,
        ..MarketDataConfig::default()
    };
    let mut h = harness(config);
    h.processor.apply_snapshot(snapshot(4));
    h.clear();
    std::thread::sleep(std::time::Duration::from_millis(5));

    h.processor.emit_idle_heartbeat();

    let messages = h.messages();
    assert_eq!(1, messages.len(), "one idle heartbeat: {messages:?}");
    let heartbeat = &messages[0];
    assert_eq!(STREAM_STATUS, heartbeat.stream);
    assert_eq!(HEARTBEAT_SUB_SEQ, heartbeat.sub_seq);
    assert_eq!(4, heartbeat.market_seq);
    match &heartbeat.body {
        MessageBody::Status(status) => {
            assert_eq!(4, status.last_market_seq);
            assert_eq!(5, status.last_log_index);
            assert_eq!(0, status.last_ordinal);
        }
        other => panic!("expected heartbeat status, got {other:?}"),
    }
    assert_eq!(1, h.stats.heartbeats.load(Ordering::Relaxed));

    // A recent frame suppresses the heartbeat.
    h.clear();
    h.processor.ingest(frame(5, 1_000, vec![], vec![]));
    h.clear();
    h.processor.emit_idle_heartbeat();
    assert!(h.messages().is_empty(), "activity suppresses the heartbeat");
}

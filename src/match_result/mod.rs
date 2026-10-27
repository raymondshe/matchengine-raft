//! Turning match results into a feed for other systems.
//!
//! [`StateMachineStore::apply`](crate::store::StateMachineStore) computes fills on every replica.
//! It must stay deterministic — no IO, no clock, no network — so *what happens* with those results
//! is delegated to a [`MatchResultSink`] chosen once at startup:
//!
//! * [`NoopSink`] — the default: compute and drop, which is what this crate did before.
//! * [`FileLogSink`] — append JSON lines to a local file. A debugging aid for tests and single-node
//!   experiments; see its documentation for why it is not a clearing feed.
//! * [`MemorySink`] — keep events in memory; useful in unit tests that assert what `apply` produced.
//!
//! Adding a real delivery mechanism (an event log inside the replicated state machine, an MQ
//! bridge, ...) means implementing this trait. The state machine does not change.
//!
//! # Event identity
//!
//! Every event carries an [`EventId`]: the Raft log index of the entry that produced it plus the
//! ordinal of the event inside that entry. This is assigned from the applied log alone, so it is
//! identical on every replica, stable across restarts, and needs no extra state in snapshots.
//! `(log_index, ordinal)` is also the key a consumer should deduplicate on.

use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::MutexGuard;

use serde::Deserialize;
use serde::Serialize;

use crate::matchengine::MatchResult;
use crate::matchengine::Order;
use crate::matchengine::RejectReason;

/// Stable identity of one event: a Raft log index plus the event's position inside that entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EventId {
    /// Index of the Raft log entry whose application produced the event.
    pub log_index: u64,
    /// 0-based position of the event inside that entry.
    pub ordinal: u32,
}

/// What happened to an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchEventKind {
    /// The taker could not be fully filled; its remainder was inserted into the book.
    Rested,
    /// The taker traded `volume` with a resting order at `price`.
    Fill,
    /// An order was removed with `volume` still open: a cancel request, the
    /// cancelled remainder of an IOC order, or whatever a market order or a
    /// stop child could not execute.
    Cancelled,
    /// A parked stop order was elected by a trade at `price` and executed
    /// immediately as its child. Its trades follow as `Fill` events, and any
    /// remainder it could not execute as a `Cancelled` one.
    Triggered,
    /// An order was removed by an expiry purge (a `GTD` deadline or the day close).
    Expired,
    /// An order was refused outright and never entered the book; `reason`
    /// says why. Nothing executed and nothing rested.
    Rejected,
}

/// A single order-lifecycle event produced while applying one Raft entry.
///
/// Fills are the reason this type exists, but rests, cancels, stop triggers,
/// expiries and rejections travel through the same feed so a consumer can
/// follow an order from placement to removal with one cursor.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MatchEvent {
    /// Position in the applied log. Unique, stable and replica-independent.
    pub event_id: EventId,
    /// What happened.
    pub kind: MatchEventKind,
    /// Sequence number of the order this event is about (the taker, the resting order, or the
    /// cancelled order). `place_order` assigns it, so it is the id `/write` returns.
    pub order_sequence: u64,
    /// For a [`MatchEventKind::Fill`]: the resting order's sequence. `None` otherwise.
    pub counterparty_sequence: Option<u64>,
    /// Execution price for a fill, the trigger price for a stop election, the order price
    /// otherwise.
    pub price: f64,
    /// Executed volume for a fill; resting remainder for [`MatchEventKind::Rested`]; the volume
    /// that was still open for [`MatchEventKind::Cancelled`] and [`MatchEventKind::Expired`]; the
    /// stop's open volume for [`MatchEventKind::Triggered`]; the submitted volume for
    /// [`MatchEventKind::Rejected`].
    pub volume: f64,
    /// The policy reason behind a rejection or expiry. Absent (and omitted from the JSON lines)
    /// for every other kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<RejectReason>,
}

impl MatchEvent {
    /// Fill events for one sweep, in execution order.
    ///
    /// `first_ordinal` continues the numbering when one entry produces more than one batch (the
    /// fills of a stop elected by the entry's own trades, for instance).
    pub fn for_fills(
        log_index: u64,
        first_ordinal: u32,
        taker_sequence: u64,
        matches: &[MatchResult],
    ) -> Vec<MatchEvent> {
        matches
            .iter()
            .enumerate()
            .map(|(offset, m)| MatchEvent {
                event_id: EventId {
                    log_index,
                    ordinal: first_ordinal + offset as u32,
                },
                kind: MatchEventKind::Fill,
                order_sequence: taker_sequence,
                counterparty_sequence: Some(m.maker.sequence),
                price: m.maker.price,
                volume: m.matches,
                reason: None,
            })
            .collect()
    }

    /// Builds the events for a `Place` request.
    ///
    /// `matches` is what `OrderBook::place_order` returned, and `resting_volume` is the volume the
    /// same call left in the book (see `OrderBook::resting_volume`). Fills keep their matching
    /// order; the `Rested` event, when present, comes last.
    pub fn for_place(
        log_index: u64,
        taker: &Order,
        matches: &[MatchResult],
        resting_volume: Option<f64>,
    ) -> Vec<MatchEvent> {
        Self::for_place_at(log_index, 0, taker, matches, resting_volume)
    }

    /// [`MatchEvent::for_place`] with its first event at `first_ordinal`, so a stop cascade
    /// produced by the same entry continues the same numbering.
    pub fn for_place_at(
        log_index: u64,
        first_ordinal: u32,
        taker: &Order,
        matches: &[MatchResult],
        resting_volume: Option<f64>,
    ) -> Vec<MatchEvent> {
        let mut events = Self::for_fills(log_index, first_ordinal, taker.sequence, matches);

        if let Some(volume) = resting_volume {
            events.push(MatchEvent {
                event_id: EventId {
                    log_index,
                    ordinal: first_ordinal + events.len() as u32,
                },
                kind: MatchEventKind::Rested,
                order_sequence: taker.sequence,
                counterparty_sequence: None,
                price: taker.price,
                volume,
                reason: None,
            });
        }

        events
    }

    /// Builds the event for a cancel. `cancelled` is the order as it was stored in the book, so
    /// `volume` is the part that was still open, not the volume originally submitted.
    pub fn for_cancel(log_index: u64, cancelled: &Order) -> MatchEvent {
        Self::for_cancel_at(log_index, 0, cancelled)
    }

    /// [`MatchEvent::for_cancel`] at an explicit ordinal.
    pub fn for_cancel_at(log_index: u64, ordinal: u32, cancelled: &Order) -> MatchEvent {
        Self::for_cancelled_volume(log_index, ordinal, cancelled, cancelled.open_volume())
    }

    /// A `Cancelled` event with an explicit open volume: the unexecuted remainder of an IOC
    /// order, of a market order, or of a stop child, which is cancelled rather than rested.
    pub fn for_cancelled_volume(
        log_index: u64,
        ordinal: u32,
        order: &Order,
        volume: f64,
    ) -> MatchEvent {
        MatchEvent {
            event_id: EventId { log_index, ordinal },
            kind: MatchEventKind::Cancelled,
            order_sequence: order.sequence,
            counterparty_sequence: None,
            price: order.price,
            volume,
            reason: None,
        }
    }

    /// A parked stop was elected by a trade at `trigger_price`; the child's executions follow as
    /// `Fill` events and its unexecuted remainder as a `Cancelled` one.
    pub fn for_triggered(
        log_index: u64,
        ordinal: u32,
        stop: &Order,
        trigger_price: f64,
    ) -> MatchEvent {
        MatchEvent {
            event_id: EventId { log_index, ordinal },
            kind: MatchEventKind::Triggered,
            order_sequence: stop.sequence,
            counterparty_sequence: None,
            price: trigger_price,
            volume: stop.open_volume(),
            reason: None,
        }
    }

    /// An order removed by an expiry purge; `volume` is what was still open.
    pub fn for_expired(log_index: u64, ordinal: u32, expired: &Order) -> MatchEvent {
        MatchEvent {
            event_id: EventId { log_index, ordinal },
            kind: MatchEventKind::Expired,
            order_sequence: expired.sequence,
            counterparty_sequence: None,
            price: expired.price,
            volume: expired.open_volume(),
            reason: None,
        }
    }

    /// An order refused outright: it never entered the book and nothing executed for it.
    pub fn for_rejected(
        log_index: u64,
        ordinal: u32,
        order: &Order,
        reason: RejectReason,
    ) -> MatchEvent {
        MatchEvent {
            event_id: EventId { log_index, ordinal },
            kind: MatchEventKind::Rejected,
            order_sequence: order.sequence,
            counterparty_sequence: None,
            price: order.price,
            volume: order.volume,
            reason: Some(reason),
        }
    }
}

/// Receives the events produced by the state machine.
///
/// Implementations are chosen when the node is built (see `StorageConfig::match_event_log` for the
/// built-in file sink) and must be cheap enough not to stall applying: `apply` calls `publish`
/// while holding the state-machine lock, and a sink must never call back into the state machine.
///
/// An error is logged and otherwise ignored. Applying an entry must not depend on whether a sink
/// is reachable, or replicas would diverge.
pub trait MatchResultSink: Send + Sync + 'static {
    /// Short name used in logs and in `Debug` output.
    fn name(&self) -> &'static str;

    /// Handles one applied entry's events. Called at most once per non-empty batch, in log order.
    fn publish(&self, events: &[MatchEvent]) -> io::Result<()>;
}

/// The default sink: match results are computed and dropped.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSink;

impl MatchResultSink for NoopSink {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn publish(&self, _events: &[MatchEvent]) -> io::Result<()> {
        Ok(())
    }
}

/// Keeps events in memory so tests can assert exactly what `apply` produced.
#[derive(Debug, Default)]
pub struct MemorySink {
    events: Mutex<Vec<MatchEvent>>,
}

impl MemorySink {
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of every event received so far, in publish order.
    pub fn events(&self) -> Vec<MatchEvent> {
        self.lock().clone()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    pub fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> MutexGuard<'_, Vec<MatchEvent>> {
        // A panicking test must not poison the recorder for the rest of the process.
        self.events.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl MatchResultSink for MemorySink {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn publish(&self, events: &[MatchEvent]) -> io::Result<()> {
        self.lock().extend_from_slice(events);
        Ok(())
    }
}

/// Appends every event as one JSON object per line to a local file.
///
/// This is a **debugging aid**, not a clearing feed:
///
/// * It is local to the node that runs it; it is not replicated and every replica writes its own
///   copy (or interleaves lines if they share a path).
/// * Replaying the log after a restart publishes the replayed events again, so the file is
///   at-least-once and contains duplicates after recovery.
/// * It is flushed after every batch but not fsynced.
///
/// Each line looks like
/// `{"node_id":1,"event_id":{"log_index":42,"ordinal":0},"kind":"fill","order_sequence":7,...}`;
/// `node_id` is omitted unless [`FileLogSink::with_node_id`] was used.
#[derive(Debug)]
pub struct FileLogSink {
    path: PathBuf,
    node_id: Option<u64>,
    writer: Mutex<BufWriter<File>>,
}

impl FileLogSink {
    /// Opens `path` in append mode, creating the file and its parent directory if needed.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            node_id: None,
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    /// Stamps `node_id` on every line, so files collected from several nodes stay distinguishable.
    pub fn with_node_id(mut self, node_id: u64) -> Self {
        self.node_id = Some(node_id);
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl MatchResultSink for FileLogSink {
    fn name(&self) -> &'static str {
        "file"
    }

    fn publish(&self, events: &[MatchEvent]) -> io::Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        for event in events {
            let record = FileRecord {
                node_id: self.node_id,
                event,
            };
            // Serialise first so a line reaches the file in one write.
            let line = serde_json::to_string(&record).map_err(io::Error::other)?;
            writer.write_all(line.as_bytes())?;
            writer.write_all(b"\n")?;
        }
        writer.flush()
    }
}

#[derive(Serialize)]
struct FileRecord<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    node_id: Option<u64>,
    #[serde(flatten)]
    event: &'a MatchEvent,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matchengine::OrderSide;

    fn order(side: OrderSide, price: f64, volume: f64, sequence: u64) -> Order {
        Order::new(side, price, volume, sequence)
    }

    #[test]
    fn place_events_identify_both_sides_in_match_order() {
        let taker = order(OrderSide::Buy, 101.0, 15.0, 7);
        let maker1 = order(OrderSide::Sell, 100.0, 10.0, 3);
        let maker2 = order(OrderSide::Sell, 101.0, 8.0, 4);
        let matches = vec![
            MatchResult::new(&taker, &maker1, 10.0),
            MatchResult::new(&taker, &maker2, 5.0),
        ];

        let events = MatchEvent::for_place(42, &taker, &matches, None);

        assert_eq!(2, events.len());
        assert_eq!(
            EventId {
                log_index: 42,
                ordinal: 0
            },
            events[0].event_id
        );
        assert_eq!(MatchEventKind::Fill, events[0].kind);
        assert_eq!(7, events[0].order_sequence);
        assert_eq!(Some(3), events[0].counterparty_sequence);
        assert_eq!(100.0, events[0].price, "fills print at the maker price");
        assert_eq!(10.0, events[0].volume);

        assert_eq!(1, events[1].event_id.ordinal);
        assert_eq!(Some(4), events[1].counterparty_sequence);
        assert_eq!(101.0, events[1].price);
        assert_eq!(5.0, events[1].volume);
    }

    #[test]
    fn resting_remainder_is_the_last_event_of_the_entry() {
        let taker = order(OrderSide::Buy, 99.0, 10.0, 2);
        let maker = order(OrderSide::Sell, 98.0, 4.0, 1);
        let matches = vec![MatchResult::new(&taker, &maker, 4.0)];

        let events = MatchEvent::for_place(9, &taker, &matches, Some(6.0));

        assert_eq!(2, events.len());
        let rested = &events[1];
        assert_eq!(MatchEventKind::Rested, rested.kind);
        assert_eq!(
            EventId {
                log_index: 9,
                ordinal: 1
            },
            rested.event_id
        );
        assert_eq!(2, rested.order_sequence);
        assert_eq!(None, rested.counterparty_sequence);
        assert_eq!(99.0, rested.price);
        assert_eq!(6.0, rested.volume);
    }

    #[test]
    fn exact_fill_has_no_rested_event() {
        let taker = order(OrderSide::Sell, 100.0, 10.0, 3);
        let maker = order(OrderSide::Buy, 100.0, 10.0, 1);

        let events =
            MatchEvent::for_place(5, &taker, &[MatchResult::new(&taker, &maker, 10.0)], None);

        assert_eq!(1, events.len());
        assert_eq!(MatchEventKind::Fill, events[0].kind);
        assert!(events.iter().all(|e| e.kind != MatchEventKind::Rested));
    }

    #[test]
    fn cancel_reports_the_volume_that_was_still_open() {
        let cancelled = order(OrderSide::Sell, 12.5, 4.5, 9);

        let event = MatchEvent::for_cancel(11, &cancelled);

        assert_eq!(
            EventId {
                log_index: 11,
                ordinal: 0
            },
            event.event_id
        );
        assert_eq!(MatchEventKind::Cancelled, event.kind);
        assert_eq!(9, event.order_sequence);
        assert_eq!(None, event.counterparty_sequence);
        assert_eq!(12.5, event.price);
        assert_eq!(4.5, event.volume);
    }

    #[test]
    fn memory_sink_records_published_events() {
        let sink = MemorySink::new();
        let taker = order(OrderSide::Buy, 10.0, 2.0, 1);
        let events = MatchEvent::for_place(3, &taker, &[], Some(2.0));

        sink.publish(&events).unwrap();
        sink.publish(&[MatchEvent::for_cancel(4, &taker)]).unwrap();

        assert_eq!(2, sink.len());
        assert_eq!(events[0], sink.events()[0]);
        assert_eq!(MatchEventKind::Cancelled, sink.events()[1].kind);

        sink.clear();
        assert!(sink.is_empty());
    }

    #[test]
    fn file_sink_writes_one_json_line_per_event_with_node_id() {
        let dir = TempDir::new();
        let path = dir.path().join("nested/events.jsonl");
        let sink = FileLogSink::open(&path).unwrap().with_node_id(3);

        let taker = order(OrderSide::Buy, 100.0, 10.0, 0);
        let maker = order(OrderSide::Sell, 100.0, 4.0, 1);
        let events = MatchEvent::for_place(
            1,
            &taker,
            &[MatchResult::new(&taker, &maker, 4.0)],
            Some(6.0),
        );
        sink.publish(&events).unwrap();
        sink.publish(&[MatchEvent::for_cancel(2, &taker)]).unwrap();

        let lines = read_lines(&path);
        assert_eq!(3, lines.len());
        assert_eq!(serde_json::json!(3), lines[0]["node_id"]);
        assert_eq!(serde_json::json!("fill"), lines[0]["kind"]);
        assert_eq!(serde_json::json!(1), lines[0]["event_id"]["log_index"]);
        assert_eq!(serde_json::json!(0), lines[0]["event_id"]["ordinal"]);
        assert_eq!(serde_json::json!(4.0), lines[0]["volume"]);
        assert_eq!(serde_json::json!(1), lines[0]["counterparty_sequence"]);
        assert_eq!(serde_json::json!("rested"), lines[1]["kind"]);
        assert_eq!(serde_json::json!(6.0), lines[1]["volume"]);
        assert_eq!(serde_json::json!("cancelled"), lines[2]["kind"]);
        assert_eq!(serde_json::json!(2), lines[2]["event_id"]["log_index"]);
    }

    #[test]
    fn file_sink_appends_across_reopen_and_can_record_without_node_id() {
        let dir = TempDir::new();
        let path = dir.path().join("events.jsonl");

        {
            let sink = FileLogSink::open(&path).unwrap();
            sink.publish(&[MatchEvent::for_cancel(
                1,
                &order(OrderSide::Buy, 1.0, 1.0, 0),
            )])
            .unwrap();
        }
        {
            let sink = FileLogSink::open(&path).unwrap();
            sink.publish(&[MatchEvent::for_cancel(
                2,
                &order(OrderSide::Buy, 1.0, 1.0, 1),
            )])
            .unwrap();
        }

        let lines = read_lines(&path);
        assert_eq!(2, lines.len());
        assert!(lines[0].get("node_id").is_none());
        assert_eq!(serde_json::json!(0), lines[0]["order_sequence"]);
        assert_eq!(serde_json::json!(1), lines[1]["order_sequence"]);
    }

    #[test]
    fn file_sink_ignores_empty_batches() {
        let dir = TempDir::new();
        let path = dir.path().join("events.jsonl");
        let sink = FileLogSink::open(&path).unwrap();

        sink.publish(&[]).unwrap();

        assert_eq!(0, std::fs::metadata(&path).unwrap().len());
    }

    fn read_lines(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// A unique directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::AtomicU64;
            use std::sync::atomic::Ordering;

            static COUNTER: AtomicU64 = AtomicU64::new(0);

            let unique = format!(
                "matchengine-sink-test-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn trigger_expiry_and_rejection_events_carry_their_semantics() {
        let stop = order(OrderSide::Sell, 0.0, 5.0, 8);

        let triggered = MatchEvent::for_triggered(40, 1, &stop, 95.0);
        assert_eq!(MatchEventKind::Triggered, triggered.kind);
        assert_eq!(
            EventId {
                log_index: 40,
                ordinal: 1
            },
            triggered.event_id
        );
        assert_eq!(8, triggered.order_sequence);
        assert_eq!(
            95.0, triggered.price,
            "the trigger prints the electing trade price"
        );
        assert_eq!(5.0, triggered.volume, "the stop's open volume");
        assert_eq!(None, triggered.reason);

        let expired = MatchEvent::for_expired(41, 0, &stop);
        assert_eq!(MatchEventKind::Expired, expired.kind);
        assert_eq!(5.0, expired.volume);
        assert_eq!(8, expired.order_sequence);

        let rejected = MatchEvent::for_rejected(42, 0, &stop, RejectReason::FokUnfillable);
        assert_eq!(MatchEventKind::Rejected, rejected.kind);
        assert_eq!(Some(RejectReason::FokUnfillable), rejected.reason);
        assert_eq!(5.0, rejected.volume, "a rejection executes nothing");
    }

    #[test]
    fn cancel_with_an_explicit_remainder_reports_that_remainder() {
        let taker = order(OrderSide::Buy, 100.0, 9.0, 4);

        let event = MatchEvent::for_cancelled_volume(7, 2, &taker, 3.5);

        assert_eq!(MatchEventKind::Cancelled, event.kind);
        assert_eq!(
            EventId {
                log_index: 7,
                ordinal: 2
            },
            event.event_id
        );
        assert_eq!(3.5, event.volume);
        assert_eq!(4, event.order_sequence);
    }

    #[test]
    fn file_sink_omits_absent_reasons_and_writes_present_ones() {
        let dir = TempDir::new();
        let path = dir.path().join("events.jsonl");
        let sink = FileLogSink::open(&path).unwrap();
        let order = order(OrderSide::Buy, 100.0, 1.0, 0);

        sink.publish(&[MatchEvent::for_cancel(1, &order)]).unwrap();
        sink.publish(&[MatchEvent::for_rejected(
            2,
            0,
            &order,
            RejectReason::PostOnlyWouldCross,
        )])
        .unwrap();

        let lines = read_lines(&path);
        assert!(
            lines[0].get("reason").is_none(),
            "a plain cancel must not grow a reason field"
        );
        assert_eq!(serde_json::json!("rejected"), lines[1]["kind"]);
        assert_eq!(
            serde_json::json!("post_only_would_cross"),
            lines[1]["reason"]
        );
    }
}

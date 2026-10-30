//! Market-data feed load test.
//!
//! Run with `cargo bench --bench feed_load`. It drives the real bounded tokio
//! queue and processor with synthetic fills at 50k and 500k fills/s, then
//! reports offered/processed fills, peak queue depth, drops and drain time.
//!
//! The sink is the null sink, so this measures the tap + processor pipeline
//! (shadow book, batching, encoding) without JSONL or network I/O.

use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use matchengine_raft::market_data::MarketDataConfig;
use matchengine_raft::market_data::MarketDataHandle;
use matchengine_raft::market_data::SnapshotFrame;
use matchengine_raft::market_data::TapFrame;
use matchengine_raft::match_result::MatchEvent;
use matchengine_raft::order_book::DepthChange;
use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::MatchResult;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;

const DURATION: Duration = Duration::from_secs(1);
const FILLS_PER_FRAME: u64 = 10;
const QUEUE_CAPACITY: usize = 8_192;
const OVERLOAD_FILLS_PER_SECOND: u64 = 2_000_000;
const OVERLOAD_QUEUE_CAPACITY: usize = 128;

struct Report {
    target: u64,
    offered_frames: u64,
    offered_fills: u64,
    processed_frames: u64,
    processed_fills: u64,
    dropped_frames: u64,
    gaps: u64,
    peak_queue: u64,
    queue_capacity: u64,
    publish_elapsed: Duration,
    drain_elapsed: Duration,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

fn frame(log_index: u64, market_seq: u64, fills: u64) -> TapFrame {
    let taker = Order::new(OrderSide::Buy, 100.0, Fixed::from(fills as i64), market_seq);
    let maker = Order::new(OrderSide::Sell, 100.0, 1.0, 0);
    let matches: Vec<MatchResult> = (0..fills)
        .map(|_| MatchResult::new(&taker, &maker, Fixed::from(1)))
        .collect();
    let events = MatchEvent::for_fills(log_index, 0, OrderSide::Buy, market_seq, &matches);
    TapFrame {
        log_index,
        market_seq,
        ts_ms: now_ms(),
        symbol_id: 0,
        events,
        depth: vec![DepthChange {
            side: OrderSide::Buy,
            price: Fixed::from(100),
            delta_volume: Fixed::from(-(fills as i64)),
            delta_orders: 0,
        }],
    }
}

async fn wait_for_snapshot(handle: &MarketDataHandle) -> anyhow::Result<()> {
    let stats = handle.tap().stats().clone();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if stats.snapshots.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("market-data snapshot was not applied");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn run_load(target_fills_per_second: u64, queue_capacity: usize) -> anyhow::Result<Report> {
    let config = MarketDataConfig {
        queue_capacity,
        leader_only: false,
        status_every: 0,
        bar_intervals_ms: Vec::new(),
        ..MarketDataConfig::default()
    };
    let mut handle = MarketDataHandle::start(config, 1).await?;
    handle.push_snapshot(SnapshotFrame {
        log_index: 0,
        market_seq: 0,
        ts_ms: now_ms(),
        symbol_id: 0,
        bids: Vec::new(),
        asks: Vec::new(),
        last_trade: None,
        book_sequence: 0,
        pending_stops: 0,
    });
    wait_for_snapshot(&handle).await?;

    let tap = handle.tap();
    let stats = tap.stats().clone();
    let frames_per_second = target_fills_per_second as f64 / FILLS_PER_FRAME as f64;
    let tick = Duration::from_millis(10);
    let started = Instant::now();
    let mut next_tick = started + tick;
    let mut published = 0u64;

    while started.elapsed() < DURATION {
        let due = (started.elapsed().as_secs_f64() * frames_per_second).floor() as u64;
        while published < due {
            let sequence = published + 1;
            tap.publish(frame(sequence, sequence, FILLS_PER_FRAME));
            published += 1;
        }
        tokio::time::sleep(next_tick.saturating_duration_since(Instant::now())).await;
        next_tick += tick;
    }
    let publish_elapsed = started.elapsed();

    // Let the processor drain the bounded queue so the drop and peak-depth
    // numbers describe the whole offered load, not just the publishing window.
    let drain_started = Instant::now();
    let deadline = drain_started + Duration::from_secs(30);
    while stats
        .queued_frames
        .load(std::sync::atomic::Ordering::Relaxed)
        > 0
    {
        if Instant::now() > deadline {
            anyhow::bail!("feed queue did not drain within 30 seconds");
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let drain_elapsed = drain_started.elapsed();

    let report = Report {
        target: target_fills_per_second,
        offered_frames: published,
        offered_fills: published * FILLS_PER_FRAME,
        processed_frames: stats
            .trade_messages
            .load(std::sync::atomic::Ordering::Relaxed),
        processed_fills: stats.trades.load(std::sync::atomic::Ordering::Relaxed),
        dropped_frames: stats
            .dropped_frames
            .load(std::sync::atomic::Ordering::Relaxed),
        gaps: stats.gaps.load(std::sync::atomic::Ordering::Relaxed),
        peak_queue: stats.queued_peak.load(std::sync::atomic::Ordering::Relaxed),
        queue_capacity: stats
            .queue_capacity
            .load(std::sync::atomic::Ordering::Relaxed),
        publish_elapsed,
        drain_elapsed,
    };

    handle.shutdown().await;
    Ok(report)
}

fn print_report(report: &Report) {
    let offered_rate = report.offered_fills as f64 / report.publish_elapsed.as_secs_f64();
    let processed_rate = report.processed_fills as f64
        / (report.publish_elapsed + report.drain_elapsed).as_secs_f64();
    println!(
        "target {:>6} fills/s: offered {:>10.0} fills/s, processed {:>10.0} fills/s",
        report.target, offered_rate, processed_rate
    );
    println!(
        "  frames: offered={} processed={} dropped={} gaps={} peak_queue={}/{}",
        report.offered_frames,
        report.processed_frames,
        report.dropped_frames,
        report.gaps,
        report.peak_queue,
        report.queue_capacity
    );
    println!(
        "  publish={:?} drain={:?} processed_fills={}",
        report.publish_elapsed, report.drain_elapsed, report.processed_fills
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!(
        "market-data feed load test: {} fills/frame, null sink",
        FILLS_PER_FRAME
    );
    for (target, queue_capacity) in [
        (50_000, QUEUE_CAPACITY),
        (500_000, QUEUE_CAPACITY),
        (OVERLOAD_FILLS_PER_SECOND, OVERLOAD_QUEUE_CAPACITY),
    ] {
        println!("queue capacity: {queue_capacity}");
        let report = run_load(target, queue_capacity).await?;
        print_report(&report);
    }
    Ok(())
}

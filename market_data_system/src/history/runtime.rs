//! The single historical writer task and the REST query handle.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio::time::MissedTickBehavior;

use matchengine_raft::market_data::BarRecord;

use crate::config::HistoryConfig;
use crate::config::SymbolSpec;
use crate::feed::Registry;
use crate::feed::SharedState;
use crate::feed::Stats;
use crate::protocol;

use super::BarRow;
use super::HistoryEvent;
use super::builder::Builder;
use super::tdengine::HistoryStore;

pub struct HistoryRuntime {
    pub tx: mpsc::Sender<HistoryEvent>,
    pub store: Arc<HistoryStore>,
    handle: JoinHandle<()>,
}

impl HistoryRuntime {
    pub async fn shutdown(self) {
        // Dropping the sender closes the event channel; the task flushes its
        // pending rows before it exits.
        drop(self.tx);
        let _ = self.handle.await;
    }
}

/// Connects, verifies the schema and starts the writer task.
pub async fn start(
    config: HistoryConfig,
    symbols: &[SymbolSpec],
    shared: HashMap<String, Arc<Mutex<SharedState>>>,
    registry: Arc<Mutex<Registry>>,
    stats: Arc<Stats>,
    mut stop: watch::Receiver<bool>,
) -> io::Result<HistoryRuntime> {
    let store = Arc::new(HistoryStore::new(&config)?);
    store.ping().await?;
    store.ensure_schema(&config, symbols).await?;

    let (tx, mut rx) = mpsc::channel::<HistoryEvent>(8_192);
    let batch_rows = config.batch_rows;
    let mut builders: HashMap<String, Builder> = HashMap::new();
    let specs: HashMap<String, SymbolSpec> = symbols
        .iter()
        .map(|spec| (spec.symbol.clone(), spec.clone()))
        .collect();
    for spec in symbols {
        builders.insert(
            spec.symbol.clone(),
            Builder::new(
                spec.symbol.clone(),
                u64::from(spec.symbol_id),
                config.seal_lag_ms,
            ),
        );
    }

    let task_store = store.clone();
    let handle = tokio::spawn(async move {
        let mut pending: HashMap<(String, u64), Vec<BarRow>> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = stop.changed() => {
                    if *stop.borrow() { break; }
                }
                event = rx.recv() => {
                    let Some(event) = event else { break };
                    match event {
                        HistoryEvent::Trade(trade) => {
                            let symbol = trade.symbol.clone();
                            let Some(builder) = builders.get_mut(&symbol) else { continue };
                            for (interval_ms, row) in builder.on_trade(trade) {
                                pending
                                    .entry((symbol.clone(), interval_ms))
                                    .or_default()
                                    .push(row);
                            }
                        }
                    }
                    let full: Vec<_> = pending
                        .iter()
                        .filter(|(_, rows)| rows.len() >= batch_rows)
                        .map(|(key, _)| key.clone())
                        .collect();
                    for key in full {
                        flush_one(&task_store, &key, &mut pending, &shared, &registry, &stats, &specs)
                            .await;
                    }
                }
                _ = tick.tick() => {
                    let now_us = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_micros() as u64;
                    for (symbol, builder) in builders.iter_mut() {
                        for (interval_ms, row) in builder.seal_due(now_us) {
                            pending
                                .entry((symbol.clone(), interval_ms))
                                .or_default()
                                .push(row);
                        }
                    }
                    let keys: Vec<_> = pending.keys().cloned().collect();
                    for key in keys {
                        flush_one(&task_store, &key, &mut pending, &shared, &registry, &stats, &specs)
                            .await;
                    }
                }
            }
        }
        let keys: Vec<_> = pending.keys().cloned().collect();
        for key in keys {
            flush_one(
                &task_store,
                &key,
                &mut pending,
                &shared,
                &registry,
                &stats,
                &specs,
            )
            .await;
        }
    });

    Ok(HistoryRuntime { tx, store, handle })
}

#[allow(clippy::too_many_arguments)]
async fn flush_one(
    store: &HistoryStore,
    key: &(String, u64),
    pending: &mut HashMap<(String, u64), Vec<BarRow>>,
    shared: &HashMap<String, Arc<Mutex<SharedState>>>,
    registry: &Arc<Mutex<Registry>>,
    stats: &Arc<Stats>,
    specs: &HashMap<String, SymbolSpec>,
) {
    let Some(rows) = pending.remove(key) else {
        return;
    };
    let (symbol, &interval_ms) = (key.0.as_str(), &key.1);
    if let Err(error) = store.upsert(interval_ms, symbol, &rows).await {
        tracing::error!(%error, %symbol, interval_ms, "historical bar upsert failed");
        // Keep the rows for the next tick: upserts are idempotent by key.
        let entry = pending.entry(key.clone()).or_default();
        if entry.len() < 10_000 {
            entry.extend(rows);
        }
        return;
    }
    let (Some(state), Some(spec)) = (shared.get(symbol), specs.get(symbol)) else {
        return;
    };
    let stream_key = format!("{symbol}@kline_{}", protocol::kline_label(interval_ms));
    for row in rows {
        let record = bar_record(interval_ms, &row);
        let last_ts_ms = record.last_ts_ms;
        {
            let mut guard = state.lock().expect("state lock");
            guard.record_bar(&record, row.sealed);
            guard
                .bar_quote
                .insert((interval_ms, record.window_start_ms), row.quote_volume);
        }
        let payload =
            protocol::kline_payload(spec, &record, row.quote_volume, last_ts_ms, row.sealed);
        if registry
            .lock()
            .expect("registry lock")
            .publish(&stream_key, Bytes::from(payload))
        {
            stats
                .published
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

fn bar_record(interval_ms: u64, row: &BarRow) -> BarRecord {
    BarRecord {
        interval_ms,
        window_start_ms: row.ts_us / 1_000,
        last_ts_ms: row.last_ts_us.unwrap_or(row.ts_us) / 1_000,
        open: row.open,
        high: row.high,
        low: row.low,
        close: row.close,
        volume: row.volume,
        trade_count: row.trades as u32,
        bar_seq: row.seq,
    }
}

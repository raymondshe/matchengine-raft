//! The single historical writer task and the REST query handle.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

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
use super::FIVE_MIN_MS;
use super::HistoryEvent;
use super::MIN_MS;
use super::SEC_MS;
use super::builder::Builder;
use super::lease::LeaseConfig;
use super::lease::LeaseManager;
use super::rest;
use super::rest::RestTail;
use super::rest::RestTailConfig;
use super::spool::TradeSpool;
use super::state;
use super::tdengine::HistoryStore;

pub struct HistoryRuntime {
    pub tx: mpsc::Sender<HistoryEvent>,
    pub store: Arc<HistoryStore>,
    handle: JoinHandle<()>,
    lease: Option<LeaseManager>,
    rest: Option<JoinHandle<()>>,
    rest_stop: Option<watch::Sender<bool>>,
}

impl HistoryRuntime {
    pub async fn shutdown(self) {
        // Dropping the sender closes the event channel; the task flushes its
        // pending rows before it exits. The Lease is released only afterwards,
        // so the final state cursor is still published by the owner.
        drop(self.tx);
        let _ = self.handle.await;
        if let Some(stop) = self.rest_stop {
            let _ = stop.send(true);
        }
        if let Some(rest) = self.rest {
            let _ = rest.await;
        }
        if let Some(lease) = self.lease {
            lease.shutdown().await;
        }
    }
}

/// Connects, verifies the schema, replays this pod's spool from the durable
/// cursor, then starts the live writer task.
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

    let mut spool = TradeSpool::open(&config.spool_dir, config.spool_days)?;
    let (tx, mut rx) = mpsc::channel::<HistoryEvent>(8_192);
    let batch_rows = config.batch_rows;
    let seal_lag_us = config.seal_lag_ms.saturating_mul(1_000);
    let writer_identity = if config.lease_identity.is_empty() {
        std::env::var("POD_NAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| format!("pid-{}", std::process::id()))
    } else {
        config.lease_identity.clone()
    };
    let epoch = format!("{writer_identity}:{}", now_us());

    // One Lease per symbol: a process serving several symbols can lose one
    // without freezing the others. No Lease prefix means the lab's single
    // writer per symbol contract, with no Kubernetes API dependency.
    let lease = match config.lease_name_prefix.as_deref() {
        Some(prefix) if !prefix.is_empty() => {
            let symbol_names: Vec<String> =
                symbols.iter().map(|spec| spec.symbol.clone()).collect();
            Some(
                LeaseManager::acquire_all(
                    &LeaseConfig {
                        name: prefix.to_string(),
                        namespace: config.lease_namespace.clone(),
                        identity: writer_identity.clone(),
                        ttl_secs: config.lease_ttl_secs,
                        renew_ms: config.lease_renew_ms,
                        endpoint: None,
                        token_file: None,
                        ca_file: None,
                    },
                    &symbol_names,
                    stop.clone(),
                )
                .await?,
            )
        }
        _ => None,
    };
    let owns: HashMap<String, Arc<AtomicBool>> = symbols
        .iter()
        .map(|spec| {
            let flag = lease.as_ref().map_or_else(
                || Arc::new(AtomicBool::new(true)),
                |manager| manager.owns(&spec.symbol),
            );
            (spec.symbol.clone(), flag)
        })
        .collect();

    let mut builders: HashMap<String, Builder> = HashMap::new();
    let mut cursors: HashMap<String, (u64, u32)> = HashMap::new();
    let mut revisions: HashMap<String, u64> = HashMap::new();
    let mut first_ts: HashMap<String, u64> = HashMap::new();
    let mut last_ts: HashMap<String, u64> = HashMap::new();
    let mut pending: HashMap<(String, u64), Vec<BarRow>> = HashMap::new();
    let specs: HashMap<String, SymbolSpec> = symbols
        .iter()
        .map(|spec| (spec.symbol.clone(), spec.clone()))
        .collect();

    for spec in symbols {
        let state = state::read_state(&store, &spec.symbol).await?;
        let cursor = state.as_ref().map(|value| (value.last_log, value.last_ord));
        revisions.insert(
            spec.symbol.clone(),
            state.as_ref().map_or(0, |v| v.revision),
        );
        first_ts.insert(
            spec.symbol.clone(),
            state.as_ref().map_or(0, |v| v.first_ts_us),
        );
        last_ts.insert(
            spec.symbol.clone(),
            state.as_ref().map_or(0, |v| v.sealed_through_us),
        );
        cursors.insert(spec.symbol.clone(), cursor.unwrap_or((0, 0)));
        let mut builder = Builder::new(
            spec.symbol.clone(),
            u64::from(spec.symbol_id),
            config.seal_lag_ms,
        );
        for trade in spool.replay(&spec.symbol, cursor)? {
            for (interval_ms, row) in builder.on_trade(trade) {
                pending
                    .entry((spec.symbol.clone(), interval_ms))
                    .or_default()
                    .push(row);
            }
        }
        builders.insert(spec.symbol.clone(), builder);
    }

    let task_store = store.clone();
    let task_epoch = epoch.clone();
    let task_owns = owns.clone();
    let task_stats = stats.clone();
    let handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut stopping = false;
        let mut last_prune_day = now_us() / 86_400_000_000;
        loop {
            tokio::select! {
                _ = stop.changed(), if !stopping => {
                    stopping = *stop.borrow_and_update();
                }
                event = rx.recv() => {
                    let Some(event) = event else { break };
                    match event {
                        HistoryEvent::Trade(trade) => {
                            let symbol = trade.symbol.clone();
                            let position = (trade.log_index, trade.ordinal);
                            let Some(builder) = builders.get_mut(&symbol) else { continue };
                            let previous = cursors.get(&symbol).copied().unwrap_or((0, 0));
                            if position <= previous {
                                continue; // replayed live frame
                            }
                            // A jump in (log index, ordinal) means frames were
                            // dropped before they reached this writer. Record
                            // the range for the REST tail / reconciler.
                            let jumped = previous.0 > 0
                                && (position.0 > previous.0.saturating_add(1)
                                    || (position.0 == previous.0
                                        && position.1 > previous.1.saturating_add(1)));
                            if let Err(error) = spool.append(&trade) {
                                tracing::warn!(%error, %symbol, "trade spool append failed");
                            }
                            cursors.insert(symbol.clone(), position);
                            let ts_us = trade.ts_ms.saturating_mul(1_000);
                            if jumped {
                                let from_us = last_ts.get(&symbol).copied().unwrap_or(0);
                                if ts_us > from_us
                                    && let Err(error) = state::append_gap(
                                        &task_store, &symbol, from_us, ts_us, true,
                                    )
                                    .await
                                {
                                    tracing::warn!(%error, %symbol, "bar gap record failed");
                                }
                                task_stats
                                    .history_gaps_detected
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            last_ts.insert(symbol.clone(), ts_us);
                            let late = builder.last_ts_us() > 0 && ts_us < builder.last_ts_us();
                            first_ts.entry(symbol.clone()).and_modify(|v| {
                                if *v == 0 { *v = ts_us; }
                            });
                            for (interval_ms, row) in builder.on_trade(trade) {
                                let row = if late
                                    && task_owns
                                        .get(&symbol)
                                        .is_some_and(|flag| flag.load(Ordering::Acquire))
                                {
                                    revise_late_row(
                                        &task_store,
                                        &symbol,
                                        interval_ms,
                                        row,
                                        &mut revisions,
                                    )
                                    .await
                                } else {
                                    row
                                };
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
                        if task_owns
                            .get(&key.0)
                            .is_some_and(|flag| flag.load(Ordering::Acquire))
                        {
                            flush_one(&task_store, &key, &mut pending, &shared, &registry, &task_stats, &specs)
                                .await;
                        } else {
                            task_stats
                                .history_lease_skipped_flushes
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                _ = tick.tick(), if !stopping => {
                    let now_us = now_us();
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
                        if task_owns
                            .get(&key.0)
                            .is_some_and(|flag| flag.load(Ordering::Acquire))
                        {
                            flush_one(&task_store, &key, &mut pending, &shared, &registry, &task_stats, &specs)
                                .await;
                        } else {
                            task_stats
                                .history_lease_skipped_flushes
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    if let Err(error) = spool.sync() {
                        tracing::warn!(%error, "trade spool fsync failed");
                    }
                    let day = now_us / 86_400_000_000;
                    if day != last_prune_day {
                        if let Err(error) = spool.prune(now_us / 1_000) {
                            tracing::warn!(%error, "trade spool prune failed");
                        }
                        last_prune_day = day;
                    }
                    for (symbol, builder) in &builders {
                        if !task_owns
                            .get(symbol)
                            .is_some_and(|flag| flag.load(Ordering::Acquire))
                        {
                            continue;
                        }
                        let cursor = cursors.get(symbol).copied().unwrap_or((0, 0));
                        if let Err(error) = state::write_state(
                            &task_store,
                            symbol,
                            builder.last_ts_us().saturating_sub(seal_lag_us),
                            first_ts.get(symbol).copied().unwrap_or(0),
                            revisions.get(symbol).copied().unwrap_or(0),
                            0,
                            cursor.0,
                            cursor.1,
                            &task_epoch,
                        )
                        .await {
                            tracing::warn!(%error, %symbol, "bar_state write failed");
                        }
                    }
                }
            }
        }
        let keys: Vec<_> = pending.keys().cloned().collect();
        for key in keys {
            if task_owns
                .get(&key.0)
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                flush_one(
                    &task_store,
                    &key,
                    &mut pending,
                    &shared,
                    &registry,
                    &task_stats,
                    &specs,
                )
                .await;
            }
        }
        let _ = spool.sync();
        for (symbol, builder) in &builders {
            if !task_owns
                .get(symbol)
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                continue;
            }
            let cursor = cursors.get(symbol).copied().unwrap_or((0, 0));
            let _ = state::write_state(
                &task_store,
                symbol,
                builder.last_ts_us().saturating_sub(seal_lag_us),
                first_ts.get(symbol).copied().unwrap_or(0),
                revisions.get(symbol).copied().unwrap_or(0),
                0,
                cursor.0,
                cursor.1,
                &task_epoch,
            )
            .await;
        }
    });

    let (rest, rest_stop) = if config.rest_tail_enabled {
        let tail = Arc::new(RestTail::new(RestTailConfig::from_config(&config))?);
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = rest::spawn(
            tail,
            store.clone(),
            &config,
            symbols.to_vec(),
            owns,
            stats,
            stop_rx,
        );
        (Some(task), Some(stop_tx))
    } else {
        (None, None)
    };

    Ok(HistoryRuntime {
        tx,
        store,
        handle,
        lease,
        rest,
        rest_stop,
    })
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

/// A late fill into an already sealed window is an audited revision: record the
/// old value first, bump `rev` on the replacement, and advance the publisher's
/// revision counter. Upserts remain idempotent by `(symbol, ts)`.
async fn revise_late_row(
    store: &HistoryStore,
    symbol: &str,
    interval_ms: u64,
    mut row: BarRow,
    revisions: &mut HashMap<String, u64>,
) -> BarRow {
    let label = match interval_ms {
        SEC_MS => "1s",
        MIN_MS => "1m",
        FIVE_MIN_MS => "5m",
        _ => "other",
    };
    let existing = match store
        .bars(interval_ms, symbol, row.ts_us, row.ts_us.saturating_add(1))
        .await
    {
        Ok(rows) => rows.into_iter().next(),
        Err(error) => {
            tracing::warn!(%error, %symbol, label, "revision lookup failed");
            return row;
        }
    };
    let Some(old) = existing else { return row };
    if !old.sealed {
        return row;
    }
    row.rev = old.rev.saturating_add(1);
    let old_json = serde_json::to_string(&old).unwrap_or_default();
    let new_json = serde_json::to_string(&row).unwrap_or_default();
    if let Err(error) = state::append_revision(
        store,
        symbol,
        label,
        row.ts_us,
        row.rev as u32,
        &old_json,
        &new_json,
        "live",
    )
    .await
    {
        tracing::warn!(%error, %symbol, label, "bar_revisions append failed");
        return old; // do not publish a revision without its audit row
    }
    let revision = revisions.entry(symbol.to_string()).or_default();
    *revision = (*revision).max(row.rev as u64);
    row
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
        } else {
            stats
                .history_dropped_rows
                .fetch_add(rows.len() as u64, Ordering::Relaxed);
            tracing::error!(
                %error, %symbol, interval_ms,
                dropped = rows.len(),
                "historical bar requeue limit reached; dropping rows"
            );
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

//! One WebSocket connection: control protocol, subscriptions, snapshots and
//! per-connection backpressure.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use actix_ws::CloseCode;
use actix_ws::CloseReason;
use actix_ws::Message;
use actix_ws::MessageStream;
use actix_ws::Session;
use bytes::Bytes;
use futures_util::StreamExt;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::OrderSide;

use crate::config::Config;
use crate::config::SymbolSpec;
use crate::feed::Registry;
use crate::feed::SharedState;
use crate::feed::Stats;
use crate::feed::SymbolRuntime;
use crate::protocol;
use crate::protocol::ControlMessage;
use crate::protocol::ProtocolError;
use crate::protocol::StreamKind;

/// Messages handed to the writer half of the session.
pub enum Outbound {
    Frame(Bytes),
    Close(u16, String),
}

/// Per-connection context cloned from the server state.
pub struct SessionCtx {
    pub config: Arc<Config>,
    pub symbols: Arc<HashMap<String, Arc<SymbolRuntime>>>,
    pub registry: Arc<Mutex<Registry>>,
    pub stats: Arc<Stats>,
    pub stop: watch::Receiver<bool>,
    pub combined: bool,
    pub initial: Vec<String>,
}

/// Decrements the connection counter when the session ends.
pub struct ConnectionGuard(pub Arc<Stats>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Runs one WebSocket session until the client, the service or a limit closes it.
pub async fn run(
    mut session: Session,
    mut stream: MessageStream,
    ctx: SessionCtx,
    _guard: ConnectionGuard,
) {
    let (out_tx, mut out_rx) = mpsc::channel::<Outbound>(ctx.config.queue.max(1));
    let mut subscriptions: HashMap<String, JoinHandle<()>> = HashMap::new();
    let mut limiter = RateLimiter::new(10);
    let mut close: Option<CloseReason> = None;
    let mut last_pong = Instant::now();
    let mut stop = ctx.stop.clone();

    // Subscribe to streams requested at connect time (raw/combined URL forms).
    for name in &ctx.initial {
        if let Err(error) = subscribe(&ctx, name, &out_tx, &mut subscriptions) {
            close = Some(CloseReason {
                code: CloseCode::Policy,
                description: Some(error.message),
            });
            break;
        }
    }

    let ping_period = ctx.config.ping_interval;
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + ping_period, ping_period);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let rotate_at = tokio::time::Instant::now() + ctx.config.connection_lifetime;
    let pong_timeout = ctx.config.pong_timeout;

    while close.is_none() {
        tokio::select! {
            _ = stop.changed() => {
                close = Some(CloseReason { code: CloseCode::Away, description: Some("service stopping".into()) });
            }
            _ = ping.tick() => {
                if session.ping(b"").await.is_err() {
                    break;
                }
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(last_pong + pong_timeout)) => {
                close = Some(CloseReason { code: CloseCode::Policy, description: Some("pong timeout".into()) });
            }
            _ = tokio::time::sleep_until(rotate_at) => {
                close = Some(CloseReason { code: CloseCode::Normal, description: Some("24h limit".into()) });
            }
            message = stream.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    handle_control(&ctx, &text, &out_tx, &mut subscriptions, &mut limiter);
                }
                Some(Ok(Message::Pong(_))) => last_pong = Instant::now(),
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
            out = out_rx.recv() => match out {
                Some(Outbound::Frame(frame)) => {
                    let text = String::from_utf8(frame.to_vec()).expect("JSON payload is UTF-8");
                    if session.text(text).await.is_err() {
                        break;
                    }
                }
                Some(Outbound::Close(code, reason)) => {
                    close = Some(CloseReason { code: CloseCode::from(code), description: Some(reason) });
                }
                None => break,
            },
        }
    }

    for (_, task) in subscriptions.drain() {
        task.abort();
    }
    let reason = close.unwrap_or(CloseReason {
        code: CloseCode::Normal,
        description: None,
    });
    let _ = session.close(Some(reason)).await;
}

fn handle_control(
    ctx: &SessionCtx,
    text: &str,
    out_tx: &mpsc::Sender<Outbound>,
    subscriptions: &mut HashMap<String, JoinHandle<()>>,
    limiter: &mut RateLimiter,
) {
    if !limiter.allow() {
        send_control(
            out_tx,
            protocol::control_error(
                &ProtocolError {
                    code: 3,
                    message: "too many control messages".to_string(),
                },
                None,
            ),
        );
        return;
    }
    let control = match serde_json::from_str::<ControlMessage>(text) {
        Ok(control) => control,
        Err(_) => {
            send_control(
                out_tx,
                protocol::control_error(
                    &ProtocolError {
                        code: 1,
                        message: "invalid request".to_string(),
                    },
                    None,
                ),
            );
            return;
        }
    };
    match control {
        ControlMessage::Subscribe { params, id } => {
            if subscriptions.len().saturating_add(params.len()) > ctx.config.max_streams {
                send_control(
                    out_tx,
                    protocol::control_error(
                        &ProtocolError {
                            code: 2,
                            message: "subscription limit exceeded".to_string(),
                        },
                        Some(id),
                    ),
                );
                return;
            }
            for name in &params {
                if let Err(error) = subscribe(ctx, name, out_tx, subscriptions) {
                    send_control(out_tx, protocol::control_error(&error, Some(id)));
                    return;
                }
            }
            send_control(out_tx, protocol::control_result(id));
        }
        ControlMessage::Unsubscribe { params, id } => {
            for name in &params {
                let key = protocol::parse_stream(ctx.config.as_ref(), name)
                    .map(|spec| spec.key)
                    .unwrap_or_else(|_| name.clone());
                // Aborting the task drops its `SubscriptionGuard`, which drops
                // the broadcast receiver and then removes the registry entry.
                if let Some(task) = subscriptions.remove(&key) {
                    task.abort();
                }
            }
            send_control(out_tx, protocol::control_result(id));
        }
        ControlMessage::List { id } => {
            let mut keys: Vec<String> = subscriptions.keys().cloned().collect();
            keys.sort();
            send_control(out_tx, protocol::control_list(id, &keys));
        }
    }
}

fn subscribe(
    ctx: &SessionCtx,
    name: &str,
    out_tx: &mpsc::Sender<Outbound>,
    subscriptions: &mut HashMap<String, JoinHandle<()>>,
) -> Result<(), ProtocolError> {
    let spec = protocol::parse_stream(&ctx.config, name)?;
    let runtime = ctx
        .symbols
        .get(&spec.symbol.to_lowercase())
        .ok_or_else(|| ProtocolError {
            code: 0,
            message: "unknown symbol".to_string(),
        })?;
    if subscriptions.contains_key(&spec.key) {
        return Ok(());
    }
    if subscriptions.len() >= ctx.config.max_streams {
        return Err(ProtocolError {
            code: 2,
            message: "subscription limit exceeded".to_string(),
        });
    }

    // Subscribe to the broadcast before reading the cache so a delta published
    // during snapshot construction is queued, not lost. The client's U/u rules
    // drop the duplicate if the snapshot already included it.
    let receiver = ctx
        .registry
        .lock()
        .expect("registry lock")
        .subscribe(&spec.key, ctx.config.broadcast_capacity);
    // Own the receiver from here on, so every early return (including a full
    // session queue) also prunes an entry this subscribe may have created.
    let guard = SubscriptionGuard {
        key: spec.key.clone(),
        registry: ctx.registry.clone(),
        receiver: Some(receiver),
    };

    // A late joiner with no cached/stale depth asks the engine for an immediate
    // snapshot on stream 2010 instead of waiting for the periodic broadcast.
    let needs_snapshot = {
        let state = runtime.shared.lock().expect("state lock");
        match spec.kind {
            StreamKind::DepthDiff(_) | StreamKind::DepthPartial { .. } => !state.ready(),
            StreamKind::BookTicker => state.bbo.is_none(),
            _ => false,
        }
    };
    if needs_snapshot
        && let Some(requester) = &runtime.snapshot_requester
    {
        let depth = ctx.config.snapshot_levels.clamp(1, usize::from(u16::MAX)) as u16;
        if requester.request(depth) {
            ctx.stats
                .snapshot_requests
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    // Push the cached snapshot for stateful streams.
    let snapshot = {
        let state = runtime.shared.lock().expect("state lock");
        snapshot_payload(&runtime.spec, &state, spec.kind, ctx.config.snapshot_levels)
    };
    if let Some(payload) = snapshot
        && out_tx
            .try_send(Outbound::Frame(frame_vec(ctx.combined, &spec.key, payload)))
            .is_err()
    {
        return Err(ProtocolError {
            code: 2,
            message: "connection queue full".to_string(),
        });
    }

    let out = out_tx.clone();
    let latest = spec.is_latest_value();
    let combined = ctx.combined;
    let stats = ctx.stats.clone();
    let task = tokio::spawn(async move {
        forward(guard, out, latest, combined, stats).await;
    });
    subscriptions.insert(spec.key, task);
    Ok(())
}

/// Owns one forward task's broadcast receiver. On exit (including `abort`) it
/// drops the receiver first and only then prunes the registry, so
/// `receiver_count() == 0` is observable and the stream key does not leak.
struct SubscriptionGuard {
    key: String,
    registry: Arc<Mutex<Registry>>,
    receiver: Option<broadcast::Receiver<Bytes>>,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        drop(self.receiver.take());
        self.registry
            .lock()
            .expect("registry lock")
            .remove_if_unused(&self.key);
    }
}

async fn forward(
    mut sub: SubscriptionGuard,
    out: mpsc::Sender<Outbound>,
    latest_value: bool,
    combined: bool,
    stats: Arc<Stats>,
) {
    let key = sub.key.clone();
    let receiver = sub.receiver.as_mut().expect("receiver present");
    loop {
        match receiver.recv().await {
            Ok(mut payload) => {
                if latest_value {
                    while let Ok(newer) = receiver.try_recv() {
                        payload = newer;
                    }
                }
                if out
                    .try_send(Outbound::Frame(frame_bytes(combined, &key, payload)))
                    .is_err()
                {
                    stats.slow_closes.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) if latest_value => continue,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                stats.slow_closes.fetch_add(1, Ordering::Relaxed);
                let _ = out.try_send(Outbound::Close(1013, "slow consumer".to_string()));
                return;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn frame_bytes(combined: bool, key: &str, payload: Bytes) -> Bytes {
    if combined {
        wrap(key, &payload)
    } else {
        payload
    }
}

fn frame_vec(combined: bool, key: &str, payload: Vec<u8>) -> Bytes {
    if combined {
        wrap(key, &payload)
    } else {
        Bytes::from(payload)
    }
}

fn wrap(key: &str, payload: &[u8]) -> Bytes {
    let mut buffer = Vec::with_capacity(payload.len() + key.len() + 24);
    buffer.extend_from_slice(b"{\"stream\":\"");
    buffer.extend_from_slice(key.as_bytes());
    buffer.extend_from_slice(b"\",\"data\":");
    buffer.extend_from_slice(payload);
    buffer.push(b'}');
    Bytes::from(buffer)
}

fn snapshot_payload(
    symbol: &SymbolSpec,
    state: &SharedState,
    kind: StreamKind,
    snapshot_levels: usize,
) -> Option<Vec<u8>> {
    match kind {
        StreamKind::BookTicker => state.bbo.as_ref().map(|bbo| {
            protocol::book_ticker_payload(symbol, bbo, state.bbo_update_id, state.last_ts_ms)
        }),
        StreamKind::DepthDiff(_) => {
            let bids = state.depth.top(OrderSide::Buy, snapshot_levels);
            let asks = state.depth.top(OrderSide::Sell, snapshot_levels);
            Some(protocol::depth_snapshot_payload(
                symbol,
                state.last_update_id,
                &bids,
                &asks,
                state.last_ts_ms,
            ))
        }
        StreamKind::DepthPartial { levels, .. } => {
            let bids = state.depth.top(OrderSide::Buy, levels);
            let asks = state.depth.top(OrderSide::Sell, levels);
            Some(protocol::depth_partial_payload(
                symbol,
                state.last_update_id,
                &bids,
                &asks,
            ))
        }
        StreamKind::Kline { interval_ms } => state.bars.get(&interval_ms).map(|bar| {
            let quote = state
                .bar_quote
                .get(&(bar.interval_ms, bar.window_start_ms))
                .copied()
                .unwrap_or(Fixed::ZERO);
            protocol::kline_payload(symbol, bar, quote, state.last_ts_ms, false)
        }),
        StreamKind::Trade | StreamKind::Sweep | StreamKind::Status | StreamKind::Lifecycle => None,
    }
}

fn send_control(out: &mpsc::Sender<Outbound>, payload: Vec<u8>) {
    let _ = out.try_send(Outbound::Frame(Bytes::from(payload)));
}

struct RateLimiter {
    limit: u32,
    window: Instant,
    used: u32,
}

impl RateLimiter {
    fn new(limit: u32) -> Self {
        Self {
            limit,
            window: Instant::now(),
            used: 0,
        }
    }

    fn allow(&mut self) -> bool {
        if self.window.elapsed() >= Duration::from_secs(1) {
            self.window = Instant::now();
            self.used = 0;
        }
        self.used = self.used.saturating_add(1);
        self.used <= self.limit
    }
}

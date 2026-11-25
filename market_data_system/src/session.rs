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
use crate::config::FeedProfile;
use crate::config::MAX_HISTORY_SNAPSHOT_BARS;
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

/// A stream that has passed every pre-subscription check. Keeping the runtime
/// alongside the parsed spec makes an entire SUBSCRIBE request atomic: no
/// subscription is mutated until every name in the request validates.
struct ValidatedStream {
    spec: protocol::StreamSpec,
    runtime: Arc<SymbolRuntime>,
}

/// Per-connection context cloned from the server state.
pub struct SessionCtx {
    pub config: Arc<Config>,
    pub symbols: Arc<HashMap<String, Arc<SymbolRuntime>>>,
    pub registry: Arc<Mutex<Registry>>,
    pub stats: Arc<Stats>,
    pub stop: watch::Receiver<bool>,
    pub combined: bool,
    pub profile: FeedProfile,
    /// Fast connections are pinned to the symbol in the request path.
    pub restrict_symbol: Option<String>,
    /// Terminal-only budget for conflatable display frames.
    pub frame_budget: Option<Arc<FrameBudget>>,
    pub initial: Vec<String>,
}

/// Fixed-window per-connection budget for terminal display frames.
pub struct FrameBudget {
    limit: u64,
    window: Mutex<(Instant, u64)>,
}

impl FrameBudget {
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            window: Mutex::new((Instant::now(), 0)),
        }
    }

    /// Returns false when this second's budget is exhausted.
    pub fn allow(&self) -> bool {
        if self.limit == 0 {
            return true;
        }
        let mut window = self.window.lock().expect("frame budget lock");
        let now = Instant::now();
        if now.duration_since(window.0) >= Duration::from_secs(1) {
            *window = (now, 0);
        }
        window.1 = window.1.saturating_add(1);
        window.1 <= self.limit
    }
}

/// Decrements the connection counters when the session ends.
pub struct ConnectionGuard {
    stats: Arc<Stats>,
    profile: FeedProfile,
}

impl ConnectionGuard {
    pub fn new(stats: Arc<Stats>, profile: FeedProfile) -> Self {
        Self { stats, profile }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.stats.connections.fetch_sub(1, Ordering::Relaxed);
        match self.profile {
            FeedProfile::Fast => {
                self.stats.fast_connections.fetch_sub(1, Ordering::Relaxed);
            }
            FeedProfile::Terminal => {
                self.stats
                    .terminal_connections
                    .fetch_sub(1, Ordering::Relaxed);
            }
        }
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
    let mut limiter = RateLimiter::new(5);
    let mut close: Option<CloseReason> = None;
    let mut last_pong = Instant::now();
    let mut stop = ctx.stop.clone();

    // Subscribe to streams requested at connect time (raw/combined URL forms).
    // Validate the whole set first, then apply it, so a bad name cannot leave a
    // partially established connection.
    let mut validated: Vec<ValidatedStream> = Vec::with_capacity(ctx.initial.len());
    for name in &ctx.initial {
        match validate_stream(&ctx, name) {
            Ok(stream) => validated.push(stream),
            Err(error) => {
                close = Some(CloseReason {
                    code: CloseCode::Policy,
                    description: Some(error.message),
                });
                break;
            }
        }
    }
    if close.is_none() {
        let mut new_keys = std::collections::BTreeSet::new();
        for stream in &validated {
            if !subscriptions.contains_key(&stream.spec.key) {
                new_keys.insert(stream.spec.key.clone());
            }
        }
        if subscriptions.len() + new_keys.len() > ctx.profile.max_streams(&ctx.config) {
            close = Some(CloseReason {
                code: CloseCode::Policy,
                description: Some("subscription limit exceeded".to_string()),
            });
        } else {
            for stream in &validated {
                subscribe(&ctx, stream, &out_tx, &mut subscriptions);
            }
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
                ctx.stats.close_code_policy.fetch_add(1, Ordering::Relaxed);
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
            // Validate every name (parse, fast-symbol restriction and runtime)
            // before touching any subscription. A mixed-symbol request on a
            // fast connection must not leave earlier streams subscribed.
            let validated: Vec<ValidatedStream> = match params
                .iter()
                .map(|name| validate_stream(ctx, name))
                .collect()
            {
                Ok(streams) => streams,
                Err(error) => {
                    send_control(out_tx, protocol::control_error(&error, Some(&id)));
                    return;
                }
            };
            let mut new_keys = std::collections::BTreeSet::new();
            for stream in &validated {
                if !subscriptions.contains_key(&stream.spec.key) {
                    new_keys.insert(stream.spec.key.clone());
                }
            }
            if subscriptions.len().saturating_add(new_keys.len())
                > ctx.profile.max_streams(&ctx.config)
            {
                send_control(
                    out_tx,
                    protocol::control_error(
                        &ProtocolError {
                            code: 2,
                            message: "subscription limit exceeded".to_string(),
                        },
                        Some(&id),
                    ),
                );
                return;
            }
            for stream in &validated {
                subscribe(ctx, stream, out_tx, subscriptions);
            }
            send_control(out_tx, protocol::control_result(&id));
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
            send_control(out_tx, protocol::control_result(&id));
        }
        ControlMessage::List { id } => {
            let mut keys: Vec<String> = subscriptions.keys().cloned().collect();
            keys.sort();
            send_control(out_tx, protocol::control_list(&id, &keys));
        }
    }
}

/// Parses and validates one stream name without mutating any subscription.
fn validate_stream(ctx: &SessionCtx, name: &str) -> Result<ValidatedStream, ProtocolError> {
    let spec = protocol::parse_stream(&ctx.config, name)?;
    if let Some(restrict) = &ctx.restrict_symbol
        && &spec.symbol != restrict
    {
        return Err(ProtocolError {
            code: 2,
            message: "fast feed is single-symbol; use /stream for multi-symbol subscriptions"
                .to_string(),
        });
    }
    let runtime = ctx
        .symbols
        .get(&spec.symbol.to_lowercase())
        .ok_or_else(|| ProtocolError {
            code: 0,
            message: "unknown symbol".to_string(),
        })?;
    Ok(ValidatedStream {
        spec,
        runtime: runtime.clone(),
    })
}

/// Applies a pre-validated stream. The stream/connection limits are enforced
/// by the caller before any of these mutations happen, so this cannot fail.
fn subscribe(
    ctx: &SessionCtx,
    stream: &ValidatedStream,
    out_tx: &mpsc::Sender<Outbound>,
    subscriptions: &mut HashMap<String, JoinHandle<()>>,
) {
    let spec = &stream.spec;
    let runtime = &stream.runtime;
    if subscriptions.contains_key(&spec.key) {
        return;
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
    if needs_snapshot && let Some(requester) = &runtime.snapshot_requester {
        let depth = ctx.config.snapshot_levels.clamp(1, usize::from(u16::MAX)) as u16;
        if requester.request(depth) {
            ctx.stats.snapshot_requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    // Push the cached snapshot(s) for stateful streams. Kline subscriptions get
    // the recent bar history so a freshly loaded chart is not blank.
    let snapshots = {
        let state = runtime.shared.lock().expect("state lock");
        snapshot_payload(
            &runtime.spec,
            &state,
            spec.kind,
            ctx.config.snapshot_levels,
            ctx.config.history.snapshot_bars,
        )
    };
    for payload in snapshots {
        // Best-effort: a long history never fails the subscription, it just
        // stops seeding once the session queue is full and live frames follow.
        if out_tx
            .try_send(Outbound::Frame(frame_vec(ctx.combined, &spec.key, payload)))
            .is_err()
        {
            break;
        }
    }

    let out = out_tx.clone();
    let latest = spec.is_latest_value();
    let combined = ctx.combined;
    let stats = ctx.stats.clone();
    // Terminal display conflates latest-value streams; the fast profile stays exact.
    let conflation = ctx
        .profile
        .is_terminal()
        .then(|| Duration::from_millis(ctx.config.terminal_conflation_ms))
        .filter(|interval| !interval.is_zero());
    let budget = ctx.frame_budget.clone();
    let queue_capacity = ctx.config.queue.max(1);
    let task = tokio::spawn(async move {
        forward(
            guard,
            out,
            latest,
            combined,
            stats,
            conflation,
            budget,
            queue_capacity,
        )
        .await;
    });
    subscriptions.insert(spec.key.clone(), task);
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

#[allow(clippy::too_many_arguments)]
async fn forward(
    mut sub: SubscriptionGuard,
    out: mpsc::Sender<Outbound>,
    latest_value: bool,
    combined: bool,
    stats: Arc<Stats>,
    conflation: Option<Duration>,
    budget: Option<Arc<FrameBudget>>,
    queue_capacity: usize,
) {
    let key = sub.key.clone();
    let mut last_sent: Option<Instant> = None;
    let receiver = sub.receiver.as_mut().expect("receiver present");
    loop {
        match receiver.recv().await {
            Ok(mut payload) => {
                if latest_value {
                    while let Ok(newer) = receiver.try_recv() {
                        payload = newer;
                    }
                }
                if latest_value && let Some(interval) = conflation {
                    let now = Instant::now();
                    if last_sent.is_some_and(|last| now.duration_since(last) < interval) {
                        stats.terminal_conflated.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    last_sent = Some(now);
                }
                if latest_value
                    && let Some(budget) = &budget
                    && !budget.allow()
                {
                    stats.terminal_throttled.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let framed_at = Instant::now();
                let frame = frame_bytes(combined, &key, payload);
                stats
                    .encode_ns_total
                    .fetch_add(framed_at.elapsed().as_nanos() as u64, Ordering::Relaxed);
                stats.frames_encoded.fetch_add(1, Ordering::Relaxed);
                // `capacity()` is the number of free slots; derive the used
                // depth from the configured capacity so the soak can prove the
                // per-connection queue stays bounded.
                let used = queue_capacity.saturating_sub(out.capacity()) as u64;
                stats.queue_depth.store(used, Ordering::Relaxed);
                stats.queue_peak.fetch_max(used, Ordering::Relaxed);
                if out.try_send(Outbound::Frame(frame)).is_err() {
                    stats.slow_closes.fetch_add(1, Ordering::Relaxed);
                    stats.close_code_1013.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) if latest_value => {
                stats.broadcast_lag.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                stats.broadcast_lag.fetch_add(1, Ordering::Relaxed);
                stats.slow_closes.fetch_add(1, Ordering::Relaxed);
                stats.close_code_1013.fetch_add(1, Ordering::Relaxed);
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

/// Frames to push when a stream is subscribed: at most one for depth/BBO, one
/// per retained bar for klines, none for pure event streams.
fn snapshot_payload(
    symbol: &SymbolSpec,
    state: &SharedState,
    kind: StreamKind,
    snapshot_levels: usize,
    history_snapshot_bars: usize,
) -> Vec<Vec<u8>> {
    match kind {
        StreamKind::BookTicker => state
            .bbo
            .as_ref()
            .map(|bbo| protocol::book_ticker_payload(symbol, bbo, state.bbo_update_id))
            .into_iter()
            .collect(),
        StreamKind::DepthDiff(_) => {
            let bids = state.depth.top(OrderSide::Buy, snapshot_levels);
            let asks = state.depth.top(OrderSide::Sell, snapshot_levels);
            vec![protocol::depth_snapshot_payload(
                symbol,
                state.last_update_id,
                &bids,
                &asks,
                state.last_ts_ms,
            )]
        }
        StreamKind::DepthPartial { levels, .. } => {
            let bids = state.depth.top(OrderSide::Buy, levels);
            let asks = state.depth.top(OrderSide::Sell, levels);
            vec![protocol::depth_partial_payload(
                symbol,
                state.last_update_id,
                &bids,
                &asks,
            )]
        }
        StreamKind::Kline { interval_ms } => state
            .bar_history
            .get(&interval_ms)
            .map(|history| {
                // Replay the most recent configured number of bars. The feed's
                // ring is capped at MAX_HISTORY_SNAPSHOT_BARS; the config can
                // only lower the replay depth.
                let cap = history_snapshot_bars.clamp(1, MAX_HISTORY_SNAPSHOT_BARS);
                let start = history.len().saturating_sub(cap);
                history[start..]
                    .iter()
                    .map(|(bar, closed)| {
                        let quote = state
                            .bar_quote
                            .get(&(bar.interval_ms, bar.window_start_ms))
                            .copied()
                            .unwrap_or(Fixed::ZERO);
                        // A replayed kline is labelled with the bar's own last
                        // trade time, never with the last frame the service saw
                        // (a reset snapshot or heartbeat must not leak in).
                        protocol::kline_payload(symbol, bar, quote, bar.last_ts_ms, *closed)
                    })
                    .collect()
            })
            .unwrap_or_default(),
        StreamKind::Trade | StreamKind::Sweep | StreamKind::Status | StreamKind::Lifecycle => {
            Vec::new()
        }
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

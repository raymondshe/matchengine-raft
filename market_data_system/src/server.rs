//! HTTP/WebSocket server and service lifecycle.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use actix_web::App;
use actix_web::HttpRequest;
use actix_web::HttpResponse;
use actix_web::HttpServer;
use actix_web::dev::ServerHandle;
use actix_web::get;
use actix_web::http::StatusCode;
use actix_web::web;
use actix_web::web::Data;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::config::FeedProfile;
use crate::config::SymbolSpec;
use crate::feed::Feed;
use crate::feed::Registry;
use crate::feed::SharedState;
use crate::feed::Stats;
use crate::feed::SymbolRuntime;
use crate::history::FIVE_MIN_MS;
use crate::history::HistoryRuntime;
use crate::history::MIN_MS;
use crate::history::SEC_MS;
use crate::history::query;
use crate::history::query::KlineWindow;
use crate::history::runtime;
use crate::history::tdengine::HistoryStore;
use crate::protocol;
use crate::session;
use crate::session::ConnectionGuard;
use crate::session::FrameBudget;
use crate::session::SessionCtx;
use crate::source::BoxedSource;
use crate::source::SnapshotRequester;

const TEST_PAGE: &str = include_str!("../web/index.html");
const TEST_JS: &str = include_str!("../web/app.js");
const TEST_CSS: &str = include_str!("../web/app.css");
/// TradingView Lightweight Charts v5.3.0 (Apache-2.0), vendored so the page
/// needs no CDN and keeps working on an isolated demo network. The banner at
/// the top of the file carries the upstream copyright and licence notice.
const LIGHTWEIGHT_CHARTS: &str = include_str!("../web/lightweight-charts.js");
// Lightweight Charts injects one <style> element for its chart layout, so the
// page has to allow inline styles. Scripts stay restricted to this origin.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                   connect-src 'self' ws: wss:; img-src 'self' data:; base-uri 'none'; \
                   frame-ancestors 'none'";

/// Shared server state handed to every Actix worker.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub symbols: Arc<HashMap<String, Arc<SymbolRuntime>>>,
    pub registry: Arc<Mutex<Registry>>,
    pub stats: Arc<Stats>,
    pub stop: watch::Sender<bool>,
    /// Present only with `--history`; serves `GET /klines` from TDengine.
    pub history: Option<Arc<HistoryStore>>,
}

/// A running service. Call [`RunningService::shutdown`] to stop cleanly.
pub struct RunningService {
    /// Bound address (`127.0.0.1:0` is resolved to the actual port).
    pub addr: String,
    server: ServerHandle,
    stop: watch::Sender<bool>,
    feeds: Vec<JoinHandle<()>>,
    server_task: JoinHandle<()>,
    history: Option<HistoryRuntime>,
}

impl RunningService {
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        self.server.stop(true).await;
        for feed in self.feeds {
            let _ = feed.await;
        }
        if let Some(history) = self.history {
            history.shutdown().await;
        }
        let _ = self.server_task.await;
    }
}

/// Starts one feed task per symbol and the shared HTTP/WebSocket listener.
pub async fn start(
    config: Config,
    sources: Vec<(SymbolSpec, BoxedSource)>,
) -> io::Result<RunningService> {
    start_with_snapshot_requesters(config, sources, HashMap::new()).await
}

/// Like [`start`], with optional stream-2010 snapshot requesters per symbol.
///
/// The requester is cloned into each [`SymbolRuntime`] so a WebSocket session
/// that subscribes to depth while the cache is empty/stale can ask the engine
/// for a fresh snapshot.
pub async fn start_with_snapshot_requesters(
    config: Config,
    sources: Vec<(SymbolSpec, BoxedSource)>,
    mut snapshot_requesters: HashMap<String, SnapshotRequester>,
) -> io::Result<RunningService> {
    let provided: HashSet<&str> = sources
        .iter()
        .map(|(spec, _)| spec.symbol.as_str())
        .collect();
    for spec in &config.symbols {
        if !provided.contains(spec.symbol.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no feed source for symbol {}", spec.symbol),
            ));
        }
    }

    let (stop_tx, stop_rx) = watch::channel(false);
    let registry = Arc::new(Mutex::new(Registry::default()));
    let stats = Arc::new(Stats::default());

    // Shared state is created before the writer so the writer can seed the
    // subscribe-time kline cache from TDengine rows.
    let mut shared_by_symbol = HashMap::new();
    for (spec, _) in &sources {
        shared_by_symbol.insert(
            spec.symbol.clone(),
            Arc::new(Mutex::new(SharedState::default())),
        );
    }
    // A read-only history instance queries TDengine for /klines but never
    // starts the writer or taps trades. The per-symbol fast services own the
    // writer so each bar series is materialized exactly once.
    let history = if config.history.enabled && !config.history.read_only {
        Some(
            runtime::start(
                config.history.clone(),
                &config.symbols,
                shared_by_symbol.clone(),
                registry.clone(),
                stats.clone(),
                stop_rx.clone(),
            )
            .await?,
        )
    } else {
        None
    };
    let history_store = if !config.history.enabled {
        None
    } else if let Some(runtime) = &history {
        Some(runtime.store.clone())
    } else {
        Some(Arc::new(HistoryStore::new(&config.history)?))
    };
    let history_tx = history.as_ref().map(|runtime| runtime.tx.clone());

    let mut symbols = HashMap::new();
    let mut feeds = Vec::with_capacity(sources.len());
    for (spec, source) in sources {
        let shared = shared_by_symbol
            .get(&spec.symbol)
            .cloned()
            .unwrap_or_else(|| Arc::new(Mutex::new(SharedState::default())));
        let snapshot_requester = snapshot_requesters.remove(&spec.symbol);
        let feed = Feed::new(
            spec.clone(),
            config.bar_intervals_ms.clone(),
            config.lifecycle,
            config.extensions,
            config.snapshot_levels,
            config.heartbeat_timeout,
            shared.clone(),
            registry.clone(),
            stats.clone(),
            snapshot_requester.clone(),
        )
        .with_history(history_tx.clone());
        feeds.push(tokio::spawn(feed.run(source, stop_rx.clone())));
        symbols.insert(
            spec.symbol.to_lowercase(),
            Arc::new(SymbolRuntime {
                spec,
                shared,
                snapshot_requester,
            }),
        );
    }

    let state = AppState {
        config: Arc::new(config),
        symbols: Arc::new(symbols),
        registry,
        stats,
        stop: stop_tx.clone(),
        history: history_store,
    };
    let bind = state.config.addr.clone();
    let listener = std::net::TcpListener::bind(bind.as_str())?;
    let addr = listener.local_addr()?.to_string();
    let server = HttpServer::new(move || {
        App::new()
            .app_data(Data::new(state.clone()))
            .service(test_page)
            .service(test_js)
            .service(test_css)
            .service(lightweight_charts)
            .service(info)
            .service(health)
            .service(ready)
            .service(metrics)
            .service(klines)
            .service(ws_bare)
            .service(ws_single)
            .service(ws_combined)
    })
    .listen(listener)?
    .run();

    let handle = server.handle();
    let server_task = tokio::spawn(async move {
        if let Err(error) = server.await {
            tracing::error!(%error, "market-data HTTP server stopped");
        }
    });

    Ok(RunningService {
        addr,
        server: handle,
        stop: stop_tx,
        feeds,
        server_task,
        history,
    })
}

fn asset(content_type: &'static str, body: &'static str) -> HttpResponse {
    HttpResponse::Ok()
        .content_type(content_type)
        .insert_header(("Content-Security-Policy", CSP))
        .body(body)
}

#[get("/")]
async fn test_page() -> HttpResponse {
    asset("text/html; charset=utf-8", TEST_PAGE)
}

#[get("/app.js")]
async fn test_js() -> HttpResponse {
    asset("application/javascript; charset=utf-8", TEST_JS)
}

#[get("/app.css")]
async fn test_css() -> HttpResponse {
    asset("text/css; charset=utf-8", TEST_CSS)
}

#[get("/lightweight-charts.js")]
async fn lightweight_charts() -> HttpResponse {
    asset("application/javascript; charset=utf-8", LIGHTWEIGHT_CHARTS)
}

#[get("/info")]
async fn info(state: Data<AppState>) -> HttpResponse {
    let mut streams = vec![
        "bookTicker",
        "trade",
        "depth@100ms",
        "depth@1000ms",
        "depth5@100ms",
        "depth10@100ms",
        "depth20@1000ms",
        "kline_1s",
        "kline_1m",
    ];
    if state.config.extensions {
        streams.extend(["sweep", "status"]);
    }
    if state.config.lifecycle {
        streams.push("lifecycle");
    }
    HttpResponse::Ok().json(json!({
        "symbols": state.config.symbols,
        "bar_intervals_ms": state.config.bar_intervals_ms,
        "extensions": state.config.extensions,
        "lifecycle": state.config.lifecycle,
        "fast_only": state.config.fast_only,
        "profiles": if state.config.fast_only {
            vec!["fast"]
        } else {
            vec!["fast", "terminal"]
        },
        "streams": streams,
    }))
}

#[get("/health")]
async fn health() -> HttpResponse {
    HttpResponse::Ok().json(json!({"status": "ok"}))
}

#[get("/ready")]
async fn ready(state: Data<AppState>) -> HttpResponse {
    let mut symbols = serde_json::Map::new();
    let mut all_ready = true;
    for (name, runtime) in state.symbols.iter() {
        let state = runtime.shared.lock().expect("state lock");
        let ready = state.ready();
        all_ready &= ready;
        symbols.insert(
            name.clone(),
            json!({
                "ready": ready,
                "snapshot_seen": state.snapshot_seen,
                "depth_stale": state.depth_stale,
                "heartbeat_fresh": state
                    .heartbeat_deadline
                    .is_some_and(|deadline| std::time::Instant::now() <= deadline),
                "last_ts_ms": state.last_ts_ms,
                "epoch": state.epoch,
                "leader_id": state.leader_id,
            }),
        );
    }
    let status = if all_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    HttpResponse::build(status).json(json!({"ready": all_ready, "symbols": symbols}))
}

#[get("/metrics")]
async fn metrics(state: Data<AppState>) -> HttpResponse {
    let registry = state.registry.lock().expect("registry lock");
    let mut body = state.stats.metrics_json(&registry);
    let mut symbols = serde_json::Map::new();
    for (name, runtime) in state.symbols.iter() {
        let shared = runtime.shared.lock().expect("state lock");
        symbols.insert(
            name.clone(),
            json!({
                "ready": shared.ready(),
                "snapshot_seen": shared.snapshot_seen,
                "depth_stale": shared.depth_stale,
                "last_ts_ms": shared.last_ts_ms,
                "streams": registry.symbol_streams(name),
                "subscribers": registry.symbol_subscriptions(name),
            }),
        );
    }
    body["symbols"] = Value::Object(symbols);
    HttpResponse::Ok().json(body)
}

#[derive(Debug, Deserialize)]
struct KlinesQuery {
    symbol: String,
    interval: String,
    #[serde(rename = "startTime")]
    start_time: Option<u64>,
    #[serde(rename = "endTime")]
    end_time: Option<u64>,
    limit: Option<usize>,
    #[serde(rename = "timeZone")]
    time_zone: Option<String>,
}

/// Binance-compatible `GET /klines`, served from TDengine rather than from a
/// process-local ring. A reader that joins mid-day therefore sees the morning.
#[get("/klines")]
async fn klines(state: Data<AppState>, params: web::Query<KlinesQuery>) -> HttpResponse {
    let Some(store) = state.history.as_ref() else {
        return HttpResponse::NotFound().json(json!({"error": "historical store disabled"}));
    };
    let Some(spec) = state.config.symbol(&params.symbol) else {
        return HttpResponse::BadRequest().json(json!({"error": -1121, "msg": "invalid symbol"}));
    };
    let Some(interval_ms) = protocol::parse_kline_interval(&params.interval) else {
        return HttpResponse::BadRequest().json(json!({"error": -1120, "msg": "invalid interval"}));
    };
    // Week/month anchors are phase P5; answering them from a fixed-width grid
    // would silently produce wrong windows.
    if interval_ms >= 604_800_000 {
        return HttpResponse::BadRequest().json(json!({
            "error": -1120,
            "msg": "1w/1M are not implemented by the historical store yet"
        }));
    }
    if params
        .time_zone
        .as_deref()
        .is_some_and(|tz| tz != "0" && tz != "UTC" && tz != "00:00")
    {
        return HttpResponse::BadRequest().json(json!({
            "error": -1124,
            "msg": "timeZone anchoring is not implemented by the historical store yet"
        }));
    }
    let limit = params.limit.unwrap_or(500).clamp(1, 1_000);
    // Prefer 5m rows for targets that are whole multiples of 5 minutes, but
    // keep old 1s/1m-only deployments working when bars_5m is still empty.
    let preferred_source = if interval_ms <= SEC_MS {
        SEC_MS
    } else {
        query::rollup_source_interval(interval_ms)
    };
    let source_interval =
        if preferred_source == FIVE_MIN_MS && !store.has_bars(FIVE_MIN_MS, &spec.symbol).await {
            MIN_MS
        } else {
            preferred_source
        };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let sealed_through_ms = now_ms.saturating_sub(state.config.history.seal_lag_ms);
    let align = |value: u64| value / interval_ms * interval_ms;

    let (start_ms, end_ms) = match (params.start_time, params.end_time) {
        (Some(start), Some(end)) => {
            let start = align(start);
            // Bound the scan to the requested page. Without this, a wide
            // startTime/endTime range materializes the whole retained table
            // before `klines` slices it down to `limit` rows.
            let end = end.max(start);
            (
                start,
                end.min(start.saturating_add(interval_ms.saturating_mul(limit as u64 - 1))),
            )
        }
        (Some(start), None) => (
            align(start),
            align(start).saturating_add(interval_ms * (limit as u64 - 1)),
        ),
        (None, Some(end)) => {
            let end = align(end);
            (end.saturating_sub(interval_ms * (limit as u64 - 1)), end)
        }
        (None, None) => match store.last_ts(source_interval, &spec.symbol).await {
            Ok(Some(last_us)) => {
                let end = align(last_us / 1_000);
                (end.saturating_sub(interval_ms * (limit as u64 - 1)), end)
            }
            Ok(None) => (0, 0),
            Err(error) => {
                tracing::error!(%error, "historical last-timestamp query failed");
                return HttpResponse::ServiceUnavailable()
                    .json(json!({"error": "store unavailable"}));
            }
        },
    };

    if start_ms == 0 && end_ms == 0 {
        return HttpResponse::Ok()
            .insert_header(("X-Bar-Schema", "1"))
            .insert_header(("X-Bar-Sealed-Through", sealed_through_ms.to_string()))
            .json(Vec::<Value>::new());
    }
    let rows = {
        let from_us = start_ms.saturating_mul(1_000);
        let to_us = end_ms.saturating_add(interval_ms).saturating_mul(1_000);
        let result = if interval_ms == SEC_MS || interval_ms == MIN_MS || interval_ms == FIVE_MIN_MS
        {
            store.bars(interval_ms, &spec.symbol, from_us, to_us).await
        } else {
            store
                .rollup(interval_ms, source_interval, &spec.symbol, from_us, to_us)
                .await
        };
        match result {
            Ok(rows) => rows,
            Err(error) => {
                tracing::error!(%error, interval_ms, "historical kline query failed");
                return HttpResponse::ServiceUnavailable()
                    .json(json!({"error": "store unavailable"}));
            }
        }
    };
    let initial_close = match store
        .previous_close(
            source_interval,
            &spec.symbol,
            start_ms.saturating_mul(1_000),
        )
        .await
    {
        Ok(close) => close,
        Err(error) => {
            tracing::warn!(%error, "historical previous-close lookup failed");
            None
        }
    };
    let window = KlineWindow {
        start_ms,
        end_ms,
        limit,
        sealed_through_ms,
        initial_close,
    };
    let bars = query::klines(rows, interval_ms, &window);
    HttpResponse::Ok()
        .insert_header(("X-Bar-Schema", "1"))
        .insert_header(("X-Bar-Sealed-Through", sealed_through_ms.to_string()))
        .json(bars)
}

#[get("/ws")]
async fn ws_bare(
    req: HttpRequest,
    body: web::Payload,
    state: Data<AppState>,
) -> Result<HttpResponse, actix_web::Error> {
    if state.config.fast_only {
        return Ok(terminal_disabled());
    }
    connect(req, body, state, false, FeedProfile::Terminal, Vec::new()).await
}

#[get("/ws/{stream}")]
async fn ws_single(
    req: HttpRequest,
    body: web::Payload,
    stream: web::Path<String>,
    state: Data<AppState>,
) -> Result<HttpResponse, actix_web::Error> {
    connect(
        req,
        body,
        state,
        false,
        FeedProfile::Fast,
        vec![stream.into_inner()],
    )
    .await
}

#[derive(Deserialize)]
struct CombinedQuery {
    streams: String,
}

#[get("/stream")]
async fn ws_combined(
    req: HttpRequest,
    body: web::Payload,
    query: web::Query<CombinedQuery>,
    state: Data<AppState>,
) -> Result<HttpResponse, actix_web::Error> {
    if state.config.fast_only {
        return Ok(terminal_disabled());
    }
    let streams: Vec<String> = query
        .streams
        .split('/')
        .filter(|stream| !stream.is_empty())
        .map(str::to_string)
        .collect();
    if streams.is_empty() {
        return Ok(HttpResponse::BadRequest().json(json!({"error": "streams is required"})));
    }
    connect(req, body, state, true, FeedProfile::Terminal, streams).await
}

fn terminal_disabled() -> HttpResponse {
    HttpResponse::NotFound().json(json!({
        "code": "fast_only",
        "error": "terminal WebSocket disabled; use /ws/<symbol>@<stream>",
    }))
}

async fn connect(
    req: HttpRequest,
    body: web::Payload,
    state: Data<AppState>,
    combined: bool,
    profile: FeedProfile,
    initial: Vec<String>,
) -> Result<HttpResponse, actix_web::Error> {
    if state.stats.connections.load(Ordering::Relaxed) >= state.config.max_connections as u64 {
        return Ok(
            HttpResponse::ServiceUnavailable().json(json!({"error": "connection limit reached"}))
        );
    }
    let (response, session, stream) = actix_ws::handle(&req, body)?;
    state.stats.connections.fetch_add(1, Ordering::Relaxed);
    match profile {
        FeedProfile::Fast => state.stats.fast_connections.fetch_add(1, Ordering::Relaxed),
        FeedProfile::Terminal => state
            .stats
            .terminal_connections
            .fetch_add(1, Ordering::Relaxed),
    };
    // A fast connection is pinned to the symbol in its request path.
    let restrict_symbol = (profile == FeedProfile::Fast)
        .then(|| {
            initial
                .first()
                .and_then(|name| crate::protocol::parse_stream(&state.config, name).ok())
                .map(|spec| spec.symbol)
        })
        .flatten();
    let frame_budget = (profile == FeedProfile::Terminal
        && state.config.terminal_max_frames_per_second > 0)
        .then(|| {
            Arc::new(FrameBudget::new(
                state.config.terminal_max_frames_per_second,
            ))
        });
    let ctx = SessionCtx {
        config: state.config.clone(),
        symbols: state.symbols.clone(),
        registry: state.registry.clone(),
        stats: state.stats.clone(),
        stop: state.stop.subscribe(),
        combined,
        profile,
        restrict_symbol,
        frame_budget,
        initial,
    };
    let guard = ConnectionGuard::new(state.stats.clone(), profile);
    // `MessageStream` is not `Send` (it wraps the Actix payload), so the
    // session runs on this Actix worker's current-thread runtime.
    actix_web::rt::spawn(session::run(session, stream, ctx, guard));
    Ok(response)
}

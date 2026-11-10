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
use serde_json::json;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::config::SymbolSpec;
use crate::feed::Feed;
use crate::feed::Registry;
use crate::feed::SharedState;
use crate::feed::Stats;
use crate::feed::SymbolRuntime;
use crate::session;
use crate::session::ConnectionGuard;
use crate::session::SessionCtx;
use crate::source::BoxedSource;
use crate::source::SnapshotRequester;

const TEST_PAGE: &str = include_str!("../web/index.html");
const TEST_JS: &str = include_str!("../web/app.js");
const TEST_CSS: &str = include_str!("../web/app.css");
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
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
}

/// A running service. Call [`RunningService::shutdown`] to stop cleanly.
pub struct RunningService {
    /// Bound address (`127.0.0.1:0` is resolved to the actual port).
    pub addr: String,
    server: ServerHandle,
    stop: watch::Sender<bool>,
    feeds: Vec<JoinHandle<()>>,
    server_task: JoinHandle<()>,
}

impl RunningService {
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        self.server.stop(true).await;
        for feed in self.feeds {
            let _ = feed.await;
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
    let mut symbols = HashMap::new();
    let mut feeds = Vec::with_capacity(sources.len());

    for (spec, source) in sources {
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let snapshot_requester = snapshot_requesters.remove(&spec.symbol);
        let feed = Feed::new(
            spec.clone(),
            config.bar_intervals_ms.clone(),
            config.lifecycle,
            config.snapshot_levels,
            config.heartbeat_timeout,
            shared.clone(),
            registry.clone(),
            stats.clone(),
            snapshot_requester.clone(),
        );
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
            .service(info)
            .service(health)
            .service(ready)
            .service(metrics)
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

#[get("/info")]
async fn info(state: Data<AppState>) -> HttpResponse {
    HttpResponse::Ok().json(json!({
        "symbols": state.config.symbols,
        "bar_intervals_ms": state.config.bar_intervals_ms,
        "lifecycle": state.config.lifecycle,
        "streams": [
            "bookTicker", "trade", "sweep",
            "depth@100ms", "depth@1000ms",
            "depth5@100ms", "depth10@100ms", "depth20@1000ms",
            "kline_1s", "kline_1m", "status", "lifecycle"
        ],
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
    HttpResponse::Ok().json(state.stats.metrics_json(&registry))
}

#[get("/ws")]
async fn ws_bare(
    req: HttpRequest,
    body: web::Payload,
    state: Data<AppState>,
) -> Result<HttpResponse, actix_web::Error> {
    connect(req, body, state, false, Vec::new()).await
}

#[get("/ws/{stream}")]
async fn ws_single(
    req: HttpRequest,
    body: web::Payload,
    stream: web::Path<String>,
    state: Data<AppState>,
) -> Result<HttpResponse, actix_web::Error> {
    connect(req, body, state, false, vec![stream.into_inner()]).await
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
    let streams: Vec<String> = query
        .streams
        .split('/')
        .filter(|stream| !stream.is_empty())
        .map(str::to_string)
        .collect();
    if streams.is_empty() {
        return Ok(HttpResponse::BadRequest().json(json!({"error": "streams is required"})));
    }
    connect(req, body, state, true, streams).await
}

async fn connect(
    req: HttpRequest,
    body: web::Payload,
    state: Data<AppState>,
    combined: bool,
    initial: Vec<String>,
) -> Result<HttpResponse, actix_web::Error> {
    if state.stats.connections.load(Ordering::Relaxed) >= state.config.max_connections as u64 {
        return Ok(
            HttpResponse::ServiceUnavailable().json(json!({"error": "connection limit reached"}))
        );
    }
    let (response, session, stream) = actix_ws::handle(&req, body)?;
    state.stats.connections.fetch_add(1, Ordering::Relaxed);
    let ctx = SessionCtx {
        config: state.config.clone(),
        symbols: state.symbols.clone(),
        registry: state.registry.clone(),
        stats: state.stats.clone(),
        stop: state.stop.subscribe(),
        combined,
        initial,
    };
    let guard = ConnectionGuard(state.stats.clone());
    // `MessageStream` is not `Send` (it wraps the Actix payload), so the
    // session runs on this Actix worker's current-thread runtime.
    actix_web::rt::spawn(session::run(session, stream, ctx, guard));
    Ok(response)
}

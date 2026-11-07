//! Match Engine Raft
//!
//! A practical implementation of a distributed matching engine built upon OpenRaft.
//! This crate provides all the components needed to run a Raft-based distributed
//! system with an order-book matching engine as the state machine.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use actix_web::App;
use actix_web::HttpServer;
use actix_web::dev::ServerHandle;
use actix_web::middleware;
use actix_web::middleware::Logger;
use actix_web::web;
use actix_web::web::Data;
use openraft::BasicNode;
use openraft::Config;
use openraft::SnapshotPolicy;
use openraft::async_runtime::WatchReceiver;

use crate::app::MatchEngineApp;
use crate::market_data::MarketDataControl;
use crate::market_data::MarketDataHandle;
use crate::match_result::FileLogSink;
use crate::match_result::MatchResultSink;
use crate::match_result::NoopSink;
use crate::network::api;
use crate::network::console;
use crate::network::grpc::HTTP2_CONNECTION_WINDOW;
use crate::network::grpc::HTTP2_KEEPALIVE_INTERVAL;
use crate::network::grpc::HTTP2_KEEPALIVE_TIMEOUT;
use crate::network::grpc::HTTP2_STREAM_WINDOW;
use crate::network::grpc::MAX_MESSAGE_SIZE;
use crate::network::grpc::client::GrpcNetworkFactory;
use crate::network::grpc::events::EventStreamService;
use crate::network::grpc::pb::event_service_server::EventServiceServer;
use crate::network::grpc::pb::raft_service_server::RaftServiceServer;
use crate::network::grpc::server::RaftGrpcService;
use crate::network::health;
use crate::network::health::GrpcListenerHealth;
use crate::network::health::GrpcListenerStopGuard;
use crate::network::management;

pub use store::StorageConfig;

/// Application state module holding the Raft node components.
pub mod app;
/// Smart client module for interacting with the Raft cluster.
pub mod client;
/// Market-data egress: the deterministic feed pipeline (tap, shadow book, wire format).
pub mod market_data;
/// Pluggable processing of the match results (fills, rests, cancels) produced by the state machine.
pub mod match_result;
/// Network layer module for HTTP endpoints and Raft RPC.
pub mod network;
/// Order-book matching engine: orders, matching and exact fixed-point values.
pub mod order_book;
/// Storage layer module for Raft logs and state machine persistence.
pub mod store;

/// Node identifier type for the Raft cluster.
///
/// Each node in the cluster must have a unique ID. Typically these are
/// assigned sequentially starting from 1.
pub type MatchEngineNodeId = u64;

// Type configuration for the Raft system.
//
// This macro declares all the types used by the Raft implementation:
// - `D`: The application request type (`MatchEngineRequest`)
// - `R`: The application response type (`MatchEngineResponse`)
openraft::declare_raft_types!(
    /// Declare the type configuration for the match engine store.
    pub MatchEngineTypeConfig:
        D = store::MatchEngineRequest,
        R = store::MatchEngineResponse,
        Node = BasicNode,
);

pub type LogStore = store::LogStore<MatchEngineTypeConfig>;
pub type StateMachineStore = store::StateMachineStore<MatchEngineTypeConfig>;
pub type MatchEngineRaft = openraft::Raft<MatchEngineTypeConfig, StateMachineStore>;

/// Number of workers for OpenRaft's dedicated runtime.
///
/// Unset or `0` returns `None`, which keeps Raft on the shared node runtime.
/// A positive `RAFT_CORE_WORKERS` isolates RaftCore and the state-machine
/// worker on their own multi-threaded runtime.
fn raft_core_workers() -> Option<usize> {
    match std::env::var("RAFT_CORE_WORKERS") {
        Ok(value) => match value.parse::<usize>() {
            Ok(0) => None,
            Ok(workers) => Some(workers),
            Err(_) => {
                tracing::warn!(
                    value,
                    "RAFT_CORE_WORKERS must be a non-negative integer; using the shared runtime"
                );
                None
            }
        },
        Err(_) => None,
    }
}

/// A running node: the Raft instance plus the HTTP and gRPC servers serving it.
///
/// Keep this value alive for as long as the node should run. Call
/// [`MatchEngineNode::shutdown`] to stop it cleanly, which is required before
/// reopening the same [`StorageConfig`] for a restart.
pub struct MatchEngineNode {
    /// The Raft instance, usable for administrative calls and tests.
    pub raft: MatchEngineRaft,
    /// The state machine, exposing application reads.
    pub state_machine: StateMachineStore,
    /// The Raft log, exposing log state and debug helpers.
    pub log_store: LogStore,

    http_addr: String,
    http_handle: ServerHandle,
    http_task: tokio::task::JoinHandle<()>,
    grpc_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    grpc_task: tokio::task::JoinHandle<()>,
    grpc_failure_rx: Option<tokio::sync::oneshot::Receiver<std::io::Error>>,
    market_data: Option<MarketDataHandle>,
    market_data_task: Option<tokio::task::JoinHandle<()>>,
    snapshot_task: Option<tokio::task::JoinHandle<()>>,
    /// When `RAFT_CORE_WORKERS` is positive, OpenRaft's RaftCore and
    /// state-machine worker run on this dedicated runtime, isolated from the
    /// HTTP, gRPC and feed tasks.
    raft_thread: Option<RaftThread>,
}

/// Runs OpenRaft's tasks on a dedicated multi-threaded runtime.
///
/// Only used when `RAFT_CORE_WORKERS` is positive; the caller passes that
/// worker count. OpenRaft spawns RaftCore and the state-machine worker with
/// `tokio::spawn`. Calling `Raft::run` inside this runtime puts both tasks here
/// instead of the shared node runtime, so they no longer compete with HTTP,
/// gRPC and feed tasks. With more than one worker the state-machine worker can
/// still migrate between them.
struct RaftThread {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl RaftThread {
    async fn spawn(raft: MatchEngineRaft, workers: usize) -> std::io::Result<Self> {
        tracing::info!(workers, "starting dedicated Raft runtime");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let join = std::thread::Builder::new()
            .name("raft-core".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(workers)
                    .thread_name("raft-worker")
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = started_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
                runtime.spawn(async move {
                    raft.run();
                    let _ = started_tx.send(Ok(()));
                    let _ = shutdown_rx.await;
                    let _ = stopped_tx.send(());
                });
                // Park this thread until the Raft runtime stops; do not join
                // the runtime's scheduler. Dropping the runtime here (outside
                // async) avoids the "drop runtime in async context" panic.
                let _ = stopped_rx.recv();
            })
            .map_err(|error| {
                std::io::Error::other(format!("failed to spawn raft thread: {error}"))
            })?;

        match started_rx.await {
            Ok(Ok(())) => Ok(Self {
                shutdown: Some(shutdown_tx),
                join: Some(join),
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(std::io::Error::other(error))
            }
            Err(error) => {
                let _ = join.join();
                Err(std::io::Error::other(format!(
                    "raft thread failed to start: {error}"
                )))
            }
        }
    }

    async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = tokio::task::spawn_blocking(move || join.join()).await;
        }
    }
}

impl Drop for RaftThread {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl MatchEngineNode {
    /// Address the HTTP API is bound to.
    pub fn http_addr(&self) -> &str {
        &self.http_addr
    }

    /// Waits until the HTTP server stops, or fails fast if a required component dies.
    ///
    /// A node whose peers cannot reach its gRPC port, or whose Raft core has
    /// stopped (including on a fatal WAL/snapshot error), is not allowed to keep
    /// serving HTTP as if it were healthy: the failure is returned to the caller
    /// after stopping the remaining tasks, so the process can exit non-zero.
    pub async fn run(mut self) -> std::io::Result<()> {
        let grpc_failure = self.grpc_failure_rx.take();
        let raft_failure = wait_for_raft_stop(self.raft.clone());
        let http_result = tokio::select! {
            result = &mut self.http_task => {
                result.map_err(|e| std::io::Error::other(format!("HTTP server task failed: {e}")))
            }
            failure = wait_for_grpc_failure(grpc_failure) => {
                tracing::error!(error = %failure, "gRPC Raft server failed; stopping node");
                self.http_handle.stop(true).await;
                let _ = (&mut self.http_task).await;
                self.stop_background().await?;
                return Err(failure);
            }
            failure = raft_failure => {
                tracing::error!(error = %failure, "Raft core stopped; stopping node");
                self.http_handle.stop(true).await;
                let _ = (&mut self.http_task).await;
                self.stop_background().await?;
                return Err(failure);
            }
        };

        self.stop_background().await?;
        http_result
    }

    /// Stops the HTTP server, the gRPC server and the Raft core.
    ///
    /// After this returns, the storage directories can be reopened by a fresh
    /// [`build_match_engine_node`] call.
    pub async fn shutdown(mut self) -> std::io::Result<()> {
        self.http_handle.stop(true).await;
        let _ = (&mut self.http_task).await;
        self.stop_background().await
    }

    /// Stops the non-HTTP tasks and the Raft core.
    async fn stop_background(mut self) -> std::io::Result<()> {
        if let Some(tx) = self.grpc_shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.grpc_task.await;

        // Stop the feed before Raft: the watcher must not push snapshots for a
        // shutting-down node, and the publisher task joins here.
        if let Some(task) = self.market_data_task.take() {
            task.abort();
        }
        if let Some(mut handle) = self.market_data.take() {
            handle.shutdown().await;
        }

        // Stop the periodic snapshot timer before shutting Raft down.
        if let Some(task) = self.snapshot_task.take() {
            task.abort();
        }

        let raft_result = self
            .raft
            .shutdown()
            .await
            .map_err(|e| std::io::Error::other(format!("Raft shutdown failed: {e}")));
        if let Some(thread) = self.raft_thread.take() {
            thread.shutdown().await;
        }
        raft_result
    }
}

/// Starts a complete Raft node with an HTTP API and a gRPC Raft listener.
pub async fn start_match_engine_node(
    node_id: MatchEngineNodeId,
    http_addr: String,
) -> std::io::Result<()> {
    start_match_engine_node_with_config(node_id, http_addr, default_raft_config()).await
}

/// Snapshot after this many applied log entries even if the timer has not fired.
pub const DEFAULT_SNAPSHOT_LOG_THRESHOLD: u64 = 100_000;

/// Periodic snapshot interval used when no explicit storage setting is supplied.
pub const DEFAULT_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Suppress a periodic heartbeat when a replication/heartbeat response arrived
/// within this window. Must leave room for `heartbeat_interval` plus one tick
/// before `election_timeout_min`.
const DEFAULT_HEARTBEAT_MIN_INTERVAL_MS: u64 = 500;

/// A follower this far behind is caught up with a snapshot instead of log
/// entries. Kept above the snapshot policy so a snapshot is newer than the
/// retained log window (openraft's documented requirement).
const DEFAULT_REPLICATION_LAG_THRESHOLD: u64 = DEFAULT_SNAPSHOT_LOG_THRESHOLD * 3 / 2;

/// The Raft configuration used by the binary and the integration tests.
pub fn default_raft_config() -> Config {
    Config {
        heartbeat_interval: 500,
        heartbeat_min_interval: Some(DEFAULT_HEARTBEAT_MIN_INTERVAL_MS),
        election_timeout_min: 1500,
        election_timeout_max: 3000,
        snapshot_policy: SnapshotPolicy::LogsSinceLast(DEFAULT_SNAPSHOT_LOG_THRESHOLD),
        replication_lag_threshold: DEFAULT_REPLICATION_LAG_THRESHOLD,
        // Ask peers before campaigning so a restarted or partitioned node cannot
        // inflate the term and depose a healthy leader. The gRPC transport
        // implements `RaftNetworkV2::pre_vote`; openraft's default grants
        // unconditionally, so setting this without the RPC would be a no-op.
        enable_pre_vote: Some(true),
        ..Default::default()
    }
}

/// Starts a node with an explicit Raft configuration and the default storage location.
///
/// Useful for tests and benchmarks that need a shorter snapshot interval or
/// different election timings. The configuration is validated before use.
pub async fn start_match_engine_node_with_config(
    node_id: MatchEngineNodeId,
    http_addr: String,
    config: Config,
) -> std::io::Result<()> {
    start_match_engine_node_with_storage(node_id, http_addr, config, StorageConfig::default()).await
}

/// Starts a node with an explicit Raft configuration and storage layout.
pub async fn start_match_engine_node_with_storage(
    node_id: MatchEngineNodeId,
    http_addr: String,
    config: Config,
    storage: StorageConfig,
) -> std::io::Result<()> {
    build_match_engine_node(node_id, http_addr, config, storage)
        .await?
        .run()
        .await
}

/// Returns true when the state machine has applied entries not covered by the
/// latest snapshot.
///
/// The periodic timer uses this to avoid rebuilding an identical snapshot on a
/// completely idle node.
fn snapshot_is_due(last_applied: Option<u64>, snapshot: Option<u64>) -> bool {
    last_applied.is_some_and(|applied| applied > snapshot.unwrap_or(0))
}

/// Starts the periodic snapshot timer.
///
/// `interval == Duration::ZERO` disables the timer; the log-count policy still
/// applies. The task checks whether new entries were applied since the last
/// snapshot, so an idle node does not rewrite the same snapshot every tick.
fn spawn_snapshot_timer(
    raft: MatchEngineRaft,
    interval: Duration,
) -> Option<tokio::task::JoinHandle<()>> {
    if interval.is_zero() {
        return None;
    }

    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // `interval` fires its first tick immediately; that is startup, not a
        // 30-minute boundary.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            let metrics = raft.metrics().borrow_watched().clone();
            let last_applied = metrics.last_applied.map(|id| id.index);
            let snapshot = metrics.snapshot.map(|id| id.index);
            if !snapshot_is_due(last_applied, snapshot) {
                continue;
            }

            match raft.trigger().snapshot().await {
                Ok(()) => tracing::info!(
                    last_applied = ?last_applied,
                    snapshot = ?snapshot,
                    "periodic snapshot requested"
                ),
                Err(error) => {
                    tracing::warn!(%error, "periodic snapshot request failed");
                    return;
                }
            }
        }
    }))
}

/// Waits for a fatal gRPC-server signal.
///
/// If the task is dropped or panics, the sender disappears and the receiver
/// resolves with an error instead of leaving the node silently degraded.
async fn wait_for_grpc_failure(
    failure: Option<tokio::sync::oneshot::Receiver<std::io::Error>>,
) -> std::io::Error {
    match failure {
        Some(failure) => match failure.await {
            Ok(error) => error,
            Err(_) => std::io::Error::other("gRPC Raft server task stopped"),
        },
        None => std::future::pending().await,
    }
}

/// Waits for the Raft core to stop.
///
/// openraft publishes `Err` in `running_state` when RaftCore exits, including a
/// fatal WAL or snapshot error, and drops the metrics sender if the task panics.
/// Either means this node can no longer make progress, so `run()` turns it into
/// a process failure instead of serving HTTP as a degraded node.
async fn wait_for_raft_stop(raft: MatchEngineRaft) -> std::io::Error {
    let mut metrics = raft.metrics();
    loop {
        {
            let current = metrics.borrow_watched();
            if let Err(fatal) = &current.running_state {
                return std::io::Error::other(format!("Raft core stopped: {fatal:?}"));
            }
        }
        if metrics.changed().await.is_err() {
            return std::io::Error::other("Raft core stopped: metrics channel closed");
        }
    }
}

/// Warns when the node is bound to a non-loopback address.
///
/// The HTTP API and gRPC Raft listener have no TLS or authentication. The
/// supported deployment contract is loopback or a trusted private network.
fn warn_if_non_loopback_bind(http_addr: &str) {
    if let Ok(addr) = http_addr.parse::<SocketAddr>()
        && !addr.ip().is_loopback()
    {
        tracing::warn!(
            %http_addr,
            "HTTP and gRPC listeners have no TLS or authentication; bind to loopback or a trusted private network"
        );
    }
}

/// Builds a node without waiting for it to stop.
///
/// The returned [`MatchEngineNode`] owns both servers and the Raft instance, so
/// callers (notably restart tests) can stop it with
/// [`MatchEngineNode::shutdown`] and open the same storage again.
pub async fn build_match_engine_node(
    node_id: MatchEngineNodeId,
    http_addr: String,
    config: Config,
    storage: StorageConfig,
) -> std::io::Result<MatchEngineNode> {
    warn_if_non_loopback_bind(&http_addr);
    // The Raft transport lives on a derived port so `BasicNode.addr` keeps one
    // meaning (the public API address). Bind it before Raft starts so a port
    // conflict fails startup instead of surfacing as an unreachable peer.
    let raft_addr = network::raft_addr_from_http(&http_addr)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let grpc_listener = tokio::net::TcpListener::bind(&raft_addr).await?;
    let grpc_health = GrpcListenerHealth::new(raft_addr.to_string());
    tracing::info!(%raft_addr, "Raft gRPC transport listening");

    let config = Arc::new(
        config
            .validate()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?,
    );

    // Open (or create) the durable log; recovery replays segments and discards a torn tail.
    // Recovery can scan many segments, so keep it off the async runtime.
    let log_dir = storage.log_dir(node_id);
    let wal_fsync = storage.wal_fsync;
    let log_store =
        tokio::task::spawn_blocking(move || LogStore::open_with_fsync(log_dir, wal_fsync))
            .await
            .map_err(|error| std::io::Error::other(format!("WAL open task failed: {error}")))??;
    log_store.start_fsync_worker()?;
    // Match results are computed by `apply`; the sink decides what happens with them. The default
    // drops them, exactly as before. Create it with the state machine so its file open also runs
    // on the blocking pool.
    let sink_path = storage.match_event_log.clone();
    // Start the market-data publisher before the state machine so the tap can
    // be installed at open; it is node-local and never affects replication.
    let market_data = match &storage.market_data {
        Some(config) => Some(MarketDataHandle::start(config.clone(), node_id).await?),
        None => None,
    };
    // Load the latest snapshot, if any. openraft replays the retained log tail on top of it.
    // Snapshot decode and state-machine restore can be large, so run them off the async runtime.
    let snapshot_dir = storage.snapshot_dir(node_id);
    let max_events = storage.max_events;
    let snapshot_tap = market_data.as_ref().map(|handle| handle.tap());
    let state_machine_store = tokio::task::spawn_blocking(move || {
        let sink: Arc<dyn MatchResultSink> = match sink_path {
            Some(path) => Arc::new(FileLogSink::open(path)?.with_node_id(node_id)),
            None => Arc::new(NoopSink),
        };
        StateMachineStore::open_with_sinks_and_retention(
            snapshot_dir,
            sink,
            snapshot_tap,
            max_events,
        )
    })
    .await
    .map_err(|error| std::io::Error::other(format!("state-machine open task failed: {error}")))??;
    if let Some(handle) = &market_data {
        let depth_levels = storage
            .market_data
            .as_ref()
            .map_or(10, |config| config.depth_levels);
        state_machine_store.set_market_data_control(handle.control(), depth_levels);
    }

    tracing::info!(
        node_id,
        log_dir = %log_store.dir().display(),
        snapshot_dir = %state_machine_store.dir().display(),
        match_result_sink = state_machine_store.match_result_sink().name(),
        "opened persistent storage"
    );

    // Create the network layer that will connect and communicate the raft instances and
    // will be used in conjunction with the store created above.
    let network = GrpcNetworkFactory::new(node_id);

    // A positive RAFT_CORE_WORKERS isolates OpenRaft on its own runtime so
    // RaftCore and the state-machine worker do not share workers with HTTP,
    // gRPC and the feed. Unset or 0 keeps the shared-runtime behavior.
    let core_workers = raft_core_workers();
    let raft = if core_workers.is_some() {
        openraft::Raft::build(
            node_id,
            config.clone(),
            network,
            log_store.clone(),
            state_machine_store.clone(),
        )
        .await
    } else {
        openraft::Raft::new(
            node_id,
            config.clone(),
            network,
            log_store.clone(),
            state_machine_store.clone(),
        )
        .await
    }
    .map_err(|e| std::io::Error::other(format!("failed to create Raft instance: {e}")))?;
    let raft_thread = match core_workers {
        Some(workers) => Some(RaftThread::spawn(raft.clone(), workers).await?),
        None => None,
    };

    // The WAL commit marker is written without its own fsync. Do not report the
    // node ready for reads until OpenRaft has re-established the cluster commit
    // covering the durable log tail and the state machine has applied it.
    let recovered = Arc::new(AtomicBool::new(false));
    let recovery_raft = raft.clone();
    let recovery_flag = recovered.clone();
    tokio::spawn(async move {
        match recovery_raft.wait_for_recovery(None).await {
            Ok(_) => recovery_flag.store(true, Ordering::Release),
            Err(error) => tracing::debug!(%error, "Raft recovery wait ended"),
        }
    });

    // The log-count policy bounds the WAL; this timer also bounds snapshot age
    // even on a mostly idle or low-volume node.
    let snapshot_task = spawn_snapshot_timer(raft.clone(), storage.snapshot_interval);

    // The feed follows leadership: on every gain it resets consumers with a
    // snapshot, then publishes this node's applied entries.
    let market_data_task = market_data.as_ref().map(|handle| {
        spawn_market_data_watch(
            node_id,
            raft.clone(),
            state_machine_store.clone(),
            handle.control(),
            storage
                .market_data
                .as_ref()
                .map_or(10, |config| config.depth_levels),
            storage
                .market_data
                .as_ref()
                .is_none_or(|config| config.leader_only),
            storage
                .market_data
                .as_ref()
                .map_or(0, |config| config.snapshot_every_ms),
        )
    });

    // Serve the gRPC transport. The listener is already bound, so peers can
    // connect as soon as this node is initialized.
    let grpc_service = RaftGrpcService::new(raft.clone());
    let event_service = EventStreamService::new(raft.clone(), state_machine_store.clone());
    let (grpc_shutdown, grpc_shutdown_rx) = tokio::sync::oneshot::channel();
    let (grpc_failure_tx, grpc_failure_rx) = tokio::sync::oneshot::channel();
    let grpc_health_task = grpc_health.clone();
    let grpc_task = tokio::spawn(async move {
        let _stop_guard = GrpcListenerStopGuard::new(grpc_health_task);
        let server = tonic::transport::Server::builder()
            .initial_stream_window_size(Some(HTTP2_STREAM_WINDOW))
            .initial_connection_window_size(Some(HTTP2_CONNECTION_WINDOW))
            .http2_keepalive_interval(Some(HTTP2_KEEPALIVE_INTERVAL))
            .http2_keepalive_timeout(Some(HTTP2_KEEPALIVE_TIMEOUT))
            .add_service(
                RaftServiceServer::new(grpc_service)
                    .max_decoding_message_size(MAX_MESSAGE_SIZE)
                    .max_encoding_message_size(MAX_MESSAGE_SIZE),
            )
            .add_service(
                EventServiceServer::new(event_service)
                    .max_decoding_message_size(MAX_MESSAGE_SIZE)
                    .max_encoding_message_size(MAX_MESSAGE_SIZE),
            )
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(grpc_listener),
                async {
                    let _ = grpc_shutdown_rx.await;
                },
            );

        let result = server.await;
        if let Err(e) = result {
            tracing::error!(
                error = %e,
                "gRPC Raft server stopped; peers can no longer reach this node"
            );
            let _ = grpc_failure_tx.send(std::io::Error::other(format!(
                "gRPC Raft server failed: {e}"
            )));
        }
    });

    // Create an application that will store all the instances created above, this will
    // be later used on the actix-web services.
    let app_data = Data::new(MatchEngineApp {
        id: node_id,
        addr: http_addr.clone(),
        raft: raft.clone(),
        state_machine_store: state_machine_store.clone(),
        log_store: log_store.clone(),
        grpc_health,
        recovered,
        min_voters: storage.min_voters,
        console: console::ConsoleState::new(),
    });

    // Start the actix-web server.
    let server = HttpServer::new(move || {
        App::new()
            .wrap(Logger::new("%a \"%r\" %s %b %Dms"))
            .wrap(middleware::Compress::default())
            .app_data(app_data.clone())
            // Bound application JSON bodies before extraction can allocate or
            // propose anything. Raft append/snapshot bodies travel over gRPC
            // and are capped in `network::grpc`.
            .app_data(web::JsonConfig::default().limit(api::MAX_REQUEST_BODY_BYTES))
            // operational API
            .service(health::health)
            .service(health::ready)
            .service(console::redirect)
            .service(console::index)
            .service(console::css)
            .service(console::js)
            .service(console::node)
            .service(console::cluster)
            .service(console::actions)
            .service(console::transfer_leader)
            .service(console::elect)
            .service(console::snapshot)
            // admin API
            .service(management::init)
            .service(management::add_learner)
            .service(management::change_membership)
            .service(management::metrics)
            .service(management::get_linearizer)
            // application API
            .service(api::write)
            .service(api::write_batch)
            .service(api::read)
            .service(api::linearizable_read)
            .service(api::follower_read)
            .service(api::events)
            .service(api::events_status)
    })
    .keep_alive(Duration::from_secs(5));

    let server = server.bind(&http_addr)?.run();
    let http_handle = server.handle();

    let http_task = tokio::spawn(async move {
        if let Err(e) = server.await {
            tracing::error!(error = %e, "HTTP server stopped unexpectedly");
        }
    });

    Ok(MatchEngineNode {
        raft,
        state_machine: state_machine_store,
        log_store,
        http_addr,
        http_handle,
        http_task,
        grpc_shutdown: Some(grpc_shutdown),
        grpc_task,
        grpc_failure_rx: Some(grpc_failure_rx),
        market_data,
        market_data_task,
        snapshot_task,
        raft_thread,
    })
}

/// Watches Raft leadership and steers the market-data feed.
///
/// On becoming leader: open the gate and push a fresh snapshot (the reset point
/// for consumers). On losing it: close the gate so a deposed leader stops
/// publishing. The loop also handles a node that starts as leader.
fn spawn_market_data_watch(
    node_id: MatchEngineNodeId,
    raft: MatchEngineRaft,
    state_machine: StateMachineStore,
    control: MarketDataControl,
    depth_levels: usize,
    leader_only: bool,
    snapshot_every_ms: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut metrics = raft.metrics();
        let mut leader = false;
        let mut started = false;
        let period = std::time::Duration::from_millis(snapshot_every_ms.max(1));
        let mut reset = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        reset.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let (is_leader, epoch, leader_id) = {
                let metrics = metrics.borrow_and_update();
                (
                    metrics.current_leader == Some(node_id),
                    metrics.current_term,
                    metrics.current_leader.unwrap_or(0),
                )
            };
            // Update the header generation before a takeover snapshot is
            // queued, so the reset and its following frames carry the new term.
            control.set_epoch(epoch, leader_id);
            if !leader_only {
                // Every replica publishes: a startup reset is all consumers need.
                if !started {
                    started = true;
                    state_machine
                        .start_market_data_feed(&control, depth_levels)
                        .await;
                }
            } else if is_leader != leader {
                leader = is_leader;
                if leader {
                    // Capture and enqueue under the state-machine lock, then open
                    // the gate: the snapshot precedes every delta it covers.
                    state_machine
                        .start_market_data_feed(&control, depth_levels)
                        .await;
                } else {
                    control.set_leader(false);
                }
            }
            let publishing = if leader_only { leader } else { true };
            tokio::select! {
                changed = metrics.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                _ = reset.tick(), if snapshot_every_ms > 0 && publishing => {
                    // A fresh reset point for consumers that joined mid-stream.
                    state_machine
                        .start_market_data_feed(&control, depth_levels)
                        .await;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periodic_snapshot_only_runs_when_new_entries_are_applied() {
        assert!(!snapshot_is_due(None, None));
        assert!(!snapshot_is_due(Some(0), None));
        assert!(!snapshot_is_due(Some(10), Some(10)));
        assert!(snapshot_is_due(Some(11), Some(10)));
        assert!(snapshot_is_due(Some(1), None));
    }

    #[test]
    fn default_config_snapshots_every_100k_log_entries() {
        assert!(matches!(
            default_raft_config().snapshot_policy,
            SnapshotPolicy::LogsSinceLast(DEFAULT_SNAPSHOT_LOG_THRESHOLD)
        ));
    }

    #[test]
    fn default_config_enables_pre_vote() {
        assert_eq!(Some(true), default_raft_config().enable_pre_vote);
    }

    #[test]
    fn default_config_tunes_openraft_for_the_workload() {
        let config = default_raft_config();

        assert_eq!(
            Some(DEFAULT_HEARTBEAT_MIN_INTERVAL_MS),
            config.heartbeat_min_interval
        );
        assert!(
            config.heartbeat_interval + DEFAULT_HEARTBEAT_MIN_INTERVAL_MS
                < config.election_timeout_min
        );
        assert!(config.replication_lag_threshold > DEFAULT_SNAPSHOT_LOG_THRESHOLD);
        config
            .validate()
            .expect("default Raft config must validate");
    }

    #[tokio::test]
    async fn lost_grpc_task_is_reported_as_fatal() {
        let (sender, receiver) = tokio::sync::oneshot::channel::<std::io::Error>();
        drop(sender);
        let error = wait_for_grpc_failure(Some(receiver)).await;
        assert!(
            error.to_string().contains("gRPC Raft server task stopped"),
            "unexpected failure: {error}"
        );
    }
}

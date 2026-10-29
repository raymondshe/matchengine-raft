//! Match Engine Raft
//!
//! A practical implementation of a distributed matching engine built upon OpenRaft.
//! This crate provides all the components needed to run a Raft-based distributed
//! system with an order-book matching engine as the state machine.

use std::net::SocketAddr;
use std::sync::Arc;
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
use openraft::async_runtime::WatchReceiver;

use crate::app::MatchEngineApp;
use crate::market_data::MarketDataControl;
use crate::market_data::MarketDataHandle;
use crate::match_result::FileLogSink;
use crate::match_result::MatchResultSink;
use crate::match_result::NoopSink;
use crate::network::api;
use crate::network::grpc::MAX_MESSAGE_SIZE;
use crate::network::grpc::pb::raft_service_server::RaftServiceServer;
use crate::network::grpc::server::RaftGrpcService;
use crate::network::management;
use crate::network::raft;
use crate::network::transport::TransportNetworkFactory;

pub use network::Transport;
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
    market_data: Option<MarketDataHandle>,
    market_data_task: Option<tokio::task::JoinHandle<()>>,
}

impl MatchEngineNode {
    /// Address the HTTP API is bound to.
    pub fn http_addr(&self) -> &str {
        &self.http_addr
    }

    /// Waits until the HTTP server stops (as the old start functions did).
    pub async fn run(self) -> std::io::Result<()> {
        self.http_task
            .await
            .map_err(|e| std::io::Error::other(format!("HTTP server task failed: {e}")))
    }

    /// Stops the HTTP server, the gRPC server and the Raft core.
    ///
    /// After this returns, the storage directories can be reopened by a fresh
    /// [`build_match_engine_node`] call.
    pub async fn shutdown(mut self) -> std::io::Result<()> {
        self.http_handle.stop(true).await;
        let _ = self.http_task.await;

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

        self.raft
            .shutdown()
            .await
            .map_err(|e| std::io::Error::other(format!("Raft shutdown failed: {e}")))
    }
}

/// Starts a complete Raft node with an HTTP server, using the default transport.
pub async fn start_match_engine_node(
    node_id: MatchEngineNodeId,
    http_addr: String,
) -> std::io::Result<()> {
    start_match_engine_node_with_transport(node_id, http_addr, Transport::default()).await
}

/// Starts a complete Raft node with an HTTP API server and a gRPC Raft server.
///
/// This function sets up and runs a complete Raft node including:
/// 1. Creating the Raft configuration with snapshot policy
/// 2. Opening or creating the persistent storage
/// 3. Restoring any existing state from snapshots and logs
/// 4. Creating the Raft consensus instance
/// 5. Starting the Actix-web HTTP server with all endpoints
/// 6. Starting the tonic gRPC server for peer-to-peer Raft RPCs
///
/// The HTTP server provides:
/// - Raft internal RPC endpoints for cluster communication
/// - Admin endpoints for cluster management
/// - Application endpoints for interacting with the state machine
///
/// The gRPC server listens on [`network::RAFT_PORT_OFFSET`] above `http_addr`
/// and carries the transport selected by `transport`.
///
/// # Arguments
///
/// * `node_id` - The unique ID for this node in the cluster
/// * `http_addr` - The address (host:port) to listen on for HTTP requests
/// * `transport` - Which transport to use for outbound Raft RPCs
///
/// # Returns
///
/// An `io::Result` that resolves when the server shuts down, or an error
/// if the server fails to start or bind to the address.
pub async fn start_match_engine_node_with_transport(
    node_id: MatchEngineNodeId,
    http_addr: String,
    transport: Transport,
) -> std::io::Result<()> {
    start_match_engine_node_with_config(node_id, http_addr, transport, default_raft_config()).await
}

/// The Raft configuration used by the binary and the integration tests.
pub fn default_raft_config() -> Config {
    Config {
        heartbeat_interval: 500,
        election_timeout_min: 1500,
        election_timeout_max: 3000,
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
    transport: Transport,
    config: Config,
) -> std::io::Result<()> {
    start_match_engine_node_with_storage(
        node_id,
        http_addr,
        transport,
        config,
        StorageConfig::default(),
    )
    .await
}

/// Starts a node with an explicit Raft configuration and storage layout.
pub async fn start_match_engine_node_with_storage(
    node_id: MatchEngineNodeId,
    http_addr: String,
    transport: Transport,
    config: Config,
    storage: StorageConfig,
) -> std::io::Result<()> {
    build_match_engine_node(node_id, http_addr, transport, config, storage)
        .await?
        .run()
        .await
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
    transport: Transport,
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
    tracing::info!(%raft_addr, %transport, "Raft gRPC transport listening");

    let config = Arc::new(
        config
            .validate()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?,
    );

    // Open (or create) the durable log; recovery replays segments and discards a torn tail.
    let log_store = LogStore::open(storage.log_dir(node_id))?;
    // Match results are computed by `apply`; the sink decides what happens with them. The default
    // drops them, exactly as before.
    let match_result_sink: Arc<dyn MatchResultSink> = match &storage.match_event_log {
        Some(path) => Arc::new(FileLogSink::open(path)?.with_node_id(node_id)),
        None => Arc::new(NoopSink),
    };
    // Start the market-data publisher before the state machine so the tap can
    // be installed at open; it is node-local and never affects replication.
    let market_data = match &storage.market_data {
        Some(config) => Some(MarketDataHandle::start(config.clone(), node_id).await?),
        None => None,
    };
    // Load the latest snapshot, if any. openraft replays the retained log tail on top of it.
    let state_machine_store = StateMachineStore::open_with_sinks_and_retention(
        storage.snapshot_dir(node_id),
        match_result_sink,
        market_data.as_ref().map(|handle| handle.tap()),
        storage.max_events,
    )?;
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
    let network = TransportNetworkFactory::new(node_id, transport);

    // Create a local raft instance.
    let raft = openraft::Raft::new(
        node_id,
        config.clone(),
        network,
        log_store.clone(),
        state_machine_store.clone(),
    )
    .await
    .map_err(|e| std::io::Error::other(format!("failed to create Raft instance: {e}")))?;

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
        )
    });

    // Serve the gRPC transport. The listener is already bound, so peers can
    // connect as soon as this node is initialized.
    let grpc_service = RaftGrpcService::new(raft.clone());
    let (grpc_shutdown, grpc_shutdown_rx) = tokio::sync::oneshot::channel();
    let grpc_task = tokio::spawn(async move {
        let server = tonic::transport::Server::builder()
            .add_service(
                RaftServiceServer::new(grpc_service)
                    .max_decoding_message_size(MAX_MESSAGE_SIZE)
                    .max_encoding_message_size(MAX_MESSAGE_SIZE),
            )
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(grpc_listener),
                async {
                    let _ = grpc_shutdown_rx.await;
                },
            );

        if let Err(e) = server.await {
            tracing::error!(
                error = %e,
                "gRPC Raft server stopped; peers can no longer reach this node"
            );
        }
    });

    // Create an application that will store all the instances created above, this will
    // be later used on the actix-web services.
    let app_data = Data::new(MatchEngineApp {
        id: node_id,
        addr: http_addr.clone(),
        raft: raft.clone(),
        state_machine_store: state_machine_store.clone(),
    });

    // Start the actix-web server.
    let server = HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .wrap(Logger::new("%a %{User-Agent}i"))
            .wrap(middleware::Compress::default())
            .app_data(app_data.clone())
            // Bound request bodies before JSON extraction can allocate or
            // propose anything. Snapshot and append bodies use explicit caps
            // in `network::raft`.
            .app_data(web::JsonConfig::default().limit(api::MAX_REQUEST_BODY_BYTES))
            // raft internal RPC
            .service(raft::append)
            .service(raft::snapshot)
            .service(raft::vote)
            // admin API
            .service(management::init)
            .service(management::add_learner)
            .service(management::change_membership)
            .service(management::metrics)
            .service(management::get_linearizer)
            // application API
            .service(api::write)
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
        market_data,
        market_data_task,
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
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut metrics = raft.metrics();
        let mut leader = false;
        let mut started = false;
        loop {
            let is_leader = metrics.borrow_and_update().current_leader == Some(node_id);
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
            if metrics.changed().await.is_err() {
                return;
            }
        }
    })
}

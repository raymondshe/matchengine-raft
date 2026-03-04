//! Match Engine Raft
//!
//! A practical implementation of a distributed key-value store and matching engine
//! built upon OpenRaft. This crate provides all the components needed to run a
//! Raft-based distributed system with a matching engine as the state machine.

use std::sync::Arc;
use std::time::Duration;

use actix_web::middleware;
use actix_web::middleware::Logger;
use actix_web::web::Data;
use actix_web::App;
use actix_web::HttpServer;
use openraft::Config;
use openraft::Raft;
use openraft::SnapshotPolicy;


use crate::app::ExampleApp;
use crate::network::api;
use crate::network::management;
use crate::network::raft;
use crate::network::raft_network_impl::ExampleNetwork;
use crate::store::ExampleRequest;
use crate::store::ExampleResponse;
use crate::store::ExampleStore;
use crate::store::Restore;

/// Application state module holding the Raft node components.
pub mod app;
/// Smart client module for interacting with the Raft cluster.
pub mod client;
/// Network layer module for HTTP endpoints and Raft RPC.
pub mod network;
/// Storage layer module for Raft logs and state machine persistence.
pub mod store;
/// Matching engine module with order book implementation.
pub mod matchengine;

/// Node identifier type for the Raft cluster.
///
/// Each node in the cluster must have a unique ID. Typically these are
/// assigned sequentially starting from 1.
pub type ExampleNodeId = u64;

// Type configuration for the Raft system.
//
// This macro declares all the types used by the Raft implementation:
// - `D`: The application request type (`ExampleRequest`)
// - `R`: The application response type (`ExampleResponse`)
// - `NodeId`: The node identifier type (`ExampleNodeId`)
openraft::declare_raft_types!(
    /// Declare the type configuration for example K/V store.
    pub ExampleTypeConfig: D = ExampleRequest, R = ExampleResponse, NodeId = ExampleNodeId
);

/// Type alias for the Raft instance with our concrete types.
pub type ExampleRaft = Raft<ExampleTypeConfig, ExampleNetwork, Arc<ExampleStore>>;

/// Starts a complete Raft node with an HTTP server.
///
/// This function sets up and runs a complete Raft node including:
/// 1. Creating the Raft configuration with snapshot policy
/// 2. Opening or creating the persistent storage
/// 3. Restoring any existing state from snapshots and logs
/// 4. Creating the Raft consensus instance
/// 5. Starting the Actix-web HTTP server with all endpoints
///
/// The HTTP server provides:
/// - Raft internal RPC endpoints for cluster communication
/// - Admin endpoints for cluster management
/// - Application endpoints for interacting with the state machine
///
/// # Arguments
///
/// * `node_id` - The unique ID for this node in the cluster
/// * `http_addr` - The address (host:port) to listen on for HTTP requests
///
/// # Returns
///
/// An `io::Result` that resolves when the server shuts down, or an error
/// if the server fails to start or bind to the address.
pub async fn start_example_raft_node(node_id: ExampleNodeId, http_addr: String) -> std::io::Result<()> {
    // Create a configuration for the raft instance.

    let mut config = Config::default().validate().unwrap();
    config.snapshot_policy = SnapshotPolicy::LogsSinceLast(500);
    config.max_applied_log_to_keep = 20000;
    config.install_snapshot_timeout = 400;

    let config = Arc::new(config);

    // Create a instance of where the Raft data will be stored.
    let es = ExampleStore::open_create(node_id);

    //es.load_latest_snapshot().await.unwrap();

    let mut store = Arc::new(es);

    store.restore().await;

    // Create the network layer that will connect and communicate the raft instances and
    // will be used in conjunction with the store created above.
    let network = ExampleNetwork::new();

    // Create a local raft instance.
    let raft = Raft::new(node_id, config.clone(), network, store.clone());

    // Create an application that will store all the instances created above, this will
    // be later used on the actix-web services.
    let app = Data::new(ExampleApp {
        id: node_id,
        addr: http_addr.clone(),
        raft,
        store,
        config,
    });

    // Start the actix-web server.
    let server = HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .wrap(Logger::new("%a %{User-Agent}i"))
            .wrap(middleware::Compress::default())
            .app_data(app.clone())
            // raft internal RPC
            .service(raft::append)
            .service(raft::snapshot)
            .service(raft::vote)
            // admin API
            .service(management::init)
            .service(management::add_learner)
            .service(management::change_membership)
            .service(management::metrics)
            // application API
            .service(api::write)
            .service(api::read)
            .service(api::consistent_read)
    }).keep_alive(Duration::from_secs(5));

    let x = server.bind(http_addr)?;

    x.run().await
}

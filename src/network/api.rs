use actix_web::post;
use actix_web::web;
use actix_web::web::Data;
use actix_web::Responder;
use openraft::error::CheckIsLeaderError;
use openraft::error::Infallible;
use openraft::raft::ClientWriteRequest;
use openraft::EntryPayload;
use web::Json;

use crate::app::ExampleApp;
use crate::store::ExampleRequest;
use crate::ExampleNodeId;

/**
 * Application API endpoints for interacting with the Raft state machine.
 *
 * These endpoints provide external access to the distributed state machine.
 * Write operations go through the Raft consensus protocol to ensure
 * consistency, while read operations can be either local (fast but possibly
 * stale) or consistent (goes through leader check).
 */

/// Writes data to the Raft state machine.
///
/// This endpoint submits a write request to the Raft cluster. The request
/// will be replicated to a majority of nodes and applied to the state
/// machine before a response is returned. This ensures strong consistency.
///
/// The request can be:
/// - `Set`: Store a key-value pair
/// - `Place`: Add an order to the order book
/// - `Cancel`: Remove an order from the order book
///
/// # Returns
///
/// The result of applying the request, or an error if the node is not
/// the leader or the write fails.
#[post("/write")]
pub async fn write(app: Data<ExampleApp>, req: Json<ExampleRequest>) -> actix_web::Result<impl Responder> {
    let request = ClientWriteRequest::new(EntryPayload::Normal(req.0));
    let response = app.raft.client_write(request).await;
    Ok(Json(response))
}

/// Reads data from the local state machine (non-consistent read).
///
/// This endpoint reads directly from the local node's state machine
/// without checking if the node is still the leader. This is fast but
/// may return stale data if the node has been partitioned or a new
/// leader has been elected.
///
/// Special keys:
/// - `orderbook_orders`: Returns all orders in the order book as JSON
/// - `orderbook_sequence`: Returns the current sequence number
/// - Any other key: Returns the value from the KV store
///
/// # Returns
///
/// The requested value, or an empty string if the key doesn't exist.
#[post("/read")]
pub async fn read(app: Data<ExampleApp>, req: Json<String>) -> actix_web::Result<impl Responder> {
    let state_machine = app.store.state_machine.read().await;
    let key = req.0;
    let value = match key.as_str() {
        "orderbook_orders" => serde_json::to_string(&state_machine.to_content().orders).unwrap_or_default(),
        "orderbook_sequence" => state_machine.orderbook.sequence.to_string(),
        // Backward compatibility
        "orderbook_sequance" => state_machine.orderbook.sequence.to_string(),
        _ => state_machine.data.get(&key).cloned().unwrap_or_default(),
    };
    let res: Result<String, Infallible> = Ok(value);
    Ok(Json(res))
}

/// Performs a consistent read from the state machine.
///
/// This endpoint first verifies that this node is still the leader
/// before returning data. This ensures linearizable consistency but
/// incurs additional latency from the leader check.
///
/// If this node is not the leader, returns an error that can be used
/// by the client to find the current leader and retry.
///
/// # Returns
///
/// The requested value if this node is the leader, otherwise a
/// `CheckIsLeaderError` with information about the current leader.
#[post("/consistent_read")]
pub async fn consistent_read(app: Data<ExampleApp>, req: Json<String>) -> actix_web::Result<impl Responder> {
    let ret = app.raft.is_leader().await;

    match ret {
        Ok(_) => {
            let state_machine = app.store.state_machine.read().await;
            let key = req.0;
            let value = state_machine.data.get(&key).cloned();

            let res: Result<String, CheckIsLeaderError<ExampleNodeId>> = Ok(value.unwrap_or_default());
            Ok(Json(res))
        }
        Err(e) => Ok(Json(Err(e))),
    }
}


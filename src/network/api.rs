use actix_web::Responder;
use actix_web::post;
use actix_web::web;
use actix_web::web::Data;
use openraft::ReadPolicy;
use openraft::async_runtime::WatchReceiver;
use openraft::errors::Infallible;
use openraft::errors::LinearizableReadError;
use openraft::errors::decompose::DecomposeResult;
use openraft::raft::linearizable_read::Linearizer;
use web::Json;

use crate::ExampleTypeConfig;
use crate::app::ExampleApp;
use crate::client::FollowerReadError;
use crate::store::ExampleRequest;

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
    let response = app.raft.client_write(req.0).await.decompose().unwrap();
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
    let inner = app.state_machine_store.inner().lock().await;
    let key = req.0;
    let value = match key.as_str() {
        "orderbook_orders" => {
            let content = inner.to_content();
            serde_json::to_string(&content.orders).unwrap_or_default()
        }
        "orderbook_sequence" => inner.orderbook.sequence.to_string(),
        // Backward compatibility
        "orderbook_sequance" => inner.orderbook.sequence.to_string(),
        _ => inner.state_machine_data.data.get(&key).cloned().unwrap_or_default(),
    };
    let res: Result<String, Infallible> = Ok(value);
    Ok(Json(res))
}

/// Performs a linearizable read from the Raft state machine.
///
/// This endpoint ensures linearizable consistency by:
/// 1. Getting a read linearizer from the Raft system
/// 2. Waiting for the state machine to catch up to the read log index
/// 3. Reading from the local state machine
///
/// This provides strong consistency guarantees but may be slower than
/// a local read.
#[post("/linearizable_read")]
pub async fn linearizable_read(app: Data<ExampleApp>, req: Json<String>) -> actix_web::Result<impl Responder> {
    let ret = app.raft.get_read_linearizer(ReadPolicy::ReadIndex).await.decompose().unwrap();

    match ret {
        Ok(linearizer) => {
            linearizer.await_ready(&app.raft).await.unwrap();

            let inner = app.state_machine_store.inner().lock().await;
            let key = req.0;
            let value = match key.as_str() {
                "orderbook_orders" => {
                    let content = inner.to_content();
                    serde_json::to_string(&content.orders).unwrap_or_default()
                }
                "orderbook_sequence" => inner.orderbook.sequence.to_string(),
                "orderbook_sequance" => inner.orderbook.sequence.to_string(),
                _ => inner.state_machine_data.data.get(&key).cloned().unwrap_or_default(),
            };

            let res: Result<String, LinearizableReadError<ExampleTypeConfig>> = Ok(value);
            Ok(Json(res))
        }
        Err(e) => Ok(Json(Err(e))),
    }
}

/// Perform a linearizable read on a follower by obtaining a linearizer from the leader
///
/// This demonstrates how to distribute read load across followers while maintaining
/// linearizability guarantees:
/// 1. Get current leader from local metrics
/// 2. Fetch linearizer from leader via HTTP
/// 3. Wait for local state machine to catch up to the linearizer's read_log_id
/// 4. Read from local state machine
#[post("/follower_read")]
pub async fn follower_read(app: Data<ExampleApp>, req: Json<String>) -> actix_web::Result<impl Responder> {
    // 1. Get current leader
    let leader_id = match app.raft.current_leader().await {
        Some(id) => id,
        None => {
            return Ok(Json(Err(FollowerReadError {
                message: "No leader available".to_string(),
            })));
        }
    };

    // 2. Get leader's address from membership config
    let metrics = app.raft.metrics().borrow_watched().clone();
    let leader_node = match metrics.membership_config.membership().get_node(&leader_id) {
        Some(node) => node,
        None => {
            return Ok(Json(Err(FollowerReadError {
                message: format!("Leader node {} not found in membership", leader_id),
            })));
        }
    };
    let leader_addr = &leader_node.addr;

    // 3. Get linearizer from leader via HTTP
    let client = reqwest::Client::new();
    let url = format!("http://{}/get_linearizer", leader_addr);

    let response = match client.post(&url).send().await {
        Ok(resp) => resp,
        Err(e) => {
            return Ok(Json(Err(FollowerReadError {
                message: format!("Failed to contact leader: {}", e),
            })));
        }
    };

    let linearizer_data_result: Result<crate::network::management::LinearizerData, LinearizableReadError<ExampleTypeConfig>> =
        match response.json().await {
            Ok(result) => result,
            Err(e) => {
                return Ok(Json(Err(FollowerReadError {
                    message: format!("Failed to parse linearizer data: {}", e),
                })));
            }
        };

    let linearizer_data = match linearizer_data_result {
        Ok(data) => data,
        Err(e) => {
            return Ok(Json(Err(FollowerReadError {
                message: format!("Leader returned error: {:?}", e),
            })));
        }
    };

    // Reconstruct linearizer from the data
    let linearizer = Linearizer::new(
        linearizer_data.node_id,
        linearizer_data.read_log_id,
        linearizer_data.applied,
    );

    // 4. Wait for local state machine to catch up
    if let Err(e) = linearizer.await_ready(&app.raft).await {
        return Ok(Json(Err(FollowerReadError {
            message: format!("Failed to wait for state machine: {:?}", e),
        })));
    }

    // 5. Read from local state machine
    let inner = app.state_machine_store.inner().lock().await;
    let key = req.0;
    let value = match key.as_str() {
        "orderbook_orders" => {
            let content = inner.to_content();
            serde_json::to_string(&content.orders).unwrap_or_default()
        }
        "orderbook_sequence" => inner.orderbook.sequence.to_string(),
        "orderbook_sequance" => inner.orderbook.sequence.to_string(),
        _ => inner.state_machine_data.data.get(&key).cloned().unwrap_or_default(),
    };

    Ok(Json(Ok(value)))
}

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

use crate::MatchEngineTypeConfig;
use crate::app::MatchEngineApp;
use crate::client::FollowerReadError;
use crate::store::MatchEngineRequest;
use crate::store::MatchEngineResponse;
use crate::store::state_machine::StateMachineStoreInner;

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
/// - `Place`: Submit an order (GTC/IOC/FOK/GTD/Day, post-only, iceberg or stop)
/// - `Cancel`: Remove a resting order or cancel a parked stop
/// - `Amend`: Change the price and/or open volume of a resting limit order
/// - `PurgeExpired`: Evict GTD/Day orders whose deadline has passed (the
///   timestamps travel in the request, so the state machine stays deterministic)
///
/// # Returns
///
/// The result of applying the request, or an error if the node is not
/// the leader or the write fails.
#[post("/write")]
pub async fn write(
    app: Data<MatchEngineApp>,
    req: Json<MatchEngineRequest>,
) -> actix_web::Result<impl Responder> {
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
/// Supported keys:
/// - `orderbook_orders`: Returns all orders resting in the book as JSON
/// - `orderbook_stops`: Returns the parked stop orders as JSON
/// - `orderbook_last_trade_price`: Returns the last trade price (`null` before the first trade)
/// - `orderbook_depth`: Aggregated visible depth for the top 10 levels per side;
///   `orderbook_depth:N` selects another level count (capped at 200)
/// - `orderbook_spread`: Returns the best ask minus the best bid (`null` if either side is empty)
/// - `orderbook_sequence`: Returns the current sequence number
/// - `orderbook_sequance`: Accepted as a legacy spelling of the above
///
/// Any other key returns an empty string.
///
/// # Returns
///
/// The requested value, or an empty string if the key is not known.
#[post("/read")]
pub async fn read(
    app: Data<MatchEngineApp>,
    req: Json<String>,
) -> actix_web::Result<impl Responder> {
    let inner = app.state_machine_store.inner().lock().await;
    let value = read_state(&inner, &req.0);
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
pub async fn linearizable_read(
    app: Data<MatchEngineApp>,
    req: Json<String>,
) -> actix_web::Result<impl Responder> {
    let ret = app
        .raft
        .get_read_linearizer(ReadPolicy::ReadIndex)
        .await
        .decompose()
        .unwrap();

    match ret {
        Ok(linearizer) => {
            linearizer.await_ready(&app.raft).await.unwrap();

            let inner = app.state_machine_store.inner().lock().await;
            let value = read_state(&inner, &req.0);

            let res: Result<String, LinearizableReadError<MatchEngineTypeConfig>> = Ok(value);
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
pub async fn follower_read(
    app: Data<MatchEngineApp>,
    req: Json<String>,
) -> actix_web::Result<impl Responder> {
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

    let linearizer_data_result: Result<
        crate::network::management::LinearizerData,
        LinearizableReadError<MatchEngineTypeConfig>,
    > = match response.json().await {
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
    let value = read_state(&inner, &req.0);

    Ok(Json(Ok(value)))
}

/// Resolves a `/read` key against the state machine.
///
/// The state machine holds an order book, so reads are named views of it rather than lookups in a
/// key-value map. Unknown keys return an empty string, which keeps the endpoint total.
fn read_state<C>(inner: &StateMachineStoreInner<C>, key: &str) -> String
where
    C: openraft::RaftTypeConfig<D = MatchEngineRequest, R = MatchEngineResponse>,
{
    match key {
        "orderbook_orders" => {
            let content = inner.to_content();
            serde_json::to_string(&content.orders).unwrap_or_default()
        }
        "orderbook_stops" => serde_json::to_string(&inner.orderbook.stops).unwrap_or_default(),
        "orderbook_depth" => depth_json(inner, 10),
        "orderbook_spread" => serde_json::to_string(&inner.orderbook.spread()).unwrap_or_default(),
        "orderbook_last_trade_price" => {
            serde_json::to_string(&inner.orderbook.last_trade_price).unwrap_or_default()
        }
        "orderbook_sequence" | "orderbook_sequance" => inner.orderbook.sequence.to_string(),
        _ if key.starts_with("orderbook_depth:") => {
            let levels = key
                .trim_start_matches("orderbook_depth:")
                .parse::<usize>()
                .unwrap_or(10)
                .min(200);
            depth_json(inner, levels)
        }
        _ => String::new(),
    }
}

/// Renders aggregated depth for both sides as one JSON object.
fn depth_json<C>(inner: &StateMachineStoreInner<C>, levels: usize) -> String
where
    C: openraft::RaftTypeConfig<D = MatchEngineRequest, R = MatchEngineResponse>,
{
    serde_json::json!({
        "bids": inner.orderbook.depth(crate::matchengine::OrderSide::Buy, levels),
        "asks": inner.orderbook.depth(crate::matchengine::OrderSide::Sell, levels),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MatchEngineTypeConfig;
    use crate::matchengine::Order;
    use crate::matchengine::OrderSide;

    fn book() -> StateMachineStoreInner<MatchEngineTypeConfig> {
        let mut inner = StateMachineStoreInner::<MatchEngineTypeConfig>::default();
        for (price, volume) in [(100.0, 3.0), (100.0, 2.0), (99.0, 4.0)] {
            let mut bid = Order::new(OrderSide::Buy, price, volume, 0);
            inner.orderbook.execute_order(&mut bid);
        }
        let mut ask = Order::new(OrderSide::Sell, 101.0, 5.0, 0);
        inner.orderbook.execute_order(&mut ask);
        inner
    }

    #[test]
    fn read_depth_aggregates_levels_and_honours_the_limit() {
        let inner = book();

        let depth: serde_json::Value =
            serde_json::from_str(&read_state(&inner, "orderbook_depth:1")).unwrap();
        assert_eq!(1, depth["bids"].as_array().unwrap().len());
        assert_eq!(100.0, depth["bids"][0]["price"]);
        assert_eq!(5.0, depth["bids"][0]["volume"]);
        assert_eq!(2, depth["bids"][0]["orders"]);
        assert_eq!(101.0, depth["asks"][0]["price"]);

        let full: serde_json::Value =
            serde_json::from_str(&read_state(&inner, "orderbook_depth")).unwrap();
        assert_eq!(2, full["bids"].as_array().unwrap().len());
    }

    #[test]
    fn read_spread_and_unknown_keys() {
        let inner = book();
        assert_eq!("1.0", read_state(&inner, "orderbook_spread"));
        assert_eq!("", read_state(&inner, "nope"));
    }
}

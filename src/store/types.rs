use std::fmt;

use serde::Deserialize;
use serde::Serialize;

use crate::matchengine::Order;

/// Application requests that can be applied to the Raft state machine.
///
/// These requests are submitted through the `/write` API endpoint and
/// go through the Raft consensus protocol before being applied to the
/// state machine on all nodes.
///
/// # Variants
///
/// * `Place` - Adds an order to the order book (or parks it as a stop)
/// * `Cancel` - Removes an order from the order book
/// * `Amend` - Changes the price and/or open volume of a resting limit order
/// * `PurgeExpired` - Evicts GTD/Day orders whose deadline has passed
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum MatchEngineRequest {
    /// Place a new order in the matching engine.
    Place { order: Order },
    /// Cancel an existing order from the matching engine.
    Cancel { order: Order },
    /// Amend a resting limit order. `order` identifies it exactly like `Cancel`
    /// (side + current price + sequence); `price` and `volume` are the new
    /// values, and `None` leaves a field unchanged.
    Amend {
        order: Order,
        /// New price; omitted/`null` keeps the current one.
        #[serde(default)]
        price: Option<f64>,
        /// New open volume; omitted/`null` keeps the current one.
        #[serde(default)]
        volume: Option<f64>,
    },
    /// Evict every order whose time in force has expired at `now_ms`.
    ///
    /// The timestamps travel in the request (and therefore through the Raft
    /// log), so the state machine never reads a clock and every replica evicts
    /// exactly the same orders. `day_close_ms` supplies the trading-day close:
    /// `Day` orders expire only when it is present and `now_ms >= close`.
    PurgeExpired {
        now_ms: u64,
        day_close_ms: Option<u64>,
    },
}

impl fmt::Display for MatchEngineRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MatchEngineRequest::Place { order } => write!(f, "Place {{ order: {:?} }}", order),
            MatchEngineRequest::Cancel { order } => write!(f, "Cancel {{ order: {:?} }}", order),
            MatchEngineRequest::Amend {
                order,
                price,
                volume,
            } => write!(
                f,
                "Amend {{ order: {:?}, price: {price:?}, volume: {volume:?} }}",
                order
            ),
            MatchEngineRequest::PurgeExpired {
                now_ms,
                day_close_ms,
            } => write!(
                f,
                "PurgeExpired {{ now_ms: {now_ms}, day_close_ms: {day_close_ms:?} }}"
            ),
        }
    }
}

/// Response returned from applying a request to the state machine.
///
/// This is the result of a successful write operation, containing
/// any output from applying the request.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MatchEngineResponse {
    /// The value returned from the state machine application, if any.
    pub value: Option<String>,
}

impl MatchEngineResponse {
    pub fn new(value: impl Into<String>) -> Self {
        MatchEngineResponse {
            value: Some(value.into()),
        }
    }

    pub fn none() -> Self {
        MatchEngineResponse { value: None }
    }
}

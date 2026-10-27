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
/// * `Set` - Stores a key-value pair in the KV store
/// * `Place` - Adds an order to the order book
/// * `Cancel` - Removes an order from the order book
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum MatchEngineRequest {
    /// Store a key-value pair in the distributed KV store.
    Set { key: String, value: String },
    /// Place a new order in the matching engine.
    Place { order: Order },
    /// Cancel an existing order from the matching engine.
    Cancel { order: Order }
}

impl MatchEngineRequest {
    pub fn set(key: impl Into<String>, value: impl Into<String>) -> Self {
        MatchEngineRequest::Set {
            key: key.into(),
            value: value.into(),
        }
    }
}

impl fmt::Display for MatchEngineRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MatchEngineRequest::Set { key, value } => write!(f, "Set {{ key: {}, value: {} }}", key, value),
            MatchEngineRequest::Place { order } => write!(f, "Place {{ order: {:?} }}", order),
            MatchEngineRequest::Cancel { order } => write!(f, "Cancel {{ order: {:?} }}", order),
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

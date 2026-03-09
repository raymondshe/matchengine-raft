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
pub enum ExampleRequest {
    /// Store a key-value pair in the distributed KV store.
    Set { key: String, value: String },
    /// Place a new order in the matching engine.
    Place { order: Order },
    /// Cancel an existing order from the matching engine.
    Cancel { order: Order }
}

impl ExampleRequest {
    pub fn set(key: impl Into<String>, value: impl Into<String>) -> Self {
        ExampleRequest::Set {
            key: key.into(),
            value: value.into(),
        }
    }
}

impl fmt::Display for ExampleRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExampleRequest::Set { key, value } => write!(f, "Set {{ key: {}, value: {} }}", key, value),
            ExampleRequest::Place { order } => write!(f, "Place {{ order: {:?} }}", order),
            ExampleRequest::Cancel { order } => write!(f, "Cancel {{ order: {:?} }}", order),
        }
    }
}

/// Response returned from applying a request to the state machine.
///
/// This is the result of a successful write operation, containing
/// any output from applying the request.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExampleResponse {
    /// The value returned from the state machine application, if any.
    pub value: Option<String>,
}

impl ExampleResponse {
    pub fn new(value: impl Into<String>) -> Self {
        ExampleResponse {
            value: Some(value.into()),
        }
    }

    pub fn none() -> Self {
        ExampleResponse { value: None }
    }
}

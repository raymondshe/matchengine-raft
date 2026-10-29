use std::fmt;

use serde::Deserialize;
use serde::Serialize;

use crate::match_result::MatchEvent;
use crate::order_book::Fixed;
use crate::order_book::Order;
use crate::order_book::SymbolConfig;

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
/// * `PurgeExpired` - Explicitly evicts expired orders (GTD is normally purged automatically)
/// * `ConfigureSymbol` - Sets this process/cluster's symbol and price/quantity precision
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum MatchEngineRequest {
    /// Place a new order in the matching engine.
    Place {
        order: Order,
        /// Optional client-supplied idempotency key. A retry with the same key
        /// returns the original sequence and events instead of placing a second
        /// order. Omit it for the old, non-idempotent behaviour. The HTTP edge
        /// requires 1–128 bytes from `[A-Za-z0-9._:-]`; empty values are
        /// rejected before the request reaches Raft.
        #[serde(default)]
        client_order_id: Option<String>,
    },
    /// Cancel an existing order from the matching engine.
    Cancel { order: Order },
    /// Amend a resting limit order. `order` identifies it exactly like `Cancel`
    /// (side + current price + sequence); `price` and `volume` are the new
    /// values, and `None` leaves a field unchanged.
    Amend {
        order: Order,
        /// New price; omitted/`null` keeps the current one.
        #[serde(default)]
        price: Option<Fixed>,
        /// New open volume; omitted/`null` keeps the current one.
        #[serde(default)]
        volume: Option<Fixed>,
    },
    /// Explicitly evict orders whose time in force has expired at `now_ms`.
    ///
    /// GTD orders are normally evicted automatically by every admissible
    /// `Place`/`Cancel`/`Amend` at its intake timestamp; this request is for
    /// administrative purges and for `Day`, which needs the trading-day close
    /// (`day_close_ms`). The timestamps travel through the log, so the state
    /// machine never reads a clock. Consumes a market sequence; the response is
    /// that sequence.
    PurgeExpired {
        now_ms: u64,
        day_close_ms: Option<u64>,
    },
    /// Add or update the accepted price/quantity precision for one symbol.
    ///
    /// The configuration is replicated and snapshotted. A process/cluster
    /// trades exactly one symbol; multi-symbol means a separate process/cluster
    /// per symbol. The configuration can only change before the first accepted
    /// order and is locked once trading starts.
    ConfigureSymbol { config: SymbolConfig },
}

impl MatchEngineRequest {
    /// Builds a `Place` without an idempotency key.
    pub fn place(order: Order) -> Self {
        Self::Place {
            order,
            client_order_id: None,
        }
    }

    /// Builds a `Place` with a client idempotency key.
    pub fn place_with_client_id(order: Order, client_order_id: impl Into<String>) -> Self {
        Self::Place {
            order,
            client_order_id: Some(client_order_id.into()),
        }
    }
}

impl fmt::Display for MatchEngineRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MatchEngineRequest::Place { order, .. } => write!(f, "Place {{ order: {:?} }}", order),
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
            MatchEngineRequest::ConfigureSymbol { config } => {
                write!(f, "ConfigureSymbol {{ config: {config:?} }}")
            }
        }
    }
}

/// Response returned from applying a request to the state machine.
///
/// This is the result of a successful write operation, containing
/// any output from applying the request.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct MatchEngineResponse {
    /// The value returned from the state machine application, if any.
    pub value: Option<String>,
    /// Explicit sequence assigned to the mutating request.
    #[serde(default)]
    pub sequence: Option<u64>,
    /// Events produced by this entry, capped at [`Self::MAX_EVENTS`]. The full
    /// set is in the replicated event log served by `POST /events`.
    #[serde(default)]
    pub events: Vec<MatchEvent>,
    /// True when `events` holds only the first [`Self::MAX_EVENTS`] records.
    #[serde(default)]
    pub events_truncated: bool,
    /// A non-fatal application error, for example an invalid symbol
    /// configuration. It is omitted from successful responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl MatchEngineResponse {
    /// Maximum number of events embedded in a `/write` response.
    pub const MAX_EVENTS: usize = 512;

    pub fn new(value: impl Into<String>) -> Self {
        MatchEngineResponse {
            value: Some(value.into()),
            ..Self::default()
        }
    }

    /// A mutating response carrying the assigned sequence and its events.
    pub fn applied(sequence: u64, events: &[MatchEvent]) -> Self {
        let truncated = events.len() > Self::MAX_EVENTS;
        Self {
            value: Some(sequence.to_string()),
            sequence: Some(sequence),
            events: events.iter().take(Self::MAX_EVENTS).copied().collect(),
            events_truncated: truncated,
            error: None,
        }
    }

    /// A duplicate submission's response: the original sequence and whatever
    /// remains of its events in the retained window.
    pub fn duplicate(sequence: u64, events: Vec<MatchEvent>, truncated: bool) -> Self {
        Self {
            value: Some(sequence.to_string()),
            sequence: Some(sequence),
            events,
            events_truncated: truncated,
            error: None,
        }
    }

    pub fn none() -> Self {
        Self::default()
    }

    /// A non-fatal application error (the write itself was committed).
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            ..Self::default()
        }
    }
}

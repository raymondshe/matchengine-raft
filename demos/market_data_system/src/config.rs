//! Service configuration shared by the feed, hub and WebSocket sessions.

use std::time::Duration;

use serde::Serialize;

/// Default Binance-style kline intervals to accept.
pub const DEFAULT_BAR_INTERVALS_MS: &[u64] = &[1_000, 60_000];
/// Default depth levels returned by a subscribe-time snapshot.
pub const DEFAULT_SNAPSHOT_LEVELS: usize = 200;

/// One instrument served by this instance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SymbolSpec {
    /// Lowercase stream symbol, e.g. `eurusd`.
    pub symbol: String,
    /// Numeric symbol id expected in that feed.
    pub symbol_id: u32,
    /// Base asset code used in fee fields.
    pub base_asset: String,
    /// Quote asset code used in fee fields.
    pub quote_asset: String,
}

/// Aeron source for one symbol.
#[derive(Clone, Debug)]
pub struct AeronSpec {
    pub channel: String,
    pub stream_base: u16,
    pub dir: Option<String>,
    pub embedded: bool,
}

/// Runtime configuration for one market-data instance.
///
/// An instance serves one or more instruments. Each symbol has its own feed
/// source, shadow state and snapshot cache; the WebSocket hub is shared, so a
/// client can switch symbols over a single connection with
/// `UNSUBSCRIBE`/`SUBSCRIBE`.
#[derive(Clone, Debug)]
pub struct Config {
    /// HTTP/WebSocket listener, e.g. `127.0.0.1:24700`.
    pub addr: String,
    /// Instruments served by this instance.
    pub symbols: Vec<SymbolSpec>,
    /// Kline intervals to accept and to quote-volume-accumulate.
    pub bar_intervals_ms: Vec<u64>,
    /// Maximum concurrent WebSocket connections.
    pub max_connections: usize,
    /// Maximum streams per connection.
    pub max_streams: usize,
    /// Per-connection sequenced FIFO capacity.
    pub queue: usize,
    /// Whether the `@lifecycle` stream is exposed.
    pub lifecycle: bool,
    /// Server ping interval.
    pub ping_interval: Duration,
    /// Disconnect when no pong arrives within this window.
    pub pong_timeout: Duration,
    /// Forced reconnect after this lifetime.
    pub connection_lifetime: Duration,
    /// Depth levels in a subscribe-time snapshot.
    pub snapshot_levels: usize,
    /// Broadcast channel capacity per stream key.
    pub broadcast_capacity: usize,
}

impl Config {
    pub fn new(addr: impl Into<String>, symbols: Vec<SymbolSpec>) -> Self {
        Self {
            addr: addr.into(),
            symbols,
            bar_intervals_ms: DEFAULT_BAR_INTERVALS_MS.to_vec(),
            max_connections: 256,
            // Binance allows up to 1024 streams on one connection.
            max_streams: 1_024,
            queue: 1_024,
            lifecycle: true,
            ping_interval: Duration::from_secs(180),
            pong_timeout: Duration::from_secs(600),
            connection_lifetime: Duration::from_secs(24 * 60 * 60),
            snapshot_levels: DEFAULT_SNAPSHOT_LEVELS,
            broadcast_capacity: 1_024,
        }
    }

    /// Test/CI configuration: one EUR/USD symbol, short timers, small limits.
    pub fn for_test(addr: impl Into<String>) -> Self {
        Self::new(
            addr,
            vec![SymbolSpec {
                symbol: "eurusd".to_string(),
                symbol_id: 0,
                base_asset: "EUR".to_string(),
                quote_asset: "USD".to_string(),
            }],
        )
        .with_test_limits()
    }

    fn with_test_limits(mut self) -> Self {
        self.max_connections = 16;
        self.max_streams = 8;
        self.queue = 64;
        self.ping_interval = Duration::from_secs(30);
        self.pong_timeout = Duration::from_secs(60);
        self.connection_lifetime = Duration::from_secs(300);
        self.snapshot_levels = 50;
        self.broadcast_capacity = 64;
        self
    }

    /// Looks up a configured symbol by stream name (case-insensitive).
    pub fn symbol(&self, name: &str) -> Option<&SymbolSpec> {
        self.symbols
            .iter()
            .find(|spec| spec.symbol.eq_ignore_ascii_case(name))
    }
}

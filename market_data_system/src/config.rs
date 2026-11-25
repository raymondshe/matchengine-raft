//! Service configuration shared by the feed, hub and WebSocket sessions.

use std::collections::HashSet;
use std::time::Duration;

use serde::Serialize;

/// Default Binance-style kline intervals to accept.
pub const DEFAULT_BAR_INTERVALS_MS: &[u64] = &[1_000, 60_000];
/// Default depth levels returned by a subscribe-time snapshot.
pub const DEFAULT_SNAPSHOT_LEVELS: usize = 200;
/// Default number of historical bars in a WebSocket subscribe-time replay.
pub const DEFAULT_HISTORY_SNAPSHOT_BARS: usize = 500;
/// Hard cap on retained/replayed bars per interval. `--history-snapshot-bars`
/// can lower this, never raise it.
pub const MAX_HISTORY_SNAPSHOT_BARS: usize = 1_000;
/// TDengine table identifiers are limited to 32 lowercase ASCII bytes.
pub const MAX_TDENGINE_IDENTIFIER_LEN: usize = 32;

/// Sanitizes a stream symbol for use as a TDengine table/tag identifier.
///
/// `parse_symbol` accepts '-' because it is safe in file names and WebSocket
/// stream keys; TDengine identifiers are not. Replace it with '_' at the
/// storage boundary only, so the public symbol stays unchanged.
pub fn history_table_name(symbol: &str) -> String {
    symbol.replace('-', "_")
}

/// Early-startup check for a symbol that will be used with `--history`.
///
/// Mirrors `HistoryStore::valid_identifier` after sanitization so a bad name
/// fails with a clear CLI error instead of inside `ensure_schema`.
pub fn validate_history_symbol(symbol: &str) -> Result<(), String> {
    let table = history_table_name(symbol);
    if table.is_empty()
        || table.len() > MAX_TDENGINE_IDENTIFIER_LEN
        || !table
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(format!(
            "historical store requires a lowercase symbol of at most \
             {MAX_TDENGINE_IDENTIFIER_LEN} ASCII letters, digits, '-' or '_' (got {symbol:?})"
        ));
    }
    Ok(())
}

/// Configuration for the optional TDengine-backed historical bar tier.
///
/// Disabled by default: the existing in-memory feed and replay behaviour is
/// unchanged unless `--history` is set. All bar values are stored as exact
/// `DECIMAL(38,18)` text and read back through `CAST(... AS VARCHAR)`.
#[derive(Clone, Debug)]
pub struct HistoryConfig {
    pub enabled: bool,
    /// Serve `/klines` and bar replay from TDengine without writing rows.
    /// Used by the global terminal service: the per-symbol fast services own
    /// the deterministic writer so history is materialized exactly once.
    pub read_only: bool,
    /// `taosAdapter` endpoint, e.g. `http://127.0.0.1:6041`.
    pub url: String,
    pub user: String,
    pub password: String,
    pub database: String,
    /// Create the database/stables on startup; disable for operator-managed schemas.
    pub auto_create: bool,
    pub vgroups: u32,
    pub replica: u32,
    pub keep_days: u32,
    pub batch_rows: usize,
    /// Event-time lag after which a closed window is sealed.
    pub seal_lag_ms: u64,
    /// Bars per interval in a WS subscribe-time replay.
    pub snapshot_bars: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            read_only: false,
            url: "http://127.0.0.1:6041".to_string(),
            user: "root".to_string(),
            password: "taosdata".to_string(),
            database: "market_bars".to_string(),
            auto_create: true,
            vgroups: 1,
            replica: 1,
            keep_days: 3650,
            batch_rows: 2_000,
            seal_lag_ms: 2_000,
            snapshot_bars: DEFAULT_HISTORY_SNAPSHOT_BARS,
        }
    }
}

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

/// Direct NATS source for one symbol (Profile N).
#[derive(Clone, Debug)]
pub struct NatsSpec {
    /// Server URL, e.g. `nats://nats.match-eu-1.svc.cluster.local:4222`.
    pub url: String,
    /// Subject prefix including the cell and feed group; the source appends
    /// `.<stream_id>` for each subscription.
    pub subject_prefix: String,
    /// First engine stream id in the prefix (`base..base+6`).
    pub stream_base: u16,
}

/// WebSocket feed class.
///
/// Binance exposes one Streams protocol with raw/combined endpoints and a
/// 1024-stream cap. We keep that protocol but split connections into two
/// server-enforced classes: a fast single-symbol feed for API/algo clients and
/// a terminal multi-symbol feed with display conflation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedProfile {
    Fast,
    Terminal,
}

impl FeedProfile {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Terminal => "terminal",
        }
    }

    pub fn max_streams(self, config: &Config) -> usize {
        match self {
            Self::Fast => config.fast_max_streams,
            Self::Terminal => config.max_streams,
        }
    }

    pub fn is_terminal(self) -> bool {
        self == Self::Terminal
    }
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
    /// Maximum streams on a terminal (multi-symbol) connection. Binance allows 1024.
    pub max_streams: usize,
    /// Maximum streams on a fast single-symbol connection.
    pub fast_max_streams: usize,
    /// Serve only the fast single-symbol profile. Terminal `/ws` and
    /// `/stream?streams=...` endpoints return 404.
    pub fast_only: bool,
    /// Terminal display conflation window for latest-value streams
    /// (`bookTicker`, partial depth). 0 disables conflation.
    pub terminal_conflation_ms: u64,
    /// Terminal per-connection frame budget for conflatable streams per second.
    /// 0 disables the budget; sequenced streams are never throttled here.
    pub terminal_max_frames_per_second: u64,
    /// Per-connection sequenced FIFO capacity.
    pub queue: usize,
    /// Whether the engine extension streams (`@sweep`, `@status`,
    /// `@lifecycle`) are exposed alongside the Binance-compatible streams.
    pub extensions: bool,
    /// Whether the `@lifecycle` stream is exposed.
    pub lifecycle: bool,
    /// Server ping interval.
    pub ping_interval: Duration,
    /// Disconnect when no pong arrives within this window.
    pub pong_timeout: Duration,
    /// Forced reconnect after this lifetime.
    pub connection_lifetime: Duration,
    /// Mark a symbol not ready when no frame/heartbeat arrives within this window.
    pub heartbeat_timeout: Duration,
    /// Depth levels in a subscribe-time snapshot.
    pub snapshot_levels: usize,
    /// Broadcast channel capacity per stream key.
    pub broadcast_capacity: usize,
    /// Optional TDengine historical bar tier.
    pub history: HistoryConfig,
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
            // A fast connection carries one symbol; the full catalogue is ~16 streams.
            fast_max_streams: 32,
            fast_only: false,
            // Terminal clients render at human speed; conflate latest-value streams.
            terminal_conflation_ms: 100,
            terminal_max_frames_per_second: 2_000,
            queue: 1_024,
            // Public defaults: only Binance-compatible streams are exposed.
            extensions: false,
            lifecycle: false,
            ping_interval: Duration::from_secs(180),
            pong_timeout: Duration::from_secs(600),
            connection_lifetime: Duration::from_secs(24 * 60 * 60),
            heartbeat_timeout: Duration::from_secs(30),
            snapshot_levels: DEFAULT_SNAPSHOT_LEVELS,
            broadcast_capacity: 1_024,
            history: HistoryConfig::default(),
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
        self.fast_max_streams = 8;
        self.fast_only = false;
        self.terminal_conflation_ms = 100;
        self.terminal_max_frames_per_second = 1_000;
        self.queue = 64;
        self.ping_interval = Duration::from_secs(30);
        self.pong_timeout = Duration::from_secs(60);
        self.connection_lifetime = Duration::from_secs(300);
        self.heartbeat_timeout = Duration::from_secs(5);
        self.snapshot_levels = 50;
        self.broadcast_capacity = 64;
        // The test suite exercises the full catalogue, including extensions.
        self.extensions = true;
        self.lifecycle = true;
        self
    }

    /// Looks up a configured symbol by stream name (case-insensitive).
    pub fn symbol(&self, name: &str) -> Option<&SymbolSpec> {
        self.symbols
            .iter()
            .find(|spec| spec.symbol.eq_ignore_ascii_case(name))
    }

    /// Rejects zero kline intervals (which would divide by zero in the feed)
    /// and removes duplicates while preserving the caller's order.
    pub fn normalize_bar_intervals(&mut self) -> Result<(), String> {
        if self.bar_intervals_ms.is_empty() {
            return Err("--bar-intervals-ms requires at least one interval".to_string());
        }
        if self.bar_intervals_ms.contains(&0) {
            return Err("--bar-intervals-ms values must be greater than zero".to_string());
        }
        let mut seen = HashSet::new();
        self.bar_intervals_ms
            .retain(|interval| seen.insert(*interval));
        Ok(())
    }
}

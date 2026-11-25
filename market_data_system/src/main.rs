//! Standalone market-data system binary.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use matchengine_marketdata::AeronSpec;
use matchengine_marketdata::BoxedSource;
use matchengine_marketdata::Config;
use matchengine_marketdata::NatsSpec;
use matchengine_marketdata::SnapshotRequester;
use matchengine_marketdata::SymbolSpec;
use matchengine_marketdata::archive_path;
use matchengine_marketdata::archive_source;
use matchengine_marketdata::config::MAX_HISTORY_SNAPSHOT_BARS;
use matchengine_marketdata::config::history_table_name;
use matchengine_marketdata::config::validate_history_symbol;
use matchengine_marketdata::jsonl_source;
use matchengine_marketdata::nats_source;
use matchengine_marketdata::start_with_snapshot_requesters;

#[derive(Parser, Debug)]
#[clap(about = "Serve Binance-style WebSocket market data from one or more engine feeds")]
struct Opt {
    /// HTTP/WebSocket listener.
    #[clap(long, default_value = "127.0.0.1:24700")]
    addr: String,

    /// Symbol spec `name:id:base:quote`, repeatable (e.g. eurusd:0:EUR:USD).
    #[clap(long = "symbol", value_name = "NAME:ID:BASE:QUOTE")]
    symbols: Vec<String>,

    /// Engine market-data JSONL file, repeatable and aligned with --symbol.
    #[clap(long, env = "MARKET_DATA_LOG")]
    market_data_log: Vec<PathBuf>,

    /// Record every consumed message to `<dir>/<symbol>.jsonl`.
    #[clap(long, env = "MARKET_DATA_ARCHIVE_DIR")]
    archive_dir: Option<PathBuf>,

    /// Replay `<dir>/<symbol>.jsonl` instead of a live source.
    #[clap(long, env = "MARKET_DATA_REPLAY_DIR")]
    replay_dir: Option<PathBuf>,

    /// NATS server URL for the direct Profile N source (one URL serves every symbol).
    #[clap(long, env = "NATS_URL")]
    nats_url: Option<String>,

    /// NATS subject prefix per symbol, including cell and feed group
    /// (repeat once per `--symbol`).
    #[clap(long, env = "NATS_SUBJECT_PREFIX")]
    nats_subject_prefix: Vec<String>,

    /// First NATS stream id per symbol; repeat once per `--symbol` (default 2001).
    #[clap(long, value_name = "ID", default_value = "2001")]
    nats_stream_base: Vec<u16>,

    /// Aeron channel per symbol (one value is reused for every symbol).
    #[clap(long, env = "AERON_CHANNEL")]
    aeron_channel: Vec<String>,

    /// First Aeron stream id per symbol; repeat once per `--symbol` (default 2001).
    #[clap(long, value_name = "ID", default_value = "2001")]
    aeron_stream_base: Vec<u16>,

    /// Aeron media-driver directory per symbol (one value is reused).
    #[clap(long, env = "AERON_DIR")]
    aeron_dir: Vec<String>,

    /// Start an embedded media driver (demos/tests).
    #[clap(long)]
    aeron_embedded_driver: bool,

    /// Kline intervals to accept, in milliseconds.
    #[clap(long, value_delimiter = ',', default_value = "1000,60000")]
    bar_intervals_ms: Vec<u64>,

    /// Maximum concurrent WebSocket connections.
    #[clap(long, default_value_t = 256)]
    max_connections: usize,

    /// Maximum streams on a terminal multi-symbol connection (Binance uses 1024).
    #[clap(long, default_value_t = 1024)]
    max_streams: usize,

    /// Maximum streams on a fast single-symbol connection.
    #[clap(long, default_value_t = 32)]
    fast_max_streams: usize,

    /// Serve only the fast single-symbol profile (disable `/ws` and `/stream`).
    #[clap(long)]
    fast_only: bool,

    /// Terminal display conflation window for latest-value streams, in ms (0 disables).
    #[clap(long, default_value_t = 100)]
    terminal_conflation_ms: u64,

    /// Terminal per-connection frame budget for conflatable streams per second (0 disables).
    #[clap(long, default_value_t = 2000)]
    terminal_max_frames_per_second: u64,

    /// Per-connection sequenced queue capacity.
    #[clap(long, default_value_t = 1024)]
    queue: usize,

    /// Expose engine extension streams (@sweep, @status, @lifecycle).
    ///
    /// Off by default so a public deployment serves the Binance-compatible
    /// stream catalogue only.
    #[clap(long)]
    extensions: bool,

    /// Disable the extension `@lifecycle` stream.
    #[clap(long)]
    no_lifecycle: bool,

    /// Server ping interval in seconds.
    #[clap(long, default_value_t = 180)]
    ping_secs: u64,

    /// Pong timeout in seconds.
    #[clap(long, default_value_t = 600)]
    pong_timeout_secs: u64,

    /// Forced reconnect lifetime in seconds (0 disables).
    #[clap(long, default_value_t = 86_400)]
    connection_lifetime_secs: u64,

    /// Mark a symbol not ready when no frame/heartbeat arrives within this window.
    #[clap(long, default_value_t = 30)]
    heartbeat_timeout_secs: u64,

    /// Depth levels in a subscribe-time snapshot.
    #[clap(long, default_value_t = 200)]
    snapshot_levels: usize,

    /// Enable the TDengine-backed historical bar tier (1s/1m rows and /klines).
    #[clap(long)]
    history: bool,
    /// Serve history reads only; do not start the TDengine bar writer.
    #[clap(long, default_value_t = false)]
    history_read_only: bool,
    /// taosAdapter endpoint for the historical store.
    #[clap(long, env = "TDENGINE_URL", default_value = "http://127.0.0.1:6041")]
    tdengine_url: String,
    #[clap(long, env = "TDENGINE_USER", default_value = "root")]
    tdengine_user: String,
    #[clap(long, env = "TDENGINE_PASSWORD", default_value = "taosdata")]
    tdengine_password: String,
    #[clap(long, env = "TDENGINE_DATABASE", default_value = "market_bars")]
    tdengine_database: String,
    /// Create the database and stables on startup.
    #[clap(long, default_value_t = true, action = clap::ArgAction::Set)]
    tdengine_auto_create: bool,
    #[clap(long, default_value_t = 1)]
    tdengine_vgroups: u32,
    #[clap(long, default_value_t = 1)]
    tdengine_replica: u32,
    #[clap(long, default_value_t = 3650)]
    tdengine_keep_days: u32,
    #[clap(long, default_value_t = 2000)]
    tdengine_batch_rows: usize,
    /// Event-time lag before a closed window is sealed.
    #[clap(long, default_value_t = 2000)]
    history_seal_lag_ms: u64,
    /// Bars per interval in a WS subscribe-time replay.
    #[clap(long, default_value_t = 500)]
    history_snapshot_bars: usize,
}

fn parse_symbol(raw: &str) -> Result<SymbolSpec, String> {
    let parts: Vec<&str> = raw.split(':').collect();
    if parts.len() != 4 {
        return Err(format!("--symbol must be name:id:base:quote (got {raw})"));
    }
    let symbol = parts[0].trim().to_ascii_lowercase();
    if symbol.is_empty() {
        return Err("--symbol name must not be empty".to_string());
    }
    if !symbol
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(format!(
            "--symbol name must contain only ASCII letters, digits, '_' or '-' (got {symbol})"
        ));
    }
    let symbol_id = parts[1]
        .parse()
        .map_err(|_| format!("invalid symbol id in --symbol {raw}"))?;
    Ok(SymbolSpec {
        symbol,
        symbol_id,
        base_asset: parts[2].to_string(),
        quote_asset: parts[3].to_string(),
    })
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let opt = Opt::parse();
    if opt.symbols.is_empty() {
        return Err("at least one --symbol name:id:base:quote is required".into());
    }
    let mut symbols = Vec::with_capacity(opt.symbols.len());
    let mut seen = HashSet::new();
    for raw in &opt.symbols {
        let spec = parse_symbol(raw)?;
        if !seen.insert(spec.symbol.clone()) {
            return Err(format!("duplicate --symbol {}", spec.symbol).into());
        }
        symbols.push(spec);
    }

    // Validate the historical-store identifiers at startup. The store sanitizes
    // '-' into '_' at the storage boundary, but names that cannot be sanitized
    // (too long, or colliding after sanitization) must fail here with a clear
    // message rather than deep inside `ensure_schema`.
    if opt.history {
        validate_history_symbol(&opt.tdengine_database)
            .map_err(|error| format!("--tdengine-database: {error}"))?;
        let mut tables = HashSet::new();
        for spec in &symbols {
            validate_history_symbol(&spec.symbol)
                .map_err(|error| format!("--symbol {}: {error}", spec.symbol))?;
            if !tables.insert(history_table_name(&spec.symbol)) {
                return Err(format!(
                    "--symbol {} collides with another symbol after sanitizing '-' to '_'",
                    spec.symbol
                )
                .into());
            }
        }
    }

    let mut config = Config::new(opt.addr, symbols.clone());
    if !opt.bar_intervals_ms.is_empty() {
        config.bar_intervals_ms = opt.bar_intervals_ms;
    }
    config.normalize_bar_intervals()?;
    config.max_connections = opt.max_connections;
    config.max_streams = opt.max_streams.max(1);
    config.fast_max_streams = opt.fast_max_streams.max(1);
    config.fast_only = opt.fast_only;
    config.terminal_conflation_ms = opt.terminal_conflation_ms;
    config.terminal_max_frames_per_second = opt.terminal_max_frames_per_second;
    config.queue = opt.queue.max(1);
    config.extensions = opt.extensions;
    config.lifecycle = opt.extensions && !opt.no_lifecycle;
    config.ping_interval = Duration::from_secs(opt.ping_secs.max(1));
    config.pong_timeout = Duration::from_secs(opt.pong_timeout_secs.max(1));
    config.connection_lifetime = if opt.connection_lifetime_secs == 0 {
        Duration::from_secs(u64::MAX / 2)
    } else {
        Duration::from_secs(opt.connection_lifetime_secs)
    };
    config.heartbeat_timeout = Duration::from_secs(opt.heartbeat_timeout_secs.max(1));
    config.snapshot_levels = opt.snapshot_levels.max(1);
    config.history.enabled = opt.history;
    config.history.read_only = opt.history_read_only;
    config.history.url = opt.tdengine_url;
    config.history.user = opt.tdengine_user;
    config.history.password = opt.tdengine_password;
    config.history.database = opt.tdengine_database;
    config.history.auto_create = opt.tdengine_auto_create;
    config.history.vgroups = opt.tdengine_vgroups.max(1);
    config.history.replica = opt.tdengine_replica.max(1);
    config.history.keep_days = opt.tdengine_keep_days.max(1);
    config.history.batch_rows = opt.tdengine_batch_rows.max(1);
    config.history.seal_lag_ms = opt.history_seal_lag_ms;
    config.history.snapshot_bars = opt
        .history_snapshot_bars
        .clamp(1, MAX_HISTORY_SNAPSHOT_BARS);

    if opt.replay_dir.is_some()
        && (!opt.market_data_log.is_empty()
            || !opt.aeron_channel.is_empty()
            || opt.nats_url.is_some())
    {
        return Err(
            "--replay-dir cannot be combined with --market-data-log, --aeron-channel or --nats-url"
                .into(),
        );
    }
    if opt.nats_url.is_some() && !opt.aeron_channel.is_empty() {
        return Err("--nats-url cannot be combined with --aeron-channel".into());
    }
    if opt.nats_url.is_some() && opt.nats_subject_prefix.is_empty() {
        return Err("--nats-url requires --nats-subject-prefix".into());
    }
    if !opt.nats_subject_prefix.is_empty() && opt.nats_subject_prefix.len() != symbols.len() {
        return Err("--nats-subject-prefix must be repeated once per --symbol".into());
    }
    if opt.replay_dir.is_some() && opt.archive_dir.is_some() {
        return Err("--archive-dir cannot be combined with --replay-dir".into());
    }

    let mut snapshot_requesters: HashMap<String, SnapshotRequester> = HashMap::new();
    let mut sources: Vec<(SymbolSpec, BoxedSource)> = if let Some(dir) = &opt.replay_dir {
        for spec in &symbols {
            let path = archive_path(dir, &spec.symbol);
            if !path.exists() {
                return Err(format!("replay file does not exist: {}", path.display()).into());
            }
        }
        symbols
            .iter()
            .map(|spec| (spec.clone(), jsonl_source(archive_path(dir, &spec.symbol))))
            .collect()
    } else if !opt.market_data_log.is_empty() {
        if opt.market_data_log.len() != symbols.len() {
            return Err("--market-data-log must be repeated once per --symbol".into());
        }
        symbols
            .iter()
            .cloned()
            .zip(
                opt.market_data_log
                    .iter()
                    .map(|path| jsonl_source(path.clone())),
            )
            .collect()
    } else if let Some(url) = &opt.nats_url {
        let url = url.clone();
        let mut sources = Vec::with_capacity(symbols.len());
        for (index, spec) in symbols.iter().enumerate() {
            let subject_prefix = opt
                .nats_subject_prefix
                .get(index)
                .ok_or("--nats-subject-prefix is empty")?
                .clone();
            let stream_base = opt
                .nats_stream_base
                .get(index)
                .or_else(|| opt.nats_stream_base.first())
                .copied()
                .unwrap_or(2001);
            let nats = NatsSpec {
                url: url.clone(),
                subject_prefix,
                stream_base,
            };
            let source = nats_source(&nats, spec.symbol_id);
            snapshot_requesters.insert(spec.symbol.clone(), source.requester);
            sources.push((spec.clone(), source.frames));
        }
        sources
    } else if !opt.aeron_channel.is_empty() {
        let mut sources = Vec::with_capacity(symbols.len());
        for (index, spec) in symbols.iter().enumerate() {
            let channel = opt
                .aeron_channel
                .get(index)
                .or_else(|| opt.aeron_channel.first())
                .ok_or("--aeron-channel is empty")?;
            // Each symbol owns its embedded media driver and directory. A
            // shared driver cannot reset a single symbol's stale Aeron image;
            // per-symbol drivers also keep one receiver per group/port.
            let dir = opt
                .aeron_dir
                .get(index)
                .or_else(|| opt.aeron_dir.first())
                .cloned()
                .unwrap_or_else(|| "/dev/shm/aeron".to_string());
            let dir = Some(format!("{dir}-{}", spec.symbol));
            let stream_base = opt
                .aeron_stream_base
                .get(index)
                .or_else(|| opt.aeron_stream_base.first())
                .copied()
                .unwrap_or(2001);
            let aeron = AeronSpec {
                channel: channel.clone(),
                stream_base,
                dir,
                embedded: opt.aeron_embedded_driver,
            };
            let (source, requester) = aeron_source_for(&aeron, spec.symbol_id).await?;
            snapshot_requesters.insert(spec.symbol.clone(), requester);
            sources.push((spec.clone(), source));
        }
        sources
    } else {
        return Err(
            "--market-data-log, --aeron-channel, --nats-url or --replay-dir is required".into(),
        );
    };

    if let Some(dir) = &opt.archive_dir {
        sources = sources
            .into_iter()
            .map(|(spec, source)| {
                let path = archive_path(dir, &spec.symbol);
                println!("  archive:         {} -> {}", spec.symbol, path.display());
                (spec, archive_source(source, path))
            })
            .collect();
    }

    let running = start_with_snapshot_requesters(config, sources, snapshot_requesters).await?;
    println!("market-data system: ws://{}/ws", running.addr);
    println!(
        "  raw stream:      ws://{}/ws/<symbol>@<stream>",
        running.addr
    );
    println!(
        "  combined:        ws://{}/stream?streams=...",
        running.addr
    );
    println!("  health/ready:    http://{}/health, /ready", running.addr);
    println!(
        "  symbols:         {}",
        symbols
            .iter()
            .map(|spec| format!("{}({})", spec.symbol, spec.symbol_id))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("Press Ctrl-C to stop.");

    wait_for_shutdown_signal().await?;
    running.shutdown().await;
    Ok(())
}

async fn wait_for_shutdown_signal() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = sigterm.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok(())
    }
}

#[cfg(feature = "aeron")]
async fn aeron_source_for(
    aeron: &AeronSpec,
    symbol_id: u32,
) -> Result<(BoxedSource, SnapshotRequester), Box<dyn std::error::Error>> {
    let source = matchengine_marketdata::aeron_source_with_requester(aeron, symbol_id).await?;
    Ok((source.frames, source.requester))
}

#[cfg(not(feature = "aeron"))]
async fn aeron_source_for(
    _aeron: &AeronSpec,
    _symbol_id: u32,
) -> Result<(BoxedSource, SnapshotRequester), Box<dyn std::error::Error>> {
    Err("--aeron-channel requires building with --features aeron".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use matchengine_marketdata::config::MAX_TDENGINE_IDENTIFIER_LEN;

    #[test]
    fn symbol_names_are_safe_for_archive_paths() {
        assert!(parse_symbol("EURUSD:0:EUR:USD").is_ok());
        assert!(parse_symbol("btc_usdt:1:BTC:USDT").is_ok());
        assert!(parse_symbol("btc-usdt:1:BTC:USDT").is_ok());
        assert!(parse_symbol("../escape:0:EUR:USD").is_err());
        assert!(parse_symbol("a/b:0:EUR:USD").is_err());
    }

    #[test]
    fn history_identifiers_sanitize_hyphens_and_reject_invalid_names() {
        assert_eq!("btc_usdt", history_table_name("btc-usdt"));
        assert!(validate_history_symbol("btc-usdt").is_ok());
        assert!(validate_history_symbol(&"x".repeat(MAX_TDENGINE_IDENTIFIER_LEN)).is_ok());
        assert!(validate_history_symbol(&"x".repeat(MAX_TDENGINE_IDENTIFIER_LEN + 1)).is_err());
        assert!(validate_history_symbol("BTC").is_err());
    }

    #[test]
    fn bar_intervals_must_be_positive_and_are_deduplicated() {
        let mut config = Config::for_test("127.0.0.1:0");
        config.bar_intervals_ms = vec![1000, 60_000, 1000, 5000];
        config.normalize_bar_intervals().unwrap();
        assert_eq!(vec![1000, 60_000, 5000], config.bar_intervals_ms);

        config.bar_intervals_ms = vec![1000, 0];
        assert!(config.normalize_bar_intervals().is_err());
    }
}

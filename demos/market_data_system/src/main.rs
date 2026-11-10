//! Standalone market-data system binary.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use matchengine_marketdata::AeronSpec;
use matchengine_marketdata::BoxedSource;
use matchengine_marketdata::Config;
use matchengine_marketdata::SnapshotRequester;
use matchengine_marketdata::SymbolSpec;
use matchengine_marketdata::archive_path;
use matchengine_marketdata::archive_source;
use matchengine_marketdata::jsonl_source;
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

    /// Aeron channel per symbol (one value is reused for every symbol).
    #[clap(long, env = "AERON_CHANNEL")]
    aeron_channel: Vec<String>,

    /// First Aeron stream id; the seven feed streams are `base..base+6`.
    #[clap(long, default_value_t = 2001)]
    aeron_stream_base: u16,

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

    /// Maximum streams per connection (Binance uses 1024).
    #[clap(long, default_value_t = 1024)]
    max_streams: usize,

    /// Per-connection sequenced queue capacity.
    #[clap(long, default_value_t = 1024)]
    queue: usize,

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

    let mut config = Config::new(opt.addr, symbols.clone());
    if !opt.bar_intervals_ms.is_empty() {
        config.bar_intervals_ms = opt.bar_intervals_ms;
    }
    config.max_connections = opt.max_connections;
    config.max_streams = opt.max_streams;
    config.queue = opt.queue.max(1);
    config.lifecycle = !opt.no_lifecycle;
    config.ping_interval = Duration::from_secs(opt.ping_secs.max(1));
    config.pong_timeout = Duration::from_secs(opt.pong_timeout_secs.max(1));
    config.connection_lifetime = if opt.connection_lifetime_secs == 0 {
        Duration::from_secs(u64::MAX / 2)
    } else {
        Duration::from_secs(opt.connection_lifetime_secs)
    };
    config.heartbeat_timeout = Duration::from_secs(opt.heartbeat_timeout_secs.max(1));
    config.snapshot_levels = opt.snapshot_levels.max(1);

    if opt.replay_dir.is_some()
        && (!opt.market_data_log.is_empty() || !opt.aeron_channel.is_empty())
    {
        return Err(
            "--replay-dir cannot be combined with --market-data-log or --aeron-channel".into(),
        );
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
    } else if !opt.aeron_channel.is_empty() {
        let mut sources = Vec::with_capacity(symbols.len());
        for (index, spec) in symbols.iter().enumerate() {
            let channel = opt
                .aeron_channel
                .get(index)
                .or_else(|| opt.aeron_channel.first())
                .ok_or("--aeron-channel is empty")?;
            let dir = opt
                .aeron_dir
                .get(index)
                .or_else(|| opt.aeron_dir.first())
                .cloned();
            let aeron = AeronSpec {
                channel: channel.clone(),
                stream_base: opt.aeron_stream_base,
                dir,
                embedded: opt.aeron_embedded_driver,
            };
            let (source, requester) = aeron_source_for(&aeron, spec.symbol_id).await?;
            snapshot_requesters.insert(spec.symbol.clone(), requester);
            sources.push((spec.clone(), source));
        }
        sources
    } else {
        return Err("--market-data-log, --aeron-channel or --replay-dir is required".into());
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

    tokio::signal::ctrl_c().await?;
    running.shutdown().await;
    Ok(())
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
    use super::parse_symbol;

    #[test]
    fn symbol_names_are_safe_for_archive_paths() {
        assert!(parse_symbol("EURUSD:0:EUR:USD").is_ok());
        assert!(parse_symbol("btc_usdt:1:BTC:USDT").is_ok());
        assert!(parse_symbol("btc-usdt:1:BTC:USDT").is_ok());
        assert!(parse_symbol("../escape:0:EUR:USD").is_err());
        assert!(parse_symbol("a/b:0:EUR:USD").is_err());
    }
}

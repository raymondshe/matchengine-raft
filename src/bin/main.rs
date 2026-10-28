use std::path::PathBuf;

use clap::Parser;
use env_logger::Env;
use matchengine_raft::StorageConfig;
use matchengine_raft::Transport;
use matchengine_raft::market_data::MarketDataConfig;
use matchengine_raft::start_match_engine_node_with_storage;

#[derive(Parser, Clone, Debug)]
#[clap(author, version, about, long_about = None)]
pub struct Opt {
    #[clap(long)]
    pub id: u64,

    #[clap(long)]
    pub http_addr: String,

    /// Transport for peer-to-peer Raft RPCs.
    ///
    /// The HTTP API and both server endpoints stay available; this only selects
    /// what this node dials when it talks to peers.
    #[clap(long, value_enum, default_value_t = Transport::Grpc)]
    pub transport: Transport,

    /// Root directory for durable state. Each node stores its log and snapshot
    /// under `<data-dir>/<instance-prefix>-<id>/`.
    #[clap(long, env = "RAFT_DATA_DIR", default_value = "/tmp/matchengine-raft")]
    pub data_dir: PathBuf,

    /// Prefix of the per-node directory, so independent clusters can share one data dir.
    #[clap(long, env = "RAFT_INSTANCE_PREFIX", default_value = "matchengine")]
    pub instance_prefix: String,

    /// Append every applied match event (fills, rests, cancels) to this JSONL file.
    ///
    /// Debugging aid: the file is node-local, not replicated, and replayed entries are
    /// written again after a restart. Omit to discard match results.
    #[clap(long, env = "MATCH_EVENT_LOG")]
    pub match_event_log: Option<PathBuf>,

    /// Append market-data messages (trades, BBO, depth, bars, lifecycle) to this
    /// JSONL file and enable the feed.
    ///
    /// Debugging aid: node-local, not replicated, and replayed entries are written
    /// again after a restart. Omit to keep the feed off.
    #[clap(long, env = "MARKET_DATA_LOG")]
    pub market_data_log: Option<PathBuf>,

    /// Depth levels per side in market-data snapshots.
    #[clap(long, default_value_t = 10)]
    pub market_data_depth: usize,

    /// Minimum spacing between BBO updates, in milliseconds.
    #[clap(long, default_value_t = 1)]
    pub market_data_bbo_interval_ms: u64,

    /// OHLC bar intervals in milliseconds, comma separated (default 1000,60000).
    #[clap(long, value_delimiter = ',')]
    pub market_data_bar_intervals_ms: Vec<u64>,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Setup the logger
    env_logger::init_from_env(Env::default().default_filter_or("info"));

    // Parse the parameters passed by arguments.
    let options = Opt::parse();

    let mut storage =
        StorageConfig::new(options.data_dir).with_instance_prefix(options.instance_prefix);
    if let Some(path) = options.match_event_log {
        storage = storage.with_match_event_log(path);
    }
    if let Some(path) = options.market_data_log {
        let mut config = MarketDataConfig::jsonl(path);
        config.depth_levels = options.market_data_depth;
        config.bbo_interval_ms = options.market_data_bbo_interval_ms;
        if !options.market_data_bar_intervals_ms.is_empty() {
            config.bar_intervals_ms = options.market_data_bar_intervals_ms;
        }
        storage = storage.with_market_data(config);
    }

    start_match_engine_node_with_storage(
        options.id,
        options.http_addr,
        options.transport,
        matchengine_raft::default_raft_config(),
        storage,
    )
    .await
}

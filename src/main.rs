use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use env_logger::Env;
use matchengine_raft::DEFAULT_SNAPSHOT_INTERVAL;
use matchengine_raft::StorageConfig;
use matchengine_raft::Transport;
use matchengine_raft::market_data::MarketDataConfig;
use matchengine_raft::start_match_engine_node_with_storage;
use matchengine_raft::store::state_machine::DEFAULT_MAX_EVENTS;

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

    /// Maximum number of match events retained in the replicated event log.
    ///
    /// Set 0 to disable history (`/events` then returns nothing). Submission
    /// dedupe uses a separate fixed cap so retries cannot diverge across nodes.
    #[clap(long, env = "MAX_EVENTS", default_value_t = DEFAULT_MAX_EVENTS)]
    pub max_events: usize,

    /// Periodic snapshot interval in milliseconds (0 disables the timer).
    ///
    /// The default asks Raft for a snapshot every 30 minutes when new entries
    /// have been applied; the log-count policy also snapshots at 100,000
    /// entries.
    #[clap(
        long,
        env = "SNAPSHOT_INTERVAL_MS",
        default_value_t = DEFAULT_SNAPSHOT_INTERVAL.as_millis() as u64
    )]
    pub snapshot_interval_ms: u64,

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

    /// Emit a deterministic STATUS message every N market sequences (default 100; 0 disables).
    #[clap(long, default_value_t = 100)]
    pub market_data_status_every: u64,

    /// OHLC bar intervals in milliseconds, comma separated (default 1000,60000).
    #[clap(long, value_delimiter = ',')]
    pub market_data_bar_intervals_ms: Vec<u64>,

    /// Publish market data only while this node is the leader (default true).
    ///
    /// Set to false to feed every replica; duplicates carry identical
    /// `(symbol_id, stream_id, market_seq, sub_seq)` keys and deduplicate
    /// downstream. This is mainly a testing switch.
    #[clap(long, num_args = 0..=1, default_missing_value = "true", default_value_t = true)]
    pub market_data_leader_only: bool,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Setup the logger
    env_logger::init_from_env(Env::default().default_filter_or("info"));

    // Parse the parameters passed by arguments.
    let options = Opt::parse();

    let mut storage = StorageConfig::new(options.data_dir)
        .with_instance_prefix(options.instance_prefix)
        .with_max_events(options.max_events)
        .with_snapshot_interval(Duration::from_millis(options.snapshot_interval_ms));
    if let Some(path) = options.match_event_log {
        storage = storage.with_match_event_log(path);
    }
    if let Some(path) = options.market_data_log {
        let mut config = MarketDataConfig::jsonl(path);
        config.depth_levels = options.market_data_depth;
        config.status_every = options.market_data_status_every;
        config.leader_only = options.market_data_leader_only;
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

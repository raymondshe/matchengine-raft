use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use env_logger::Env;
use matchengine_raft::DEFAULT_SNAPSHOT_INTERVAL;
use matchengine_raft::StorageConfig;
use matchengine_raft::market_data::MarketDataConfig;
use matchengine_raft::network::read_policy::ReadPolicyMode;
use matchengine_raft::start_match_engine_node_with_storage;
use matchengine_raft::store::state_machine::DEFAULT_MAX_EVENTS;

#[derive(Parser, Clone, Debug)]
#[clap(author, version, about, long_about = None)]
pub struct Opt {
    #[clap(long)]
    pub id: u64,

    #[clap(long)]
    pub http_addr: String,

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

    /// Leader-consistent read policy: `lease` (default) or `read_index`.
    ///
    /// Lease reads reuse a recent quorum acknowledgement and fall back to
    /// ReadIndex when the lease is stale. They assume bounded clock drift.
    #[clap(long, env = "MATCHENGINE_READ_POLICY", default_value = "lease")]
    pub read_policy: ReadPolicyMode,

    /// Prototype asynchronous WAL append (requires the `async-wal` feature and
    /// `RAFT_WAL_FSYNC_MS=0`). Off by default.
    #[clap(
        long,
        env = "MATCHENGINE_ASYNC_WAL",
        num_args = 0..=1,
        default_missing_value = "true",
        default_value_t = false,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    pub async_wal: bool,

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

    /// Aeron channel for the market-data feed, e.g. `aeron:ipc` or a multicast
    /// URI. Requires a binary built with the `market-data-aeron` feature.
    #[clap(long, env = "MARKET_DATA_AERON_CHANNEL")]
    pub market_data_aeron_channel: Option<String>,

    /// First Aeron stream id; the seven feed streams are `base..base+6`.
    #[clap(long, default_value_t = 2001)]
    pub market_data_aeron_stream_base: u16,

    /// Aeron media-driver directory. With an embedded driver this is where it
    /// runs; otherwise the client connects there (falling back to `AERON_DIR`).
    #[clap(long, env = "MARKET_DATA_AERON_DIR")]
    pub market_data_aeron_dir: Option<String>,

    /// Start an embedded media driver (demos/tests; production normally runs a
    /// supervised driver).
    #[clap(long)]
    pub market_data_aeron_embedded_driver: bool,

    /// Re-broadcast a fresh reset depth snapshot on this cadence while
    /// publishing, so consumers can join mid-stream (0 disables).
    #[clap(long, default_value_t = 30_000)]
    pub market_data_aeron_snapshot_every_ms: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::io::Result<()> {
    // Setup the logger
    env_logger::init_from_env(Env::default().default_filter_or("info"));

    // Parse the parameters passed by arguments.
    let options = Opt::parse();

    match options.read_policy {
        ReadPolicyMode::Lease => tracing::warn!(
            "leader-consistent reads use lease mode by default; this requires bounded clock drift (NTP/PTP). Use --read-policy read_index when that is not guaranteed"
        ),
        ReadPolicyMode::ReadIndex => tracing::info!("leader-consistent reads use ReadIndex"),
    }

    let wal_fsync = matchengine_raft::store::config::wal_fsync_from_env();
    let min_voters = if wal_fsync.is_none() {
        matchengine_raft::store::config::NO_FSYNC_MIN_VOTERS
    } else {
        1
    };
    match wal_fsync {
        None => tracing::warn!(
            min_voters,
            "WAL fsync is disabled (RAFT_WAL_FSYNC_MS unset); a crash can lose acknowledged writes and the cluster requires at least 4 voters"
        ),
        Some(interval) if interval.is_zero() => tracing::info!("WAL fsync: every append"),
        Some(interval) => {
            tracing::info!(interval_ms = interval.as_millis(), "WAL fsync: periodic")
        }
    }
    let mut storage = StorageConfig::new(options.data_dir)
        .with_instance_prefix(options.instance_prefix)
        .with_max_events(options.max_events)
        .with_snapshot_interval(Duration::from_millis(options.snapshot_interval_ms))
        .with_wal_fsync(wal_fsync)
        .with_min_voters(min_voters)
        .with_read_policy(options.read_policy)
        .with_async_wal(options.async_wal);
    if let Some(path) = options.match_event_log {
        storage = storage.with_match_event_log(path);
    }
    if options.market_data_log.is_some() || options.market_data_aeron_channel.is_some() {
        let mut config = match options.market_data_log {
            Some(path) => MarketDataConfig::jsonl(path),
            None => MarketDataConfig::default(),
        };
        config.depth_levels = options.market_data_depth;
        config.status_every = options.market_data_status_every;
        config.leader_only = options.market_data_leader_only;
        config.aeron_channel = options.market_data_aeron_channel;
        config.aeron_stream_base = options.market_data_aeron_stream_base;
        config.aeron_dir = options.market_data_aeron_dir;
        config.aeron_embedded_driver = options.market_data_aeron_embedded_driver;
        config.snapshot_every_ms = options.market_data_aeron_snapshot_every_ms;
        if !options.market_data_bar_intervals_ms.is_empty() {
            config.bar_intervals_ms = options.market_data_bar_intervals_ms;
        }
        storage = storage.with_market_data(config);
    }

    start_match_engine_node_with_storage(
        options.id,
        options.http_addr,
        matchengine_raft::default_raft_config(),
        storage,
    )
    .await
}

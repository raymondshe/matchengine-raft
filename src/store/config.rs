//! Node storage layout and runtime configuration.
//!
//! Every node owns a directory so that several nodes can share one machine (the tests do exactly
//! that). The layout is:
//!
//! ```text
//! <data_dir>/<instance_prefix>-<node_id>/
//!   log/
//!     state            # vote + last purged log id, written atomically
//!     0000000000.wal   # append-only segments, fsync policy is configurable
//!   snapshot/
//!     current.snap     # latest state machine snapshot, written atomically
//! ```

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use crate::DEFAULT_SNAPSHOT_INTERVAL;
use crate::MatchEngineNodeId;
use crate::market_data::MarketDataConfig;
use crate::network::read_policy::ReadPolicyMode;
use crate::store::state_machine::DEFAULT_MAX_EVENTS;

/// Environment variable that overrides [`StorageConfig::data_dir`].
pub const DATA_DIR_ENV: &str = "RAFT_DATA_DIR";
/// Environment variable that overrides [`StorageConfig::instance_prefix`].
pub const INSTANCE_PREFIX_ENV: &str = "RAFT_INSTANCE_PREFIX";
/// Environment variable that sets [`StorageConfig::match_event_log`].
pub const MATCH_EVENT_LOG_ENV: &str = "MATCH_EVENT_LOG";
/// Environment variable that enables the market-data JSONL feed.
pub const MARKET_DATA_LOG_ENV: &str = "MARKET_DATA_LOG";
/// Environment variable that overrides the match-event retention limit.
pub const MAX_EVENTS_ENV: &str = "MAX_EVENTS";
/// Environment variable that overrides the periodic snapshot interval in milliseconds.
pub const SNAPSHOT_INTERVAL_MS_ENV: &str = "SNAPSHOT_INTERVAL_MS";
/// Environment variable that sets the WAL fsync policy in milliseconds.
///
/// Unset means no fsync; `0` fsyncs every append; `N > 0` fsyncs every N ms.
pub const WAL_FSYNC_MS_ENV: &str = "RAFT_WAL_FSYNC_MS";

/// Default root directory for node data.
pub const DEFAULT_DATA_DIR: &str = "/tmp/matchengine-raft";
/// Default prefix used when naming a node directory.
pub const DEFAULT_INSTANCE_PREFIX: &str = "matchengine";
/// Voter floor used when WAL fsync is disabled.
///
/// Four voters put a committed entry on at least three nodes, so one node
/// losing its unsynced tail still leaves at least two copies.
pub const NO_FSYNC_MIN_VOTERS: usize = 4;

/// Where a node keeps its durable state.
#[derive(Clone, Debug)]
pub struct StorageConfig {
    /// Root directory; each node gets a subdirectory below it.
    pub data_dir: PathBuf,

    /// Prefix of the per-node directory name. Change it to run independent
    /// clusters from the same `data_dir`.
    pub instance_prefix: String,

    /// Optional path of a JSONL file that receives every applied match event
    /// (fills, rests, cancels). `None` discards match results, which is the default.
    ///
    /// This is a debugging aid: the file is node-local, is not replicated, and
    /// replays after a restart. See [`crate::match_result::FileLogSink`].
    pub match_event_log: Option<PathBuf>,

    /// Optional market-data egress configuration. `None` keeps the feed off;
    /// enabling it installs the tap on the state machine and starts the
    /// publisher task bounded by this configuration.
    pub market_data: Option<MarketDataConfig>,

    /// Maximum number of match events retained in the replicated event log.
    /// The same limit bounds the client-order-id dedupe map.
    pub max_events: usize,

    /// How often the node asks Raft to build a snapshot when new entries have
    /// been applied. `Duration::ZERO` disables the timer; the log-count policy
    /// still applies.
    pub snapshot_interval: Duration,

    /// WAL fsync policy.
    ///
    /// `None` disables fsync, `Some(Duration::ZERO)` fsyncs every append, and
    /// `Some(interval)` fsyncs the active segment every `interval`.
    pub wal_fsync: Option<Duration>,

    /// Minimum voter count required for initialization, membership changes and
    /// writes. The binary sets 4 when WAL fsync is disabled; library callers
    /// default to 1.
    pub min_voters: usize,

    /// Leader-consistent read policy. `ReadIndex` is the default; `Lease` is
    /// opt-in and falls back to `ReadIndex` when the lease is stale.
    pub read_policy: ReadPolicyMode,

    /// Prototype asynchronous WAL append. Only effective with the `async-wal`
    /// Cargo feature and `RAFT_WAL_FSYNC_MS=0`; off by default.
    pub async_wal: bool,
}

/// Reads the WAL fsync policy from `RAFT_WAL_FSYNC_MS`.
///
/// Unset means no fsync; `0` fsyncs every append; `N > 0` fsyncs every N ms.
pub fn wal_fsync_from_env() -> Option<Duration> {
    match std::env::var(WAL_FSYNC_MS_ENV) {
        Ok(value) => match value.parse::<u64>() {
            Ok(ms) => Some(Duration::from_millis(ms)),
            Err(_) => {
                tracing::warn!(
                    value,
                    "RAFT_WAL_FSYNC_MS must be a non-negative integer; WAL fsync is disabled"
                );
                None
            }
        },
        Err(_) => None,
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        let data_dir = std::env::var_os(DATA_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR));
        let instance_prefix = std::env::var(INSTANCE_PREFIX_ENV)
            .unwrap_or_else(|_| DEFAULT_INSTANCE_PREFIX.to_string());
        let match_event_log = std::env::var_os(MATCH_EVENT_LOG_ENV).map(PathBuf::from);
        let market_data = std::env::var_os(MARKET_DATA_LOG_ENV)
            .map(|path| MarketDataConfig::jsonl(PathBuf::from(path)));
        let max_events = std::env::var(MAX_EVENTS_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_MAX_EVENTS);
        let snapshot_interval = std::env::var(SNAPSHOT_INTERVAL_MS_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_SNAPSHOT_INTERVAL);

        let wal_fsync = wal_fsync_from_env();
        let min_voters = if wal_fsync.is_none() {
            NO_FSYNC_MIN_VOTERS
        } else {
            1
        };

        Self {
            data_dir,
            instance_prefix,
            match_event_log,
            market_data,
            max_events,
            snapshot_interval,
            wal_fsync,
            min_voters,
            read_policy: ReadPolicyMode::default(),
            async_wal: false,
        }
    }
}

impl StorageConfig {
    /// Uses `data_dir` with the default instance prefix.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            instance_prefix: DEFAULT_INSTANCE_PREFIX.to_string(),
            match_event_log: None,
            market_data: None,
            max_events: DEFAULT_MAX_EVENTS,
            snapshot_interval: DEFAULT_SNAPSHOT_INTERVAL,
            wal_fsync: None,
            min_voters: 1,
            read_policy: ReadPolicyMode::default(),
            async_wal: false,
        }
    }

    /// Sets the instance prefix, e.g. to isolate two clusters sharing a disk.
    pub fn with_instance_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.instance_prefix = prefix.into();
        self
    }

    /// Writes applied match events to `path` as JSON lines (debugging aid).
    pub fn with_match_event_log(mut self, path: impl Into<PathBuf>) -> Self {
        self.match_event_log = Some(path.into());
        self
    }

    /// Enables the market-data egress with `config`.
    pub fn with_market_data(mut self, config: MarketDataConfig) -> Self {
        self.market_data = Some(config);
        self
    }

    /// Sets the match-event retention limit (also bounds submission dedupe).
    pub fn with_max_events(mut self, max_events: usize) -> Self {
        self.max_events = max_events;
        self
    }

    /// Sets the periodic snapshot interval; zero disables the timer.
    pub fn with_snapshot_interval(mut self, interval: Duration) -> Self {
        self.snapshot_interval = interval;
        self
    }

    /// Sets the WAL fsync policy.
    ///
    /// `None` disables fsync, `Some(Duration::ZERO)` fsyncs every append, and
    /// `Some(interval)` fsyncs the active segment every `interval`.
    pub fn with_wal_fsync(mut self, fsync: Option<Duration>) -> Self {
        self.wal_fsync = fsync;
        self
    }

    /// Sets the minimum voter count required for initialization, membership
    /// changes and writes.
    pub fn with_min_voters(mut self, min_voters: usize) -> Self {
        self.min_voters = min_voters.max(1);
        self
    }

    /// Sets the leader-consistent read policy.
    pub fn with_read_policy(mut self, read_policy: ReadPolicyMode) -> Self {
        self.read_policy = read_policy;
        self
    }

    /// Enables the prototype asynchronous WAL append path.
    ///
    /// Requires a binary built with the `async-wal` Cargo feature and
    /// `RAFT_WAL_FSYNC_MS=0` (every append); otherwise startup returns an error.
    pub fn with_async_wal(mut self, async_wal: bool) -> Self {
        self.async_wal = async_wal;
        self
    }

    /// Directory holding this node's durable state.
    pub fn node_dir(&self, node_id: MatchEngineNodeId) -> PathBuf {
        self.data_dir
            .join(format!("{}-{node_id}", self.instance_prefix))
    }

    /// Directory holding the Raft log segments.
    pub fn log_dir(&self, node_id: MatchEngineNodeId) -> PathBuf {
        self.node_dir(node_id).join("log")
    }

    /// Directory holding the state machine snapshot.
    pub fn snapshot_dir(&self, node_id: MatchEngineNodeId) -> PathBuf {
        self.node_dir(node_id).join("snapshot")
    }
}

/// Best-effort fsync of a directory, so a rename or unlink survives a crash.
pub(crate) fn sync_dir(dir: &Path) -> std::io::Result<()> {
    // Opening a directory for reading is not possible on every platform; failing to
    // fsync it only weakens crash safety, so it must not fail the operation.
    match std::fs::File::open(dir) {
        Ok(f) => f.sync_all(),
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_dirs_are_isolated_per_node() {
        let config = StorageConfig::new("/data").with_instance_prefix("cluster-a");

        assert_eq!(Path::new("/data/cluster-a-1"), config.node_dir(1));
        assert_eq!(Path::new("/data/cluster-a-2"), config.node_dir(2));
        assert_ne!(config.log_dir(1), config.log_dir(2));
        assert_eq!(Path::new("/data/cluster-a-1/log"), config.log_dir(1));
        assert_eq!(
            Path::new("/data/cluster-a-1/snapshot"),
            config.snapshot_dir(1)
        );
    }

    #[test]
    fn snapshot_interval_defaults_to_thirty_minutes_and_is_configurable() {
        assert_eq!(
            DEFAULT_SNAPSHOT_INTERVAL,
            StorageConfig::new("/data").snapshot_interval
        );
        assert_eq!(
            Duration::from_millis(250),
            StorageConfig::new("/data")
                .with_snapshot_interval(Duration::from_millis(250))
                .snapshot_interval
        );
    }

    #[test]
    fn match_event_log_is_opt_in() {
        assert!(StorageConfig::new("/data").match_event_log.is_none());

        let config = StorageConfig::new("/data").with_match_event_log("/data/events.jsonl");
        assert_eq!(
            Some(PathBuf::from("/data/events.jsonl")),
            config.match_event_log
        );
    }

    #[test]
    fn event_retention_is_configurable() {
        assert_eq!(DEFAULT_MAX_EVENTS, StorageConfig::new("/data").max_events);
        let config = StorageConfig::new("/data").with_max_events(32);
        assert_eq!(32, config.max_events);
    }

    #[test]
    fn min_voters_defaults_to_one_and_is_configurable() {
        assert_eq!(1, StorageConfig::new("/data").min_voters);
        assert_eq!(4, StorageConfig::new("/data").with_min_voters(4).min_voters);
    }

    #[test]
    fn async_wal_defaults_to_disabled_and_is_configurable() {
        assert!(!StorageConfig::new("/data").async_wal);
        assert!(StorageConfig::new("/data").with_async_wal(true).async_wal);
    }

    #[test]
    fn read_policy_defaults_to_read_index_and_is_configurable() {
        assert_eq!(
            ReadPolicyMode::ReadIndex,
            StorageConfig::new("/data").read_policy
        );
        assert_eq!(
            ReadPolicyMode::Lease,
            StorageConfig::new("/data")
                .with_read_policy(ReadPolicyMode::Lease)
                .read_policy
        );
    }

    #[test]
    fn wal_fsync_defaults_to_disabled_and_is_configurable() {
        assert!(StorageConfig::new("/data").wal_fsync.is_none());
        assert_eq!(
            Some(Duration::ZERO),
            StorageConfig::new("/data")
                .with_wal_fsync(Some(Duration::ZERO))
                .wal_fsync
        );
        assert_eq!(
            Some(Duration::from_millis(50)),
            StorageConfig::new("/data")
                .with_wal_fsync(Some(Duration::from_millis(50)))
                .wal_fsync
        );
    }

    #[test]
    fn market_data_is_opt_in() {
        assert!(StorageConfig::new("/data").market_data.is_none());

        let config =
            StorageConfig::new("/data").with_market_data(MarketDataConfig::jsonl("/data/md.jsonl"));
        let market_data = config.market_data.expect("market data enabled");
        assert_eq!(Some(PathBuf::from("/data/md.jsonl")), market_data.log_path);
    }
}

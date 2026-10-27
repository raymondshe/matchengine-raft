//! Filesystem layout for a node's durable state.
//!
//! Every node owns a directory so that several nodes can share one machine (the tests do exactly
//! that). The layout is:
//!
//! ```text
//! <data_dir>/<instance_prefix>-<node_id>/
//!   log/
//!     state            # vote + last purged log id, written atomically
//!     0000000000.wal   # append-only segments, one fsync per append
//!   snapshot/
//!     current.snap     # latest state machine snapshot, written atomically
//! ```

use std::path::Path;
use std::path::PathBuf;

use crate::MatchEngineNodeId;

/// Environment variable that overrides [`StorageConfig::data_dir`].
pub const DATA_DIR_ENV: &str = "RAFT_DATA_DIR";
/// Environment variable that overrides [`StorageConfig::instance_prefix`].
pub const INSTANCE_PREFIX_ENV: &str = "RAFT_INSTANCE_PREFIX";
/// Environment variable that sets [`StorageConfig::match_event_log`].
pub const MATCH_EVENT_LOG_ENV: &str = "MATCH_EVENT_LOG";

/// Default root directory for node data.
pub const DEFAULT_DATA_DIR: &str = "/tmp/matchengine-raft";
/// Default prefix used when naming a node directory.
pub const DEFAULT_INSTANCE_PREFIX: &str = "matchengine";

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
}

impl Default for StorageConfig {
    fn default() -> Self {
        let data_dir = std::env::var_os(DATA_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR));
        let instance_prefix = std::env::var(INSTANCE_PREFIX_ENV)
            .unwrap_or_else(|_| DEFAULT_INSTANCE_PREFIX.to_string());
        let match_event_log = std::env::var_os(MATCH_EVENT_LOG_ENV).map(PathBuf::from);

        Self {
            data_dir,
            instance_prefix,
            match_event_log,
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

    /// Directory holding this node's durable state.
    pub fn node_dir(&self, node_id: MatchEngineNodeId) -> PathBuf {
        self.data_dir.join(format!("{}-{node_id}", self.instance_prefix))
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
        assert_eq!(Path::new("/data/cluster-a-1/snapshot"), config.snapshot_dir(1));
    }

    #[test]
    fn match_event_log_is_opt_in() {
        assert!(StorageConfig::new("/data").match_event_log.is_none());

        let config = StorageConfig::new("/data").with_match_event_log("/data/events.jsonl");
        assert_eq!(Some(PathBuf::from("/data/events.jsonl")), config.match_event_log);
    }
}

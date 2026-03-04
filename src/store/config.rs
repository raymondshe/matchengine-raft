use clap::Parser;

use serde::Deserialize;
use serde::Serialize;

/// Configuration for the Raft storage layer.
///
/// This struct defines all configurable parameters for the persistent storage
/// system, including paths for snapshots and journal files, as well as
/// snapshot triggering behavior.
#[derive(Clone, Debug, Serialize, Deserialize, Parser)]
pub struct Config {
    /// Directory path where state machine snapshots will be stored.
    ///
    /// Snapshots are periodic saves of the entire state machine state,
    /// allowing for faster recovery by replaying only the logs after
    /// the snapshot was taken.
    #[clap(long, env = "RAFT_SNAPSHOT_PATH", default_value = "/tmp/snapshot")]
    pub snapshot_path: String,

    /// Prefix string used for naming storage files.
    ///
    /// This prefix helps identify which Raft instance the files belong to,
    /// useful when running multiple clusters or instances on the same machine.
    #[clap(long, env = "RAFT_INSTANCE_PREFIX", default_value = "match")]
    pub instance_prefix: String,

    /// Directory path where the Raft journal (write-ahead log) will be stored.
    ///
    /// The journal contains all Raft log entries persisted using Sled,
    /// ensuring durability of all operations before they are applied.
    #[clap(long, env = "RAFT_JOURNAL_PATH", default_value = "/tmp/journal")]
    pub journal_path: String,

    /// Number of log entries after which a new snapshot should be created.
    ///
    /// A lower value creates snapshots more frequently, reducing recovery
    /// time but increasing I/O overhead. A higher value reduces I/O but
    /// may increase recovery time.
    #[clap(long, env = "RAFT_SNAPSHOT_PER_EVENTS", default_value = "500")]
    pub snapshot_per_events: u32,
}

impl Default for Config {
    /// Creates a default configuration using default values for all fields.
    ///
    /// The default values are suitable for development and testing environments.
    /// For production use, consider adjusting paths and snapshot frequency
    /// based on your workload requirements.
    fn default() -> Self {
        <Self as Parser>::parse_from(&Vec::<&'static str>::new())
    }
}
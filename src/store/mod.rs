//! Persistent Raft storage: segmented WAL for the log, snapshots for the state machine.

pub mod config;
pub mod log_store;
pub mod state_machine;
pub mod types;

pub use self::config::StorageConfig;
pub use self::log_store::LogStore;
pub use self::state_machine::StateMachineData;
pub use self::state_machine::StateMachineStore;
pub use self::state_machine::StoredSnapshot;
pub use self::types::MatchEngineRequest;
pub use self::types::MatchEngineResponse;

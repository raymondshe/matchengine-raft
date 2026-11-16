//! Lease reads by default, with a ReadIndex fallback.
//!
//! `LocalLease` is the default: it uses OpenRaft's caller-side lease cache and
//! falls back to the core `LeaseRead` path on a cache miss, then to `ReadIndex`
//! if the lease is stale. It assumes bounded clock drift between nodes. A
//! caller-side cache hit can overtake an unfinished write; a client that must
//! observe a write should await its response first. `Lease` keeps the core
//! `ReadPolicy::LeaseRead` path for comparison; set `--read-policy read_index`
//! to force a fresh quorum round when clock discipline is not guaranteed.
//!
//! `/read` stays a local, possibly stale read and is intentionally not routed
//! through this policy.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use openraft::ReadPolicy;
use openraft::errors::LinearizableReadError;
use openraft::errors::RaftError;
use openraft::errors::decompose::DecomposeResult;
use openraft::raft::linearizable_read::Linearizer;
use serde::Deserialize;
use serde::Serialize;

use crate::MatchEngineRaft;
use crate::MatchEngineTypeConfig;

type C = MatchEngineTypeConfig;

/// Environment variable accepted by the node binary (`--read-policy`).
pub const READ_POLICY_ENV: &str = "MATCHENGINE_READ_POLICY";

/// Read policy for leader-consistent reads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReadPolicyMode {
    /// Confirm leadership with a fresh quorum round on every read.
    ReadIndex,
    /// Try a recent quorum acknowledgement through OpenRaft's core path and
    /// fall back to `ReadIndex`.
    Lease,
    /// Try OpenRaft's caller-side local lease cache first, falling back to the
    /// core lease path and then `ReadIndex` on a miss or stale lease.
    #[default]
    LocalLease,
}

impl ReadPolicyMode {
    /// Stable name used by `/read-policy/status` and the CLI.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadIndex => "read_index",
            Self::Lease => "lease",
            Self::LocalLease => "local_lease",
        }
    }
}

impl fmt::Display for ReadPolicyMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReadPolicyMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "read_index" | "read-index" | "readindex" | "index" => Ok(Self::ReadIndex),
            "lease" | "lease_read" | "lease-read" => Ok(Self::Lease),
            "local_lease" | "local-lease" | "locallease" => Ok(Self::LocalLease),
            other => Err(format!(
                "unknown read policy {other:?}; expected read_index, lease or local_lease"
            )),
        }
    }
}

/// The four leader-consistent read paths that share one policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadPath {
    /// `POST /linearizable_read`.
    LinearizableRead,
    /// `POST /events`.
    Events,
    /// gRPC `EventService.StreamEvents` anchor.
    StreamEvents,
    /// `POST /get_linearizer`, used by follower reads.
    GetLinearizer,
}

impl ReadPath {
    const fn index(self) -> usize {
        match self {
            Self::LinearizableRead => 0,
            Self::Events => 1,
            Self::StreamEvents => 2,
            Self::GetLinearizer => 3,
        }
    }

    /// Stable name for diagnostics and benchmark output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LinearizableRead => "linearizable_read",
            Self::Events => "events",
            Self::StreamEvents => "stream_events",
            Self::GetLinearizer => "get_linearizer",
        }
    }
}

#[derive(Debug, Default)]
struct PathCounters {
    lease_attempts: AtomicU64,
    lease_hits: AtomicU64,
    lease_fallbacks: AtomicU64,
    lease_failures: AtomicU64,
    read_index_attempts: AtomicU64,
    read_index_failures: AtomicU64,
}

impl PathCounters {
    fn snapshot(&self) -> ReadPathMetrics {
        ReadPathMetrics {
            lease_attempts: self.lease_attempts.load(Ordering::Relaxed),
            lease_hits: self.lease_hits.load(Ordering::Relaxed),
            lease_fallbacks: self.lease_fallbacks.load(Ordering::Relaxed),
            lease_failures: self.lease_failures.load(Ordering::Relaxed),
            read_index_attempts: self.read_index_attempts.load(Ordering::Relaxed),
            read_index_failures: self.read_index_failures.load(Ordering::Relaxed),
        }
    }
}

/// Counters for one read path.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReadPathMetrics {
    /// Number of reads that entered the lease path.
    pub lease_attempts: u64,
    /// Lease reads that returned a linearizer without a fallback.
    pub lease_hits: u64,
    /// Lease reads that fell back to `ReadIndex`.
    pub lease_fallbacks: u64,
    /// Lease reads that failed before a fallback was possible.
    pub lease_failures: u64,
    /// `ReadIndex` attempts, including fallback attempts.
    pub read_index_attempts: u64,
    /// `ReadIndex` attempts that did not return a linearizer.
    pub read_index_failures: u64,
}

impl ReadPathMetrics {
    fn add_assign(&mut self, other: &Self) {
        self.lease_attempts += other.lease_attempts;
        self.lease_hits += other.lease_hits;
        self.lease_fallbacks += other.lease_fallbacks;
        self.lease_failures += other.lease_failures;
        self.read_index_attempts += other.read_index_attempts;
        self.read_index_failures += other.read_index_failures;
    }
}

/// Snapshot returned by `GET /read-policy/status`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadPolicyStatus {
    /// `"read_index"`, `"lease"` or `"local_lease"`.
    pub mode: String,
    /// `/linearizable_read`.
    pub linearizable_read: ReadPathMetrics,
    /// `/events`.
    pub events: ReadPathMetrics,
    /// gRPC `StreamEvents`.
    pub stream_events: ReadPathMetrics,
    /// `/get_linearizer`.
    pub get_linearizer: ReadPathMetrics,
    /// Sum of all four paths.
    pub totals: ReadPathMetrics,
}

#[derive(Debug, Default)]
struct ReadPolicyMetrics {
    paths: [PathCounters; 4],
}

/// Cloneable read-policy runtime shared by HTTP and gRPC handlers.
#[derive(Clone, Debug)]
pub struct ReadPolicyRuntime {
    mode: ReadPolicyMode,
    metrics: Arc<ReadPolicyMetrics>,
}

impl ReadPolicyRuntime {
    /// Creates a runtime with a fixed mode and zeroed counters.
    pub fn new(mode: ReadPolicyMode) -> Self {
        Self {
            mode,
            metrics: Arc::new(ReadPolicyMetrics::default()),
        }
    }

    /// The configured mode.
    pub const fn mode(&self) -> ReadPolicyMode {
        self.mode
    }

    /// Returns a snapshot for `GET /read-policy/status`.
    pub fn status(&self) -> ReadPolicyStatus {
        let paths: [ReadPathMetrics; 4] =
            std::array::from_fn(|index| self.metrics.paths[index].snapshot());
        let mut totals = ReadPathMetrics::default();
        for path in &paths {
            totals.add_assign(path);
        }
        ReadPolicyStatus {
            mode: self.mode.as_str().to_string(),
            linearizable_read: paths[ReadPath::LinearizableRead.index()],
            events: paths[ReadPath::Events.index()],
            stream_events: paths[ReadPath::StreamEvents.index()],
            get_linearizer: paths[ReadPath::GetLinearizer.index()],
            totals,
        }
    }

    /// Obtains a linearizer for one read path.
    ///
    /// In lease mode a stale lease is not an error: the call falls back to
    /// `ReadIndex`. The returned outer error is a fatal Raft error; the inner
    /// error is the same `LinearizableReadError` the existing handlers return.
    pub async fn linearizer(
        &self,
        raft: &MatchEngineRaft,
        path: ReadPath,
    ) -> Result<Result<Linearizer<C>, LinearizableReadError<C>>, RaftError<C>> {
        let counters = &self.metrics.paths[path.index()];

        if self.mode == ReadPolicyMode::ReadIndex {
            counters.read_index_attempts.fetch_add(1, Ordering::Relaxed);
            let result = raft
                .get_read_linearizer(ReadPolicy::ReadIndex)
                .await
                .decompose();
            if !matches!(&result, Ok(Ok(_))) {
                counters.read_index_failures.fetch_add(1, Ordering::Relaxed);
            }
            return result;
        }

        counters.lease_attempts.fetch_add(1, Ordering::Relaxed);
        let lease = if self.mode == ReadPolicyMode::LocalLease {
            raft.get_local_read_linearizer(ReadPolicy::LeaseRead)
                .await
                .decompose()
        } else {
            raft.get_read_linearizer(ReadPolicy::LeaseRead)
                .await
                .decompose()
        };
        match lease {
            Ok(Ok(linearizer)) => {
                counters.lease_hits.fetch_add(1, Ordering::Relaxed);
                Ok(Ok(linearizer))
            }
            Ok(Err(_lease_error)) => {
                counters.lease_fallbacks.fetch_add(1, Ordering::Relaxed);
                counters.read_index_attempts.fetch_add(1, Ordering::Relaxed);
                let result = raft
                    .get_read_linearizer(ReadPolicy::ReadIndex)
                    .await
                    .decompose();
                if !matches!(&result, Ok(Ok(_))) {
                    counters.read_index_failures.fetch_add(1, Ordering::Relaxed);
                }
                result
            }
            Err(error) => {
                counters.lease_failures.fetch_add(1, Ordering::Relaxed);
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_read_policy_names() {
        assert_eq!(ReadPolicyMode::ReadIndex, "read_index".parse().unwrap());
        assert_eq!(ReadPolicyMode::ReadIndex, "read-index".parse().unwrap());
        assert_eq!(ReadPolicyMode::Lease, "lease".parse().unwrap());
        assert_eq!(ReadPolicyMode::Lease, "lease_read".parse().unwrap());
        assert_eq!(ReadPolicyMode::LocalLease, "local_lease".parse().unwrap());
        assert_eq!(ReadPolicyMode::LocalLease, "local-lease".parse().unwrap());
        assert!("sticky".parse::<ReadPolicyMode>().is_err());
        assert_eq!("lease", ReadPolicyMode::Lease.to_string());
        assert_eq!("local_lease", ReadPolicyMode::LocalLease.to_string());
        assert_eq!(ReadPolicyMode::LocalLease, ReadPolicyMode::default());
    }

    #[test]
    fn status_aggregates_per_path_counters() {
        let runtime = ReadPolicyRuntime::new(ReadPolicyMode::Lease);
        let linearizable = &runtime.metrics.paths[ReadPath::LinearizableRead.index()];
        linearizable.lease_attempts.fetch_add(2, Ordering::Relaxed);
        linearizable.lease_hits.fetch_add(1, Ordering::Relaxed);
        linearizable.lease_fallbacks.fetch_add(1, Ordering::Relaxed);
        linearizable
            .read_index_attempts
            .fetch_add(1, Ordering::Relaxed);
        let events = &runtime.metrics.paths[ReadPath::Events.index()];
        events.lease_attempts.fetch_add(1, Ordering::Relaxed);
        events.lease_failures.fetch_add(1, Ordering::Relaxed);

        let status = runtime.status();
        assert_eq!("lease", status.mode);
        assert_eq!(2, status.linearizable_read.lease_attempts);
        assert_eq!(1, status.linearizable_read.lease_hits);
        assert_eq!(1, status.linearizable_read.lease_fallbacks);
        assert_eq!(1, status.linearizable_read.read_index_attempts);
        assert_eq!(3, status.totals.lease_attempts);
        assert_eq!(1, status.totals.lease_failures);
        assert_eq!(0, status.stream_events.lease_attempts);
    }
}

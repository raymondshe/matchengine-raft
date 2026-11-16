//! Process-local transport counters for the gRPC Raft transport.
//!
//! These complement OpenRaft's `data_metrics`/`server_metrics`: they count the
//! transport sessions this process actually opened, and measure snapshot
//! transfer time on both the sending and receiving side.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;

/// Counters shared by every gRPC client/server in one node process.
#[derive(Debug, Default)]
pub struct TransportMetrics {
    heartbeat_streams: AtomicU64,
    replication_streams: AtomicU64,
    heartbeat_requests: AtomicU64,
    replication_requests: AtomicU64,
    snapshot_sent: AtomicU64,
    snapshot_sent_bytes: AtomicU64,
    snapshot_sent_micros: AtomicU64,
    snapshot_sent_max_micros: AtomicU64,
    snapshot_send_failures: AtomicU64,
    snapshot_received: AtomicU64,
    snapshot_received_bytes: AtomicU64,
    snapshot_received_micros: AtomicU64,
    snapshot_received_max_micros: AtomicU64,
    snapshot_receive_failures: AtomicU64,
}

/// Immutable view of [`TransportMetrics`] for `/metrics` and the console.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransportMetricsSnapshot {
    pub heartbeat_streams: u64,
    pub replication_streams: u64,
    pub heartbeat_requests: u64,
    pub replication_requests: u64,
    pub snapshot_sent: u64,
    pub snapshot_sent_bytes: u64,
    pub snapshot_sent_micros: u64,
    pub snapshot_sent_max_micros: u64,
    pub snapshot_sent_avg_micros: u64,
    pub snapshot_send_failures: u64,
    pub snapshot_received: u64,
    pub snapshot_received_bytes: u64,
    pub snapshot_received_micros: u64,
    pub snapshot_received_max_micros: u64,
    pub snapshot_received_avg_micros: u64,
    pub snapshot_receive_failures: u64,
}

impl TransportMetrics {
    /// Wraps the counters in a shared handle.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Records one `AppendStream` session by role.
    pub fn record_stream(&self, heartbeat: bool) {
        let counter = if heartbeat {
            &self.heartbeat_streams
        } else {
            &self.replication_streams
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Records one AppendEntries request sent over an `AppendStream`.
    pub fn record_append_request(&self, heartbeat: bool) {
        let counter = if heartbeat {
            &self.heartbeat_requests
        } else {
            &self.replication_requests
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Records one completed snapshot send. `ok` is false when the RPC failed
    /// or was cancelled.
    pub fn record_snapshot_sent(&self, elapsed: Duration, bytes: usize, ok: bool) {
        let micros = duration_micros(elapsed);
        self.snapshot_sent.fetch_add(1, Ordering::Relaxed);
        self.snapshot_sent_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.snapshot_sent_micros
            .fetch_add(micros, Ordering::Relaxed);
        self.snapshot_sent_max_micros
            .fetch_max(micros, Ordering::Relaxed);
        if !ok {
            self.snapshot_send_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records one completed snapshot receive/install.
    pub fn record_snapshot_received(&self, elapsed: Duration, bytes: usize, ok: bool) {
        let micros = duration_micros(elapsed);
        self.snapshot_received.fetch_add(1, Ordering::Relaxed);
        self.snapshot_received_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.snapshot_received_micros
            .fetch_add(micros, Ordering::Relaxed);
        self.snapshot_received_max_micros
            .fetch_max(micros, Ordering::Relaxed);
        if !ok {
            self.snapshot_receive_failures
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Takes a consistent-enough point-in-time view for the metrics endpoint.
    pub fn snapshot(&self) -> TransportMetricsSnapshot {
        let sent = self.snapshot_sent.load(Ordering::Relaxed);
        let received = self.snapshot_received.load(Ordering::Relaxed);
        TransportMetricsSnapshot {
            heartbeat_streams: self.heartbeat_streams.load(Ordering::Relaxed),
            replication_streams: self.replication_streams.load(Ordering::Relaxed),
            heartbeat_requests: self.heartbeat_requests.load(Ordering::Relaxed),
            replication_requests: self.replication_requests.load(Ordering::Relaxed),
            snapshot_sent: sent,
            snapshot_sent_bytes: self.snapshot_sent_bytes.load(Ordering::Relaxed),
            snapshot_sent_micros: self.snapshot_sent_micros.load(Ordering::Relaxed),
            snapshot_sent_max_micros: self.snapshot_sent_max_micros.load(Ordering::Relaxed),
            snapshot_sent_avg_micros: self
                .snapshot_sent_micros
                .load(Ordering::Relaxed)
                .checked_div(sent)
                .unwrap_or(0),
            snapshot_send_failures: self.snapshot_send_failures.load(Ordering::Relaxed),
            snapshot_received: received,
            snapshot_received_bytes: self.snapshot_received_bytes.load(Ordering::Relaxed),
            snapshot_received_micros: self.snapshot_received_micros.load(Ordering::Relaxed),
            snapshot_received_max_micros: self.snapshot_received_max_micros.load(Ordering::Relaxed),
            snapshot_received_avg_micros: self
                .snapshot_received_micros
                .load(Ordering::Relaxed)
                .checked_div(received)
                .unwrap_or(0),
            snapshot_receive_failures: self.snapshot_receive_failures.load(Ordering::Relaxed),
        }
    }
}

fn duration_micros(elapsed: Duration) -> u64 {
    elapsed.as_micros().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_averages_and_maxima() {
        let metrics = TransportMetrics::default();
        metrics.record_stream(true);
        metrics.record_stream(false);
        metrics.record_append_request(true);
        metrics.record_append_request(false);
        metrics.record_snapshot_sent(Duration::from_micros(100), 1_024, true);
        metrics.record_snapshot_sent(Duration::from_micros(300), 2_048, false);
        metrics.record_snapshot_received(Duration::from_micros(200), 4_096, true);

        let snapshot = metrics.snapshot();
        assert_eq!(1, snapshot.heartbeat_streams);
        assert_eq!(1, snapshot.replication_streams);
        assert_eq!(1, snapshot.heartbeat_requests);
        assert_eq!(1, snapshot.replication_requests);
        assert_eq!(2, snapshot.snapshot_sent);
        assert_eq!(3_072, snapshot.snapshot_sent_bytes);
        assert_eq!(200, snapshot.snapshot_sent_avg_micros);
        assert_eq!(300, snapshot.snapshot_sent_max_micros);
        assert_eq!(1, snapshot.snapshot_send_failures);
        assert_eq!(1, snapshot.snapshot_received);
        assert_eq!(4_096, snapshot.snapshot_received_bytes);
        assert_eq!(200, snapshot.snapshot_received_avg_micros);
    }
}

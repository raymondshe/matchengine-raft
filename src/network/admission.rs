//! Bounded in-flight admission for write requests.
//!
//! The matching engine's write sweep peaks around 256 concurrent requests;
//! beyond that, throughput falls and p99 latency rises sharply. This gate
//! rejects excess `/write` and `/write_batch` requests at the HTTP edge instead
//! of letting them queue behind Raft and apply work.
//!
//! The limit counts requests, not orders: one `/write_batch` with 128 orders
//! consumes one slot. Set the limit to zero to disable rejection while still
//! collecting in-flight/peak counters.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;

/// Default concurrent write-request limit.
///
/// The 2026-11-08 concurrency sweep measured the throughput peak at 256
/// in-flight writes; 1,024 and 4,096 in-flight requests had much worse p99.
pub const DEFAULT_MAX_INFLIGHT_WRITES: usize = 256;

/// Tracks and optionally bounds concurrent write requests on one node.
#[derive(Debug)]
pub struct AdmissionControl {
    limit: usize,
    in_flight: AtomicUsize,
    peak: AtomicUsize,
    admitted: AtomicU64,
    rejected: AtomicU64,
}

impl AdmissionControl {
    /// Creates a gate with `limit` concurrent requests; zero disables rejection.
    pub const fn new(limit: usize) -> Self {
        Self {
            limit,
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            admitted: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }
    }

    /// The configured concurrent-request limit; zero means unlimited.
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Tries to reserve one in-flight slot.
    ///
    /// Returns `None` when the limit is reached. The returned guard releases
    /// the slot when dropped, including on cancellation or error paths.
    pub fn try_enter(&self) -> Option<AdmissionGuard<'_>> {
        let mut current = self.in_flight.load(Ordering::Acquire);
        loop {
            if self.limit != 0 && current >= self.limit {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                return None;
            }

            match self.in_flight.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.peak.fetch_max(current + 1, Ordering::Relaxed);
                    self.admitted.fetch_add(1, Ordering::Relaxed);
                    return Some(AdmissionGuard { control: self });
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// Returns a point-in-time snapshot of the admission counters.
    pub fn status(&self) -> AdmissionStatus {
        AdmissionStatus {
            limit: self.limit,
            in_flight: self.in_flight.load(Ordering::Acquire),
            peak: self.peak.load(Ordering::Acquire),
            admitted: self.admitted.load(Ordering::Acquire),
            rejected: self.rejected.load(Ordering::Acquire),
        }
    }
}

impl Default for AdmissionControl {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_INFLIGHT_WRITES)
    }
}

/// Releases one in-flight slot on drop.
#[derive(Debug)]
pub struct AdmissionGuard<'a> {
    control: &'a AdmissionControl,
}

impl Drop for AdmissionGuard<'_> {
    fn drop(&mut self) {
        self.control.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Serializable admission snapshot served by `GET /admission/status`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct AdmissionStatus {
    /// Configured limit; zero means rejection is disabled.
    pub limit: usize,
    /// Requests currently holding a slot.
    pub in_flight: usize,
    /// Highest `in_flight` observed since startup.
    pub peak: usize,
    /// Requests admitted since startup.
    pub admitted: u64,
    /// Requests rejected because the limit was reached.
    pub rejected: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_rejects_and_guard_releases() {
        let admission = AdmissionControl::new(2);

        let first = admission.try_enter().expect("first slot");
        let second = admission.try_enter().expect("second slot");
        assert!(admission.try_enter().is_none());

        let status = admission.status();
        assert_eq!(2, status.limit);
        assert_eq!(2, status.in_flight);
        assert_eq!(2, status.peak);
        assert_eq!(2, status.admitted);
        assert_eq!(1, status.rejected);

        drop(first);
        assert_eq!(1, admission.status().in_flight);
        let third = admission.try_enter().expect("released slot");
        assert_eq!(2, admission.status().in_flight);
        assert_eq!(3, admission.status().admitted);
        assert_eq!(1, admission.status().rejected);
        drop(second);
        drop(third);
        assert_eq!(0, admission.status().in_flight);
    }

    #[test]
    fn zero_limit_tracks_without_rejecting() {
        let admission = AdmissionControl::new(0);
        let first = admission.try_enter().expect("unlimited");
        let second = admission.try_enter().expect("unlimited");
        let status = admission.status();
        assert_eq!(0, status.limit);
        assert_eq!(2, status.in_flight);
        assert_eq!(2, status.peak);
        assert_eq!(2, status.admitted);
        assert_eq!(0, status.rejected);
        drop(first);
        drop(second);
        assert_eq!(0, admission.status().in_flight);
    }

    #[test]
    fn status_serializes() {
        let status = AdmissionControl::new(256).status();
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(
            r#"{"limit":256,"in_flight":0,"peak":0,"admitted":0,"rejected":0}"#,
            json
        );
    }
}

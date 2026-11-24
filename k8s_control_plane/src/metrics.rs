//! Dependency-free Prometheus text metrics for the cell operator.
//!
//! The operator already runs an axum process for its manager sibling but has no
//! scrape endpoint of its own. These counters live in atomics, so a scrape is a
//! lock-free read and does not touch the reconcile path beyond a few
//! `fetch_add`s. They are deliberately coarse: reconcile count/failures,
//! duration, object writes, rollouts and leadership are enough to alert on a
//! wedged operator or a roll loop without adding a metrics crate (and its
//! MSRV/lockfile surface) to this crate.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Default)]
pub struct Metrics {
    reconciles: AtomicU64,
    failures: AtomicU64,
    duration_ms_sum: AtomicU64,
    duration_ms_max: AtomicU64,
    applied: AtomicU64,
    pruned: AtomicU64,
    status_writes: AtomicU64,
    rollouts: AtomicU64,
    action_failures: AtomicU64,
    leader: AtomicU64,
}

impl Metrics {
    pub fn set_leader(&self, leader: bool) {
        self.leader.store(u64::from(leader), Ordering::Relaxed);
    }

    pub fn observe_reconcile(
        &self,
        applied: usize,
        pruned: usize,
        status_writes: usize,
        rollouts: usize,
        action_failures: usize,
        elapsed: Duration,
    ) {
        let ms = elapsed.as_millis().min(u64::MAX as u128) as u64;
        self.reconciles.fetch_add(1, Ordering::Relaxed);
        self.duration_ms_sum.fetch_add(ms, Ordering::Relaxed);
        self.duration_ms_max.fetch_max(ms, Ordering::Relaxed);
        self.applied.fetch_add(applied as u64, Ordering::Relaxed);
        self.pruned.fetch_add(pruned as u64, Ordering::Relaxed);
        self.status_writes
            .fetch_add(status_writes as u64, Ordering::Relaxed);
        self.rollouts.fetch_add(rollouts as u64, Ordering::Relaxed);
        self.action_failures
            .fetch_add(action_failures as u64, Ordering::Relaxed);
    }

    pub fn observe_failure(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn render(&self) -> String {
        let reconciles = self.reconciles.load(Ordering::Relaxed);
        let average_ms = self
            .duration_ms_sum
            .load(Ordering::Relaxed)
            .checked_div(reconciles)
            .unwrap_or(0);
        let mut out = String::new();
        counter(
            &mut out,
            "match_operator_reconciles_total",
            reconciles,
            "Successful reconcile passes since start.",
        );
        counter(
            &mut out,
            "match_operator_reconcile_failures_total",
            self.failures.load(Ordering::Relaxed),
            "Reconcile passes that returned an error.",
        );
        gauge(
            &mut out,
            "match_operator_reconcile_duration_ms",
            average_ms,
            "Average successful reconcile duration in milliseconds.",
        );
        gauge(
            &mut out,
            "match_operator_reconcile_duration_ms_max",
            self.duration_ms_max.load(Ordering::Relaxed),
            "Slowest successful reconcile duration in milliseconds (lifetime).",
        );
        counter(
            &mut out,
            "match_operator_reconcile_duration_ms_sum",
            self.duration_ms_sum.load(Ordering::Relaxed),
            "Sum of successful reconcile durations; rate() gives a rolling average.",
        );
        counter(
            &mut out,
            "match_operator_applied_objects_total",
            self.applied.load(Ordering::Relaxed),
            "Desired objects server-side applied.",
        );
        counter(
            &mut out,
            "match_operator_pruned_objects_total",
            self.pruned.load(Ordering::Relaxed),
            "Managed objects pruned.",
        );
        counter(
            &mut out,
            "match_operator_status_writes_total",
            self.status_writes.load(Ordering::Relaxed),
            "CRD status writes.",
        );
        counter(
            &mut out,
            "match_operator_rollouts_total",
            self.rollouts.load(Ordering::Relaxed),
            "Pod rollout actions performed by the operator.",
        );
        counter(
            &mut out,
            "match_operator_action_failures_total",
            self.action_failures.load(Ordering::Relaxed),
            "Individual reconcile actions that failed.",
        );
        gauge(
            &mut out,
            "match_operator_leader",
            self.leader.load(Ordering::Relaxed),
            "1 while this replica holds the operator Lease.",
        );
        out
    }
}

fn counter(out: &mut String, name: &str, value: u64, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} counter");
    let _ = writeln!(out, "{name} {value}");
}

fn gauge(out: &mut String, name: &str, value: u64, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} gauge");
    let _ = writeln!(out, "{name} {value}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_render_prometheus_text() {
        let m = Metrics::default();
        m.set_leader(true);
        m.observe_reconcile(3, 1, 2, 1, 0, Duration::from_millis(40));
        m.observe_reconcile(1, 0, 1, 0, 1, Duration::from_millis(60));
        m.observe_failure();
        let text = m.render();
        assert!(text.contains("match_operator_reconciles_total 2"));
        assert!(text.contains("match_operator_reconcile_failures_total 1"));
        assert!(text.contains("match_operator_reconcile_duration_ms 50"));
        assert!(text.contains("match_operator_reconcile_duration_ms_max 60"));
        assert!(text.contains("match_operator_reconcile_duration_ms_sum 100"));
        assert!(text.contains("match_operator_applied_objects_total 4"));
        assert!(text.contains("match_operator_rollouts_total 1"));
        assert!(text.contains("match_operator_leader 1"));
        assert!(text.contains("# TYPE match_operator_rollouts_total counter"));
    }
}

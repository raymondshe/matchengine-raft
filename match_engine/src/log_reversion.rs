//! Leader-side recovery for a follower whose log reverted while it was down.
//!
//! With WAL fsync disabled, a restarted node can lose its unsynced tail. The
//! leader may still hold a replication match index at the newer tail, so it
//! never backfills the missing entries and `wait_for_recovery` never finishes
//! on the returning node. OpenRaft already has the fix for this: a one-shot
//! `allow_next_revert` on the leader lets the next conflict response reset that
//! peer's progress to `None`, after which normal probing selects snapshot
//! replication. This watcher only supplies the detection and the one-shot
//! trigger; it never pulls or installs a snapshot itself.
//!
//! # On races and read skew
//!
//! There is no atomic snapshot across two nodes: `matched` is the leader's
//! view at one instant and `peer_tail` is the peer's view at another. The
//! comparison is deliberately tolerant of that skew:
//!
//! - `matched` only ever moves forward (OpenRaft does not lower it without an
//!   explicit reversion grant), so a stale leader read is *conservative* — it
//!   can miss a reversion but cannot invent one.
//! - The action is a one-shot, speculative hint, not a mutation of data or
//!   membership. OpenRaft only consumes it when an actual conflict response
//!   arrives; arming it against a healthy peer is a no-op until such a
//!   conflict, and the next real conflict is exactly when a reset is wanted.
//! - Two consecutive, identical observations are required. A peer that is
//!   merely catching up changes `peer_tail` (and advances `matched` when it
//!   ACKs), which resets the count, so a slow-but-healthy follower is not
//!   mistaken for one that lost acknowledged entries.
//! - Leader identity and peer membership are both re-checked after the probe
//!   and again before the trigger, and the peer's own snapshot must carry the
//!   id we expect, so a stale address or a lost leadership cannot misdirect
//!   the call. OpenRaft also rejects the trigger if leadership moved.

use std::collections::BTreeMap;
use std::time::Duration;
use std::time::Instant;

use openraft::async_runtime::WatchReceiver;
use tokio::task::JoinHandle;

use crate::MatchEngineNodeId;
use crate::MatchEngineRaft;
use crate::network::console::NodeSnapshot;

/// How often the leader samples peer snapshots.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
/// Consecutive identical samples with a stale peer before acting.
const REQUIRED_SAMPLES: u8 = 2;
/// Cooldown per peer after a reset attempt.
const COOLDOWN: Duration = Duration::from_secs(120);
/// Bound on the read-only peer probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// One coherent-ish comparison: the leader's match index for a peer versus
/// that peer's own reported log tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Observation {
    matched: u64,
    peer_tail: u64,
}

impl Observation {
    /// The peer lost acknowledged entries if its own log tail is behind the
    /// index the leader believes it matched.
    fn new(matched: Option<u64>, peer_tail: Option<u64>) -> Option<Self> {
        let (matched, peer_tail) = matched.zip(peer_tail)?;
        (peer_tail < matched).then_some(Self { matched, peer_tail })
    }

    fn lag(&self) -> u64 {
        self.matched - self.peer_tail
    }
}

/// Advances the per-peer suspicion counter. Only an *identical* repeat
/// confirms a reversion: a peer that is catching up reports a different tail
/// (or a higher match index once it ACKs), which starts over at 1.
fn next_count(previous: Option<(u8, Observation)>, current: Observation) -> (u8, Observation) {
    let count = match previous {
        Some((count, prev)) if prev == current => count + 1,
        _ => 1,
    };
    (count, current)
}

/// Spawns the leader-side watcher. It is a no-op on followers and on nodes that
/// are not yet initialized; the caller gates it behind the opt-in setting.
pub(crate) fn spawn(
    raft: MatchEngineRaft,
    node_id: MatchEngineNodeId,
    client: reqwest::Client,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
        let tick = &mut tick;
        let mut suspicions: BTreeMap<MatchEngineNodeId, (u8, Observation)> = BTreeMap::new();
        let mut cooldowns: BTreeMap<MatchEngineNodeId, Instant> = BTreeMap::new();

        loop {
            tick.tick().await;

            // Leader-only gate from a single metrics read. Peer addresses come
            // from the same read so id -> addr mapping is internally coherent.
            let peers: Vec<(MatchEngineNodeId, String)> = {
                let metrics = raft.metrics().borrow_watched().clone();
                if metrics.current_leader != Some(node_id) {
                    suspicions.clear();
                    continue;
                }
                let Some(replication) = metrics.replication.as_ref() else {
                    continue;
                };
                replication
                    .keys()
                    .filter(|peer| **peer != node_id)
                    .filter_map(|peer| {
                        let addr = metrics
                            .membership_config
                            .membership()
                            .get_node(peer)?
                            .addr
                            .clone();
                        Some((*peer, addr))
                    })
                    .collect()
            };

            for (peer, addr) in peers {
                if cooldowns
                    .get(&peer)
                    .is_some_and(|since| since.elapsed() < COOLDOWN)
                {
                    continue;
                }

                // Probe first, then read the leader's view again so `matched`
                // is sampled *after* the round-trip rather than before it.
                let snapshot = match client
                    .get(format!("http://{addr}/console/api/node"))
                    .timeout(PROBE_TIMEOUT)
                    .send()
                    .await
                {
                    Ok(response) if response.status().is_success() => {
                        match response.json::<NodeSnapshot>().await {
                            Ok(snapshot) if snapshot.node_id == peer => snapshot,
                            Ok(other) => {
                                tracing::warn!(peer, found = other.node_id, "wrong peer snapshot");
                                suspicions.remove(&peer);
                                continue;
                            }
                            Err(error) => {
                                tracing::debug!(peer, %error, "peer snapshot decode failed");
                                suspicions.remove(&peer);
                                continue;
                            }
                        }
                    }
                    Ok(response) => {
                        tracing::debug!(peer, status = %response.status(), "peer snapshot probe failed");
                        suspicions.remove(&peer);
                        continue;
                    }
                    Err(error) => {
                        tracing::debug!(peer, %error, "peer snapshot probe failed");
                        suspicions.remove(&peer);
                        continue;
                    }
                };

                // Fresh leader view at comparison time. Re-validate leadership
                // and membership in the same read: a peer that was removed, or
                // a leader that was deposed during the probe, must not act.
                let (matched, still_leader, still_member) = {
                    let metrics = raft.metrics().borrow_watched().clone();
                    let matched = metrics
                        .replication
                        .as_ref()
                        .and_then(|replication| replication.get(&peer))
                        .and_then(|entry| entry.as_ref().map(|log_id| log_id.index));
                    let still_member = metrics
                        .membership_config
                        .membership()
                        .get_node(&peer)
                        .is_some();
                    (
                        matched,
                        metrics.current_leader == Some(node_id),
                        still_member,
                    )
                };
                if !still_leader {
                    suspicions.clear();
                    break;
                }
                if !still_member {
                    suspicions.remove(&peer);
                    continue;
                }

                let Some(current) = Observation::new(matched, snapshot.raft.last_log_index) else {
                    suspicions.remove(&peer);
                    continue;
                };
                let (count, current) = next_count(suspicions.get(&peer).copied(), current);
                suspicions.insert(peer, (count, current));

                if count < REQUIRED_SAMPLES {
                    continue;
                }

                // Final guard: leadership can move between the comparison and
                // this call. OpenRaft rejects the trigger when we are no longer
                // leader, but checking here keeps the log quiet and avoids
                // arming a stale node.
                if raft.metrics().borrow_watched().current_leader != Some(node_id) {
                    suspicions.clear();
                    break;
                }

                // One-shot opt-in for a single peer: OpenRaft will lower this
                // peer's progress on the next conflict and re-probe from the
                // start, selecting snapshot replication when the log is purged.
                match raft.trigger().allow_next_revert(&peer, true).await {
                    Ok(Ok(())) => {
                        tracing::warn!(
                            peer,
                            matched = current.matched,
                            peer_tail = current.peer_tail,
                            lag = current.lag(),
                            "follower log reverted; allowing one replication reset"
                        );
                        if let Err(error) = raft.trigger().heartbeat().await {
                            tracing::debug!(peer, %error, "probe heartbeat failed");
                        }
                        cooldowns.insert(peer, Instant::now());
                        suspicions.remove(&peer);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(peer, %error, "allow_next_revert refused");
                        cooldowns.insert(peer, Instant::now());
                        suspicions.remove(&peer);
                    }
                    Err(error) => {
                        tracing::warn!(peer, %error, "Raft fatal during recovery trigger");
                        break;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::Observation;
    use super::next_count;

    #[test]
    fn detects_only_a_peer_tail_below_the_match_index() {
        assert!(Observation::new(Some(10), Some(9)).is_some());
        assert!(Observation::new(Some(10), Some(10)).is_none());
        assert!(Observation::new(Some(10), Some(11)).is_none());
        assert!(Observation::new(None, Some(9)).is_none());
        assert!(Observation::new(Some(10), None).is_none());
    }

    #[test]
    fn detection_requires_both_values() {
        let found = Observation::new(Some(42), Some(7)).unwrap();
        assert_eq!(found.matched, 42);
        assert_eq!(found.peer_tail, 7);
        assert_eq!(found.lag(), 35);
    }

    #[test]
    fn only_an_identical_repeat_advances_the_count() {
        let stuck = Observation::new(Some(100), Some(95)).unwrap();
        let (count, _) = next_count(None, stuck);
        assert_eq!(count, 1);
        let (count, _) = next_count(Some((1, stuck)), stuck);
        assert_eq!(count, 2);

        // A peer catching up reports a new tail: the suspicion restarts.
        let catching_up = Observation::new(Some(100), Some(98)).unwrap();
        let (count, _) = next_count(Some((1, stuck)), catching_up);
        assert_eq!(count, 1);
    }

    #[test]
    fn an_advanced_match_index_restarts_the_count() {
        // Once the peer ACKs, `matched` moves forward; that is not a reversion.
        let before = Observation::new(Some(100), Some(95)).unwrap();
        let after = Observation::new(Some(101), Some(95)).unwrap();
        let (count, _) = next_count(Some((1, before)), after);
        assert_eq!(count, 1);
    }
}

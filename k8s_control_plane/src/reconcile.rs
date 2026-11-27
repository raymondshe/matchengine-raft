//! Reconciliation planning: what the operator would do for one shard, in order.
//!
//! The operator itself needs a Kubernetes client and a `Lease`; neither belongs in this demo.
//! What is easy to get wrong (and is fully implemented here) is the decision logic:
//!
//! - `/init` runs exactly once per process, and only after all of that process's pods are
//!   healthy (the bootstrap deadlock in the design doc, §5.6);
//! - membership is changed only through `/add-learner` + `/change-membership`, never by editing
//!   the StatefulSet replica count;
//! - the symbol config is applied through the leader and only when the applied hash differs;
//! - a same-version upgrade rolls one pod at a time with a leadership transfer first, while a
//!   wire/snapshot-incompatible upgrade is stop-the-world and requires a recent backup.
//!
//! The output is a plan, not a mutation. The manager API exposes it so a human (or the GUI) can
//! see exactly what the operator would do.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::model::{
    MarketDataSymbolStatus, Release, SymbolDeployment, SymbolRoute, SymbolShard, TradingSymbol,
    UpgradeStrategy, desired_config_hash,
};
use crate::render::{render_market_data_service, rolling_upgrade_safe};
use crate::store::{Catalog, now_epoch};
use crate::validate::validate_catalog;

const BACKUP_FRESHNESS_SECS: u64 = 24 * 60 * 60;

/// One engine process as observed by the operator. The engine has no single "status" endpoint
/// yet (that is phase 2 in the design doc), so the operator assembles this from `/health`,
/// `/ready`, `/metrics`, `/read symbol_config` and pod state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessObservation {
    pub symbol: String,
    pub deployment: String,
    /// Pod ordinal (`0..replicas`), equal to the engine's `--id`.
    pub pod: u8,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub leader: bool,
    #[serde(default)]
    pub bootstrapped: bool,
    /// Current voter IDs from the process's Raft metrics. Empty before `/init`.
    #[serde(default)]
    pub voters: Vec<u8>,
    /// Applied symbol config hash; empty before `ConfigureSymbol`.
    #[serde(default)]
    pub config_hash: String,
    #[serde(default)]
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_index: Option<u64>,
}

/// Observation of a whole shard (every process on every pod).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShardObservation {
    pub shard: String,
    #[serde(default)]
    pub processes: Vec<ProcessObservation>,
    /// Unix seconds of the last verified volume-group backup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_backup_at: Option<u64>,
    /// Unix seconds used for freshness checks; the GUI passes the current time.
    #[serde(default)]
    pub now: u64,
}

impl ShardObservation {
    pub fn process(&self, symbol: &str, pod: u8) -> Option<&ProcessObservation> {
        self.processes
            .iter()
            .find(|p| p.symbol == symbol && p.pod == pod)
    }

    pub fn ready_of(&self, symbol: &str, pod: u8) -> bool {
        self.process(symbol, pod).is_some_and(|p| p.ready)
    }

    pub fn leaders(&self) -> Vec<&ProcessObservation> {
        self.processes
            .iter()
            .filter(|p| p.leader && p.ready)
            .collect()
    }
}

/// Read-only view of one market-data deployment. The operator assembles this from the
/// Deployment, one running pod and its `/ready` + `/metrics` endpoints; the manager's plan API
/// accepts the same shape so a human can replay a real observation without touching a cluster.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketDataObservation {
    /// Service name. Empty is allowed in an explicit POST body; the path supplies it.
    #[serde(default)]
    pub service: String,
    #[serde(default)]
    pub cell: String,
    /// Image running on the observed pod. Empty when no Deployment exists yet.
    #[serde(default)]
    pub image: String,
    /// Release reported by pod annotations, when the operator could read one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<Release>,
    #[serde(default)]
    pub replicas: u16,
    #[serde(default)]
    pub ready_replicas: u16,
    #[serde(default)]
    pub updated_replicas: u16,
    #[serde(default)]
    pub available: bool,
    /// False only before the operator has applied the first Deployment.
    #[serde(default)]
    pub deployment_exists: bool,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub symbols: Vec<MarketDataSymbolStatus>,
    #[serde(default)]
    pub feed_ready: bool,
    /// Effective HPA bounds observed on the workload; `None` defers to the CRD spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hpa_min_replicas: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hpa_max_replicas: Option<u16>,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Unix seconds for last-frame age checks; `0` defers to the heartbeat timeout only.
    #[serde(default)]
    pub now: u64,
}

/// A concrete step the operator would take. `detail` is human-readable; `voters` carries the
/// exact `/init` or `/change-membership` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub action: Action,
    /// Symbol name, pod name or shard name, depending on the action.
    pub target: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub voters: Vec<Voter>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Voter {
    pub id: u8,
    pub addr: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Pods or containers are missing/not ready; do nothing else yet.
    WaitForPods,
    /// `POST /init` with the shard's voter list.
    InitCluster,
    /// `POST /change-membership` with the desired voter IDs.
    ChangeMembership,
    /// `POST /write {"ConfigureSymbol": ...}` through the leader.
    ConfigureSymbol,
    /// `POST /console/api/actions/transfer-leader` before restarting a leader's pod.
    TransferLeader,
    /// Delete one pod; the StatefulSet recreates it with the target image.
    RestartPod,
    /// `POST /console/api/actions/snapshot` for one process.
    Snapshot,
    /// Run a volume-group backup and record its ID before a destructive step.
    Backup,
    /// Stop serving the symbol at the gateway.
    PauseTraffic,
    /// Patch `SymbolRoute`/`SymbolDeployment` status with the observed leader.
    PublishRoute,
    /// Preflight the catalog and target release (also used by market-data plans).
    ValidateCatalog,
    /// Server-side apply one rendered market-data object (ConfigMap, Deployment, ...).
    RenderManifests,
    /// Adjust replicas or HPA bounds on a market-data Deployment.
    ScaleDeployment,
    /// Replace market-data pods with the target image under `maxUnavailable: 0`.
    RollDeployment,
    /// Probe `/ready` and `/metrics` for every declared symbol.
    VerifyFeed,
    /// Write the resulting status back to the CRD.
    WriteStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanPhase {
    Blocked,
    Paused,
    /// An empty shard reservation; no process exists until a SymbolDeployment is bound.
    Reserved,
    Bootstrap,
    Membership,
    Configuring,
    Upgrading,
    Ready,
    /// Market-data: the operator still needs to apply the rendered objects.
    Provisioning,
    /// Market-data: observed replicas sit outside the effective HPA bounds.
    Scaling,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcilePlan {
    pub shard: String,
    pub phase: PlanPhase,
    #[serde(default)]
    pub steps: Vec<PlanStep>,
    /// Reasons the plan cannot proceed; empty when steps are runnable.
    #[serde(default)]
    pub blocked: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    /// Target image used for the plan.
    pub target_image: String,
}

struct ProcessSpec<'a> {
    deployment: &'a SymbolDeployment,
    symbol: &'a TradingSymbol,
    route: Option<&'a SymbolRoute>,
    config_hash: String,
}

impl ProcessSpec<'_> {
    fn leader<'a>(&self, observation: &'a ShardObservation) -> Option<&'a ProcessObservation> {
        observation
            .processes
            .iter()
            .find(|p| p.symbol == self.symbol.name && p.leader && p.ready)
    }
}

/// Build a plan for one shard from explicit observations.
pub fn plan_shard(
    catalog: &Catalog,
    shard_name: &str,
    observation: &ShardObservation,
    target_image: Option<&str>,
    target_release: Option<&Release>,
) -> Result<ReconcilePlan, ApiError> {
    let shard = catalog.shards.get(shard_name).ok_or_else(|| {
        ApiError::not_found("shard_not_found", format!("unknown shard {shard_name}"))
    })?;
    let deployments = catalog.shard_deployments(shard_name);
    let active: Vec<&SymbolDeployment> = deployments
        .iter()
        .copied()
        .filter(|deployment| {
            deployment.spec.desired_state != crate::model::DeploymentState::Disabled
        })
        .collect();
    if active.is_empty() {
        let disabled = deployments.len();
        let note = if disabled > 0 {
            format!(
                "{disabled} deployment(s) are disabled: no engine process is desired. Apply the updated ConfigMap/workload and keep the PVCs; slots stay reserved until an explicit re-enable."
            )
        } else {
            "reserved shard: no SymbolDeployment is bound; the StatefulSet materializes when the first symbol is placed"
                .to_string()
        };
        return Ok(ReconcilePlan {
            shard: shard_name.to_string(),
            phase: PlanPhase::Reserved,
            steps: Vec::new(),
            blocked: Vec::new(),
            notes: vec![note],
            target_image: target_image.unwrap_or(&shard.spec.image).to_string(),
        });
    }
    let mut processes = Vec::new();
    for deployment in active {
        let symbol = catalog
            .symbols
            .get(&deployment.spec.symbol_ref)
            .ok_or_else(|| {
                ApiError::bad_request(
                    "symbol_missing",
                    format!(
                        "deployment {} references unknown symbol {}",
                        deployment.name, deployment.spec.symbol_ref
                    ),
                )
            })?;
        let route = catalog.route_for_symbol(&symbol.name);
        processes.push(ProcessSpec {
            deployment,
            symbol,
            route,
            config_hash: desired_config_hash(deployment, symbol),
        });
    }
    Ok(build_plan(
        shard,
        &processes,
        observation,
        target_image,
        target_release,
    ))
}

/// Build a plan from a bundle-shaped catalog. Used by tests and the render CLI path.
pub fn plan_bundle(
    catalog: &Catalog,
    shard_name: &str,
    observation: &ShardObservation,
    target_image: Option<&str>,
    target_release: Option<&Release>,
) -> Result<ReconcilePlan, ApiError> {
    plan_shard(
        catalog,
        shard_name,
        observation,
        target_image,
        target_release,
    )
}

fn pod_name(shard: &str, pod: u8) -> String {
    format!("{shard}-{pod}")
}

fn endpoint(shard: &SymbolShard, namespace: &str, pod: u8, port: u16) -> String {
    format!(
        "{}.{}-headless.{namespace}.svc.cluster.local:{port}",
        pod_name(&shard.name, pod),
        shard.name
    )
}

fn desired_voters(shard: &SymbolShard, namespace: &str, port: u16) -> Vec<Voter> {
    (0..shard.spec.replicas)
        .map(|pod| Voter {
            id: pod,
            addr: endpoint(shard, namespace, pod, port),
        })
        .collect()
}

fn build_plan(
    shard: &SymbolShard,
    processes: &[ProcessSpec<'_>],
    observation: &ShardObservation,
    target_image: Option<&str>,
    target_release: Option<&Release>,
) -> ReconcilePlan {
    let target_image = target_image.unwrap_or(&shard.spec.image).to_string();
    let mut plan = ReconcilePlan {
        shard: shard.name.clone(),
        phase: PlanPhase::Ready,
        steps: Vec::new(),
        blocked: Vec::new(),
        notes: Vec::new(),
        target_image: target_image.clone(),
    };

    if shard.spec.paused {
        plan.phase = PlanPhase::Paused;
        plan.notes
            .push("shard is paused; the operator must not mutate membership or images".to_string());
        return plan;
    }

    // 1. Every process needs every pod present and ready before anything is initialised.
    let mut wait_steps = Vec::new();
    for process in processes {
        for pod in 0..shard.spec.replicas {
            let name = pod_name(&shard.name, pod);
            match observation.process(&process.symbol.name, pod) {
                None => {
                    wait_steps.push(pod_name(&shard.name, pod));
                    plan.blocked.push(format!(
                        "process {} has no observation on pod {name}",
                        process.symbol.name
                    ));
                }
                Some(observed) if !observed.ready => {
                    wait_steps.push(name.clone());
                    plan.blocked.push(format!(
                        "process {} on pod {name} is not ready",
                        process.symbol.name
                    ));
                }
                Some(_) => {}
            }
        }
    }
    if !wait_steps.is_empty() {
        wait_steps.sort();
        wait_steps.dedup();
        plan.phase = PlanPhase::Blocked;
        plan.steps.push(PlanStep {
            action: Action::WaitForPods,
            target: wait_steps.join(","),
            endpoint: String::new(),
            voters: Vec::new(),
            detail: "wait for every container to be healthy and DNS to resolve before /init"
                .to_string(),
        });
        plan.notes.push(
            "bootstrapping before all pods are healthy is the classic four-node deadlock"
                .to_string(),
        );
        return plan;
    }

    // 2. Bootstrap each process exactly once. A bootstrapped process has a non-empty voter set;
    //    `!bootstrapped` with voters is ambiguous and must be inspected, not re-initialised.
    let mut init_steps = Vec::new();
    for process in processes {
        let voters = desired_voters(
            shard,
            &shard.namespace,
            process.deployment.spec.placement.http_port,
        );
        let rows: Vec<&ProcessObservation> = (0..shard.spec.replicas)
            .filter_map(|pod| observation.process(&process.symbol.name, pod))
            .collect();
        let bootstrapped = rows.iter().filter(|row| row.bootstrapped).count();
        let any_voters = rows.iter().any(|row| !row.voters.is_empty());
        if bootstrapped == rows.len() {
            continue;
        }
        if bootstrapped > 0 {
            plan.blocked.push(format!(
                "process {} reports a mixed bootstrap state across pods; inspect before calling /init",
                process.symbol.name
            ));
            continue;
        }
        if any_voters {
            plan.blocked.push(format!(
                "process {} reports voters but no bootstrap marker; refusing to call /init twice",
                process.symbol.name
            ));
            continue;
        }
        init_steps.push(PlanStep {
            action: Action::InitCluster,
            target: process.symbol.name.clone(),
            endpoint: endpoint(
                shard,
                &shard.namespace,
                0,
                process.deployment.spec.placement.http_port,
            ),
            voters,
            detail: "POST /init with the shard voter list from pod 0".to_string(),
        });
    }
    if !init_steps.is_empty() {
        plan.phase = PlanPhase::Bootstrap;
        plan.steps = init_steps;
        plan.notes.push(
            "the node id is the pod ordinal and repeats across processes; the port selects the Raft instance"
                .to_string(),
        );
        return plan;
    }
    if !plan.blocked.is_empty() {
        plan.phase = PlanPhase::Blocked;
        return plan;
    }

    // 3. Membership drift is fixed through Raft, not by editing StatefulSet replicas.
    let mut membership_steps = Vec::new();
    for process in processes {
        let desired: BTreeSet<u8> = (0..shard.spec.replicas).collect();
        let rows: Vec<&ProcessObservation> = (0..shard.spec.replicas)
            .filter_map(|pod| observation.process(&process.symbol.name, pod))
            .collect();
        let views: BTreeSet<BTreeSet<u8>> = rows
            .iter()
            .map(|row| row.voters.iter().copied().collect())
            .collect();
        if views.len() > 1 {
            plan.blocked.push(format!(
                "process {} reports inconsistent voter sets across pods; inspect before changing membership",
                process.symbol.name
            ));
            continue;
        }
        let current = views.into_iter().next().unwrap_or_default();
        if current == desired {
            continue;
        }
        match process.leader(observation) {
            Some(leader) => membership_steps.push(PlanStep {
                action: Action::ChangeMembership,
                target: process.symbol.name.clone(),
                endpoint: endpoint(
                    shard,
                    &shard.namespace,
                    leader.pod,
                    process.deployment.spec.placement.http_port,
                ),
                voters: desired_voters(
                    shard,
                    &shard.namespace,
                    process.deployment.spec.placement.http_port,
                ),
                detail: format!(
                    "add the missing learners, wait for catch-up, then POST /change-membership with {:?}",
                    desired
                ),
            }),
            None => plan.blocked.push(format!(
                "process {} needs a membership change but has no leader",
                process.symbol.name
            )),
        }
    }
    if !membership_steps.is_empty() {
        plan.phase = PlanPhase::Membership;
        plan.steps = membership_steps;
        return plan;
    }
    if !plan.blocked.is_empty() {
        plan.phase = PlanPhase::Blocked;
        return plan;
    }

    // 4. Apply the symbol config through the leader when the applied hash differs.
    let mut config_steps = Vec::new();
    for process in processes {
        let rows: Vec<&ProcessObservation> = (0..shard.spec.replicas)
            .filter_map(|pod| observation.process(&process.symbol.name, pod))
            .collect();
        if rows
            .iter()
            .all(|row| row.config_hash == process.config_hash)
        {
            continue;
        }
        match process.leader(observation) {
            Some(leader) => config_steps.push(PlanStep {
                action: Action::ConfigureSymbol,
                target: process.symbol.name.clone(),
                endpoint: endpoint(
                    shard,
                    &shard.namespace,
                    leader.pod,
                    process.deployment.spec.placement.http_port,
                ),
                voters: Vec::new(),
                detail: format!(
                    "POST /write {{\"ConfigureSymbol\": ...}} and verify /read symbol_config == {}",
                    process.config_hash
                ),
            }),
            None => plan.blocked.push(format!(
                "process {} has no leader to apply the symbol config",
                process.symbol.name
            )),
        }
    }
    if !config_steps.is_empty() {
        plan.phase = PlanPhase::Configuring;
        plan.steps = config_steps;
        return plan;
    }
    if !plan.blocked.is_empty() {
        plan.phase = PlanPhase::Blocked;
        return plan;
    }

    // 5. Leader-aware routing: publish the observed leader for every symbol.
    for process in processes {
        let Some(route) = process.route else { continue };
        if process.deployment.spec.desired_state == crate::model::DeploymentState::Shadow {
            plan.notes.push(format!(
                "process {} is a shadow deployment; the source route stays active until the migration cutover",
                process.symbol.name
            ));
            continue;
        }
        let Some(leader) = process.leader(observation) else {
            plan.blocked.push(format!(
                "process {} has no leader; /ready would be 503 and the route must not move",
                process.symbol.name
            ));
            continue;
        };
        let leader_addr = endpoint(
            shard,
            &shard.namespace,
            leader.pod,
            process.deployment.spec.placement.http_port,
        );
        if route.spec.active_deployment_ref != process.deployment.name {
            plan.blocked.push(format!(
                "route {} points at {} instead of {}",
                route.name, route.spec.active_deployment_ref, process.deployment.name
            ));
            continue;
        }
        if route.status.leader_addr.as_deref() != Some(leader_addr.as_str()) {
            plan.steps.push(PlanStep {
                action: Action::PublishRoute,
                target: route.name.clone(),
                endpoint: leader_addr.clone(),
                voters: Vec::new(),
                detail: format!(
                    "patch status.leaderAddr={leader_addr} on SymbolDeployment and SymbolRoute"
                ),
            });
        }
    }
    if !plan.blocked.is_empty() {
        plan.phase = PlanPhase::Blocked;
        return plan;
    }

    // 6. Upgrade planning only starts from a fully ready, configured shard.
    let observed_images: BTreeSet<&str> = observation
        .processes
        .iter()
        .map(|row| row.image.as_str())
        .filter(|image| !image.is_empty())
        .collect();
    let needs_upgrade = observed_images
        .iter()
        .any(|image| *image != plan.target_image);
    if needs_upgrade {
        if target_release.is_none() {
            plan.notes.push(
                "target release metadata absent; assuming a same-version image rebuild. Pass targetRelease when the wire/snapshot version may change."
                    .to_string(),
            );
        }
        // An upgrade is the dominant action; routing is republished after it completes.
        plan.steps.clear();
        let strategy = effective_strategy(shard, target_release);
        match strategy {
            UpgradeStrategy::Rolling => {
                rolling_steps(shard, processes, observation, &target_image, &mut plan)
            }
            _ => stop_the_world_steps(shard, processes, observation, &target_image, &mut plan),
        }
        return plan;
    }

    if plan.steps.is_empty() {
        plan.phase = PlanPhase::Ready;
        plan.notes
            .push("all processes are bootstrapped, configured and reporting a leader".to_string());
    }
    plan
}

fn effective_strategy(shard: &SymbolShard, target_release: Option<&Release>) -> UpgradeStrategy {
    match shard.spec.upgrade.strategy {
        UpgradeStrategy::Auto => match target_release {
            Some(release) if rolling_upgrade_safe(shard, release) => UpgradeStrategy::Rolling,
            Some(_) => UpgradeStrategy::StopTheWorld,
            // No release metadata: assume a same-version image rebuild. Callers that may
            // change the wire/snapshot version must pass the target release.
            None => UpgradeStrategy::Rolling,
        },
        explicit => explicit,
    }
}

fn rolling_steps(
    shard: &SymbolShard,
    processes: &[ProcessSpec<'_>],
    observation: &ShardObservation,
    target_image: &str,
    plan: &mut ReconcilePlan,
) {
    plan.phase = PlanPhase::Upgrading;
    for pod in 0..shard.spec.replicas {
        let leaders: Vec<&ProcessSpec<'_>> = processes
            .iter()
            .filter(|process| {
                observation
                    .process(&process.symbol.name, pod)
                    .is_some_and(|row| row.leader && row.ready)
            })
            .collect();
        for process in &leaders {
            plan.steps.push(PlanStep {
                action: Action::TransferLeader,
                target: process.symbol.name.clone(),
                endpoint: endpoint(
                    shard,
                    &shard.namespace,
                    pod,
                    process.deployment.spec.placement.http_port,
                ),
                voters: Vec::new(),
                detail: format!(
                    "transfer leadership off pod {} before restarting it",
                    pod_name(&shard.name, pod)
                ),
            });
        }
        plan.steps.push(PlanStep {
            action: Action::RestartPod,
            target: pod_name(&shard.name, pod),
            endpoint: String::new(),
            voters: Vec::new(),
            detail: format!(
                "delete pod; wait for /ready, applied index and membership before the next pod (image {target_image})"
            ),
        });
    }
    plan.notes
        .push("same wire/snapshot version: one pod at a time, leader transfer first".to_string());
}

fn stop_the_world_steps(
    shard: &SymbolShard,
    processes: &[ProcessSpec<'_>],
    observation: &ShardObservation,
    target_image: &str,
    plan: &mut ReconcilePlan,
) {
    let fresh_backup = observation.now > 0
        && observation.last_backup_at.is_some_and(|at| {
            at <= observation.now && observation.now.saturating_sub(at) <= BACKUP_FRESHNESS_SECS
        });
    if !fresh_backup {
        plan.phase = PlanPhase::Blocked;
        plan.blocked.push(
            "stop-the-world upgrade requires a verified backup newer than 24 hours".to_string(),
        );
        plan.steps.push(PlanStep {
            action: Action::Backup,
            target: shard.name.clone(),
            endpoint: String::new(),
            voters: Vec::new(),
            detail: shard
                .spec
                .backup
                .as_ref()
                .map(|backup| format!("volume-group snapshot to {}", backup.destination))
                .unwrap_or_else(|| {
                    "take a volume-group backup before changing the image".to_string()
                }),
        });
        return;
    }

    plan.phase = PlanPhase::Upgrading;
    plan.steps.push(PlanStep {
        action: Action::PauseTraffic,
        target: shard.name.clone(),
        endpoint: String::new(),
        voters: Vec::new(),
        detail:
            "move every SymbolRoute on this shard to Draining so the gateway stops sending writes"
                .to_string(),
    });
    for process in processes {
        plan.steps.push(PlanStep {
            action: Action::Snapshot,
            target: process.symbol.name.clone(),
            endpoint: process
                .leader(observation)
                .map(|leader| {
                    endpoint(
                        shard,
                        &shard.namespace,
                        leader.pod,
                        process.deployment.spec.placement.http_port,
                    )
                })
                .unwrap_or_default(),
            voters: Vec::new(),
            detail: "trigger and verify a snapshot before stopping the pod".to_string(),
        });
    }
    for pod in 0..shard.spec.replicas {
        plan.steps.push(PlanStep {
            action: Action::RestartPod,
            target: pod_name(&shard.name, pod),
            endpoint: String::new(),
            voters: Vec::new(),
            detail: format!(
                "restart with the incompatible image after every snapshot is verified (image {target_image})"
            ),
        });
    }
    plan.notes.push(
        "wire/snapshot-incompatible upgrade: no mixed-version peers, so every process stops together"
            .to_string(),
    );
    plan.notes.push(
        "restore traffic only after every process is /ready and membership matches".to_string(),
    );
}

// ---------------------------------------------------------------------------
// Demo observation synthesis
// ---------------------------------------------------------------------------

/// Scenario the GUI/CLI can simulate without a cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scenario {
    /// Pods healthy, nothing initialised.
    Fresh,
    /// Bootstrapped, configured, leaders published.
    Ready,
    /// One pod not ready.
    Degraded,
    /// No process reports a leader.
    LeaderLoss,
    /// One process has an older symbol config.
    ConfigDrift,
    /// Ready shard running an older image.
    Upgrade,
}

impl Scenario {
    pub fn parse(value: &str) -> Result<Self, ApiError> {
        match value {
            "fresh" => Ok(Self::Fresh),
            "ready" => Ok(Self::Ready),
            "degraded" => Ok(Self::Degraded),
            "leader-loss" => Ok(Self::LeaderLoss),
            "config-drift" => Ok(Self::ConfigDrift),
            "upgrade" => Ok(Self::Upgrade),
            other => Err(ApiError::bad_request(
                "scenario",
                format!(
                    "unknown scenario {other:?}; use fresh|ready|degraded|leader-loss|config-drift|upgrade"
                ),
            )),
        }
    }
}

const DEMO_TARGET_IMAGE: &str = crate::DEFAULT_IMAGE;
const DEMO_PREVIOUS_IMAGE: &str = "registry.example.com/matchengine-raft:1.3.0-nonroot";

/// Synthesize an observation for the demo API. This is explicitly fake: it exists so the
/// bootstrap/upgrade plan can be exercised without a cluster.
pub fn simulate(
    catalog: &Catalog,
    shard_name: &str,
    scenario: Scenario,
) -> Result<ShardObservation, ApiError> {
    let shard = catalog.shards.get(shard_name).ok_or_else(|| {
        ApiError::not_found("shard_not_found", format!("unknown shard {shard_name}"))
    })?;
    let deployments: Vec<&SymbolDeployment> = catalog
        .shard_deployments(shard_name)
        .into_iter()
        .filter(|deployment| {
            deployment.spec.desired_state != crate::model::DeploymentState::Disabled
        })
        .collect();
    let now = now_epoch();
    if deployments.is_empty() {
        return Ok(ShardObservation {
            shard: shard.name.clone(),
            processes: Vec::new(),
            last_backup_at: Some(now.saturating_sub(3_600)),
            now,
        });
    }
    let observed_image = match scenario {
        Scenario::Upgrade => DEMO_PREVIOUS_IMAGE,
        _ if shard.spec.image.is_empty() => DEMO_TARGET_IMAGE,
        _ => shard.spec.image.as_str(),
    };
    let mut processes = Vec::new();
    for (index, deployment) in deployments.iter().enumerate() {
        let symbol = catalog
            .symbols
            .get(&deployment.spec.symbol_ref)
            .ok_or_else(|| ApiError::bad_request("symbol_missing", "deployment has no symbol"))?;
        let config_hash = desired_config_hash(deployment, symbol);
        for pod in 0..shard.spec.replicas {
            let ready = !(scenario == Scenario::Degraded && pod == shard.spec.replicas - 1);
            let bootstrapped = scenario != Scenario::Fresh;
            let leader = bootstrapped && scenario != Scenario::LeaderLoss && pod == 0;
            let mut row_hash = if bootstrapped {
                config_hash.clone()
            } else {
                String::new()
            };
            if scenario == Scenario::ConfigDrift && index == 0 && pod == 1 {
                row_hash = "0000000000000000".to_string();
            }
            processes.push(ProcessObservation {
                symbol: symbol.name.clone(),
                deployment: deployment.name.clone(),
                pod,
                ready,
                leader,
                bootstrapped,
                voters: if bootstrapped {
                    (0..shard.spec.replicas).collect()
                } else {
                    Vec::new()
                },
                config_hash: row_hash,
                image: observed_image.to_string(),
                applied_index: bootstrapped.then_some(1_000),
            });
        }
    }
    Ok(ShardObservation {
        shard: shard.name.clone(),
        processes,
        last_backup_at: Some(now.saturating_sub(3_600)),
        now,
    })
}

// ---------------------------------------------------------------------------
// Market-data service planning (read-only)
// ---------------------------------------------------------------------------

/// Scenario the market-data plan API can simulate without a cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MarketDataScenario {
    /// No Deployment applied yet.
    Fresh,
    /// Pods ready and every symbol fed.
    Ready,
    /// Ready service still running the previous image.
    Behind,
    /// Deployment ready but one or more symbol feeds stale.
    Degraded,
    /// Service paused by spec; the renderer scales it to zero.
    Paused,
}

impl MarketDataScenario {
    pub fn parse(value: &str) -> Result<Self, ApiError> {
        match value {
            "fresh" => Ok(Self::Fresh),
            "ready" | "healthy" => Ok(Self::Ready),
            "behind" | "upgrade" => Ok(Self::Behind),
            "degraded" => Ok(Self::Degraded),
            "paused" => Ok(Self::Paused),
            other => Err(ApiError::bad_request(
                "market_data_scenario",
                format!("unknown scenario {other:?}; use fresh|ready|behind|degraded|paused"),
            )),
        }
    }
}

const DEMO_MARKET_DATA_PREVIOUS_IMAGE: &str =
    "registry.example.com/matchengine-marketdata:arm64-v3";

/// Synthesize a market-data observation for the demo API. No store or Kubernetes call is made.
pub fn simulate_market_data(
    catalog: &Catalog,
    name: &str,
    scenario: MarketDataScenario,
) -> Result<MarketDataObservation, ApiError> {
    let service = catalog.market_data_services.get(name).ok_or_else(|| {
        ApiError::not_found(
            "market_data_not_found",
            format!("unknown market-data service {name}"),
        )
    })?;
    let paused = service.spec.paused || scenario == MarketDataScenario::Paused;
    let fresh = scenario == MarketDataScenario::Fresh;
    let degraded = scenario == MarketDataScenario::Degraded;
    let replicas = if paused || fresh {
        0
    } else {
        service.spec.replicas.max(service.spec.min_replicas())
    };
    let ready_replicas = if paused || fresh || degraded {
        0
    } else {
        replicas
    };
    let image = match scenario {
        MarketDataScenario::Fresh => String::new(),
        MarketDataScenario::Behind => DEMO_MARKET_DATA_PREVIOUS_IMAGE.to_string(),
        _ => service.spec.image.clone(),
    };
    let release = if scenario == MarketDataScenario::Behind {
        service
            .status
            .previous_release
            .clone()
            .or_else(|| Some(service.spec.release.clone()))
    } else {
        Some(service.spec.release.clone())
    };
    let symbols: Vec<MarketDataSymbolStatus> = catalog
        .market_data_symbols(service)
        .into_iter()
        .map(|symbol| MarketDataSymbolStatus {
            ready: !paused && !fresh && !degraded,
            last_frame_age_ms: if degraded { 90_000 } else { 1_000 },
            subscribers: if paused || fresh || degraded { 0 } else { 1 },
            name: symbol,
            ..MarketDataSymbolStatus::default()
        })
        .collect();
    let feed_ready = !symbols.is_empty() && symbols.iter().all(|symbol| symbol.ready);
    let message = match scenario {
        MarketDataScenario::Fresh => "Deployment has not been created yet".to_string(),
        MarketDataScenario::Paused => "paused by spec".to_string(),
        MarketDataScenario::Degraded => "feed not ready".to_string(),
        MarketDataScenario::Behind => "running an older image".to_string(),
        MarketDataScenario::Ready => String::new(),
    };
    Ok(MarketDataObservation {
        service: name.to_string(),
        cell: service.spec.cell.clone(),
        image,
        release,
        replicas,
        ready_replicas,
        updated_replicas: if scenario == MarketDataScenario::Behind {
            0
        } else {
            replicas
        },
        available: !paused && !fresh,
        deployment_exists: !fresh,
        paused,
        symbols,
        feed_ready,
        hpa_min_replicas: Some(service.spec.autoscaling.min_replicas),
        hpa_max_replicas: Some(service.spec.autoscaling.max_replicas),
        message,
        error: None,
        now: now_epoch(),
    })
}

fn market_data_step(
    action: Action,
    target: impl Into<String>,
    detail: impl Into<String>,
) -> PlanStep {
    PlanStep {
        action,
        target: target.into(),
        endpoint: String::new(),
        voters: Vec::new(),
        detail: detail.into(),
    }
}

/// Build a read-only plan for one market-data service from an explicit observation.
///
/// `ReconcilePlan.shard` carries the service name so the response shape stays identical to the
/// shard plan endpoint. The plan follows the operator's real order: validate, render, roll with
/// `maxUnavailable: 0`, verify each feed and write status.
pub fn plan_market_data(
    catalog: &Catalog,
    name: &str,
    observation: &MarketDataObservation,
    target_image: Option<&str>,
    target_release: Option<&Release>,
) -> Result<ReconcilePlan, ApiError> {
    let service = catalog.market_data_services.get(name).ok_or_else(|| {
        ApiError::not_found(
            "market_data_not_found",
            format!("unknown market-data service {name}"),
        )
    })?;
    if !observation.service.is_empty() && observation.service != name {
        return Err(ApiError::bad_request(
            "observation_service",
            format!(
                "observation is for {} but the path is {name}",
                observation.service
            ),
        ));
    }
    let image = target_image
        .unwrap_or(&service.spec.image)
        .trim()
        .to_string();
    if image.is_empty() {
        return Err(ApiError::bad_request(
            "target_image",
            "target image must not be empty",
        ));
    }
    let release = target_release.unwrap_or(&service.spec.release).clone();
    if release.wire_version == 0 || release.snapshot_version == 0 {
        return Err(ApiError::bad_request(
            "target_release",
            "wireVersion and snapshotVersion must be greater than zero",
        ));
    }

    // Plan against the target spec without mutating the catalog: the same validation and the
    // same renderer the operator uses decide which objects would be applied.
    let mut candidate = catalog.clone();
    if let Some(target) = candidate.market_data_services.get_mut(name) {
        target.spec.image = image.clone();
        target.spec.release = release.clone();
    }
    let mut scope = BTreeSet::new();
    scope.insert(name.to_string());
    let report = validate_catalog(&candidate).scoped(&scope);
    let mut blocked: Vec<String> = report
        .errors
        .iter()
        .map(|issue| issue.message.clone())
        .collect();
    let objects = if blocked.is_empty() {
        match render_market_data_service(&candidate, name, None) {
            Ok(objects) => objects,
            Err(error) => {
                blocked.push(error.message);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let mut plan = ReconcilePlan {
        shard: name.to_string(),
        phase: PlanPhase::Blocked,
        steps: Vec::new(),
        blocked,
        notes: vec![
            "market-data services are stateless: no leader transfer, snapshot install or quorum gate"
                .to_string(),
            "the operator rolls the Deployment with maxUnavailable: 0 and maxSurge: 1; the preStop hook drains WebSocket clients"
                .to_string(),
        ],
        target_image: image.clone(),
    };
    plan.steps.push(market_data_step(
        Action::ValidateCatalog,
        name,
        if plan.blocked.is_empty() {
            format!(
                "validate the catalog and target release (wire v{}, snapshot v{}) before applying objects",
                release.wire_version, release.snapshot_version
            )
        } else {
            format!("catalog validation failed: {}", plan.blocked.join("; "))
        },
    ));
    if !plan.blocked.is_empty() {
        return Ok(plan);
    }

    let kinds: Vec<String> = objects
        .iter()
        .map(|object| {
            object
                .get("kind")
                .and_then(|kind| kind.as_str())
                .unwrap_or("object")
                .to_string()
        })
        .collect();
    plan.notes
        .push(format!("rendered objects: {}", kinds.join(", ")));

    let active = catalog.market_data_symbols(service);
    let write_status = || {
        market_data_step(
            Action::WriteStatus,
            name,
            format!(
                "write {name} status with replica, per-symbol readiness and frame ages into the CRD"
            ),
        )
    };
    if active.is_empty() {
        plan.phase = PlanPhase::Reserved;
        plan.notes.push(
            "no active symbols: the renderer scales the Deployment to zero and omits the HPA"
                .to_string(),
        );
        plan.steps.push(market_data_step(
            Action::PauseTraffic,
            name,
            format!("scale {name} to zero replicas; no feed is expected until a symbol starts"),
        ));
        plan.steps.push(write_status());
        return Ok(plan);
    }

    if service.spec.paused || observation.paused {
        plan.phase = PlanPhase::Paused;
        plan.steps.push(market_data_step(
            Action::PauseTraffic,
            name,
            format!("keep {name} scaled to zero while paused; no image rollout is attempted"),
        ));
        plan.steps.push(write_status());
        return Ok(plan);
    }

    if !observation.deployment_exists || observation.replicas == 0 {
        plan.phase = PlanPhase::Provisioning;
        for (object, kind) in objects.iter().zip(kinds.iter()) {
            let object_name = object
                .get("metadata")
                .and_then(|metadata| metadata.get("name"))
                .and_then(|value| value.as_str())
                .unwrap_or(name);
            plan.steps.push(market_data_step(
                Action::RenderManifests,
                format!("{kind}/{object_name}"),
                format!("server-side apply the {kind} rendered for {name}"),
            ));
        }
        plan.steps.push(market_data_step(
            Action::WaitForPods,
            name,
            format!("wait for a running {name} pod before probing /ready and /metrics"),
        ));
    } else {
        let behind = observation.image != image
            || observation
                .release
                .as_ref()
                .is_some_and(|observed| observed != &release);
        let unready = observation.ready_replicas < observation.replicas;
        let hpa_min = observation
            .hpa_min_replicas
            .unwrap_or(service.spec.min_replicas());
        // `0` is the CRD sentinel for "derive from spec.replicas", not a real upper bound.
        let hpa_max = observation
            .hpa_max_replicas
            .filter(|max| *max > 0)
            .unwrap_or(service.spec.max_replicas());
        let scaling = observation.replicas < hpa_min || observation.replicas > hpa_max;
        if unready {
            plan.phase = PlanPhase::Blocked;
            plan.blocked.push(format!(
                "{} of {} replicas are not ready",
                observation.replicas - observation.ready_replicas,
                observation.replicas
            ));
            plan.steps.push(market_data_step(
                Action::WaitForPods,
                name,
                format!(
                    "wait for {} ready replicas before any rollout or scale action",
                    observation.replicas
                ),
            ));
        } else if behind {
            plan.phase = PlanPhase::Upgrading;
            for pod in 0..observation.replicas {
                plan.steps.push(market_data_step(
                    Action::RollDeployment,
                    format!("{name}#{pod}"),
                    format!(
                        "replace observed pod {} with image {image} under maxUnavailable: 0 / maxSurge: 1",
                        pod + 1
                    ),
                ));
            }
            plan.notes.push(
                "the preStop hook drains existing WebSocket connections before each replacement"
                    .to_string(),
            );
        } else if scaling {
            plan.phase = PlanPhase::Scaling;
            plan.steps.push(market_data_step(
                Action::ScaleDeployment,
                name,
                format!(
                    "move observed replicas {} into the HPA bounds {hpa_min}..{hpa_max}",
                    observation.replicas
                ),
            ));
        } else {
            plan.phase = PlanPhase::Ready;
        }
    }

    let heartbeat_ms = service
        .spec
        .websocket
        .heartbeat_timeout_secs
        .saturating_mul(1_000);
    let stale: Vec<&str> = observation
        .symbols
        .iter()
        .filter(|symbol| !symbol.ready || symbol.last_frame_age_ms > heartbeat_ms)
        .map(|symbol| symbol.name.as_str())
        .collect();
    let feed_detail = if stale.is_empty() {
        format!(
            "probe /ready and /metrics: {} symbols ready within the heartbeat window",
            observation.symbols.len()
        )
    } else {
        format!(
            "probe /ready and /metrics: stale or unready feeds for {}",
            stale.join(", ")
        )
    };
    plan.steps
        .push(market_data_step(Action::VerifyFeed, name, feed_detail));
    if !stale.is_empty() || !observation.feed_ready {
        if plan.phase == PlanPhase::Ready {
            plan.phase = PlanPhase::Blocked;
        }
        plan.blocked.push(format!(
            "feed not ready for {}",
            if stale.is_empty() {
                "no symbol reported ready".to_string()
            } else {
                stale.join(", ")
            }
        ));
    }
    plan.steps.push(write_status());
    Ok(plan)
}

/// Patch route status from an observation. The operator does this after a plan's
/// `PublishRoute` step; the demo API uses it to keep the GUI's leader column honest.
pub fn apply_observed_routes(catalog: &mut Catalog, observation: &ShardObservation) {
    for route in catalog.routes.values_mut() {
        if route.spec.state == crate::model::RouteState::Disabled {
            continue;
        }
        let Some(deployment) = catalog
            .deployments
            .get(&route.spec.active_deployment_ref)
            .cloned()
        else {
            continue;
        };
        if deployment.spec.desired_state == crate::model::DeploymentState::Disabled {
            continue;
        }
        let Some(leader) = observation
            .processes
            .iter()
            .find(|p| p.symbol == route.spec.symbol_ref && p.leader && p.ready)
        else {
            continue;
        };
        let Some(shard) = catalog.shards.get(&deployment.spec.shard_ref) else {
            continue;
        };
        let addr = endpoint(
            shard,
            &shard.namespace,
            leader.pod,
            deployment.spec.placement.http_port,
        );
        route.status.leader_addr = Some(addr.clone());
        route.status.phase = crate::model::Phase::Ready;
        route.status.active_deployment_ref = route.spec.active_deployment_ref.clone();
        if let Some(deployment) = catalog
            .deployments
            .get_mut(&route.spec.active_deployment_ref)
        {
            deployment.status.leader_addr = Some(addr.clone());
            deployment.status.leader_pod = Some(pod_name(&shard.name, leader.pod));
            deployment.status.ready = true;
            deployment.status.phase = crate::model::Phase::Ready;
        }
        if let Some(symbol) = catalog.symbols.get_mut(&route.spec.symbol_ref) {
            symbol.status.phase = crate::model::Phase::Ready;
        }
    }
}

/// Best-effort summary used by the GUI header and the CLI.
pub fn summarize(plan: &ReconcilePlan) -> String {
    let actions: BTreeMap<&str, usize> =
        plan.steps.iter().fold(BTreeMap::new(), |mut acc, step| {
            *acc.entry(action_name(step.action)).or_insert(0) += 1;
            acc
        });
    if actions.is_empty() {
        return match plan.phase {
            PlanPhase::Ready => "ready; no changes".to_string(),
            PlanPhase::Paused => "paused; no changes".to_string(),
            PlanPhase::Reserved => "reserved; no processes yet".to_string(),
            _ => format!("{:?}; blocked", plan.phase).to_lowercase(),
        };
    }
    actions
        .into_iter()
        .map(|(name, count)| format!("{count}x {name}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn action_name(action: Action) -> &'static str {
    match action {
        Action::WaitForPods => "wait-for-pods",
        Action::InitCluster => "init-cluster",
        Action::ChangeMembership => "change-membership",
        Action::ConfigureSymbol => "configure-symbol",
        Action::TransferLeader => "transfer-leader",
        Action::RestartPod => "restart-pod",
        Action::Snapshot => "snapshot",
        Action::Backup => "backup",
        Action::PauseTraffic => "pause-traffic",
        Action::PublishRoute => "publish-route",
        Action::ValidateCatalog => "validate-catalog",
        Action::RenderManifests => "render-manifests",
        Action::ScaleDeployment => "scale-deployment",
        Action::RollDeployment => "roll-deployment",
        Action::VerifyFeed => "verify-feed",
        Action::WriteStatus => "write-status",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MarketDataService, MarketDataServiceSpec, MarketDataTransport};
    use crate::store::demo_catalog;

    fn catalog() -> Catalog {
        demo_catalog()
    }

    fn market_data_catalog() -> Catalog {
        let mut catalog = catalog();
        let service = MarketDataService {
            name: "fx-marketdata".to_string(),
            namespace: "match-eu-1".to_string(),
            spec: MarketDataServiceSpec {
                cell: "eu-1".to_string(),
                symbols: vec!["eurusd".to_string(), "gbpusd".to_string()],
                feed_groups: Vec::new(),
                image: "registry.example.com/matchengine-marketdata@sha256:abc".to_string(),
                release: Release::default(),
                replicas: 2,
                transport: MarketDataTransport {
                    channel: Some(
                        "aeron:udp?endpoint=239.1.2.11:24001|interface={pod_ip}|ttl=1".to_string(),
                    ),
                    ..MarketDataTransport::default()
                },
                websocket: Default::default(),
                autoscaling: Default::default(),
                resources: None,
                paused: false,
                history: None,
            },
            status: Default::default(),
        };
        catalog
            .market_data_services
            .insert(service.name.clone(), service);
        catalog
    }

    fn md_plan(scenario: MarketDataScenario) -> ReconcilePlan {
        let catalog = market_data_catalog();
        let observation = simulate_market_data(&catalog, "fx-marketdata", scenario).unwrap();
        plan_market_data(&catalog, "fx-marketdata", &observation, None, None).unwrap()
    }

    fn plan(shard: &str, scenario: Scenario) -> ReconcilePlan {
        let catalog = catalog();
        let observation = simulate(&catalog, shard, scenario).unwrap();
        plan_shard(&catalog, shard, &observation, None, None).unwrap()
    }

    #[test]
    fn ready_scenario_uses_the_shard_image() {
        let mut catalog = catalog();
        catalog.shards.get_mut("fx-eu-01").unwrap().spec.image =
            "matchengine-raft:k8s-operator-v2".to_string();
        let observation = simulate(&catalog, "fx-eu-01", Scenario::Ready).unwrap();
        assert!(
            observation
                .processes
                .iter()
                .all(|row| row.image == "matchengine-raft:k8s-operator-v2")
        );
        let plan = plan_shard(&catalog, "fx-eu-01", &observation, None, None).unwrap();
        assert_eq!(plan.phase, PlanPhase::Ready);
        assert!(
            !plan
                .steps
                .iter()
                .any(|step| matches!(step.action, Action::RestartPod | Action::TransferLeader)),
            "a ready shard at the target image has no rollout steps"
        );
    }

    #[test]
    fn reserved_shard_plan_waits_for_a_symbol() {
        let catalog = catalog();
        let observation = simulate(&catalog, "fx-eu-02", Scenario::Ready).unwrap();
        assert!(observation.processes.is_empty());
        let plan = plan_shard(&catalog, "fx-eu-02", &observation, None, None).unwrap();
        assert_eq!(plan.phase, PlanPhase::Reserved);
        assert!(plan.steps.is_empty());
        assert!(
            plan.notes
                .iter()
                .any(|note| note.contains("reserved shard"))
        );
    }

    #[test]
    fn shadow_deployment_does_not_block_on_the_source_route() {
        let mut catalog = catalog();
        crate::provision::migrate_symbol(&mut catalog, "eurusd", "fx-eu-02", false).unwrap();
        let observation = simulate(&catalog, "fx-eu-02", Scenario::Ready).unwrap();
        let plan = plan_shard(&catalog, "fx-eu-02", &observation, None, None).unwrap();
        assert!(plan.notes.iter().any(|note| note.contains("shadow")));
        assert!(
            !plan.blocked.iter().any(|blocked| blocked.contains("route")),
            "{:?}",
            plan.blocked
        );
    }

    #[test]
    fn disabled_deployments_are_skipped_by_simulation_and_plans() {
        let mut catalog = catalog();
        crate::provision::shutdown_symbol(&mut catalog, "eurusd", true).unwrap();
        let observation = simulate(&catalog, "fx-eu-01", Scenario::Ready).unwrap();
        assert!(
            observation
                .processes
                .iter()
                .all(|row| row.symbol != "eurusd"),
            "{:?}",
            observation.processes
        );
        let plan = plan_shard(&catalog, "fx-eu-01", &observation, None, None).unwrap();
        assert_eq!(plan.phase, PlanPhase::Ready);
        assert!(
            plan.steps.iter().all(|step| step.target != "eurusd"),
            "{:?}",
            plan.steps
        );
        assert!(
            plan.notes.iter().all(|note| !note.contains("eurusd")),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn all_disabled_shard_plan_retains_data_and_slots() {
        let mut catalog = catalog();
        crate::provision::shutdown_symbol(&mut catalog, "eurusd", true).unwrap();
        crate::provision::shutdown_symbol(&mut catalog, "gbpusd", false).unwrap();
        let observation = simulate(&catalog, "fx-eu-01", Scenario::Ready).unwrap();
        assert!(observation.processes.is_empty());
        let plan = plan_shard(&catalog, "fx-eu-01", &observation, None, None).unwrap();
        assert_eq!(plan.phase, PlanPhase::Reserved);
        assert!(plan.steps.is_empty());
        assert!(
            plan.notes
                .iter()
                .any(|note| note.contains("PVC") || note.contains("disabled")),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn fresh_shard_bootstraps_each_process_once() {
        let result = plan("fx-eu-01", Scenario::Fresh);
        assert_eq!(result.phase, PlanPhase::Bootstrap);
        let inits: Vec<&PlanStep> = result
            .steps
            .iter()
            .filter(|s| s.action == Action::InitCluster)
            .collect();
        assert_eq!(inits.len(), 2);
        assert_eq!(inits[0].target, "eurusd");
        assert_eq!(
            inits[0].endpoint,
            "fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001"
        );
        assert_eq!(inits[0].voters.len(), 4);
        assert_eq!(inits[0].voters[3].id, 3);
        assert_eq!(
            inits[0].voters[3].addr,
            "fx-eu-01-3.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001"
        );
        assert_eq!(
            inits[1].endpoint,
            "fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21002"
        );
    }

    #[test]
    fn unready_pods_block_bootstrap() {
        let catalog = catalog();
        let observation = simulate(&catalog, "fx-eu-01", Scenario::Degraded).unwrap();
        let result = plan_shard(&catalog, "fx-eu-01", &observation, None, None).unwrap();
        assert_eq!(result.phase, PlanPhase::Blocked);
        assert_eq!(result.steps.len(), 1);
        assert_eq!(result.steps[0].action, Action::WaitForPods);
        assert!(
            result.steps[0].target.contains("fx-eu-01-3"),
            "{}",
            result.steps[0].target
        );
        assert!(result.blocked.iter().any(|b| b.contains("not ready")));
    }

    #[test]
    fn mixed_bootstrap_state_is_not_reinitialised() {
        let catalog = catalog();
        let mut observation = simulate(&catalog, "fx-eu-01", Scenario::Ready).unwrap();
        for row in &mut observation.processes {
            if row.symbol == "eurusd" && row.pod == 2 {
                row.bootstrapped = false;
            }
        }
        let result = plan_shard(&catalog, "fx-eu-01", &observation, None, None).unwrap();
        assert_eq!(result.phase, PlanPhase::Blocked);
        assert!(
            result.blocked.iter().any(|b| b.contains("mixed bootstrap")),
            "{:?}",
            result.blocked
        );
        assert!(
            !result
                .steps
                .iter()
                .any(|step| step.action == Action::InitCluster)
        );
    }

    #[test]
    fn ready_shard_only_publishes_leaders() {
        let result = plan("fx-eu-01", Scenario::Ready);
        assert_eq!(result.phase, PlanPhase::Ready);
        assert_eq!(result.steps.len(), 2);
        assert!(
            result
                .steps
                .iter()
                .all(|s| s.action == Action::PublishRoute)
        );
        assert_eq!(
            result.steps[0].endpoint,
            "fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001"
        );
    }

    #[test]
    fn leader_loss_blocks_routing() {
        let result = plan("fx-eu-01", Scenario::LeaderLoss);
        assert_eq!(result.phase, PlanPhase::Blocked);
        assert!(result.blocked.iter().any(|b| b.contains("no leader")));
        assert!(result.steps.is_empty());
    }

    #[test]
    fn config_drift_is_applied_through_the_leader() {
        let result = plan("fx-eu-01", Scenario::ConfigDrift);
        assert_eq!(result.phase, PlanPhase::Configuring);
        let steps: Vec<&PlanStep> = result
            .steps
            .iter()
            .filter(|s| s.action == Action::ConfigureSymbol)
            .collect();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].target, "eurusd");
        assert_eq!(
            steps[0].endpoint,
            "fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001"
        );
    }

    #[test]
    fn same_version_upgrade_rolls_one_pod_at_a_time() {
        let result = plan("fx-eu-01", Scenario::Upgrade);
        assert_eq!(result.phase, PlanPhase::Upgrading);
        let restarts: Vec<&PlanStep> = result
            .steps
            .iter()
            .filter(|s| s.action == Action::RestartPod)
            .collect();
        assert_eq!(restarts.len(), 4);
        assert_eq!(restarts[0].target, "fx-eu-01-0");
        assert_eq!(restarts[3].target, "fx-eu-01-3");
        // Pod 0 hosts the leader of both processes, so it needs two transfers before restart.
        let first_restart = result
            .steps
            .iter()
            .position(|s| s.action == Action::RestartPod)
            .unwrap();
        let transfers = result.steps[..first_restart]
            .iter()
            .filter(|s| s.action == Action::TransferLeader)
            .count();
        assert_eq!(transfers, 2);
    }

    #[test]
    fn incompatible_upgrade_requires_a_recent_backup() {
        let catalog = catalog();
        let mut observation = simulate(&catalog, "fx-eu-01", Scenario::Upgrade).unwrap();
        observation.last_backup_at = Some(observation.now - 100 * 60 * 60);
        let target = Release {
            wire_version: Release::default().wire_version + 1,
            ..Default::default()
        };
        let result = plan_shard(&catalog, "fx-eu-01", &observation, None, Some(&target)).unwrap();
        assert_eq!(result.phase, PlanPhase::Blocked);
        assert_eq!(result.steps[0].action, Action::Backup);
        assert!(result.blocked.iter().any(|b| b.contains("backup")));
    }

    #[test]
    fn incompatible_upgrade_snapshots_every_process_before_restart() {
        let catalog = catalog();
        let mut observation = simulate(&catalog, "fx-eu-01", Scenario::Upgrade).unwrap();
        observation.last_backup_at = Some(observation.now - 60);
        let target = Release {
            snapshot_version: Release::default().snapshot_version + 1,
            ..Default::default()
        };
        let result = plan_shard(&catalog, "fx-eu-01", &observation, None, Some(&target)).unwrap();
        assert_eq!(result.phase, PlanPhase::Upgrading);
        let actions: Vec<Action> = result.steps.iter().map(|s| s.action).collect();
        assert_eq!(actions[0], Action::PauseTraffic);
        assert_eq!(
            actions.iter().filter(|a| **a == Action::Snapshot).count(),
            2
        );
        assert_eq!(
            actions.iter().filter(|a| **a == Action::RestartPod).count(),
            4
        );
    }

    #[test]
    fn mixed_version_release_can_roll() {
        let catalog = catalog();
        let observation = simulate(&catalog, "fx-eu-01", Scenario::Upgrade).unwrap();
        let target = Release {
            wire_version: Release::default().wire_version + 1,
            mixed_version_safe: true,
            ..Default::default()
        };
        let result = plan_shard(&catalog, "fx-eu-01", &observation, None, Some(&target)).unwrap();
        assert_eq!(result.phase, PlanPhase::Upgrading);
        assert!(
            result
                .steps
                .iter()
                .any(|s| s.action == Action::TransferLeader)
        );
    }

    #[test]
    fn paused_shard_never_mutates() {
        let mut catalog = catalog();
        catalog.shards.get_mut("fx-eu-01").unwrap().spec.paused = true;
        let observation = simulate(&catalog, "fx-eu-01", Scenario::Fresh).unwrap();
        let result = plan_shard(&catalog, "fx-eu-01", &observation, None, None).unwrap();
        assert_eq!(result.phase, PlanPhase::Paused);
        assert!(result.steps.is_empty());
    }

    #[test]
    fn membership_drift_uses_raft_not_replicas() {
        let catalog = catalog();
        let mut observation = simulate(&catalog, "fx-eu-01", Scenario::Ready).unwrap();
        for row in &mut observation.processes {
            row.voters = vec![0, 1, 2];
        }
        let result = plan_shard(&catalog, "fx-eu-01", &observation, None, None).unwrap();
        assert_eq!(result.phase, PlanPhase::Membership);
        assert_eq!(result.steps.len(), 2);
        assert!(
            result
                .steps
                .iter()
                .all(|s| s.action == Action::ChangeMembership && s.voters.len() == 4)
        );
    }

    #[test]
    fn writing_routes_back_updates_the_catalog() {
        let catalog = catalog();
        let observation = simulate(&catalog, "fx-eu-01", Scenario::Ready).unwrap();
        let mut catalog = catalog;
        apply_observed_routes(&mut catalog, &observation);
        let route = catalog.routes.get("eurusd").unwrap();
        assert_eq!(
            route.status.leader_addr.as_deref(),
            Some("fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001")
        );
        assert_eq!(route.status.phase, crate::model::Phase::Ready);
    }

    #[test]
    fn market_data_scenarios_produce_matching_observations() {
        let catalog = market_data_catalog();
        let fresh =
            simulate_market_data(&catalog, "fx-marketdata", MarketDataScenario::Fresh).unwrap();
        assert_eq!(fresh.service, "fx-marketdata");
        assert_eq!(fresh.cell, "eu-1");
        assert!(fresh.image.is_empty());
        assert!(!fresh.deployment_exists);
        assert_eq!(fresh.replicas, 0);
        assert!(!fresh.feed_ready);

        let ready =
            simulate_market_data(&catalog, "fx-marketdata", MarketDataScenario::Ready).unwrap();
        assert_eq!(
            ready.image,
            "registry.example.com/matchengine-marketdata@sha256:abc"
        );
        assert_eq!(ready.replicas, 2);
        assert_eq!(ready.ready_replicas, 2);
        assert!(ready.deployment_exists);
        assert!(ready.feed_ready);
        assert_eq!(ready.symbols.len(), 2);

        let behind =
            simulate_market_data(&catalog, "fx-marketdata", MarketDataScenario::Behind).unwrap();
        assert_eq!(behind.image, DEMO_MARKET_DATA_PREVIOUS_IMAGE);
        assert_eq!(behind.updated_replicas, 0);

        let paused =
            simulate_market_data(&catalog, "fx-marketdata", MarketDataScenario::Paused).unwrap();
        assert!(paused.paused);
        assert_eq!(paused.replicas, 0);
        assert!(!paused.feed_ready);
    }

    #[test]
    fn fresh_market_data_plan_renders_every_object() {
        let plan = md_plan(MarketDataScenario::Fresh);
        assert_eq!(plan.phase, PlanPhase::Provisioning);
        assert_eq!(plan.steps[0].action, Action::ValidateCatalog);
        let kinds: Vec<&str> = plan
            .steps
            .iter()
            .filter(|step| step.action == Action::RenderManifests)
            .map(|step| step.target.split('/').next().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "ConfigMap",
                "Deployment",
                "Service",
                "PodDisruptionBudget",
                "NetworkPolicy",
                "HorizontalPodAutoscaler"
            ]
        );
        assert!(
            plan.steps
                .iter()
                .any(|step| step.action == Action::WaitForPods)
        );
        assert!(summarize(&plan).contains("render-manifests"));
    }

    #[test]
    fn healthy_and_behind_market_data_plans_differ() {
        let healthy = md_plan(MarketDataScenario::Ready);
        assert_eq!(healthy.phase, PlanPhase::Ready);
        assert!(
            !healthy
                .steps
                .iter()
                .any(|step| step.action == Action::RollDeployment)
        );
        assert_eq!(
            healthy
                .steps
                .iter()
                .filter(|step| step.action == Action::VerifyFeed)
                .count(),
            1
        );
        assert!(
            healthy
                .steps
                .iter()
                .any(|step| step.action == Action::WriteStatus)
        );

        let behind = md_plan(MarketDataScenario::Behind);
        assert_eq!(behind.phase, PlanPhase::Upgrading);
        assert_eq!(
            behind
                .steps
                .iter()
                .filter(|step| step.action == Action::RollDeployment)
                .count(),
            2
        );
        assert!(
            behind
                .notes
                .iter()
                .any(|note| note.contains("maxUnavailable: 0"))
        );
    }

    #[test]
    fn paused_market_data_plan_never_rolls() {
        let mut catalog = market_data_catalog();
        catalog
            .market_data_services
            .get_mut("fx-marketdata")
            .unwrap()
            .spec
            .paused = true;
        let observation =
            simulate_market_data(&catalog, "fx-marketdata", MarketDataScenario::Ready).unwrap();
        let plan = plan_market_data(&catalog, "fx-marketdata", &observation, None, None).unwrap();
        assert_eq!(plan.phase, PlanPhase::Paused);
        assert!(
            plan.steps
                .iter()
                .any(|step| step.action == Action::PauseTraffic)
        );
        assert!(
            !plan
                .steps
                .iter()
                .any(|step| step.action == Action::RollDeployment)
        );
    }

    #[test]
    fn degraded_market_data_plan_is_blocked_on_the_feed() {
        let plan = md_plan(MarketDataScenario::Degraded);
        assert_eq!(plan.phase, PlanPhase::Blocked);
        assert!(
            plan.blocked
                .iter()
                .any(|blocked| blocked.contains("feed not ready")),
            "{:?}",
            plan.blocked
        );
        assert!(
            plan.steps
                .iter()
                .any(|step| step.action == Action::VerifyFeed)
        );
    }

    #[test]
    fn incompatible_market_data_release_blocks_before_render() {
        let catalog = market_data_catalog();
        let observation =
            simulate_market_data(&catalog, "fx-marketdata", MarketDataScenario::Ready).unwrap();
        let target = Release {
            wire_version: Release::default().wire_version + 1,
            ..Release::default()
        };
        let plan =
            plan_market_data(&catalog, "fx-marketdata", &observation, None, Some(&target)).unwrap();
        assert_eq!(plan.phase, PlanPhase::Blocked);
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].action, Action::ValidateCatalog);
        assert!(
            plan.blocked
                .iter()
                .any(|blocked| blocked.contains("wire version")),
            "{:?}",
            plan.blocked
        );
    }
}

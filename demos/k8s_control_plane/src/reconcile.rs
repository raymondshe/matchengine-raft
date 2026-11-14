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
    Release, SymbolDeployment, SymbolRoute, SymbolShard, TradingSymbol, UpgradeStrategy,
    desired_config_hash,
};
use crate::render::rolling_upgrade_safe;
use crate::store::{Catalog, now_epoch};

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
    if deployments.is_empty() {
        return Ok(ReconcilePlan {
            shard: shard_name.to_string(),
            phase: PlanPhase::Reserved,
            steps: Vec::new(),
            blocked: Vec::new(),
            notes: vec![
                "reserved shard: no SymbolDeployment is bound; the StatefulSet materializes when the first symbol is placed"
                    .to_string(),
            ],
            target_image: target_image.unwrap_or(&shard.spec.image).to_string(),
        });
    }
    let mut processes = Vec::new();
    for deployment in deployments {
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
    let deployments = catalog.shard_deployments(shard_name);
    let now = now_epoch();
    if deployments.is_empty() {
        return Ok(ShardObservation {
            shard: shard.name.clone(),
            processes: Vec::new(),
            last_backup_at: Some(now.saturating_sub(3_600)),
            now,
        });
    }
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
            let image = match scenario {
                Scenario::Upgrade => DEMO_PREVIOUS_IMAGE.to_string(),
                _ => DEMO_TARGET_IMAGE.to_string(),
            };
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
                image,
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

/// Patch route status from an observation. The operator does this after a plan's
/// `PublishRoute` step; the demo API uses it to keep the GUI's leader column honest.
pub fn apply_observed_routes(catalog: &mut Catalog, observation: &ShardObservation) {
    for route in catalog.routes.values_mut() {
        let Some(deployment) = catalog
            .deployments
            .get(&route.spec.active_deployment_ref)
            .cloned()
        else {
            continue;
        };
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::demo_catalog;

    fn catalog() -> Catalog {
        demo_catalog()
    }

    fn plan(shard: &str, scenario: Scenario) -> ReconcilePlan {
        let catalog = catalog();
        let observation = simulate(&catalog, shard, scenario).unwrap();
        plan_shard(&catalog, shard, &observation, None, None).unwrap()
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
}

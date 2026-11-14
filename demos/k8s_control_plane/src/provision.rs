//! Provisioning: create reserved shards, place a new symbol, and prepare a migration.
//!
//! The manager is the writer of desired state. These functions allocate the shard slot/port,
//! build the `SymbolDeployment` and `SymbolRoute`, and validate the whole candidate catalog
//! before committing. The operator still owns workload materialization, bootstrap and status.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::model::{
    DeploymentState, PackingMode, Phase, Placement, RouteSpec, RouteState, ShardStatus,
    SymbolDeployment, SymbolRoute, SymbolShard, TradingSymbol, dns_label,
};
use crate::store::Catalog;
use crate::validate::validate_catalog;

/// Create a symbol and bind it to a shard.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSymbolRequest {
    pub symbol: TradingSymbol,
    /// Explicit target shard. Omit to auto-place in `cell`.
    #[serde(default)]
    pub shard: Option<String>,
    /// Cell used for auto-placement. Optional when the catalog has exactly one cell.
    #[serde(default)]
    pub cell: Option<String>,
    /// Explicitly permit a containers-mode pod roll for a non-empty shard.
    ///
    /// The default is false: adding a container changes the StatefulSet pod template and
    /// recreates every pod in the shard, restarting the other symbols. Use a supervisor-mode
    /// shard, an empty shard, or opt in deliberately.
    #[serde(default)]
    pub allow_restart: bool,
}

/// Result of placing a symbol.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacementResult {
    pub shard: String,
    pub slot: u16,
    pub http_port: u16,
    pub symbol: TradingSymbol,
    pub deployment: SymbolDeployment,
    pub route: SymbolRoute,
}

/// A migration's desired-state shadow and the operator sequence to execute.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationPlan {
    pub symbol: String,
    pub source_shard: String,
    pub source_deployment: String,
    pub target_shard: String,
    pub target_deployment: String,
    pub target_slot: u16,
    pub target_http_port: u16,
    pub target_voters: Vec<String>,
    pub steps: Vec<MigrationStep>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationStep {
    pub action: String,
    pub detail: String,
}

/// Result of a graceful symbol shutdown.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShutdownResult {
    pub symbol: String,
    pub deployment: String,
    pub shard: String,
    /// `supervisor` or `containers`.
    pub mode: String,
    /// True when no other engine in the shard is restarted.
    pub zero_impact: bool,
    pub steps: Vec<LifecycleStep>,
    pub notes: Vec<String>,
}

/// Result of re-enabling a disabled symbol from its retained data.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartResult {
    pub symbol: String,
    pub deployment: String,
    pub shard: String,
    /// `supervisor` or `containers`.
    pub mode: String,
    /// True when no other engine in the shard is restarted.
    pub zero_impact: bool,
    pub steps: Vec<LifecycleStep>,
    pub notes: Vec<String>,
}

/// Result of dropping one shard from the catalog.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DropShardResult {
    pub shard: String,
    pub namespace: String,
    /// `supervisor`, `containers` or `reserved`.
    pub mode: String,
    /// True when the retained storage keeps the shard data recoverable.
    pub retain_storage: bool,
    /// All dropped deployments must already be Disabled, so no engine restarts.
    pub zero_impact: bool,
    pub removed_deployments: Vec<String>,
    pub removed_routes: Vec<String>,
    pub removed_symbols: Vec<String>,
    /// PVC names predicted from the StatefulSet volume claim template.
    pub pvcs: Vec<String>,
    pub steps: Vec<LifecycleStep>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleStep {
    pub action: String,
    pub detail: String,
}

/// Create an empty shard reservation. Workloads are materialized by the first deployment.
pub fn create_empty_shard(
    catalog: &mut Catalog,
    mut shard: SymbolShard,
) -> Result<SymbolShard, ApiError> {
    if catalog.shards.contains_key(&shard.name) {
        return Err(ApiError::conflict(
            "shard_exists",
            format!("shard {} already exists", shard.name),
        ));
    }
    let cell = catalog.cells.get(&shard.spec.cell).ok_or_else(|| {
        ApiError::not_found(
            "unknown_cell",
            format!("cell {} is not registered", shard.spec.cell),
        )
    })?;
    let cell_namespace = cell.namespace.clone();
    if shard.namespace.trim().is_empty() {
        shard.namespace = cell_namespace;
    }
    shard.status = ShardStatus {
        phase: Phase::Reserved,
        ..Default::default()
    };

    let mut candidate = catalog.clone();
    candidate.shards.insert(shard.name.clone(), shard.clone());
    let report = validate_catalog(&candidate);
    if !report.is_ok() {
        return Err(ApiError::unprocessable(report));
    }

    *catalog = candidate;
    catalog.audit(
        "api",
        "create-shard",
        &shard.name,
        format!(
            "reserved {} shard in {} with {} slots on ports {}-{}",
            class_name(&shard),
            shard.spec.cell,
            shard.spec.packing.max_processes_per_pod,
            shard.spec.network.http_base_port,
            shard.spec.network.http_base_port + shard.spec.packing.max_processes_per_pod - 1,
        ),
    );
    Ok(shard)
}

/// Place a new symbol in an explicit or automatically selected shard.
pub fn place_symbol(
    catalog: &mut Catalog,
    request: CreateSymbolRequest,
) -> Result<PlacementResult, ApiError> {
    let mut symbol = request.symbol;
    if symbol.name.trim().is_empty() || dns_label(&symbol.name) != symbol.name {
        return Err(ApiError::bad_request(
            "symbol_name",
            "symbol name must be a non-empty DNS-1123 label",
        ));
    }
    if catalog.symbols.contains_key(&symbol.name) {
        return Err(ApiError::conflict(
            "symbol_exists",
            format!("symbol {} already exists", symbol.name),
        ));
    }
    if let Some(existing) = catalog
        .symbols
        .values()
        .find(|s| s.spec.symbol_id == symbol.spec.symbol_id)
    {
        return Err(ApiError::conflict(
            "symbol_id_exists",
            format!(
                "symbol id {} is already used by {}",
                symbol.spec.symbol_id, existing.name
            ),
        ));
    }
    let route_name = symbol.name.clone();
    if catalog.routes.contains_key(&route_name) {
        return Err(ApiError::conflict(
            "route_exists",
            format!("route {route_name} already exists"),
        ));
    }

    let shard = select_shard(
        catalog,
        request.shard.as_deref(),
        request.cell.as_deref(),
        request.allow_restart,
    )?
    .clone();
    let slot = allocate_slot(catalog, &shard)?;
    let http_port = shard
        .spec
        .network
        .http_base_port
        .checked_add(slot)
        .ok_or_else(|| ApiError::bad_request("port_range", "shard HTTP port range overflow"))?;

    symbol.namespace = shard.namespace.clone();
    symbol.status = Default::default();
    symbol.spec.generation = 1;
    if symbol.spec.owner.trim().is_empty() {
        symbol.spec.owner = "manager".to_string();
    }

    let deployment = SymbolDeployment {
        name: format!("{}-g1", symbol.name),
        namespace: shard.namespace.clone(),
        spec: crate::model::DeploymentSpec {
            symbol_ref: symbol.name.clone(),
            generation: 1,
            shard_ref: shard.name.clone(),
            placement: Placement { slot, http_port },
            image: None,
            resources: None,
            config_hash: String::new(),
            desired_state: DeploymentState::Active,
        },
        status: Default::default(),
    };
    if catalog.deployments.contains_key(&deployment.name) {
        return Err(ApiError::conflict(
            "deployment_exists",
            format!("deployment {} already exists", deployment.name),
        ));
    }
    let route = SymbolRoute {
        name: route_name,
        namespace: shard.namespace.clone(),
        spec: RouteSpec {
            symbol_ref: symbol.name.clone(),
            cell_ref: shard.spec.cell.clone(),
            active_deployment_ref: deployment.name.clone(),
            previous_deployment_ref: None,
            state: Default::default(),
        },
        status: Default::default(),
    };

    let mut candidate = catalog.clone();
    candidate
        .symbols
        .insert(symbol.name.clone(), symbol.clone());
    candidate
        .deployments
        .insert(deployment.name.clone(), deployment.clone());
    candidate.routes.insert(route.name.clone(), route.clone());
    if let Some(target) = candidate.shards.get_mut(&shard.name) {
        target.status.phase = Phase::Pending;
    }
    let report = validate_catalog(&candidate);
    if !report.is_ok() {
        return Err(ApiError::unprocessable(report));
    }

    *catalog = candidate;
    catalog.audit(
        "api",
        "create-symbol",
        &symbol.name,
        format!(
            "placed on shard {} slot {} (http {} / raft {})",
            shard.name,
            slot,
            http_port,
            http_port + crate::model::RAFT_PORT_OFFSET
        ),
    );
    Ok(PlacementResult {
        shard: shard.name.clone(),
        slot,
        http_port,
        symbol,
        deployment,
        route,
    })
}

/// Create a shadow deployment in `target_shard` and return the operator's migration sequence.
///
/// The source route is deliberately unchanged. Traffic stays on the source until the target
/// has caught up and the operator performs the cutover step.
pub fn migrate_symbol(
    catalog: &mut Catalog,
    symbol_name: &str,
    target_shard: &str,
    allow_restart: bool,
) -> Result<MigrationPlan, ApiError> {
    let symbol = catalog.symbols.get(symbol_name).cloned().ok_or_else(|| {
        ApiError::not_found("symbol_not_found", format!("unknown symbol {symbol_name}"))
    })?;
    let route = catalog
        .route_for_symbol(symbol_name)
        .cloned()
        .ok_or_else(|| {
            ApiError::conflict(
                "route_missing",
                format!("symbol {symbol_name} has no SymbolRoute"),
            )
        })?;
    let source = catalog
        .deployments
        .get(&route.spec.active_deployment_ref)
        .cloned()
        .ok_or_else(|| {
            ApiError::conflict(
                "deployment_missing",
                format!(
                    "route {} points at missing deployment {}",
                    route.name, route.spec.active_deployment_ref
                ),
            )
        })?;
    if source.spec.shard_ref == target_shard {
        return Err(ApiError::bad_request(
            "migration_same_shard",
            "target shard must differ from the source shard",
        ));
    }
    let target = select_shard(catalog, Some(target_shard), None, allow_restart)?.clone();
    let slot = allocate_slot(catalog, &target)?;
    let http_port = target
        .spec
        .network
        .http_base_port
        .checked_add(slot)
        .ok_or_else(|| ApiError::bad_request("port_range", "target shard HTTP port overflow"))?;
    let shadow_name = format!("{symbol_name}-shadow");
    if catalog.deployments.contains_key(&shadow_name) {
        return Err(ApiError::conflict(
            "migration_exists",
            format!("shadow deployment {shadow_name} already exists"),
        ));
    }

    let shadow = SymbolDeployment {
        name: shadow_name.clone(),
        namespace: target.namespace.clone(),
        spec: crate::model::DeploymentSpec {
            symbol_ref: symbol_name.to_string(),
            generation: symbol.spec.generation,
            shard_ref: target.name.clone(),
            placement: Placement { slot, http_port },
            image: None,
            resources: None,
            config_hash: String::new(),
            desired_state: DeploymentState::Shadow,
        },
        status: Default::default(),
    };
    let mut candidate = catalog.clone();
    candidate
        .deployments
        .insert(shadow.name.clone(), shadow.clone());
    if let Some(target) = candidate.shards.get_mut(&target.name) {
        target.status.phase = Phase::Pending;
    }
    let report = validate_catalog(&candidate);
    if !report.is_ok() {
        return Err(ApiError::unprocessable(report));
    }

    *catalog = candidate;
    let voters = target_voters(&target, http_port);
    catalog.audit(
        "api",
        "migrate-symbol",
        symbol_name,
        format!(
            "shadow {} on shard {} slot {} (http {}), source {}",
            shadow_name, target.name, slot, http_port, source.spec.shard_ref
        ),
    );

    let source_addr = format!(
        "{}-0.{}-headless.{}:{}",
        source.spec.shard_ref,
        source.spec.shard_ref,
        source.namespace,
        source.spec.placement.http_port
    );
    let target_addr = format!(
        "{}-0.{}-headless.{}:{}",
        target.name, target.name, target.namespace, http_port
    );
    Ok(MigrationPlan {
        symbol: symbol_name.to_string(),
        source_shard: source.spec.shard_ref.clone(),
        source_deployment: source.name.clone(),
        target_shard: target.name.clone(),
        target_deployment: shadow_name.clone(),
        target_slot: slot,
        target_http_port: http_port,
        target_voters: voters.clone(),
        steps: vec![
            MigrationStep {
                action: "apply-target-shard".to_string(),
                detail: format!(
                    "render and apply shard {}; wait for the new {symbol_name} container on all {} pods",
                    target.name, target.spec.replicas
                ),
            },
            MigrationStep {
                action: "init-shadow".to_string(),
                detail: format!(
                    "POST {target_addr}/init with the {} target voters: {}",
                    voters.len(),
                    voters.join(", ")
                ),
            },
            MigrationStep {
                action: "add-learners".to_string(),
                detail: format!(
                    "POST the target voters as learners to the source leader at {source_addr}; wait for catch-up"
                ),
            },
            MigrationStep {
                action: "change-membership".to_string(),
                detail: format!(
                    "POST {source_addr}/change-membership with the {} target voters after the shadow catches up",
                    voters.len()
                ),
            },
            MigrationStep {
                action: "cutover-route".to_string(),
                detail: format!(
                    "patch SymbolRoute {} activeDeploymentRef={}, previousDeploymentRef={}, state=Cutover, then Active",
                    symbol_name, shadow_name, source.name
                ),
            },
            MigrationStep {
                action: "drain-source".to_string(),
                detail: format!(
                    "stop routing to {} and remove its container after the retention window",
                    source.name
                ),
            },
        ],
        notes: vec![
            "the source route is unchanged until the operator performs cutover".to_string(),
            "the target deployment is desiredState=Shadow and consumes a target shard slot"
                .to_string(),
            "slots are not reused while the source data is retained".to_string(),
        ],
    })
}

/// Gracefully shut down one symbol's engine.
///
/// Supervisor mode removes the child from `processes.json`; the supervisor stops it on every pod
/// while the other children keep running. Containers mode can only remove a container by
/// recreating pods, so a non-empty shard is rejected unless `allow_restart` is set. The deployment
/// record, route and slot remain, and the data directory is retained.
pub fn shutdown_symbol(
    catalog: &mut Catalog,
    symbol_name: &str,
    allow_restart: bool,
) -> Result<ShutdownResult, ApiError> {
    let symbol = catalog.symbols.get(symbol_name).cloned().ok_or_else(|| {
        ApiError::not_found("symbol_not_found", format!("unknown symbol {symbol_name}"))
    })?;
    let route = catalog
        .route_for_symbol(symbol_name)
        .cloned()
        .ok_or_else(|| {
            ApiError::conflict(
                "route_missing",
                format!("symbol {symbol_name} has no SymbolRoute"),
            )
        })?;
    let deployment = catalog
        .deployments
        .get(&route.spec.active_deployment_ref)
        .cloned()
        .ok_or_else(|| {
            ApiError::conflict(
                "deployment_missing",
                format!(
                    "route {} points at missing deployment {}",
                    route.name, route.spec.active_deployment_ref
                ),
            )
        })?;
    if deployment.spec.desired_state == DeploymentState::Disabled
        || route.spec.state == RouteState::Disabled
    {
        return Err(ApiError::conflict(
            "already_disabled",
            format!("symbol {symbol_name} is already shut down"),
        ));
    }
    let shard = catalog
        .shards
        .get(&deployment.spec.shard_ref)
        .cloned()
        .ok_or_else(|| {
            ApiError::conflict(
                "shard_missing",
                format!("shard {} is not registered", deployment.spec.shard_ref),
            )
        })?;
    let active_others = catalog
        .shard_deployments(&shard.name)
        .into_iter()
        .filter(|other| {
            other.name != deployment.name && other.spec.desired_state != DeploymentState::Disabled
        })
        .count();
    let supervisor = shard.spec.packing.mode == PackingMode::Supervisor;
    let zero_impact = supervisor || active_others == 0;
    if !supervisor && active_others > 0 && !allow_restart {
        return Err(ApiError::conflict(
            "restart_required",
            format!(
                "shard {} uses containers packing and hosts {} other active engine(s); removing this container recreates every pod and restarts them. Use a supervisor-mode shard, an empty shard, or set allowRestart=true.",
                shard.name, active_others
            ),
        ));
    }

    let mut candidate = catalog.clone();
    if let Some(target) = candidate.deployments.get_mut(&deployment.name) {
        target.spec.desired_state = DeploymentState::Disabled;
        target.status.phase = Phase::Disabled;
    }
    if let Some(target) = candidate.routes.get_mut(&route.name) {
        target.spec.state = RouteState::Disabled;
        target.status.phase = Phase::Disabled;
    }
    if let Some(target) = candidate.symbols.get_mut(&symbol.name) {
        target.status.phase = Phase::Disabled;
    }
    if let Some(target) = candidate.shards.get_mut(&shard.name) {
        target.status.phase = Phase::Pending;
    }
    let report = validate_catalog(&candidate);
    if !report.is_ok() {
        return Err(ApiError::unprocessable(report));
    }

    *catalog = candidate;
    catalog.audit(
        "api",
        "shutdown-symbol",
        &symbol.name,
        format!(
            "{} on shard {}; zeroImpact={}, allowRestart={}",
            deployment.name, shard.name, zero_impact, allow_restart
        ),
    );

    let mode = if supervisor {
        "supervisor"
    } else {
        "containers"
    };
    let mut steps = vec![
        LifecycleStep {
            action: "pause-traffic".to_string(),
            detail: format!(
                "patch SymbolRoute {} state=Disabled; the gateway stops new writes",
                route.name
            ),
        },
        LifecycleStep {
            action: "drain".to_string(),
            detail:
                "wait for in-flight writes; the engine flushes its WAL when it receives SIGTERM"
                    .to_string(),
        },
    ];
    if supervisor {
        steps.push(LifecycleStep {
            action: "stop-process".to_string(),
            detail: format!(
                "apply the updated {}-shard ConfigMap; the supervisor sends SIGTERM to {} on all four pods; the other children keep running",
                shard.name, symbol.name
            ),
        });
    } else if active_others == 0 {
        steps.push(LifecycleStep {
            action: "remove-workload".to_string(),
            detail: format!(
                "the shard has no other active engine; prune the {} StatefulSet and pods; PVC data is retained",
                shard.name
            ),
        });
    } else {
        steps.push(LifecycleStep {
            action: "rolling-remove".to_string(),
            detail: format!(
                "update the {} StatefulSet and recreate pods one at a time; the other engines restart one voter at a time",
                shard.name
            ),
        });
    }
    steps.push(LifecycleStep {
        action: "retain-data".to_string(),
        detail: format!(
            "keep /data/{}-<pod> and the deployment record/slot; do not reuse the slot until the retention window ends",
            symbol.name
        ),
    });

    let mut notes = vec![
        "the manager writes desired state; the operator applies the ConfigMap or workload change"
            .to_string(),
    ];
    if supervisor {
        notes.push(format!(
            "supervisor mode: stopping {} does not restart any other engine in {}",
            symbol.name, shard.name
        ));
    } else if active_others == 0 {
        notes.push(format!(
            "containers mode: {} was the only active engine in {}; no other engine is affected",
            symbol.name, shard.name
        ));
    } else {
        notes.push(format!(
            "containers mode: the opt-in roll restarts {} other engine(s) one voter at a time; data stays on the PVCs",
            active_others
        ));
    }
    notes.push(format!(
        "the deployment and route stay Disabled; starting {} again needs a new generation or an explicit re-enable operation",
        symbol.name
    ));

    Ok(ShutdownResult {
        symbol: symbol.name,
        deployment: deployment.name,
        shard: shard.name,
        mode: mode.to_string(),
        zero_impact,
        steps,
        notes,
    })
}

/// Re-enable a disabled symbol from its retained data directory and slot.
///
/// Supervisor mode adds the child back to `processes.json`; the supervisor starts it on every pod
/// while the other children keep running. Containers mode can only add a container back by
/// recreating pods, so a non-empty shard is rejected unless `allow_restart` is set. No new
/// generation, slot or port is allocated, and no `/init` or `ConfigureSymbol` is required when the
/// retained data directory is intact.
pub fn start_symbol(
    catalog: &mut Catalog,
    symbol_name: &str,
    allow_restart: bool,
) -> Result<StartResult, ApiError> {
    let symbol = catalog.symbols.get(symbol_name).cloned().ok_or_else(|| {
        ApiError::not_found("symbol_not_found", format!("unknown symbol {symbol_name}"))
    })?;
    let route = catalog
        .route_for_symbol(symbol_name)
        .cloned()
        .ok_or_else(|| {
            ApiError::conflict(
                "route_missing",
                format!("symbol {symbol_name} has no SymbolRoute"),
            )
        })?;
    let deployment = catalog
        .deployments
        .get(&route.spec.active_deployment_ref)
        .cloned()
        .ok_or_else(|| {
            ApiError::conflict(
                "deployment_missing",
                format!(
                    "route {} points at missing deployment {}",
                    route.name, route.spec.active_deployment_ref
                ),
            )
        })?;
    let deployment_disabled = deployment.spec.desired_state == DeploymentState::Disabled;
    let route_disabled = route.spec.state == RouteState::Disabled;
    if !deployment_disabled && !route_disabled {
        return Err(ApiError::conflict(
            "already_active",
            format!("symbol {symbol_name} is already active"),
        ));
    }
    let shard = catalog
        .shards
        .get(&deployment.spec.shard_ref)
        .cloned()
        .ok_or_else(|| {
            ApiError::conflict(
                "shard_missing",
                format!("shard {} is not registered", deployment.spec.shard_ref),
            )
        })?;
    let active_others = catalog
        .shard_deployments(&shard.name)
        .into_iter()
        .filter(|other| {
            other.name != deployment.name && other.spec.desired_state != DeploymentState::Disabled
        })
        .count();
    let supervisor = shard.spec.packing.mode == PackingMode::Supervisor;
    let zero_impact = supervisor || active_others == 0;
    if !supervisor && active_others > 0 && !allow_restart {
        return Err(ApiError::conflict(
            "restart_required",
            format!(
                "shard {} uses containers packing and hosts {} other active engine(s); adding this container back recreates every pod and restarts them. Use a supervisor-mode shard, an empty shard, or set allowRestart=true.",
                shard.name, active_others
            ),
        ));
    }

    let mut candidate = catalog.clone();
    if let Some(target) = candidate.deployments.get_mut(&deployment.name) {
        target.spec.desired_state = DeploymentState::Active;
        target.status.phase = Phase::Pending;
        target.status.ready = false;
        target.status.leader_pod = None;
        target.status.leader_addr = None;
        target.status.lag = None;
        target.status.conditions.clear();
    }
    if let Some(target) = candidate.routes.get_mut(&route.name) {
        target.spec.state = RouteState::Active;
        target.status.phase = Phase::Pending;
        target.status.leader_addr = None;
        target.status.active_deployment_ref = deployment.name.clone();
    }
    if let Some(target) = candidate.symbols.get_mut(&symbol.name) {
        target.status.phase = Phase::Pending;
    }
    if let Some(target) = candidate.shards.get_mut(&shard.name) {
        target.status.phase = Phase::Pending;
    }
    let report = validate_catalog(&candidate);
    if !report.is_ok() {
        return Err(ApiError::unprocessable(report));
    }

    *catalog = candidate;
    catalog.audit(
        "api",
        "start-symbol",
        &symbol.name,
        format!(
            "{} on shard {} slot {} http {}; zeroImpact={}, allowRestart={}",
            deployment.name,
            shard.name,
            deployment.spec.placement.slot,
            deployment.spec.placement.http_port,
            zero_impact,
            allow_restart
        ),
    );

    let mode = if supervisor {
        "supervisor"
    } else {
        "containers"
    };
    let mut steps = vec![LifecycleStep {
        action: "resume-route".to_string(),
        detail: format!(
            "set SymbolRoute {} state=Active but hold gateway traffic until the engine is ready",
            route.name
        ),
    }];
    if supervisor {
        steps.push(LifecycleStep {
            action: "start-process".to_string(),
            detail: format!(
                "apply the updated {}-shard ConfigMap; the supervisor starts {} on all four pods from its retained data directory; the other children keep running",
                shard.name, symbol.name
            ),
        });
    } else if active_others == 0 {
        steps.push(LifecycleStep {
            action: "materialize-workload".to_string(),
            detail: format!(
                "apply the {} manifests; the StatefulSet and pods recreate and mount the retained PVC",
                shard.name
            ),
        });
    } else {
        steps.push(LifecycleStep {
            action: "rolling-add".to_string(),
            detail: format!(
                "update the {} StatefulSet and recreate pods one at a time; the other engines restart one voter at a time",
                shard.name
            ),
        });
    }
    steps.push(LifecycleStep {
        action: "recover".to_string(),
        detail: format!(
            "open the existing /data/{}-<pod> WAL and snapshot; retain the existing slot {} and ports {}/{}; do not call /init or ConfigureSymbol unless the data directory was deleted",
            symbol.name,
            deployment.spec.placement.slot,
            deployment.spec.placement.http_port,
            deployment.spec.placement.raft_port()
        ),
    });
    steps.push(LifecycleStep {
        action: "verify-ready".to_string(),
        detail:
            "wait for GET /ready and the applied symbol config, then publish the leader and route"
                .to_string(),
    });

    let mut notes = vec![
        "the manager writes desired state; the operator applies the ConfigMap or workload change"
            .to_string(),
        format!(
            "the retained slot {} and data directory are reused; no new generation or port is allocated",
            deployment.spec.placement.slot
        ),
    ];
    if supervisor {
        notes.push(format!(
            "supervisor mode: starting {} does not restart any other engine in {}",
            symbol.name, shard.name
        ));
    } else if active_others == 0 {
        notes.push(format!(
            "containers mode: {} is the only active engine in {}; no other engine is affected",
            symbol.name, shard.name
        ));
    } else {
        notes.push(format!(
            "containers mode: the opt-in roll restarts {} other engine(s) one voter at a time; data stays on the PVCs",
            active_others
        ));
    }
    notes.push(
        "the engine must recover its own WAL/snapshot and rejoin the same four-voter group; the manager does not copy data"
            .to_string(),
    );

    Ok(StartResult {
        symbol: symbol.name,
        deployment: deployment.name,
        shard: shard.name,
        mode: mode.to_string(),
        zero_impact,
        steps,
        notes,
    })
}

/// Drop a shard after every engine in it has been shut down.
///
/// `retain_storage` decides whether the operator keeps the PVCs. The catalog always loses the
/// shard, its deployments and the routes that point at them; symbols still used by a deployment in
/// another shard are kept. This function is deliberately blocked while any deployment is active:
/// dropping an idle shard never restarts another engine.
pub fn drop_shard(
    catalog: &mut Catalog,
    shard_name: &str,
    retain_storage: bool,
) -> Result<DropShardResult, ApiError> {
    let shard = catalog.shards.get(shard_name).cloned().ok_or_else(|| {
        ApiError::not_found("shard_not_found", format!("unknown shard {shard_name}"))
    })?;
    let deployments: Vec<SymbolDeployment> = catalog
        .shard_deployments(shard_name)
        .into_iter()
        .cloned()
        .collect();
    let active: Vec<&SymbolDeployment> = deployments
        .iter()
        .filter(|deployment| deployment.spec.desired_state != DeploymentState::Disabled)
        .collect();
    if !active.is_empty() {
        let symbols = active
            .iter()
            .map(|deployment| deployment.spec.symbol_ref.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ApiError::conflict(
            "shard_not_idle",
            format!(
                "shard {shard_name} still hosts active process(es): {symbols}; shut them down before dropping the shard"
            ),
        ));
    }

    let mode = if deployments.is_empty() {
        "reserved"
    } else if shard.spec.packing.mode == PackingMode::Supervisor {
        "supervisor"
    } else {
        "containers"
    };
    let pvcs: Vec<String> = (0..shard.spec.replicas)
        .map(|pod| format!("data-{}-{pod}", shard.name))
        .collect();
    let removed_deployments: Vec<String> = deployments
        .iter()
        .map(|deployment| deployment.name.clone())
        .collect();
    let removed_names: BTreeSet<String> = removed_deployments.iter().cloned().collect();
    let dropped_symbols: BTreeSet<String> = deployments
        .iter()
        .map(|deployment| deployment.spec.symbol_ref.clone())
        .collect();

    let mut candidate = catalog.clone();
    candidate.shards.remove(shard_name);
    for deployment in &deployments {
        candidate.deployments.remove(&deployment.name);
    }
    let mut removed_routes = Vec::new();
    candidate.routes.retain(|route_name, route| {
        let touches_dropped = removed_names.contains(&route.spec.active_deployment_ref)
            || route
                .spec
                .previous_deployment_ref
                .as_ref()
                .is_some_and(|previous| removed_names.contains(previous));
        if touches_dropped {
            removed_routes.push(route_name.clone());
            false
        } else {
            true
        }
    });
    let mut removed_symbols = Vec::new();
    for symbol in dropped_symbols {
        let still_used = candidate
            .deployments
            .values()
            .any(|deployment| deployment.spec.symbol_ref == symbol);
        if !still_used && candidate.symbols.remove(&symbol).is_some() {
            removed_symbols.push(symbol);
        }
    }
    let report = validate_catalog(&candidate);
    if !report.is_ok() {
        return Err(ApiError::unprocessable(report));
    }

    *catalog = candidate;
    catalog.audit(
        "api",
        "drop-shard",
        shard_name,
        format!(
            "removed {} deployment(s), {} route(s), {} symbol(s); retainStorage={}",
            removed_deployments.len(),
            removed_routes.len(),
            removed_symbols.len(),
            retain_storage
        ),
    );

    let mut steps = Vec::new();
    if deployments.is_empty() {
        steps.push(LifecycleStep {
            action: "remove-catalog".to_string(),
            detail:
                "no deployment is bound; the manager removed the reserved shard record; verify any prior StatefulSet and pods are gone"
                    .to_string(),
        });
    } else {
        steps.push(LifecycleStep {
            action: "prune-workload".to_string(),
            detail: format!(
                "delete the {} StatefulSet, Services, PodDisruptionBudget, NetworkPolicy and ConfigMap; all engines are already Disabled",
                shard.name
            ),
        });
    }
    if retain_storage {
        steps.push(LifecycleStep {
            action: "keep-storage".to_string(),
            detail: format!(
                "keep the PVCs: {}; re-create a shard with the same name, replicas and storage class to rebind them",
                pvcs.join(", ")
            ),
        });
    } else {
        steps.push(LifecycleStep {
            action: "delete-storage".to_string(),
            detail: format!(
                "delete the PVCs: {}; this permanently deletes the retained WAL/snapshot data",
                pvcs.join(", ")
            ),
        });
    }
    steps.push(LifecycleStep {
        action: "verify".to_string(),
        detail: format!(
            "verify the {} pods and workloads are gone; the manager already removed the catalog records",
            shard.name
        ),
    });

    let mut notes = vec![format!(
        "the manager removed shard {} from the catalog with {} deployment(s), {} route(s) and {} symbol(s)",
        shard.name,
        removed_deployments.len(),
        removed_routes.len(),
        removed_symbols.len()
    )];
    if deployments.is_empty() {
        notes.push(format!(
            "reserved shard: no deployment is bound; the PVC names ({}) are the ones a previous workload with this name would have used",
            pvcs.join(", ")
        ));
    } else {
        notes.push(
            "all engines were already Disabled, so dropping the shard does not restart another engine"
                .to_string(),
        );
        if retain_storage {
            notes.push(format!(
                "PVCs remain ({}) and are the only copy of the data; do not reuse the shard name for unrelated data, and delete the PVCs only after an explicit retention decision",
                pvcs.join(", ")
            ));
            notes.push(
                "to bring the shard back, re-create it with the same name, replicas and storage class, re-add the same symbols/slots, then apply the manifests; new pods bind the retained PVCs and recover their WAL/snapshot"
                    .to_string(),
            );
        } else {
            notes.push(
                "PVC deletion is irreversible; only an external backup can restore this shard's data"
                    .to_string(),
            );
        }
    }
    notes.push(
        "the manager writes desired state; the operator performs the prune and PVC operations"
            .to_string(),
    );

    Ok(DropShardResult {
        shard: shard.name,
        namespace: shard.namespace,
        mode: mode.to_string(),
        retain_storage,
        zero_impact: true,
        removed_deployments,
        removed_routes,
        removed_symbols,
        pvcs,
        steps,
        notes,
    })
}

fn class_name(shard: &SymbolShard) -> &'static str {
    match shard.spec.class {
        crate::model::ShardClass::Dedicated => "dedicated",
        crate::model::ShardClass::Packed => "packed",
    }
}

fn target_voters(shard: &SymbolShard, http_port: u16) -> Vec<String> {
    (0..shard.spec.replicas)
        .map(|pod| {
            format!(
                "{}-{pod}.{}-headless.{}:{http_port}",
                shard.name, shard.name, shard.namespace
            )
        })
        .collect()
}

fn capacity(catalog: &Catalog, shard: &SymbolShard) -> usize {
    (shard.spec.packing.max_processes_per_pod as usize)
        .saturating_sub(catalog.shard_processes(&shard.name))
}

fn is_placeable(catalog: &Catalog, shard: &SymbolShard) -> bool {
    !shard.spec.paused && capacity(catalog, shard) > 0
}

fn select_shard<'a>(
    catalog: &'a Catalog,
    requested: Option<&str>,
    cell: Option<&str>,
    allow_restart: bool,
) -> Result<&'a SymbolShard, ApiError> {
    if let Some(name) = requested {
        let shard = catalog.shards.get(name).ok_or_else(|| {
            ApiError::not_found("shard_not_found", format!("unknown shard {name}"))
        })?;
        if shard.spec.paused {
            return Err(ApiError::conflict(
                "shard_paused",
                format!("shard {name} is paused"),
            ));
        }
        if capacity(catalog, shard) == 0 {
            return Err(ApiError::conflict(
                "shard_full",
                format!(
                    "shard {name} has no free slot ({} processes per pod)",
                    shard.spec.packing.max_processes_per_pod
                ),
            ));
        }
        ensure_zero_impact(catalog, shard, allow_restart)?;
        return Ok(shard);
    }

    let cell = match cell {
        Some(cell) => {
            if !catalog.cells.contains_key(cell) {
                return Err(ApiError::not_found(
                    "unknown_cell",
                    format!("cell {cell} is not registered"),
                ));
            }
            cell.to_string()
        }
        None if catalog.cells.len() == 1 => {
            catalog.cells.keys().next().cloned().unwrap_or_default()
        }
        None => {
            return Err(ApiError::bad_request(
                "cell_required",
                "select a shard or provide a cell for auto-placement",
            ));
        }
    };

    let mut candidates: Vec<&SymbolShard> = catalog
        .shards
        .values()
        .filter(|shard| {
            shard.spec.cell == cell
                && is_placeable(catalog, shard)
                && (allow_restart || zero_impact(catalog, shard))
        })
        .collect();
    // Best-fit: fill the most populated shard first, then break ties by name. A caller that
    // wants a reserved shard targets it explicitly.
    candidates.sort_by(|a, b| {
        catalog
            .shard_processes(&b.name)
            .cmp(&catalog.shard_processes(&a.name))
            .then_with(|| a.name.cmp(&b.name))
    });
    if candidates.is_empty() {
        let any_placeable = catalog
            .shards
            .values()
            .any(|shard| shard.spec.cell == cell && is_placeable(catalog, shard));
        if any_placeable && !allow_restart {
            return Err(ApiError::conflict(
                "restart_required",
                "only non-empty containers-mode shards have free capacity; use a supervisor-mode shard, an empty shard, or set allowRestart=true",
            ));
        }
        return Err(ApiError::conflict(
            "no_shard_capacity",
            format!("no shard in cell {cell} has a free process slot"),
        ));
    }
    Ok(candidates[0])
}

/// True when adding a process will not recreate any pod in the shard.
fn zero_impact(catalog: &Catalog, shard: &SymbolShard) -> bool {
    shard.spec.packing.mode == PackingMode::Supervisor || catalog.shard_processes(&shard.name) == 0
}

fn ensure_zero_impact(
    catalog: &Catalog,
    shard: &SymbolShard,
    allow_restart: bool,
) -> Result<(), ApiError> {
    if allow_restart || zero_impact(catalog, shard) {
        return Ok(());
    }
    Err(ApiError::conflict(
        "restart_required",
        format!(
            "shard {} uses containers packing and already runs {} process(es); adding a container changes the StatefulSet pod template and recreates every pod, restarting every engine one voter at a time. Use a supervisor-mode shard, an empty shard, or set allowRestart=true.",
            shard.name,
            catalog.shard_processes(&shard.name)
        ),
    ))
}

fn allocate_slot(catalog: &Catalog, shard: &SymbolShard) -> Result<u16, ApiError> {
    let used: BTreeSet<u16> = catalog
        .shard_deployments(&shard.name)
        .iter()
        .map(|deployment| deployment.spec.placement.slot)
        .collect();
    (0..shard.spec.packing.max_processes_per_pod)
        .find(|slot| !used.contains(slot))
        .ok_or_else(|| {
            ApiError::conflict(
                "shard_full",
                format!("shard {} has no free slot", shard.name),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssetClass, AssetId, FeeAssetPolicy, StpMode, TradingSymbolSpec};
    use crate::store::demo_catalog;

    fn symbol(name: &str, id: u32) -> TradingSymbol {
        TradingSymbol {
            name: name.to_string(),
            namespace: String::new(),
            spec: TradingSymbolSpec {
                symbol_id: id,
                base: AssetId {
                    code: "USD".to_string(),
                    class: AssetClass::Fiat,
                    decimals: 2,
                },
                quote: AssetId {
                    code: "JPY".to_string(),
                    class: AssetClass::Fiat,
                    decimals: 0,
                },
                price_decimals: 3,
                price_tick: "0.001".to_string(),
                base_lot: "0.01".to_string(),
                fee_asset_policy: FeeAssetPolicy::Received,
                stp: StpMode::Off,
                owner: "test".to_string(),
                generation: 1,
            },
            status: Default::default(),
        }
    }

    fn request(
        name: &str,
        id: u32,
        shard: Option<&str>,
        cell: Option<&str>,
    ) -> CreateSymbolRequest {
        CreateSymbolRequest {
            symbol: symbol(name, id),
            shard: shard.map(str::to_string),
            cell: cell.map(str::to_string),
            allow_restart: false,
        }
    }

    #[test]
    fn create_empty_shard_sets_reserved_phase() {
        let mut catalog = demo_catalog();
        let mut shard = catalog.shards["fx-eu-02"].clone();
        shard.name = "fx-eu-03".to_string();
        shard.spec.network.http_base_port = 21061;
        shard.status = Default::default();
        let created = create_empty_shard(&mut catalog, shard).unwrap();
        assert_eq!(created.status.phase, Phase::Reserved);
        assert_eq!(catalog.shard_processes("fx-eu-03"), 0);
        assert!(
            catalog
                .audit
                .iter()
                .any(|entry| entry.action == "create-shard" && entry.target == "fx-eu-03")
        );
    }

    #[test]
    fn place_symbol_uses_the_first_free_slot() {
        let mut catalog = demo_catalog();
        let placement =
            place_symbol(&mut catalog, request("usdjpy", 10, Some("fx-eu-02"), None)).unwrap();
        assert_eq!(placement.shard, "fx-eu-02");
        assert_eq!(placement.slot, 0);
        assert_eq!(placement.http_port, 21041);
        assert_eq!(placement.deployment.name, "usdjpy-g1");
        assert_eq!(placement.route.spec.active_deployment_ref, "usdjpy-g1");
        assert_eq!(catalog.shards["fx-eu-02"].status.phase, Phase::Pending);
        assert_eq!(catalog.shard_processes("fx-eu-02"), 1);
    }

    #[test]
    fn second_symbol_uses_the_next_slot() {
        let mut catalog = demo_catalog();
        catalog
            .shards
            .get_mut("fx-eu-02")
            .unwrap()
            .spec
            .packing
            .mode = PackingMode::Supervisor;
        place_symbol(&mut catalog, request("usdjpy", 10, Some("fx-eu-02"), None)).unwrap();
        let placement =
            place_symbol(&mut catalog, request("audusd", 11, Some("fx-eu-02"), None)).unwrap();
        assert_eq!(placement.slot, 1);
        assert_eq!(placement.http_port, 21042);
    }

    #[test]
    fn containers_shard_requires_opt_in_to_restart_existing_engines() {
        let mut catalog = demo_catalog();
        let error =
            place_symbol(&mut catalog, request("usdjpy", 10, Some("fx-eu-01"), None)).unwrap_err();
        assert_eq!(error.code, "restart_required");
        assert!(error.message.contains("recreates every pod"));

        let mut allowed = request("usdjpy", 10, Some("fx-eu-01"), None);
        allowed.allow_restart = true;
        let placement = place_symbol(&mut catalog, allowed).unwrap();
        assert_eq!(placement.slot, 2);
        assert_eq!(placement.http_port, 21003);
    }

    #[test]
    fn supervisor_shard_adds_without_a_restart() {
        let mut catalog = demo_catalog();
        catalog
            .shards
            .get_mut("fx-eu-02")
            .unwrap()
            .spec
            .packing
            .mode = PackingMode::Supervisor;
        let first =
            place_symbol(&mut catalog, request("usdjpy", 10, Some("fx-eu-02"), None)).unwrap();
        assert_eq!(first.slot, 0);
        let second =
            place_symbol(&mut catalog, request("audusd", 11, Some("fx-eu-02"), None)).unwrap();
        assert_eq!(second.slot, 1);
    }

    #[test]
    fn full_shard_is_rejected() {
        let mut catalog = demo_catalog();
        catalog
            .shards
            .get_mut("fx-eu-02")
            .unwrap()
            .spec
            .packing
            .mode = PackingMode::Supervisor;
        for index in 0..4 {
            place_symbol(
                &mut catalog,
                request(&format!("sym{index}"), 10 + index, Some("fx-eu-02"), None),
            )
            .unwrap();
        }
        let error =
            place_symbol(&mut catalog, request("sym9", 20, Some("fx-eu-02"), None)).unwrap_err();
        assert_eq!(error.code, "shard_full");
    }

    #[test]
    fn auto_placement_uses_a_zero_impact_shard() {
        let mut catalog = demo_catalog();
        let placement =
            place_symbol(&mut catalog, request("usdjpy", 10, None, Some("eu-1"))).unwrap();
        // fx-eu-01 is a non-empty containers shard (would restart engines), me-btcusdt is full;
        // the empty fx-eu-02 shard is the safe choice.
        assert_eq!(placement.shard, "fx-eu-02");
        assert_eq!(placement.slot, 0);
        assert_eq!(placement.http_port, 21041);
    }

    #[test]
    fn auto_placement_can_opt_in_to_restarting_an_old_shard() {
        let mut catalog = demo_catalog();
        let mut request = request("usdjpy", 10, None, Some("eu-1"));
        request.allow_restart = true;
        let placement = place_symbol(&mut catalog, request).unwrap();
        assert_eq!(placement.shard, "fx-eu-01");
        assert_eq!(placement.slot, 2);
        assert_eq!(placement.http_port, 21003);
    }

    #[test]
    fn duplicate_symbol_and_id_are_rejected() {
        let mut catalog = demo_catalog();
        let error =
            place_symbol(&mut catalog, request("eurusd", 10, Some("fx-eu-02"), None)).unwrap_err();
        assert_eq!(error.code, "symbol_exists");
        let error =
            place_symbol(&mut catalog, request("usdjpy", 1, Some("fx-eu-02"), None)).unwrap_err();
        assert_eq!(error.code, "symbol_id_exists");
    }

    #[test]
    fn migrate_creates_a_shadow_without_moving_the_route() {
        let mut catalog = demo_catalog();
        let plan = migrate_symbol(&mut catalog, "eurusd", "fx-eu-02", false).unwrap();
        assert_eq!(plan.source_shard, "fx-eu-01");
        assert_eq!(plan.target_shard, "fx-eu-02");
        assert_eq!(plan.target_slot, 0);
        assert_eq!(plan.target_http_port, 21041);
        assert_eq!(plan.target_voters.len(), 4);
        let shadow = &catalog.deployments["eurusd-shadow"];
        assert_eq!(shadow.spec.shard_ref, "fx-eu-02");
        assert_eq!(shadow.spec.placement.slot, 0);
        assert_eq!(shadow.spec.desired_state, DeploymentState::Shadow);
        assert_eq!(
            catalog.routes["eurusd"].spec.active_deployment_ref,
            "eurusd-g1"
        );
        assert_eq!(catalog.shard_processes("fx-eu-02"), 1);
        assert!(plan.steps.iter().any(|step| step.action == "cutover-route"));
        assert!(plan.steps.iter().any(|step| step.action == "drain-source"));
    }

    #[test]
    fn migration_into_the_same_shard_is_rejected() {
        let mut catalog = demo_catalog();
        let error = migrate_symbol(&mut catalog, "eurusd", "fx-eu-01", false).unwrap_err();
        assert_eq!(error.code, "migration_same_shard");
    }

    fn supervisor_catalog() -> Catalog {
        let mut catalog = demo_catalog();
        catalog
            .shards
            .get_mut("fx-eu-01")
            .unwrap()
            .spec
            .packing
            .mode = PackingMode::Supervisor;
        catalog
    }

    #[test]
    fn supervisor_shutdown_stops_one_child_and_retains_its_slot() {
        let mut catalog = supervisor_catalog();
        let result = shutdown_symbol(&mut catalog, "eurusd", false).unwrap();
        assert_eq!(result.mode, "supervisor");
        assert!(result.zero_impact);
        assert!(
            result
                .steps
                .iter()
                .any(|step| step.action == "stop-process")
        );
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("does not restart any other engine"))
        );
        assert_eq!(
            catalog.deployments["eurusd-g1"].spec.desired_state,
            DeploymentState::Disabled
        );
        assert_eq!(
            catalog.deployments["eurusd-g1"].status.phase,
            Phase::Disabled
        );
        assert_eq!(catalog.routes["eurusd"].spec.state, RouteState::Disabled);
        assert_eq!(catalog.routes["eurusd"].status.phase, Phase::Disabled);
        assert_eq!(catalog.symbols["eurusd"].status.phase, Phase::Disabled);
        assert_eq!(
            catalog.deployments["gbpusd-g1"].spec.desired_state,
            DeploymentState::Active
        );
        // The data directory and slot are retained: the disabled record still owns slot 0.
        assert_eq!(catalog.shard_processes("fx-eu-01"), 2);
        assert_eq!(
            catalog
                .shard_deployments("fx-eu-01")
                .iter()
                .filter(|deployment| deployment.spec.desired_state == DeploymentState::Disabled)
                .count(),
            1
        );
        let error = shutdown_symbol(&mut catalog, "eurusd", false).unwrap_err();
        assert_eq!(error.code, "already_disabled");
    }

    #[test]
    fn containers_shutdown_requires_opt_in_when_other_engines_run() {
        let mut catalog = demo_catalog();
        let error = shutdown_symbol(&mut catalog, "eurusd", false).unwrap_err();
        assert_eq!(error.code, "restart_required");
        assert_eq!(
            catalog.deployments["eurusd-g1"].spec.desired_state,
            DeploymentState::Active
        );

        let result = shutdown_symbol(&mut catalog, "eurusd", true).unwrap();
        assert_eq!(result.mode, "containers");
        assert!(!result.zero_impact);
        assert!(
            result
                .steps
                .iter()
                .any(|step| step.action == "rolling-remove")
        );
        assert_eq!(
            catalog.deployments["gbpusd-g1"].spec.desired_state,
            DeploymentState::Active
        );
    }

    #[test]
    fn containers_shutdown_of_the_last_active_engine_is_zero_impact() {
        let mut catalog = demo_catalog();
        shutdown_symbol(&mut catalog, "eurusd", true).unwrap();
        let result = shutdown_symbol(&mut catalog, "gbpusd", false).unwrap();
        assert_eq!(result.mode, "containers");
        assert!(result.zero_impact);
        assert!(
            result
                .steps
                .iter()
                .any(|step| step.action == "remove-workload")
        );
        assert!(result.steps.iter().any(|step| step.action == "retain-data"));
    }

    #[test]
    fn dedicated_shutdown_is_zero_impact() {
        let mut catalog = demo_catalog();
        let result = shutdown_symbol(&mut catalog, "btcusdt", false).unwrap();
        assert_eq!(result.mode, "containers");
        assert!(result.zero_impact);
        assert_eq!(
            catalog.deployments["btcusdt-g1"].spec.desired_state,
            DeploymentState::Disabled
        );
    }

    #[test]
    fn shutdown_unknown_symbol_is_not_found() {
        let mut catalog = demo_catalog();
        let error = shutdown_symbol(&mut catalog, "nope", false).unwrap_err();
        assert_eq!(error.code, "symbol_not_found");
        assert_eq!(error.status, axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn supervisor_start_reuses_the_retained_slot() {
        let mut catalog = supervisor_catalog();
        shutdown_symbol(&mut catalog, "eurusd", false).unwrap();
        let result = start_symbol(&mut catalog, "eurusd", false).unwrap();
        assert_eq!(result.mode, "supervisor");
        assert!(result.zero_impact);
        for action in ["resume-route", "start-process", "recover", "verify-ready"] {
            assert!(
                result.steps.iter().any(|step| step.action == action),
                "missing {action}: {:?}",
                result
                    .steps
                    .iter()
                    .map(|step| step.action.as_str())
                    .collect::<Vec<_>>()
            );
        }
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("does not restart any other engine"))
        );
        let deployment = &catalog.deployments["eurusd-g1"];
        assert_eq!(deployment.spec.desired_state, DeploymentState::Active);
        assert_eq!(deployment.spec.placement.slot, 0);
        assert_eq!(deployment.spec.placement.http_port, 21001);
        assert_eq!(deployment.status.phase, Phase::Pending);
        assert!(!deployment.status.ready);
        assert_eq!(catalog.routes["eurusd"].spec.state, RouteState::Active);
        assert_eq!(catalog.routes["eurusd"].status.phase, Phase::Pending);
        assert_eq!(catalog.symbols["eurusd"].status.phase, Phase::Pending);
        assert_eq!(catalog.shard_processes("fx-eu-01"), 2);
        let error = start_symbol(&mut catalog, "eurusd", false).unwrap_err();
        assert_eq!(error.code, "already_active");
    }

    #[test]
    fn containers_start_requires_opt_in_when_other_engines_run() {
        let mut catalog = demo_catalog();
        shutdown_symbol(&mut catalog, "eurusd", true).unwrap();
        let error = start_symbol(&mut catalog, "eurusd", false).unwrap_err();
        assert_eq!(error.code, "restart_required");
        assert_eq!(
            catalog.deployments["eurusd-g1"].spec.desired_state,
            DeploymentState::Disabled
        );

        let result = start_symbol(&mut catalog, "eurusd", true).unwrap();
        assert_eq!(result.mode, "containers");
        assert!(!result.zero_impact);
        assert!(result.steps.iter().any(|step| step.action == "rolling-add"));
        assert_eq!(
            catalog.deployments["eurusd-g1"].spec.desired_state,
            DeploymentState::Active
        );
        assert_eq!(
            catalog.deployments["gbpusd-g1"].spec.desired_state,
            DeploymentState::Active
        );
    }

    #[test]
    fn containers_start_of_the_only_engine_is_zero_impact() {
        let mut catalog = demo_catalog();
        shutdown_symbol(&mut catalog, "eurusd", true).unwrap();
        shutdown_symbol(&mut catalog, "gbpusd", false).unwrap();
        let result = start_symbol(&mut catalog, "eurusd", false).unwrap();
        assert_eq!(result.mode, "containers");
        assert!(result.zero_impact);
        assert!(
            result
                .steps
                .iter()
                .any(|step| step.action == "materialize-workload")
        );
        assert_eq!(catalog.deployments["eurusd-g1"].spec.placement.slot, 0);
        assert_eq!(
            catalog.deployments["gbpusd-g1"].spec.desired_state,
            DeploymentState::Disabled
        );
    }

    #[test]
    fn start_unknown_symbol_is_not_found() {
        let mut catalog = demo_catalog();
        let error = start_symbol(&mut catalog, "nope", false).unwrap_err();
        assert_eq!(error.code, "symbol_not_found");
    }

    #[test]
    fn drop_reserved_shard_removes_only_its_catalog_record() {
        let mut catalog = demo_catalog();
        let result = drop_shard(&mut catalog, "fx-eu-02", true).unwrap();
        assert_eq!(result.mode, "reserved");
        assert!(result.zero_impact);
        assert!(result.removed_deployments.is_empty());
        assert_eq!(result.pvcs.len(), 4);
        assert!(
            result
                .steps
                .iter()
                .any(|step| step.action == "remove-catalog")
        );
        assert!(
            result
                .steps
                .iter()
                .any(|step| step.action == "keep-storage")
        );
        assert!(!catalog.shards.contains_key("fx-eu-02"));
        assert!(catalog.shards.contains_key("fx-eu-01"));
        assert!(catalog.deployments.contains_key("eurusd-g1"));
        assert!(
            catalog
                .audit
                .iter()
                .any(|entry| entry.action == "drop-shard" && entry.target == "fx-eu-02")
        );
    }

    #[test]
    fn drop_shard_requires_every_engine_to_be_disabled() {
        let mut catalog = demo_catalog();
        let error = drop_shard(&mut catalog, "fx-eu-01", true).unwrap_err();
        assert_eq!(error.code, "shard_not_idle");
        assert!(error.message.contains("eurusd"));
        assert!(catalog.shards.contains_key("fx-eu-01"));
    }

    #[test]
    fn drop_supervisor_shard_retains_pvcs_and_removes_dependents() {
        let mut catalog = supervisor_catalog();
        shutdown_symbol(&mut catalog, "eurusd", false).unwrap();
        shutdown_symbol(&mut catalog, "gbpusd", false).unwrap();
        let result = drop_shard(&mut catalog, "fx-eu-01", true).unwrap();
        assert_eq!(result.mode, "supervisor");
        assert!(result.retain_storage);
        assert_eq!(
            result.removed_deployments,
            vec!["eurusd-g1".to_string(), "gbpusd-g1".to_string()]
        );
        assert_eq!(
            result.pvcs,
            vec![
                "data-fx-eu-01-0".to_string(),
                "data-fx-eu-01-1".to_string(),
                "data-fx-eu-01-2".to_string(),
                "data-fx-eu-01-3".to_string()
            ]
        );
        assert!(
            result
                .steps
                .iter()
                .any(|step| step.action == "prune-workload")
        );
        assert!(
            result
                .steps
                .iter()
                .any(|step| step.action == "keep-storage")
        );
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("re-create it with the same name"))
        );
        assert!(!catalog.shards.contains_key("fx-eu-01"));
        assert!(!catalog.deployments.contains_key("eurusd-g1"));
        assert!(!catalog.deployments.contains_key("gbpusd-g1"));
        assert!(!catalog.routes.contains_key("eurusd"));
        assert!(!catalog.routes.contains_key("gbpusd"));
        assert!(!catalog.symbols.contains_key("eurusd"));
        assert!(!catalog.symbols.contains_key("gbpusd"));
        assert!(catalog.symbols.contains_key("btcusdt"));
    }

    #[test]
    fn drop_shard_can_delete_storage() {
        let mut catalog = demo_catalog();
        shutdown_symbol(&mut catalog, "btcusdt", false).unwrap();
        let result = drop_shard(&mut catalog, "me-btcusdt", false).unwrap();
        assert_eq!(result.mode, "containers");
        assert!(!result.retain_storage);
        assert_eq!(result.pvcs.len(), 4);
        assert!(
            result
                .steps
                .iter()
                .any(|step| step.action == "delete-storage")
        );
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("PVC deletion is irreversible"))
        );
        assert!(!catalog.shards.contains_key("me-btcusdt"));
        assert!(!catalog.symbols.contains_key("btcusdt"));
    }

    #[test]
    fn drop_keeps_a_symbol_that_still_has_a_shadow_elsewhere() {
        let mut catalog = demo_catalog();
        migrate_symbol(&mut catalog, "eurusd", "fx-eu-02", false).unwrap();
        shutdown_symbol(&mut catalog, "eurusd", false).unwrap_err();
        // The source shard is containers mode while gbpusd is still active; shut both down.
        shutdown_symbol(&mut catalog, "eurusd", true).unwrap();
        shutdown_symbol(&mut catalog, "gbpusd", false).unwrap();
        let result = drop_shard(&mut catalog, "fx-eu-01", true).unwrap();
        assert_eq!(result.removed_symbols, vec!["gbpusd".to_string()]);
        assert!(catalog.symbols.contains_key("eurusd"));
        assert!(catalog.deployments.contains_key("eurusd-shadow"));
        assert!(!catalog.routes.contains_key("eurusd"));
    }
}

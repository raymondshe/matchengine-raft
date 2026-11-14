//! Provisioning: create reserved shards, place a new symbol, and prepare a migration.
//!
//! The manager is the writer of desired state. These functions allocate the shard slot/port,
//! build the `SymbolDeployment` and `SymbolRoute`, and validate the whole candidate catalog
//! before committing. The operator still owns workload materialization, bootstrap and status.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::model::{
    DeploymentState, PackingMode, Phase, Placement, RouteSpec, ShardStatus, SymbolDeployment,
    SymbolRoute, SymbolShard, TradingSymbol, dns_label,
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
}

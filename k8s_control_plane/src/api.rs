//! Manager API.
//!
//! This is the read/validate/plan surface a real manager would expose in front of the catalog.
//! It never mutates Kubernetes directly: the operator owns cluster mutation. The manager does
//! read node image caches and engine pods so the upgrade picker can list valid local images. The
//! demo adds two convenience endpoints (`/simulate`, `POST /bundles`) so the GUI can be exercised
//! without a cluster or a database.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::Response;
use axum::routing::{any, get, post};
use axum::{Extension, Json, Router};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::backup;
use crate::bundle::ShardBundle;
use crate::error::ApiError;
use crate::model::dns_label;
use crate::model::{
    Cell, DeploymentState, MarketDataService, MarketDataServiceSpec, OrderSystem, Release,
    SymbolDeployment, SymbolRoute, SymbolShard, TradingSymbol,
};
use crate::provision::{self, CreateSymbolRequest};
use crate::reconcile::{
    Action, MarketDataObservation, MarketDataScenario, PlanStep, ReconcilePlan, Scenario,
    ShardObservation, summarize,
};
use crate::render::{
    Profile, RenderOptions, as_list, render_market_data_service, render_order_system, render_shard,
};
use crate::store::{AuditEntry, Catalog, Store, now_epoch};
use crate::validate::{Report, per_pod_totals, validate_bundle, validate_catalog, validate_symbol};

/// Options for the engine-console reverse proxy.
#[derive(Debug, Clone, Default)]
pub struct ConsoleOptions {
    /// Dial this host instead of the pod's FQDN. Local demos set `127.0.0.1`; production leaves
    /// it unset and the manager resolves the headless-service DNS name in-cluster.
    pub host: Option<String>,
}

/// Build the manager router. The GUI is mounted at `/` and `/console`.
pub fn router(store: Arc<Store>) -> Router {
    router_with_options(store, ConsoleOptions::default())
}

/// Build the manager router with an explicit engine-console proxy setup.
pub fn router_with_options(store: Arc<Store>, console: ConsoleOptions) -> Router {
    Router::new()
        .route("/", get(crate::gui::index))
        .route("/console", get(crate::gui::index))
        .route("/console/", get(crate::gui::index))
        .route("/api/v1/health", get(health))
        .route("/api/v1/catalog", get(catalog))
        .route("/api/v1/backup-status", get(backup_status))
        .route("/api/v1/images", get(images))
        .route("/api/v1/cells", get(cells))
        .route("/api/v1/shards", get(shards).post(create_shard))
        .route("/api/v1/shards/{name}", get(shard_detail))
        .route("/api/v1/shards/{name}/upgrade", post(upgrade_shard))
        .route("/api/v1/shards/{name}/drop", post(drop_shard))
        .route("/api/v1/shards/{name}/manifests", get(manifests))
        .route(
            "/api/v1/shards/{name}/plan",
            get(shard_plan).post(post_plan),
        )
        .route("/api/v1/shards/{name}/simulate", post(simulate))
        .route(
            "/api/v1/shards/{name}/processes/{symbol}/pods/{pod}/console",
            get(console_index),
        )
        .route(
            "/api/v1/shards/{name}/processes/{symbol}/pods/{pod}/console/",
            get(console_index),
        )
        .route(
            "/api/v1/shards/{name}/processes/{symbol}/pods/{pod}/console/{*path}",
            any(console_proxy),
        )
        .route("/api/v1/symbols", get(symbols).post(create_symbol))
        .route("/api/v1/symbols/{name}/migrate", post(migrate_symbol))
        .route("/api/v1/symbols/{name}/shutdown", post(shutdown_symbol))
        .route("/api/v1/symbols/{name}/start", post(start_symbol))
        .route("/api/v1/deployments", get(deployments))
        .route("/api/v1/routes", get(routes))
        .route(
            "/api/v1/market-data-services",
            get(market_data_services).post(create_market_data_service),
        )
        .route(
            "/api/v1/market-data-services/{name}",
            get(market_data_service_detail).put(edit_market_data_service),
        )
        .route(
            "/api/v1/market-data-services/{name}/manifests",
            get(market_data_manifests),
        )
        .route(
            "/api/v1/market-data-services/{name}/upgrade",
            post(upgrade_market_data),
        )
        .route(
            "/api/v1/market-data-services/{name}/rollback",
            post(rollback_market_data),
        )
        .route(
            "/api/v1/market-data-services/{name}/scale",
            post(scale_market_data),
        )
        .route(
            "/api/v1/market-data-services/{name}/plan",
            get(market_data_plan).post(post_market_data_plan),
        )
        .route(
            "/api/v1/order-systems",
            get(order_systems).post(create_order_system),
        )
        .route("/api/v1/order-systems/{name}", get(order_system_detail))
        .route(
            "/api/v1/order-systems/{name}/manifests",
            get(order_system_manifests),
        )
        .route(
            "/api/v1/order-systems/{name}/upgrade",
            post(upgrade_order_system),
        )
        .route("/api/v1/validate", get(validate))
        .route("/api/v1/bundles", post(upsert_bundle))
        .route("/api/v1/audit", get(audit))
        .layer(Extension(console))
        .with_state(store)
}

async fn health(State(store): State<Arc<Store>>) -> Json<Value> {
    let catalog = store.snapshot();
    let report = validate_catalog(&catalog);
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "store": store.mode(),
        "cell": if catalog.cells.len() == 1 {
            catalog.cells.keys().next().cloned()
        } else {
            None
        },
        "counts": counts(&catalog),
        "errors": report.errors.len(),
        "warnings": report.warnings.len(),
    }))
}

#[derive(Debug, Deserialize)]
struct BackupStatusQuery {
    #[serde(default)]
    shard: Option<String>,
}

/// Latest backup and signed restore drill published by the backup/status Jobs.
async fn backup_status(
    State(store): State<Arc<Store>>,
    Query(query): Query<BackupStatusQuery>,
) -> Result<Json<backup::BackupStatusView>, ApiError> {
    let shard = query
        .shard
        .ok_or_else(|| ApiError::bad_request("backup_status_query", "pass ?shard=<shard-name>"))?;
    let catalog = store.snapshot();
    if !catalog.shards.contains_key(&shard) {
        return Err(ApiError::not_found(
            "shard_not_found",
            format!("unknown shard {shard}"),
        ));
    }
    let record = store.backup_status(&shard).await?;
    Ok(Json(backup::view_backup_status(
        &shard,
        record.as_ref(),
        now_epoch(),
    )))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImagesQuery {
    #[serde(default)]
    shard: Option<String>,
    #[serde(default)]
    market_data_service: Option<String>,
    #[serde(default)]
    order_system: Option<String>,
}

/// Images the engine, market-data or order-system upgrade picker can offer.
///
/// The CRD backend also lists node-cached images and running engine/market-data/order-system pods
/// (read-only). Other backends return the references recorded in the catalog.
async fn images(
    State(store): State<Arc<Store>>,
    Query(query): Query<ImagesQuery>,
) -> Result<Json<Value>, ApiError> {
    let selectors = [
        query.shard.is_some(),
        query.market_data_service.is_some(),
        query.order_system.is_some(),
    ];
    if selectors.iter().filter(|selected| **selected).count() > 1 {
        return Err(ApiError::bad_request(
            "images_query",
            "pass only one of shard, marketDataService or orderSystem",
        ));
    }
    let inventory = match (
        query.shard.as_deref(),
        query.market_data_service.as_deref(),
        query.order_system.as_deref(),
    ) {
        (_, Some(name), _) => store.available_market_data_images(name).await?,
        (_, _, Some(name)) => store.available_order_system_images(name).await?,
        (shard, None, None) => store.available_images(shard).await?,
    };
    Ok(Json(json!(inventory)))
}

fn counts(catalog: &Catalog) -> Value {
    let active_processes = catalog
        .deployments
        .values()
        .filter(|deployment| deployment.spec.desired_state != DeploymentState::Disabled)
        .count();
    json!({
        "cells": catalog.cells.len(),
        "shards": catalog.shards.len(),
        "symbols": catalog.symbols.len(),
        "deployments": catalog.deployments.len(),
        "routes": catalog.routes.len(),
        "marketDataServices": catalog.market_data_services.len(),
        "orderSystems": catalog.order_systems.len(),
        "processes": active_processes,
    })
}

async fn catalog(State(store): State<Arc<Store>>) -> Json<Value> {
    let catalog = store.snapshot();
    let report = validate_catalog(&catalog);
    Json(catalog_value(&catalog, &report))
}

fn catalog_value(catalog: &Catalog, report: &Report) -> Value {
    json!({
        "cells": catalog.cells,
        "shards": catalog.shards,
        "symbols": catalog.symbols,
        "deployments": catalog.deployments,
        "routes": catalog.routes,
        "marketDataServices": catalog.market_data_services,
        "orderSystems": catalog.order_systems,
        "counts": counts(catalog),
        "validation": report,
    })
}

async fn cells(State(store): State<Arc<Store>>) -> Json<Value> {
    let catalog = store.snapshot();
    let report = validate_catalog(&catalog);
    Json(json!({
        "items": catalog.cells.values().collect::<Vec<_>>(),
        "validation": report,
    }))
}

async fn shards(State(store): State<Arc<Store>>) -> Json<Value> {
    let catalog = store.snapshot();
    let report = validate_catalog(&catalog);
    let items: Vec<Value> = catalog
        .shards
        .values()
        .map(|shard| {
            let all_deployments = catalog.shard_deployments(&shard.name);
            let deployments: Vec<&SymbolDeployment> = all_deployments
                .iter()
                .copied()
                .filter(|d| d.spec.desired_state != DeploymentState::Disabled)
                .collect();
            let symbols: Vec<String> = deployments
                .iter()
                .map(|d| d.spec.symbol_ref.clone())
                .collect();
            let totals = per_pod_totals(shard, &deployments);
            json!({
                "name": shard.name,
                "namespace": shard.namespace,
                "cell": shard.spec.cell,
                "class": shard.spec.class,
                "environment": shard.spec.environment,
                "replicas": shard.spec.replicas,
                "image": shard.spec.image,
                "paused": shard.spec.paused,
                "processes": totals.processes,
                "disabledProcesses": all_deployments.len() - deployments.len(),
                "slots": shard.spec.packing.max_processes_per_pod,
                "freeSlots": shard
                    .spec
                    .packing
                    .max_processes_per_pod
                    .saturating_sub(all_deployments.len() as u16),
                "symbols": symbols,
                "perPod": {
                    "cpuRequestMillis": totals.cpu_request_millis,
                    "cpuRequest": format_cpu(totals.cpu_request_millis),
                    "memoryRequestBytes": totals.memory_request_bytes,
                    "memoryRequest": format_bytes(totals.memory_request_bytes),
                },
                "phase": shard.status.phase,
            })
        })
        .collect();
    Json(json!({ "items": items, "validation": report }))
}

async fn create_shard(
    State(store): State<Arc<Store>>,
    Json(shard): Json<SymbolShard>,
) -> Result<Json<Value>, ApiError> {
    let mut created = None;
    store
        .mutate(|catalog| {
            created = Some(provision::create_empty_shard(catalog, shard)?);
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "shard": created })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DropShardRequest {
    /// Keep the PVCs so the shard can be recreated from its data. Defaults to the safe choice.
    #[serde(default = "default_retain_storage")]
    retain_storage: bool,
}

fn default_retain_storage() -> bool {
    true
}

async fn drop_shard(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Json(request): Json<DropShardRequest>,
) -> Result<Json<Value>, ApiError> {
    let mut drop = None;
    store
        .mutate(|catalog| {
            drop = Some(provision::drop_shard(
                catalog,
                &name,
                request.retain_storage,
            )?);
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "drop": drop })))
}

async fn create_symbol(
    State(store): State<Arc<Store>>,
    Json(request): Json<CreateSymbolRequest>,
) -> Result<Json<Value>, ApiError> {
    let mut placement = None;
    store
        .mutate(|catalog| {
            placement = Some(provision::place_symbol(catalog, request)?);
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "placement": placement })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpgradeShardRequest {
    image: String,
    release: Release,
}

async fn upgrade_shard(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Json(request): Json<UpgradeShardRequest>,
) -> Result<Json<Value>, ApiError> {
    let mut upgrade = None;
    store
        .mutate(|catalog| {
            upgrade = Some(provision::upgrade_shard(
                catalog,
                &name,
                request.image,
                request.release,
            )?);
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "upgrade": upgrade })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpgradeMarketDataRequest {
    image: String,
    /// Omit to keep the service's current release while changing only the image.
    #[serde(default)]
    release: Option<Release>,
}

async fn upgrade_market_data(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Json(request): Json<UpgradeMarketDataRequest>,
) -> Result<Json<Value>, ApiError> {
    let mut upgrade = None;
    store
        .mutate(|catalog| {
            let release = request
                .release
                .clone()
                .or_else(|| {
                    catalog
                        .market_data_services
                        .get(&name)
                        .map(|service| service.spec.release.clone())
                })
                .ok_or_else(|| {
                    ApiError::not_found(
                        "market_data_not_found",
                        format!("unknown market-data service {name}"),
                    )
                })?;
            upgrade = Some(provision::upgrade_market_data_service(
                catalog,
                &name,
                request.image.clone(),
                release,
            )?);
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "upgrade": upgrade })))
}

#[derive(Debug, Deserialize)]
struct EditMarketDataServiceRequest {
    spec: MarketDataServiceSpec,
}

/// Edit the desired spec of a market-data service. Image and release are owned by the
/// upgrade/rollback endpoints and are silently preserved, so a GUI can submit the whole spec it
/// loaded without accidentally scheduling a rollout.
async fn edit_market_data_service(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Json(request): Json<EditMarketDataServiceRequest>,
) -> Result<Json<Value>, ApiError> {
    let mut updated = None;
    store
        .mutate(|catalog| {
            let existing = catalog.market_data_services.get(&name).cloned().ok_or_else(|| {
                ApiError::not_found(
                    "market_data_not_found",
                    format!("unknown market-data service {name}"),
                )
            })?;
            let mut spec = request.spec;
            if spec.replicas == 0 {
                return Err(ApiError::bad_request(
                    "invalid_replicas",
                    "spec.replicas must be greater than zero",
                ));
            }
            let image = existing.spec.image.clone();
            let release = existing.spec.release.clone();
            spec.image = image.clone();
            spec.release = release.clone();
            let service = MarketDataService {
                name: existing.name.clone(),
                namespace: existing.namespace.clone(),
                spec,
                status: existing.status,
            };
            catalog.market_data_services.insert(name.clone(), service.clone());
            let mut scope = BTreeSet::new();
            scope.insert(name.clone());
            let report = validate_catalog(catalog).scoped(&scope);
            if !report.is_ok() {
                return Err(ApiError::unprocessable(report));
            }
            catalog.audit(
                "api",
                "edit-market-data",
                &name,
                format!(
                    "updated market-data spec; kept image {image} and release wire v{} snapshot v{}",
                    release.wire_version, release.snapshot_version
                ),
            );
            updated = Some(service);
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "marketDataService": updated })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScaleMarketDataRequest {
    replicas: u32,
    #[serde(default)]
    min_replicas: Option<u32>,
    #[serde(default)]
    max_replicas: Option<u32>,
}

fn bounded_u16(value: u32, field: &str) -> Result<u16, ApiError> {
    u16::try_from(value).map_err(|_| {
        ApiError::bad_request(
            field,
            format!(
                "{field} must be between 0 and {} for a Deployment",
                u16::MAX
            ),
        )
    })
}

async fn scale_market_data(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Json(request): Json<ScaleMarketDataRequest>,
) -> Result<Json<Value>, ApiError> {
    let mut updated = None;
    store
        .mutate(|catalog| {
            let service = catalog.market_data_services.get(&name).cloned().ok_or_else(|| {
                ApiError::not_found(
                    "market_data_not_found",
                    format!("unknown market-data service {name}"),
                )
            })?;
            let replicas = bounded_u16(request.replicas, "replicas")?;
            if replicas == 0 && !service.spec.paused {
                return Err(ApiError::bad_request(
                    "invalid_replicas",
                    "replicas must be greater than zero; pause the service to scale to zero",
                ));
            }
            let min_replicas = match request.min_replicas {
                Some(value) => {
                    let min = bounded_u16(value, "min_replicas")?;
                    if min == 0 {
                        return Err(ApiError::bad_request(
                            "invalid_min_replicas",
                            "min_replicas must be greater than zero",
                        ));
                    }
                    min
                }
                None => service.spec.autoscaling.min_replicas,
            };
            let max_replicas = match request.max_replicas {
                Some(value) => bounded_u16(value, "max_replicas")?,
                None => service.spec.autoscaling.max_replicas,
            };
            if max_replicas > 0 && (min_replicas > replicas || replicas > max_replicas) {
                return Err(ApiError::bad_request(
                    "invalid_replica_bounds",
                    format!(
                        "replicas {replicas} must be between minReplicas {min_replicas} and maxReplicas {max_replicas}"
                    ),
                ));
            }
            let target = catalog.market_data_services.get_mut(&name).ok_or_else(|| {
                ApiError::not_found(
                    "market_data_not_found",
                    format!("unknown market-data service {name}"),
                )
            })?;
            target.spec.replicas = replicas;
            target.spec.autoscaling.min_replicas = min_replicas;
            target.spec.autoscaling.max_replicas = max_replicas;
            let scaled = target.clone();
            let mut scope = BTreeSet::new();
            scope.insert(name.clone());
            let report = validate_catalog(catalog).scoped(&scope);
            if !report.is_ok() {
                return Err(ApiError::unprocessable(report));
            }
            catalog.audit(
                "api",
                "scale-market-data",
                &name,
                format!(
                    "scaled {name} to {replicas} replicas (HPA {min_replicas}..{}); release and image unchanged",
                    if max_replicas == 0 {
                        "derived".to_string()
                    } else {
                        max_replicas.to_string()
                    }
                ),
            );
            updated = Some(scaled);
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "marketDataService": updated })))
}

/// Roll back a market-data service to the image/release saved by the last update. The response
/// uses the same `{ "upgrade": ... }` shape as `/upgrade` so callers can share one lifecycle UI.
async fn rollback_market_data(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let mut rollback = None;
    store
        .mutate(|catalog| {
            rollback = Some(provision::rollback_market_data_service(catalog, &name)?);
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "upgrade": rollback })))
}

/// Read-only plan preview for a market-data service. `?scenario=` synthesises an observation;
/// there is no store mutation and no Kubernetes call.
async fn market_data_plan(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Query(query): Query<ScenarioQuery>,
) -> Result<Json<Value>, ApiError> {
    let catalog = store.snapshot();
    let scenario = query
        .scenario
        .as_deref()
        .map(MarketDataScenario::parse)
        .transpose()?
        .unwrap_or(MarketDataScenario::Ready);
    let observation = crate::reconcile::simulate_market_data(&catalog, &name, scenario)?;
    let plan = crate::reconcile::plan_market_data(
        &catalog,
        &name,
        &observation,
        query.target_image.as_deref(),
        query.target_release().as_ref(),
    )?;
    Ok(Json(plan_value(&plan)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketDataPlanRequest {
    /// Explicit observation; when absent a scenario is simulated.
    #[serde(default)]
    observation: Option<MarketDataObservation>,
    #[serde(default)]
    scenario: Option<String>,
    #[serde(default)]
    target_image: Option<String>,
    #[serde(default)]
    target_release: Option<Release>,
}

async fn post_market_data_plan(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Json(request): Json<MarketDataPlanRequest>,
) -> Result<Json<Value>, ApiError> {
    let catalog = store.snapshot();
    let observation = match request.observation {
        Some(mut observation) => {
            if !observation.service.is_empty() && observation.service != name {
                return Err(ApiError::bad_request(
                    "observation_service",
                    format!(
                        "observation is for {} but the path is {name}",
                        observation.service
                    ),
                ));
            }
            if observation.service.is_empty() {
                observation.service = name.clone();
            }
            if observation.now == 0 {
                observation.now = now_epoch();
            }
            observation
        }
        None => {
            let scenario = request
                .scenario
                .as_deref()
                .map(MarketDataScenario::parse)
                .transpose()?
                .unwrap_or(MarketDataScenario::Ready);
            crate::reconcile::simulate_market_data(&catalog, &name, scenario)?
        }
    };
    let plan = crate::reconcile::plan_market_data(
        &catalog,
        &name,
        &observation,
        request.target_image.as_deref(),
        request.target_release.as_ref(),
    )?;
    Ok(Json(plan_value(&plan)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MigrateRequest {
    target_shard: String,
    #[serde(default)]
    allow_restart: bool,
}

/// Body for `POST /api/v1/symbols/{name}/shutdown` and `/start`.
///
/// `allowRestart` is the explicit opt-in for a containers-mode shard that still hosts other
/// active engines. The default rejects the request because adding or removing a container
/// recreates every pod in the StatefulSet.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct LifecycleRequest {
    #[serde(default)]
    allow_restart: bool,
}

async fn shutdown_symbol(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Json(request): Json<LifecycleRequest>,
) -> Result<Json<Value>, ApiError> {
    let mut shutdown = None;
    store
        .mutate(|catalog| {
            shutdown = Some(provision::shutdown_symbol(
                catalog,
                &name,
                request.allow_restart,
            )?);
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "shutdown": shutdown })))
}

async fn start_symbol(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Json(request): Json<LifecycleRequest>,
) -> Result<Json<Value>, ApiError> {
    let mut start = None;
    store
        .mutate(|catalog| {
            start = Some(provision::start_symbol(
                catalog,
                &name,
                request.allow_restart,
            )?);
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "start": start })))
}

async fn migrate_symbol(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Json(request): Json<MigrateRequest>,
) -> Result<Json<Value>, ApiError> {
    let mut migration = None;
    store
        .mutate(|catalog| {
            migration = Some(provision::migrate_symbol(
                catalog,
                &name,
                &request.target_shard,
                request.allow_restart,
            )?);
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "migration": migration })))
}

#[derive(Debug, Deserialize)]
struct ScenarioQuery {
    #[serde(default)]
    scenario: Option<String>,
    #[serde(default)]
    target_image: Option<String>,
    #[serde(default)]
    wire_version: Option<u16>,
    #[serde(default)]
    snapshot_version: Option<u16>,
    #[serde(default)]
    mixed_version_safe: Option<bool>,
}

impl ScenarioQuery {
    fn target_release(&self) -> Option<Release> {
        if self.wire_version.is_none() && self.snapshot_version.is_none() {
            return None;
        }
        let default = Release::default();
        Some(Release {
            wire_version: self.wire_version.unwrap_or(default.wire_version),
            snapshot_version: self.snapshot_version.unwrap_or(default.snapshot_version),
            mixed_version_safe: self.mixed_version_safe.unwrap_or(false),
        })
    }
}

/// Direct URL of one engine process's operational console (`/console/`).
///
/// The link is not proxied by the manager: it assumes the operator's browser or bastion can
/// resolve the pod's headless-service DNS name. Inside the cluster that is always true; from a
/// workstation the URL needs a port-forward or an internal ingress.
fn engine_console_url(shard: &SymbolShard, pod: u8, http_port: u16) -> String {
    format!(
        "http://{name}-{pod}.{name}-headless.{namespace}.svc.cluster.local:{http_port}/console/",
        name = shard.name,
        namespace = shard.namespace,
    )
}

type ConsoleClient = Client<HttpConnector, Full<Bytes>>;

fn console_client() -> &'static ConsoleClient {
    static CLIENT: OnceLock<ConsoleClient> = OnceLock::new();
    CLIENT.get_or_init(|| Client::builder(TokioExecutor::new()).build_http())
}

async fn console_index(
    State(store): State<Arc<Store>>,
    Extension(options): Extension<ConsoleOptions>,
    Path((name, symbol, pod)): Path<(String, String, u8)>,
    request: Request,
) -> Result<Response, ApiError> {
    proxy_console(&store, &options, &name, &symbol, pod, "", request).await
}

async fn console_proxy(
    State(store): State<Arc<Store>>,
    Extension(options): Extension<ConsoleOptions>,
    Path((name, symbol, pod, path)): Path<(String, String, u8, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    proxy_console(&store, &options, &name, &symbol, pod, &path, request).await
}

/// Reverse-proxy one engine console so the browser only needs the manager origin.
///
/// This is deliberately narrow: only the console assets, its JSON APIs and the five guarded
/// console actions are forwardable. The engine's `/init`, `/write`, `/read` and Raft endpoints
/// are not reachable through this path.
async fn proxy_console(
    store: &Store,
    options: &ConsoleOptions,
    shard_name: &str,
    symbol: &str,
    pod: u8,
    path: &str,
    request: Request,
) -> Result<Response, ApiError> {
    let catalog = store.snapshot();
    let shard = catalog.shards.get(shard_name).ok_or_else(|| {
        ApiError::not_found("shard_not_found", format!("unknown shard {shard_name}"))
    })?;
    if pod >= shard.spec.replicas {
        return Err(ApiError::bad_request(
            "pod_out_of_range",
            format!("pod {pod} is outside 0..{}", shard.spec.replicas),
        ));
    }
    let deployment = catalog
        .shard_deployments(shard_name)
        .into_iter()
        .find(|deployment| deployment.spec.symbol_ref == symbol)
        .ok_or_else(|| {
            ApiError::not_found(
                "process_not_found",
                format!("shard {shard_name} does not host symbol {symbol}"),
            )
        })?;
    if deployment.spec.desired_state == DeploymentState::Disabled {
        return Err(ApiError::conflict(
            "process_disabled",
            format!("symbol {symbol} is shut down; no engine console is available"),
        ));
    }
    let upstream_path = console_upstream_path(path)?;
    let host = options.host.clone().unwrap_or_else(|| {
        format!(
            "{shard_name}-{pod}.{shard_name}-headless.{}.svc.cluster.local",
            shard.namespace
        )
    });
    let port = deployment.spec.placement.http_port;
    let query = request
        .uri()
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let url = format!("http://{host}:{port}{upstream_path}{query}");
    let uri: axum::http::Uri = url
        .parse()
        .map_err(|error| ApiError::internal(format!("engine console URL {url}: {error}")))?;

    let (parts, body) = request.into_parts();
    // This route handles a raw Request, so axum's DefaultBodyLimit does not apply.
    // Cap the buffered body explicitly before forwarding it to the engine.
    const MAX_PROXY_BODY_BYTES: usize = 1024 * 1024;
    let body = axum::body::to_bytes(body, MAX_PROXY_BODY_BYTES)
        .await
        .map_err(|error| ApiError::bad_request("body", format!("read request body: {error}")))?;
    let mut upstream = axum::http::Request::builder().method(parts.method).uri(uri);
    for (name, value) in &parts.headers {
        if name.as_str() == "host"
            || name.as_str() == "content-length"
            || name.as_str() == "accept-encoding"
            || is_hop_by_hop(name)
        {
            continue;
        }
        upstream = upstream.header(name.clone(), value.clone());
    }
    let upstream = upstream
        .body(Full::new(body))
        .map_err(|error| ApiError::internal(format!("build console request: {error}")))?;
    let response = console_client().request(upstream).await.map_err(|error| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "engine_console_unreachable",
            format!("cannot reach the engine console at http://{host}:{port}: {error}"),
        )
    })?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "engine_console_body",
                format!("read console response: {error}"),
            )
        })?
        .to_bytes();
    let base = format!("/api/v1/shards/{shard_name}/processes/{symbol}/pods/{pod}/console");
    let body = if should_rewrite(&headers) {
        rewrite_console_body(&body, &base)
    } else {
        body.to_vec()
    };
    let mut out = Response::new(Body::from(body));
    *out.status_mut() = status;
    for (name, value) in &headers {
        if matches!(
            name.as_str(),
            "content-length" | "transfer-encoding" | "connection" | "content-encoding"
        ) {
            continue;
        }
        out.headers_mut().insert(name.clone(), value.clone());
    }
    Ok(out)
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn should_rewrite(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.starts_with("text/html")
                || value.starts_with("application/javascript")
                || value.starts_with("text/javascript")
        })
}

/// Rewrite the console's absolute asset/API paths onto the manager proxy prefix.
fn rewrite_console_body(body: &[u8], base: &str) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(body) else {
        return body.to_vec();
    };
    text.replace("\"/console/", &format!("\"{base}/"))
        .replace("\"/health\"", &format!("\"{base}/health\""))
        .replace("\"/events\"", &format!("\"{base}/events\""))
        .into_bytes()
}

/// Map a proxy subpath onto the engine console and reject everything else.
fn console_upstream_path(path: &str) -> Result<String, ApiError> {
    if path.is_empty() {
        return Ok("/console/".to_string());
    }
    if path == "health" {
        return Ok("/health".to_string());
    }
    if path == "events" {
        return Ok("/events".to_string());
    }
    let lower = path.to_ascii_lowercase();
    if lower.contains("..") || lower.contains("%2e") || lower.contains('\\') {
        return Err(ApiError::bad_request(
            "console_path",
            "engine console paths may not contain dot segments",
        ));
    }
    let allowed = matches!(
        path,
        "app.css" | "app.js" | "api/node" | "api/cluster" | "api/actions"
    ) || matches!(
        path,
        "api/actions/transfer-leader"
            | "api/actions/elect"
            | "api/actions/snapshot"
            | "api/actions/purge-log"
            | "api/actions/runtime-config"
    );
    if allowed {
        Ok(format!("/console/{path}"))
    } else {
        Err(ApiError::not_found(
            "console_path",
            format!("unknown engine console path {path}"),
        ))
    }
}

/// Use the real published backup timestamp when the CRD backend can read it.
///
/// Offline/demo backends keep the simulated observation so the GUI remains usable without a
/// cluster or a backup ConfigMap.
async fn apply_live_backup_status(store: &Store, shard: &str, observation: &mut ShardObservation) {
    if store.mode() != "crd" {
        return;
    }
    match store.backup_status(shard).await {
        Ok(Some(status)) => observation.last_backup_at = Some(status.report.bundle_created_at),
        Ok(None) => observation.last_backup_at = None,
        Err(_) => {}
    }
}

async fn shard_detail(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Query(query): Query<ScenarioQuery>,
) -> Result<Json<Value>, ApiError> {
    let catalog = store.snapshot();
    let shard = catalog
        .shards
        .get(&name)
        .ok_or_else(|| ApiError::not_found("shard_not_found", format!("unknown shard {name}")))?;
    let scenario = match query.scenario.as_deref() {
        Some(value) => Scenario::parse(value)?,
        None => Scenario::Ready,
    };
    let mut observation = crate::reconcile::simulate(&catalog, &name, scenario);
    if let Ok(observed) = observation.as_mut() {
        apply_live_backup_status(&store, &name, observed).await;
    }
    let validation = validate_catalog(&catalog);
    let all_deployments: Vec<&SymbolDeployment> = catalog.shard_deployments(&name);
    let deployments: Vec<&SymbolDeployment> = all_deployments
        .iter()
        .copied()
        .filter(|d| d.spec.desired_state != DeploymentState::Disabled)
        .collect();
    let symbols: Vec<&TradingSymbol> = deployments
        .iter()
        .filter_map(|d| catalog.symbols.get(&d.spec.symbol_ref))
        .collect();
    let routes: Vec<&SymbolRoute> = catalog
        .routes
        .values()
        .filter(|r| symbols.iter().any(|s| s.name == r.spec.symbol_ref))
        .collect();
    let cell = catalog.cells.get(&shard.spec.cell);
    let totals = per_pod_totals(shard, &deployments);
    let consoles: Vec<Value> = deployments
        .iter()
        .map(|deployment| {
            let leader = observation.as_ref().ok().and_then(|observation| {
                observation
                    .processes
                    .iter()
                    .find(|row| row.symbol == deployment.spec.symbol_ref && row.leader)
            });
            let pod = leader.map(|row| row.pod).unwrap_or(0);
            json!({
                "symbol": deployment.spec.symbol_ref,
                "deployment": deployment.name,
                "pod": pod,
                "leader": leader.is_some(),
                "url": engine_console_url(shard, pod, deployment.spec.placement.http_port),
                "proxyUrl": format!(
                    "/api/v1/shards/{}/processes/{}/pods/{pod}/console/",
                    shard.name, deployment.spec.symbol_ref
                ),
            })
        })
        .collect();
    let mut scope: BTreeSet<String> = BTreeSet::from([shard.name.clone(), shard.spec.cell.clone()]);
    for deployment in &all_deployments {
        scope.insert(deployment.name.clone());
    }
    for symbol in catalog.symbols.values() {
        if all_deployments
            .iter()
            .any(|d| d.spec.symbol_ref == symbol.name)
        {
            scope.insert(symbol.name.clone());
        }
    }
    for route in &routes {
        scope.insert(route.name.clone());
    }
    let shard_validation = validation.scoped(&scope);
    Ok(Json(json!({
        "shard": shard,
        "cell": cell,
        "deployments": all_deployments,
        "symbols": symbols,
        "routes": routes,
        "observation": observation.ok(),
        "scenario": scenario,
        "engineConsoles": consoles,
        "slots": shard.spec.packing.max_processes_per_pod,
        "freeSlots": shard
            .spec
            .packing
            .max_processes_per_pod
            .saturating_sub(all_deployments.len() as u16),
        "activeProcesses": deployments.len(),
        "disabledProcesses": all_deployments.len() - deployments.len(),
        "empty": deployments.is_empty(),
        "perPod": {
            "processes": totals.processes,
            "cpuRequestMillis": totals.cpu_request_millis,
            "cpuRequest": format_cpu(totals.cpu_request_millis),
            "memoryRequestBytes": totals.memory_request_bytes,
            "memoryRequest": format_bytes(totals.memory_request_bytes),
            "storageRequest": shard.spec.storage.size,
        },
        "validation": validation,
        "shardValidation": shard_validation,
    })))
}

#[derive(Debug, Deserialize)]
struct RenderQuery {
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    storage_class: Option<String>,
    #[serde(default)]
    replicas: Option<u8>,
    #[serde(default)]
    non_root: Option<bool>,
}

async fn manifests(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Query(query): Query<RenderQuery>,
) -> Result<Json<Value>, ApiError> {
    let catalog = store.snapshot();
    let options = render_options(query)?;
    let objects = render_shard(&catalog, &name, &options)?;
    let mut list = as_list(objects);
    list["metadata"] = json!({
        "name": format!("{name}-manifests"),
        "annotations": {
            "matching.io/profile": match options.profile {
                Profile::Local => "local",
                Profile::Production => "production",
            },
            "matching.io/rendered-by": "k8s_control_plane",
        },
    });
    Ok(Json(list))
}

fn render_options(query: RenderQuery) -> Result<RenderOptions, ApiError> {
    let profile = match query.profile.as_deref() {
        None | Some("production") | Some("prod") => Profile::Production,
        Some("local") => Profile::Local,
        Some(other) => {
            return Err(ApiError::bad_request(
                "profile",
                format!("unknown profile {other:?}; use production|local"),
            ));
        }
    };
    Ok(RenderOptions {
        profile,
        namespace: query.namespace,
        image_pull_policy: None,
        storage_class: query.storage_class,
        replicas: query.replicas,
        non_root: query.non_root,
    })
}

async fn shard_plan(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Query(query): Query<ScenarioQuery>,
) -> Result<Json<Value>, ApiError> {
    let catalog = store.snapshot();
    let scenario = match query.scenario.as_deref() {
        Some(value) => Scenario::parse(value)?,
        None => Scenario::Ready,
    };
    let mut observation = crate::reconcile::simulate(&catalog, &name, scenario)?;
    apply_live_backup_status(&store, &name, &mut observation).await;
    let plan = crate::reconcile::plan_shard(
        &catalog,
        &name,
        &observation,
        query.target_image.as_deref(),
        query.target_release().as_ref(),
    )?;
    Ok(Json(plan_value(&plan)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanRequest {
    /// Explicit observation; when absent the demo simulation is used.
    #[serde(default)]
    observation: Option<ShardObservation>,
    #[serde(default)]
    scenario: Option<String>,
    #[serde(default)]
    target_image: Option<String>,
    /// Target release metadata drives rolling vs stop-the-world.
    #[serde(default)]
    target_release: Option<Release>,
}

async fn post_plan(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Json(request): Json<PlanRequest>,
) -> Result<Json<Value>, ApiError> {
    let catalog = store.snapshot();
    let explicit = request.observation.is_some();
    let mut observation = match request.observation {
        Some(mut observation) => {
            if !observation.shard.is_empty() && observation.shard != name {
                return Err(ApiError::bad_request(
                    "observation_shard",
                    format!(
                        "observation is for {} but the path is {name}",
                        observation.shard
                    ),
                ));
            }
            if observation.shard.is_empty() {
                observation.shard = name.clone();
            }
            if observation.now == 0 {
                observation.now = crate::store::now_epoch();
            }
            observation
        }
        None => {
            let scenario = request
                .scenario
                .as_deref()
                .map(Scenario::parse)
                .transpose()?
                .unwrap_or(Scenario::Ready);
            crate::reconcile::simulate(&catalog, &name, scenario)?
        }
    };
    if !explicit {
        apply_live_backup_status(&store, &name, &mut observation).await;
    }
    let plan = crate::reconcile::plan_shard(
        &catalog,
        &name,
        &observation,
        request.target_image.as_deref(),
        request.target_release.as_ref(),
    )?;
    Ok(Json(plan_value(&plan)))
}

fn plan_value(plan: &ReconcilePlan) -> Value {
    json!({
        "plan": plan,
        "summary": summarize(plan),
    })
}

async fn simulate(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Query(query): Query<ScenarioQuery>,
) -> Result<Json<Value>, ApiError> {
    let scenario = query
        .scenario
        .as_deref()
        .map(Scenario::parse)
        .transpose()?
        .unwrap_or(Scenario::Ready);
    let mut result = Value::Null;
    store
        .mutate(|catalog| {
            let observation = crate::reconcile::simulate(catalog, &name, scenario)?;
            crate::reconcile::apply_observed_routes(catalog, &observation);
            catalog.audit(
                "demo",
                "simulate",
                &name,
                format!("applied observed routes for scenario {:?}", scenario),
            );
            let symbols: Vec<String> = catalog
                .shard_deployments(&name)
                .iter()
                .map(|d| d.spec.symbol_ref.clone())
                .collect();
            result = json!({
                "scenario": scenario,
                "observation": observation,
                "routes": catalog.routes.values()
                    .filter(|r| symbols.contains(&r.spec.symbol_ref))
                    .collect::<Vec<_>>(),
                "audit": catalog.recent_audit(5),
            });
            Ok(())
        })
        .await?;
    Ok(Json(result))
}

async fn symbols(State(store): State<Arc<Store>>) -> Json<Value> {
    let catalog = store.snapshot();
    let items: Vec<Value> = catalog
        .symbols
        .values()
        .map(|symbol| {
            let deployment = catalog
                .deployments
                .values()
                .find(|d| d.spec.symbol_ref == symbol.name);
            let shard = deployment.and_then(|d| catalog.shards.get(&d.spec.shard_ref));
            let route = catalog.route_for_symbol(&symbol.name);
            let validation = validate_symbol(symbol);
            json!({
                "symbol": symbol,
                "deployment": deployment,
                "shard": shard.map(|s| s.name.clone()),
                "shardClass": shard.map(|s| s.spec.class),
                "route": route,
                "validation": validation,
            })
        })
        .collect();
    Json(json!({ "items": items }))
}

async fn deployments(State(store): State<Arc<Store>>) -> Json<Value> {
    let catalog = store.snapshot();
    Json(json!({
        "items": catalog.deployments.values().collect::<Vec<_>>(),
    }))
}

async fn routes(State(store): State<Arc<Store>>) -> Json<Value> {
    let catalog = store.snapshot();
    Json(json!({
        "items": catalog.routes.values().collect::<Vec<_>>(),
    }))
}

async fn market_data_services(State(store): State<Arc<Store>>) -> Json<Value> {
    let catalog = store.snapshot();
    let items: Vec<Value> = catalog
        .market_data_services
        .values()
        .map(|service| {
            let mut value = serde_json::to_value(service).unwrap_or(Value::Null);
            value["sources"] = Value::Array(market_data_sources(&catalog, service));
            value
        })
        .collect();
    Json(json!({ "items": items }))
}

/// Engine shards that currently publish the symbols served by this fan-out tier.
fn market_data_sources(catalog: &Catalog, service: &MarketDataService) -> Vec<Value> {
    catalog
        .market_data_symbols(service)
        .into_iter()
        .map(|symbol| {
            let deployment = catalog
                .deployments
                .values()
                .find(|deployment| deployment.spec.symbol_ref == symbol);
            let shard =
                deployment.and_then(|deployment| catalog.shards.get(&deployment.spec.shard_ref));
            json!({
                "symbol": symbol,
                "deployment": deployment.map(|deployment| deployment.name.clone()),
                "shard": shard.map(|shard| shard.name.clone()),
                "namespace": deployment.map(|deployment| deployment.namespace.clone()),
            })
        })
        .collect()
}

async fn create_market_data_service(
    State(store): State<Arc<Store>>,
    Json(mut service): Json<MarketDataService>,
) -> Result<Json<Value>, ApiError> {
    service.name = service.name.trim().to_string();
    if service.name.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_name",
            "market-data service name is required",
        ));
    }
    if service.spec.cell.trim().is_empty() {
        return Err(ApiError::bad_request(
            "cell_required",
            "spec.cell is required",
        ));
    }
    if service.spec.image.trim().is_empty() {
        return Err(ApiError::bad_request(
            "image_required",
            "spec.image is required",
        ));
    }
    if service.spec.replicas == 0 {
        return Err(ApiError::bad_request(
            "invalid_replicas",
            "spec.replicas must be greater than zero",
        ));
    }
    if service.spec.feed_groups.is_empty() && service.spec.symbols.is_empty() {
        return Err(ApiError::bad_request(
            "feed_group_required",
            "declare at least one feed group or symbol",
        ));
    }
    let mut group_names = BTreeSet::new();
    for group in &service.spec.feed_groups {
        if group.name.trim().is_empty() || !group_names.insert(group.name.clone()) {
            return Err(ApiError::bad_request(
                "invalid_feed_group",
                "feed group names must be non-empty and unique",
            ));
        }
    }
    let name = service.name.clone();
    store
        .mutate(|catalog| {
            if !catalog.cells.contains_key(&service.spec.cell) {
                return Err(ApiError::bad_request(
                    "unknown_cell",
                    format!("cell {} is not in the catalog", service.spec.cell),
                ));
            }
            let namespace = if !service.namespace.is_empty() {
                service.namespace.clone()
            } else {
                catalog
                    .cells
                    .get(&service.spec.cell)
                    .map(|cell| cell.namespace.clone())
                    .unwrap_or_default()
            };
            if catalog.market_data_services.contains_key(&name) {
                return Err(ApiError::bad_request(
                    "duplicate_name",
                    format!("market data service {name} already exists"),
                ));
            }
            service.namespace = namespace;
            catalog
                .market_data_services
                .insert(name.clone(), service.clone());
            // Run the same preflight as the GUI/validate endpoint so an invalid service
            // (unknown symbol, even multicast address, overlapping stream bases, release
            // mismatch) never reaches the CRD store.
            let mut candidate = catalog.clone();
            candidate
                .market_data_services
                .insert(name.clone(), service.clone());
            let report = validate_catalog(&candidate);
            let mut scope = BTreeSet::new();
            scope.insert(name.clone());
            let scoped = report.scoped(&scope);
            if !scoped.is_ok() {
                return Err(ApiError::unprocessable(scoped));
            }
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "marketDataService": service })))
}

async fn market_data_service_detail(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let catalog = store.snapshot();
    let service = catalog.market_data_services.get(&name).ok_or_else(|| {
        ApiError::not_found("market_data_not_found", format!("unknown service {name}"))
    })?;
    let report = validate_catalog(&catalog);
    let mut scope = BTreeSet::new();
    scope.insert(name.clone());
    let (scheme, host) = if service.spec.websocket.ingress.is_empty() {
        (
            "ws",
            format!(
                "{}.{}.svc.cluster.local:{}",
                service.name, service.namespace, service.spec.websocket.port
            ),
        )
    } else {
        // The renderer only adds Ingress `spec.tls` when a Secret is configured;
        // a plaintext demo ingress must be advertised as ws://, not wss://.
        let scheme = if service.spec.websocket.tls_secret.trim().is_empty() {
            "ws"
        } else {
            "wss"
        };
        (
            scheme,
            service
                .spec
                .websocket
                .ingress
                .trim_end_matches('/')
                .to_string(),
        )
    };
    let fast_endpoint = format!("{scheme}://{host}/ws/<symbol>@<stream>");
    let terminal_endpoint =
        format!("{scheme}://{host}/stream?streams=<symbol>@<stream>/<symbol>@<stream>");
    let fast_only = service.spec.websocket.fast_only;
    Ok(Json(json!({
        "marketDataService": service,
        "sources": market_data_sources(&catalog, service),
        "endpoint": if fast_only {
            fast_endpoint.clone()
        } else {
            format!("{scheme}://{host}/ws")
        },
        "endpoints": {
            "fast": fast_endpoint,
            "terminal": if fast_only { Value::Null } else { json!(terminal_endpoint) },
        },
        "validation": report.scoped(&scope),
    })))
}

async fn market_data_manifests(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let catalog = store.snapshot();
    let objects = render_market_data_service(&catalog, &name, None)?;
    let mut list = as_list(objects);
    list["metadata"] = json!({
        "name": format!("{name}-manifests"),
        "annotations": {
            "matching.io/rendered-by": "k8s_control_plane",
        },
    });
    Ok(Json(list))
}

async fn order_systems(State(store): State<Arc<Store>>) -> Json<Value> {
    let catalog = store.snapshot();
    Json(json!({
        "items": catalog.order_systems.values().collect::<Vec<_>>(),
    }))
}

async fn create_order_system(
    State(store): State<Arc<Store>>,
    Json(mut system): Json<OrderSystem>,
) -> Result<Json<Value>, ApiError> {
    system.name = system.name.trim().to_string();
    if system.name.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_name",
            "order-system name is required",
        ));
    }
    if system.spec.cell.trim().is_empty() {
        return Err(ApiError::bad_request(
            "cell_required",
            "spec.cell is required",
        ));
    }
    if system.spec.image.trim().is_empty() {
        return Err(ApiError::bad_request(
            "image_required",
            "spec.image is required",
        ));
    }
    if system.spec.replicas == 0 {
        return Err(ApiError::bad_request(
            "invalid_replicas",
            "spec.replicas must be greater than zero",
        ));
    }
    if system.spec.shards.is_empty() {
        return Err(ApiError::bad_request(
            "shard_required",
            "declare at least one user-ID shard",
        ));
    }
    if system.spec.targets.is_empty() {
        return Err(ApiError::bad_request(
            "target_required",
            "declare at least one engine/symbol target",
        ));
    }
    let name = system.name.clone();
    store
        .mutate(|catalog| {
            if !catalog.cells.contains_key(&system.spec.cell) {
                return Err(ApiError::bad_request(
                    "unknown_cell",
                    format!("cell {} is not in the catalog", system.spec.cell),
                ));
            }
            if system.namespace.is_empty() {
                system.namespace = catalog
                    .cells
                    .get(&system.spec.cell)
                    .map(|cell| cell.namespace.clone())
                    .unwrap_or_default();
            }
            if catalog.order_systems.contains_key(&name) {
                return Err(ApiError::bad_request(
                    "duplicate_name",
                    format!("order system {name} already exists"),
                ));
            }
            catalog.order_systems.insert(name.clone(), system.clone());
            let report = validate_catalog(catalog);
            let mut scope = BTreeSet::new();
            scope.insert(name.clone());
            let scoped = report.scoped(&scope);
            if !scoped.is_ok() {
                return Err(ApiError::unprocessable(scoped));
            }
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "orderSystem": system })))
}

async fn order_system_detail(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let catalog = store.snapshot();
    let system = catalog.order_systems.get(&name).ok_or_else(|| {
        ApiError::not_found(
            "order_system_not_found",
            format!("unknown order system {name}"),
        )
    })?;
    let report = validate_catalog(&catalog);
    let mut scope = BTreeSet::new();
    scope.insert(name.clone());
    let base = system.spec.ingress.trim().trim_end_matches('.');
    let service = dns_label(&system.name);
    let url = if base.is_empty() {
        format!(
            "http://{service}.{}.svc.cluster.local:{}",
            system.namespace, system.spec.port
        )
    } else {
        format!("http://{base}")
    };
    // Shard ranges are capacity metadata; every instance serves every user,
    // so all shard rows link to the same stateless order desk.
    let shards: Vec<Value> = system
        .spec
        .shards
        .iter()
        .map(|shard| {
            json!({
                "name": shard.name,
                "userIdMin": shard.user_id_min,
                "userIdMax": shard.user_id_max,
                "service": service,
                "url": url,
            })
        })
        .collect();
    Ok(Json(json!({
        "orderSystem": system,
        "service": service,
        "url": url,
        "shards": shards,
        "validation": report.scoped(&scope),
    })))
}

async fn order_system_manifests(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let catalog = store.snapshot();
    let objects = render_order_system(&catalog, &name, None)?;
    let mut list = as_list(objects);
    list["metadata"] = json!({
        "name": format!("{name}-manifests"),
        "annotations": { "matching.io/rendered-by": "k8s_control_plane" },
    });
    Ok(Json(list))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpgradeOrderSystemRequest {
    image: String,
}

async fn upgrade_order_system(
    State(store): State<Arc<Store>>,
    Path(name): Path<String>,
    Json(request): Json<UpgradeOrderSystemRequest>,
) -> Result<Json<Value>, ApiError> {
    let image = request.image.trim().to_string();
    if image.is_empty() {
        return Err(ApiError::bad_request(
            "image_required",
            "target image must not be empty",
        ));
    }
    let mut result = Value::Null;
    store
        .mutate(|catalog| {
            let system = catalog.order_systems.get(&name).cloned().ok_or_else(|| {
                ApiError::not_found(
                    "order_system_not_found",
                    format!("unknown order system {name}"),
                )
            })?;
            if system.spec.paused {
                return Err(ApiError::conflict(
                    "order_system_paused",
                    format!("order system {name} is paused; resume it before changing the image"),
                ));
            }
            if system.spec.image == image {
                result = json!({
                    "system": name,
                    "image": image,
                    "changed": false,
                    "shards": system.spec.shards.iter().map(|shard| shard.name.clone()).collect::<Vec<_>>(),
                    "steps": [],
                    "notes": ["the order system already has this image"],
                });
                return Ok(());
            }
            if let Some(target) = catalog.order_systems.get_mut(&name) {
                target.spec.image = image.clone();
                target.status.phase = crate::model::Phase::Pending;
                target.status.message =
                    "image update requested; operator rolling the order-system shards".to_string();
                target.status.conditions.clear();
            }
            let report = validate_catalog(catalog);
            let mut scope = BTreeSet::new();
            scope.insert(name.clone());
            let scoped = report.scoped(&scope);
            if !scoped.is_ok() {
                return Err(ApiError::unprocessable(scoped));
            }
            catalog.audit(
                "api",
                "upgrade-order-system",
                &name,
                format!("{image} on order system {name}; rolling user shards"),
            );
            result = json!({
                "system": name,
                "image": image,
                "changed": true,
                "shards": system.spec.shards.iter().map(|shard| shard.name.clone()).collect::<Vec<_>>(),
                "steps": [
                    { "action": "update-deployments", "detail": "operator writes the new image to every user-shard Deployment" },
                    { "action": "rolling-update", "detail": "each shard rolls with maxUnavailable: 0 and one surge pod; existing HTTP requests drain through termination" },
                    { "action": "verify", "detail": "operator reports per-shard ready replicas in OrderSystem.status" }
                ],
                "notes": ["order-system shards are stateless; no Raft quorum or leader transfer is involved"],
            });
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "upgrade": result })))
}

async fn validate(State(store): State<Arc<Store>>) -> Json<Value> {
    let catalog = store.snapshot();
    let report = validate_catalog(&catalog);
    Json(json!({
        "report": report,
        "ok": report.is_ok(),
        "text": report.to_text(),
    }))
}

async fn upsert_bundle(
    State(store): State<Arc<Store>>,
    Json(bundle): Json<ShardBundle>,
) -> Result<Json<Value>, ApiError> {
    let bundle_report = validate_bundle(&bundle);
    if !bundle_report.is_ok() {
        return Err(ApiError::unprocessable(bundle_report));
    }
    let shard = bundle.shard.name.clone();
    let mut result = Value::Null;
    store
        .mutate(|catalog| {
            let mut candidate = catalog.clone();
            candidate.insert_bundle(bundle.clone());
            let report = validate_catalog(&candidate);
            if !report.is_ok() {
                return Err(ApiError::unprocessable(report));
            }
            catalog.insert_bundle(bundle.clone());
            catalog.audit(
                "api",
                "upsert-bundle",
                &shard,
                format!(
                    "{} symbols, {} deployments",
                    bundle.symbols.len(),
                    bundle.deployments.len()
                ),
            );
            let report = validate_catalog(catalog);
            result = catalog_value(catalog, &report);
            Ok(())
        })
        .await?;
    Ok(Json(result))
}

#[derive(Debug, Deserialize)]
struct AuditQuery {
    #[serde(default)]
    limit: Option<usize>,
}

async fn audit(State(store): State<Arc<Store>>, Query(query): Query<AuditQuery>) -> Json<Value> {
    let catalog = store.snapshot();
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let entries: Vec<AuditEntry> = catalog.recent_audit(limit);
    Json(json!({ "items": entries }))
}

/// Derived cluster-level numbers for the GUI.
pub fn cluster_totals(catalog: &Catalog) -> BTreeMap<String, u64> {
    let mut totals = BTreeMap::new();
    totals.insert("cells".to_string(), catalog.cells.len() as u64);
    totals.insert("shards".to_string(), catalog.shards.len() as u64);
    totals.insert("symbols".to_string(), catalog.symbols.len() as u64);
    totals.insert(
        "processes".to_string(),
        catalog
            .deployments
            .values()
            .filter(|deployment| deployment.spec.desired_state != DeploymentState::Disabled)
            .count() as u64,
    );
    totals
}

pub fn format_cpu(millis: u64) -> String {
    if millis.is_multiple_of(1000) {
        format!("{}", millis / 1000)
    } else {
        format!("{millis}m")
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KI: u64 = 1024;
    const MI: u64 = 1024 * KI;
    const GI: u64 = 1024 * MI;
    if bytes >= GI && bytes.is_multiple_of(GI) {
        format!("{}Gi", bytes / GI)
    } else if bytes >= MI && bytes.is_multiple_of(MI) {
        format!("{}Mi", bytes / MI)
    } else if bytes >= KI && bytes.is_multiple_of(KI) {
        format!("{}Ki", bytes / KI)
    } else {
        format!("{bytes}")
    }
}

/// Serialize only the fields the GUI needs from a plan step. Kept public so tests can reuse it.
pub fn step_summary(step: &PlanStep) -> String {
    match step.action {
        Action::InitCluster => format!("init {} ({})", step.target, step.voters.len()),
        Action::TransferLeader => format!("transfer leader for {}", step.target),
        Action::RestartPod => format!("restart {}", step.target),
        _ => format!(
            "{} {}",
            crate::reconcile::action_name(step.action),
            step.target
        ),
    }
}

/// The public list of objects in the catalog (used by tests and the GUI's export button).
pub fn export(catalog: &Catalog) -> Value {
    json!({
        "cells": catalog.cells.values().collect::<Vec<&Cell>>(),
        "shards": catalog.shards.values().collect::<Vec<&SymbolShard>>(),
        "symbols": catalog.symbols.values().collect::<Vec<&TradingSymbol>>(),
        "deployments": catalog.deployments.values().collect::<Vec<&SymbolDeployment>>(),
        "routes": catalog.routes.values().collect::<Vec<&SymbolRoute>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app() -> Router {
        router(Arc::new(Store::in_memory(true)))
    }

    async fn get_json(uri: &str) -> (StatusCode, Value) {
        let response = app()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn post_json(uri: &str, body: Value) -> (StatusCode, Value) {
        let response = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn put_json_to(app: Router, uri: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn get_json_from(app: Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn post_json_to(app: Router, uri: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn symbol_body(name: &str, id: u32) -> Value {
        json!({
            "name": name,
            "namespace": "",
            "spec": {
                "symbolId": id,
                "base": { "code": "USD", "class": "fiat", "decimals": 2 },
                "quote": { "code": "JPY", "class": "fiat", "decimals": 0 },
                "priceDecimals": 3,
                "priceTick": "0.001",
                "baseLot": "0.01",
                "feeAssetPolicy": "received",
                "stp": "off",
                "owner": "test",
                "generation": 1
            }
        })
    }

    #[tokio::test]
    async fn catalog_and_health_are_served() {
        let (status, body) = get_json("/api/v1/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["counts"]["shards"], 3);
        let (status, body) = get_json("/api/v1/catalog").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["counts"]["symbols"], 3);
        assert_eq!(body["validation"]["errors"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn backup_status_reports_not_configured_for_a_known_shard() {
        let (status, body) = get_json("/api/v1/backup-status").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "backup_status_query");

        let (status, body) = get_json("/api/v1/backup-status?shard=missing").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "shard_not_found");

        let (status, body) = get_json("/api/v1/backup-status?shard=fx-eu-01").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["shard"], "fx-eu-01");
        assert_eq!(body["configured"], false);
        assert_eq!(body["status"], "not-configured");
        assert_eq!(body["ok"], false);
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("deploy/backup.yaml")
        );
    }

    #[tokio::test]
    async fn market_data_service_can_be_created_and_listed() {
        let app = app();
        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/market-data-services",
            json!({
                "name": "eu-1-feeds",
                "spec": {
                    "cell": "eu-1",
                    "image": "registry.example.com/matchengine-marketdata@sha256:abc",
                    "replicas": 2,
                    "feedGroups": [{
                        "name": "fx-a",
                        "symbols": ["gbpusd"],
                        "streamBase": 2001
                    }],
                    "transport": {
                        "mode": "multicast",
                        "channel": "aeron:udp?endpoint=239.1.2.11:24001|interface={pod_ip}|ttl=1"
                    }
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["marketDataService"]["spec"]["replicas"], 2);
        let (status, body) = get_json_from(app, "/api/v1/market-data-services").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"][0]["name"], "eu-1-feeds");
        assert_eq!(body["items"][0]["namespace"], "match-eu-1");
        assert_eq!(body["items"][0]["sources"][0]["symbol"], "gbpusd");
        assert_eq!(body["items"][0]["sources"][0]["shard"], "fx-eu-01");
    }

    #[tokio::test]
    async fn market_data_manifests_render_as_a_kubectl_list() {
        let app = app();
        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/market-data-services",
            json!({
                "name": "btc-marketdata",
                "spec": {
                    "cell": "eu-1",
                    "image": "registry.example.com/matchengine-marketdata@sha256:abc",
                    "replicas": 2,
                    "feedGroups": [{
                        "name": "btc",
                        "symbols": ["btcusdt"],
                        "streamBase": 2001
                    }],
                    "transport": {
                        "mode": "multicast",
                        "channel": "aeron:udp?endpoint=239.1.2.51:24001|interface={pod_ip}|ttl=1"
                    }
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = get_json_from(
            app.clone(),
            "/api/v1/market-data-services/btc-marketdata/manifests",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["kind"], "List");
        let kinds: Vec<&str> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|object| object["kind"].as_str().unwrap())
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
        let (status, body) =
            get_json_from(app, "/api/v1/market-data-services/btc-marketdata").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["endpoint"],
            "ws://btc-marketdata.match-eu-1.svc.cluster.local:8080/ws"
        );
        assert_eq!(
            body["endpoints"]["fast"],
            "ws://btc-marketdata.match-eu-1.svc.cluster.local:8080/ws/<symbol>@<stream>"
        );
        assert!(
            body["endpoints"]["terminal"]
                .as_str()
                .unwrap()
                .contains("/stream?streams=")
        );
    }

    async fn create_btc_market_data(app: axum::Router) {
        let (status, body) = post_json_to(
            app,
            "/api/v1/market-data-services",
            json!({
                "name": "btc-marketdata",
                "spec": {
                    "cell": "eu-1",
                    "image": "registry.example.com/matchengine-marketdata@sha256:abc",
                    "replicas": 2,
                    "feedGroups": [{
                        "name": "btc",
                        "symbols": ["btcusdt"],
                        "streamBase": 2001
                    }],
                    "transport": {
                        "mode": "multicast",
                        "channel": "aeron:udp?endpoint=239.1.2.51:24001|interface={pod_ip}|ttl=1"
                    }
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    async fn create_order_system(app: axum::Router) {
        let (status, body) = post_json_to(
            app,
            "/api/v1/order-systems",
            json!({
                "name": "orders",
                "spec": {
                    "cell": "eu-1",
                    "image": "registry.example.com/matchengine-order-system:arm64-v5",
                    "replicas": 1,
                    "targets": [{"symbol": "gbpusd", "label": "GBP/USD"}],
                    "shards": [{"name": "users-0", "userIdMin": 0, "userIdMax": 249}],
                    "ingress": "orders.match-eu-1.example"
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn market_data_upgrade_updates_the_image_and_keeps_the_release() {
        let app = app();
        create_btc_market_data(app.clone()).await;

        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/market-data-services/btc-marketdata/upgrade",
            json!({ "image": "registry.example.com/matchengine-marketdata@sha256:def" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["upgrade"]["changed"], true);
        assert_eq!(
            body["upgrade"]["release"]["wireVersion"],
            serde_json::Value::from(7)
        );
        assert_eq!(body["upgrade"]["sourceShards"][0], "me-btcusdt");

        let (status, detail) =
            get_json_from(app, "/api/v1/market-data-services/btc-marketdata").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            detail["marketDataService"]["spec"]["image"],
            "registry.example.com/matchengine-marketdata@sha256:def"
        );
        assert_eq!(detail["marketDataService"]["status"]["phase"], "Pending");
    }

    #[tokio::test]
    async fn market_data_upgrade_rejects_an_incompatible_wire_version() {
        let app = app();
        create_btc_market_data(app.clone()).await;

        let (status, body) = post_json_to(
            app,
            "/api/v1/market-data-services/btc-marketdata/upgrade",
            json!({
                "image": "registry.example.com/matchengine-marketdata@sha256:def",
                "release": {
                    "wireVersion": 8,
                    "snapshotVersion": 11,
                    "mixedVersionSafe": false
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "release_incompatible");
    }

    #[tokio::test]
    async fn market_data_image_inventory_uses_the_service_image() {
        let app = app();
        create_btc_market_data(app.clone()).await;

        let (status, body) =
            get_json_from(app, "/api/v1/images?marketDataService=btc-marketdata").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["current"],
            "registry.example.com/matchengine-marketdata@sha256:abc"
        );
        assert_eq!(
            body["repository"],
            "registry.example.com/matchengine-marketdata"
        );
        assert!(body["items"].as_array().unwrap().iter().any(|image| {
            image["current"] == true
                && image["reference"] == "registry.example.com/matchengine-marketdata@sha256:abc"
        }));
        assert!(
            body["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|image| image["repository"] == "registry.example.com/matchengine-marketdata")
        );
    }

    #[tokio::test]
    async fn market_data_edit_updates_spec_and_preserves_image_release_and_status() {
        let app = app();
        create_btc_market_data(app.clone()).await;
        let (status, _) = post_json_to(
            app.clone(),
            "/api/v1/market-data-services/btc-marketdata/upgrade",
            json!({ "image": "registry.example.com/matchengine-marketdata@sha256:def" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (_, detail) =
            get_json_from(app.clone(), "/api/v1/market-data-services/btc-marketdata").await;
        let mut spec = detail["marketDataService"]["spec"].clone();
        spec["replicas"] = json!(3);
        spec["image"] = json!("registry.example.com/matchengine-marketdata@sha256:submitted");
        spec["release"]["wireVersion"] = json!(8);
        let (status, body) = put_json_to(
            app.clone(),
            "/api/v1/market-data-services/btc-marketdata",
            json!({
                "name": "wrong-name",
                "namespace": "other-namespace",
                "spec": spec,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let service = &body["marketDataService"];
        assert_eq!(service["name"], "btc-marketdata");
        assert_eq!(service["namespace"], "match-eu-1");
        assert_eq!(service["spec"]["replicas"], 3);
        assert_eq!(
            service["spec"]["image"],
            "registry.example.com/matchengine-marketdata@sha256:def"
        );
        assert_eq!(service["spec"]["release"]["wireVersion"], 7);
        assert_eq!(
            service["status"]["previousImage"],
            "registry.example.com/matchengine-marketdata@sha256:abc"
        );
        assert_eq!(service["status"]["phase"], "Pending");
    }

    #[tokio::test]
    async fn market_data_edit_rejects_zero_replicas() {
        let app = app();
        create_btc_market_data(app.clone()).await;
        let (_, detail) =
            get_json_from(app.clone(), "/api/v1/market-data-services/btc-marketdata").await;
        let mut spec = detail["marketDataService"]["spec"].clone();
        spec["replicas"] = json!(0);
        let (status, body) = put_json_to(
            app,
            "/api/v1/market-data-services/btc-marketdata",
            json!({ "spec": spec }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "invalid_replicas");
    }

    #[tokio::test]
    async fn market_data_scale_updates_replicas_and_bounds() {
        let app = app();
        create_btc_market_data(app.clone()).await;
        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/market-data-services/btc-marketdata/scale",
            json!({ "replicas": 4, "minReplicas": 2, "maxReplicas": 6 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["marketDataService"]["spec"]["replicas"], 4);
        assert_eq!(
            body["marketDataService"]["spec"]["autoscaling"]["minReplicas"],
            2
        );
        assert_eq!(
            body["marketDataService"]["spec"]["autoscaling"]["maxReplicas"],
            6
        );

        for (request, error) in [
            (json!({ "replicas": 0 }), "invalid_replicas"),
            (
                json!({ "replicas": 4, "minReplicas": 0 }),
                "invalid_min_replicas",
            ),
            (
                json!({ "replicas": 4, "minReplicas": 2, "maxReplicas": 3 }),
                "invalid_replica_bounds",
            ),
        ] {
            let (status, body) = post_json_to(
                app.clone(),
                "/api/v1/market-data-services/btc-marketdata/scale",
                request,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(body["error"], error);
        }
        let (_, detail) = get_json_from(app, "/api/v1/market-data-services/btc-marketdata").await;
        assert_eq!(detail["marketDataService"]["spec"]["replicas"], 4);
    }

    #[tokio::test]
    async fn market_data_scale_allows_zero_when_paused() {
        let app = app();
        create_btc_market_data(app.clone()).await;
        let (_, detail) =
            get_json_from(app.clone(), "/api/v1/market-data-services/btc-marketdata").await;
        let mut spec = detail["marketDataService"]["spec"].clone();
        spec["paused"] = json!(true);
        let (status, body) = put_json_to(
            app.clone(),
            "/api/v1/market-data-services/btc-marketdata",
            json!({ "spec": spec }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, body) = post_json_to(
            app,
            "/api/v1/market-data-services/btc-marketdata/scale",
            json!({ "replicas": 0 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["marketDataService"]["spec"]["replicas"], 0);
    }

    #[tokio::test]
    async fn market_data_upgrade_rollback_is_reversible() {
        let app = app();
        create_btc_market_data(app.clone()).await;
        let (status, _) = post_json_to(
            app.clone(),
            "/api/v1/market-data-services/btc-marketdata/upgrade",
            json!({ "image": "registry.example.com/matchengine-marketdata@sha256:def" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/market-data-services/btc-marketdata/rollback",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["upgrade"]["image"],
            "registry.example.com/matchengine-marketdata@sha256:abc"
        );
        let (_, detail) =
            get_json_from(app.clone(), "/api/v1/market-data-services/btc-marketdata").await;
        assert_eq!(
            detail["marketDataService"]["spec"]["image"],
            "registry.example.com/matchengine-marketdata@sha256:abc"
        );
        assert_eq!(
            detail["marketDataService"]["status"]["previousImage"],
            "registry.example.com/matchengine-marketdata@sha256:def"
        );

        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/market-data-services/btc-marketdata/rollback",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["upgrade"]["image"],
            "registry.example.com/matchengine-marketdata@sha256:def"
        );

        let (_, audit) = get_json_from(app, "/api/v1/audit?limit=50").await;
        let rollback = audit["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["action"] == "rollback-market-data")
            .expect("rollback audit entry");
        assert!(
            rollback["detail"].as_str().unwrap().contains("@sha256:def")
                && rollback["detail"].as_str().unwrap().contains("@sha256:abc")
        );
    }

    #[tokio::test]
    async fn market_data_rollback_without_previous_image_is_rejected() {
        let app = app();
        create_btc_market_data(app.clone()).await;
        let (status, body) = post_json_to(
            app,
            "/api/v1/market-data-services/btc-marketdata/rollback",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "previous_image_missing");
    }

    #[tokio::test]
    async fn market_data_plan_get_and_post_return_steps_and_summary() {
        let app = app();
        create_btc_market_data(app.clone()).await;
        let (status, body) = get_json_from(
            app.clone(),
            "/api/v1/market-data-services/btc-marketdata/plan?scenario=behind",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["plan"]["phase"], "upgrading");
        assert!(
            body["plan"]["steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step["action"] == "roll-deployment")
        );
        assert!(
            body["summary"]
                .as_str()
                .unwrap()
                .contains("roll-deployment")
        );

        let (status, body) = get_json_from(
            app.clone(),
            "/api/v1/market-data-services/btc-marketdata/plan?scenario=fresh",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["plan"]["phase"], "provisioning");
        assert!(
            body["plan"]["steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step["action"] == "render-manifests")
        );

        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/market-data-services/btc-marketdata/plan",
            json!({
                "observation": {
                    "service": "btc-marketdata",
                    "cell": "eu-1",
                    "image": "registry.example.com/matchengine-marketdata@sha256:abc",
                    "release": { "wireVersion": 7, "snapshotVersion": 11, "mixedVersionSafe": false },
                    "replicas": 2,
                    "readyReplicas": 2,
                    "updatedReplicas": 2,
                    "available": true,
                    "deploymentExists": true,
                    "feedReady": true,
                    "symbols": [{
                        "name": "btcusdt",
                        "ready": true,
                        "feedLag": 1000,
                        "lastFrameAgeMs": 1000,
                        "subscribers": 1,
                        "drops": 0
                    }],
                    "hpaMinReplicas": 1,
                    "hpaMaxReplicas": 0
                },
                "targetImage": "registry.example.com/matchengine-marketdata@sha256:def"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["plan"]["phase"], "upgrading");
        assert_eq!(
            body["plan"]["targetImage"],
            "registry.example.com/matchengine-marketdata@sha256:def"
        );

        let (status, body) = get_json_from(
            app,
            "/api/v1/market-data-services/btc-marketdata/plan?scenario=nope",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "market_data_scenario");
    }

    #[tokio::test]
    async fn order_system_can_be_created_rendered_and_upgraded() {
        let app = app();
        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/order-systems",
            json!({
                "name": "orders",
                "spec": {
                    "cell": "eu-1",
                    "image": "matchengine-order-system:arm64",
                    "replicas": 1,
                    "autoscaling": {
                        "minReplicas": 1,
                        "maxReplicas": 2,
                        "targetCpuUtilization": 70
                    },
                    "port": 24300,
                    "targets": [
                        {"symbol": "gbpusd", "label": "GBP/USD"},
                        {"symbol": "btcusdt", "label": "BTC/USDT"}
                    ],
                    "shards": [
                        {"name": "users-0", "userIdMin": 0, "userIdMax": 499},
                        {"name": "users-1", "userIdMin": 500, "userIdMax": 999}
                    ],
                    "ingress": "orders.match-eu-1.example"
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, manifests) =
            get_json_from(app.clone(), "/api/v1/order-systems/orders/manifests").await;
        assert_eq!(status, StatusCode::OK, "{manifests}");
        let kinds: Vec<&str> = manifests["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|object| object["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            1,
            kinds.iter().filter(|kind| **kind == "Deployment").count()
        );
        assert_eq!(
            1,
            kinds
                .iter()
                .filter(|kind| **kind == "HorizontalPodAutoscaler")
                .count()
        );
        // Base host + one alias per declared shard, all on the single Service.
        assert_eq!(3, kinds.iter().filter(|kind| **kind == "Ingress").count());

        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/order-systems/orders/upgrade",
            json!({"image": "matchengine-order-system:arm64-v2"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["upgrade"]["changed"], true);
        assert_eq!(
            body["upgrade"]["shards"],
            serde_json::json!(["users-0", "users-1"])
        );
        let (status, detail) = get_json_from(app, "/api/v1/order-systems/orders").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            detail["orderSystem"]["spec"]["image"],
            "matchengine-order-system:arm64-v2"
        );
    }

    #[tokio::test]
    async fn order_system_image_inventory_uses_the_system_image() {
        let app = app();
        create_order_system(app.clone()).await;

        let (status, body) = get_json_from(app.clone(), "/api/v1/images?orderSystem=orders").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["current"],
            "registry.example.com/matchengine-order-system:arm64-v5"
        );
        assert_eq!(
            body["repository"],
            "registry.example.com/matchengine-order-system"
        );
        assert!(body["items"].as_array().unwrap().iter().any(|image| {
            image["current"] == true
                && image["reference"] == "registry.example.com/matchengine-order-system:arm64-v5"
        }));
        assert!(
            body["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|image| image["repository"] == "registry.example.com/matchengine-order-system")
        );

        let (status, body) = get_json_from(app, "/api/v1/images?orderSystem=nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "order_system_not_found");
    }

    #[tokio::test]
    async fn shard_list_reports_pod_totals() {
        let (status, body) = get_json("/api/v1/shards").await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        let packed = items
            .iter()
            .find(|i| i["name"] == "fx-eu-01")
            .expect("packed shard");
        assert_eq!(packed["processes"], 2);
        assert_eq!(packed["perPod"]["cpuRequest"], "500m");
        assert_eq!(packed["perPod"]["memoryRequest"], "1Gi");
        let reserved = items
            .iter()
            .find(|i| i["name"] == "fx-eu-02")
            .expect("reserved shard");
        assert_eq!(reserved["processes"], 0);
        assert_eq!(reserved["freeSlots"], 4);
        assert_eq!(reserved["phase"], "Reserved");
        let dedicated = items
            .iter()
            .find(|i| i["name"] == "me-btcusdt")
            .expect("dedicated shard");
        assert_eq!(dedicated["processes"], 1);
    }

    #[tokio::test]
    async fn images_endpoint_marks_the_current_catalog_image() {
        let (status, body) = get_json("/api/v1/images?shard=fx-eu-01").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["current"], crate::DEFAULT_IMAGE);
        assert_eq!(body["repository"], "registry.example.com/matchengine-raft");
        let items = body["items"].as_array().unwrap();
        let current = items
            .iter()
            .find(|item| item["current"] == true)
            .expect("current image is listed");
        assert_eq!(current["reference"], crate::DEFAULT_IMAGE);
        assert_eq!(current["tag"], "1.4.0-nonroot");
        assert_eq!(current["runningHere"], false);
    }

    #[tokio::test]
    async fn images_endpoint_rejects_an_unknown_shard() {
        let (status, body) = get_json("/api/v1/images?shard=nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "shard_not_found");
    }

    #[tokio::test]
    async fn upgrade_shard_endpoint_updates_image_and_release() {
        let app = app();
        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/shards/fx-eu-01/upgrade",
            json!({
                "image": "matchengine-raft@sha256:abc",
                "release": {
                    "wireVersion": 7,
                    "snapshotVersion": 11,
                    "mixedVersionSafe": false
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["upgrade"]["changed"], true);
        let (status, detail) = get_json_from(app, "/api/v1/shards/fx-eu-01").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            detail["shard"]["spec"]["image"],
            "matchengine-raft@sha256:abc"
        );
        assert_eq!(detail["shard"]["spec"]["release"]["wireVersion"], 7);
    }

    #[tokio::test]
    async fn shard_detail_scopes_validation_to_the_shard() {
        let (status, body) = get_json("/api/v1/shards/fx-eu-01").await;
        assert_eq!(status, StatusCode::OK);
        let scoped = body["shardValidation"]["warnings"].as_array().unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0]["object"], "fx-eu-01");
        // The full catalog report is still returned for callers that need cross-shard issues.
        let full = body["validation"]["warnings"].as_array().unwrap();
        assert_eq!(full.len(), 3);
        assert!(full.iter().any(|w| w["object"] == "me-btcusdt"));
        assert!(full.iter().any(|w| w["object"] == "fx-eu-02"));
    }

    #[tokio::test]
    async fn shard_detail_includes_engine_console_links() {
        let (status, body) = get_json("/api/v1/shards/fx-eu-01").await;
        assert_eq!(status, StatusCode::OK);
        let consoles = body["engineConsoles"].as_array().unwrap();
        assert_eq!(consoles.len(), 2);
        let eurusd = consoles.iter().find(|c| c["symbol"] == "eurusd").unwrap();
        assert_eq!(eurusd["pod"], 0);
        assert_eq!(eurusd["leader"], true);
        assert_eq!(
            eurusd["url"],
            "http://fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21001/console/"
        );
        assert_eq!(
            eurusd["proxyUrl"],
            "/api/v1/shards/fx-eu-01/processes/eurusd/pods/0/console/"
        );
        let gbpusd = consoles.iter().find(|c| c["symbol"] == "gbpusd").unwrap();
        assert_eq!(
            gbpusd["url"],
            "http://fx-eu-01-0.fx-eu-01-headless.match-eu-1.svc.cluster.local:21002/console/"
        );
        assert_eq!(
            gbpusd["proxyUrl"],
            "/api/v1/shards/fx-eu-01/processes/gbpusd/pods/0/console/"
        );

        // Before bootstrap there is no leader; the link still points at a deterministic pod.
        let (status, body) = get_json("/api/v1/shards/fx-eu-01?scenario=fresh").await;
        assert_eq!(status, StatusCode::OK);
        let consoles = body["engineConsoles"].as_array().unwrap();
        assert!(
            consoles
                .iter()
                .all(|c| c["leader"] == false && c["pod"] == 0)
        );
    }

    #[tokio::test]
    async fn console_proxy_rejects_unknown_processes_and_pods() {
        let (status, body) =
            get_json("/api/v1/shards/fx-eu-01/processes/nope/pods/0/console/").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "process_not_found");
        let (status, body) =
            get_json("/api/v1/shards/fx-eu-01/processes/eurusd/pods/9/console/").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "pod_out_of_range");
        let (status, body) = get_json("/api/v1/shards/nope/processes/eurusd/pods/0/console/").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "shard_not_found");
    }

    #[tokio::test]
    async fn console_proxy_rejects_disabled_processes() {
        let store = Arc::new(Store::in_memory(true));
        let app = router(store);
        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/symbols/eurusd/shutdown",
            json!({ "allowRestart": true }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = get_json_from(
            app,
            "/api/v1/shards/fx-eu-01/processes/eurusd/pods/0/console/",
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "process_disabled");
    }

    #[test]
    fn console_paths_are_allowlisted() {
        assert_eq!(console_upstream_path("").unwrap(), "/console/");
        assert_eq!(
            console_upstream_path("app.css").unwrap(),
            "/console/app.css"
        );
        assert_eq!(console_upstream_path("app.js").unwrap(), "/console/app.js");
        assert_eq!(
            console_upstream_path("api/node").unwrap(),
            "/console/api/node"
        );
        assert_eq!(
            console_upstream_path("api/actions/transfer-leader").unwrap(),
            "/console/api/actions/transfer-leader"
        );
        assert_eq!(console_upstream_path("health").unwrap(), "/health");
        assert_eq!(console_upstream_path("events").unwrap(), "/events");
        for rejected in [
            "init",
            "api/unknown",
            "api/actions/unknown",
            "../init",
            "api/../../init",
            "api/%2e%2e/init",
            "api\\..\\init",
        ] {
            assert!(
                console_upstream_path(rejected).is_err(),
                "{rejected} should be rejected"
            );
        }
    }

    #[test]
    fn console_assets_are_rewritten_onto_the_proxy_prefix() {
        let base = "/api/v1/shards/fx-eu-01/processes/eurusd/pods/2/console";
        let html = br#"<link href="/console/app.css"><script src="/console/app.js"></script><a href="/health">health</a>"#;
        let rewritten = String::from_utf8(rewrite_console_body(html, base)).unwrap();
        assert_eq!(
            rewritten,
            format!(
                r#"<link href="{base}/app.css"><script src="{base}/app.js"></script><a href="{base}/health">health</a>"#
            )
        );
        let js = br#"fetch("/console/api/node"); fetch("/console/api/cluster"); fetch("/events");"#;
        let rewritten = String::from_utf8(rewrite_console_body(js, base)).unwrap();
        assert_eq!(
            rewritten,
            format!(
                r#"fetch("{base}/api/node"); fetch("{base}/api/cluster"); fetch("{base}/events");"#
            )
        );
    }

    #[tokio::test]
    async fn manifests_render_as_a_kubectl_list() {
        let (status, body) = get_json("/api/v1/shards/fx-eu-01/manifests?profile=local").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["kind"], "List");
        assert_eq!(body["items"].as_array().unwrap().len(), 6);
        let sts = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["kind"] == "StatefulSet")
            .unwrap();
        assert_eq!(
            sts["spec"]["volumeClaimTemplates"][0]["spec"]["storageClassName"],
            "local-path"
        );
        assert!(
            sts["spec"]["template"]["spec"]["affinity"]["podAntiAffinity"]
                .get("requiredDuringSchedulingIgnoredDuringExecution")
                .is_none()
        );
    }

    #[tokio::test]
    async fn manifests_reject_unknown_profile() {
        let (status, body) = get_json("/api/v1/shards/fx-eu-01/manifests?profile=minikube").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "profile");
    }

    #[tokio::test]
    async fn fresh_plan_lists_bootstrap_steps() {
        let (status, body) = get_json("/api/v1/shards/fx-eu-01/plan?scenario=fresh").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["plan"]["phase"], "bootstrap");
        let steps = body["plan"]["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2);
        assert!(steps.iter().all(|s| s["action"] == "init-cluster"));
        assert_eq!(steps[0]["voters"].as_array().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn get_plan_honours_target_release() {
        let (status, body) = get_json(
            "/api/v1/shards/fx-eu-01/plan?scenario=upgrade&wire_version=8&snapshot_version=11&mixed_version_safe=false",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["plan"]["phase"], "upgrading");
        assert_eq!(body["plan"]["steps"][0]["action"], "pause-traffic");
    }

    #[tokio::test]
    async fn explicit_observation_drives_upgrade_strategy() {
        let catalog = Store::in_memory(true).snapshot();
        let mut observation =
            crate::reconcile::simulate(&catalog, "fx-eu-01", Scenario::Upgrade).unwrap();
        observation.last_backup_at = None;
        let (status, body) = post_json(
            "/api/v1/shards/fx-eu-01/plan",
            json!({
                "observation": observation,
                "targetRelease": { "wireVersion": 8, "snapshotVersion": 11, "mixedVersionSafe": false }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["plan"]["phase"], "blocked");
        assert_eq!(body["plan"]["steps"][0]["action"], "backup");
    }

    #[tokio::test]
    async fn symbol_detail_contains_shard_and_validation() {
        let (status, body) = get_json("/api/v1/symbols").await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        let btc = items
            .iter()
            .find(|i| i["symbol"]["name"] == "btcusdt")
            .unwrap();
        assert_eq!(btc["shard"], "me-btcusdt");
        assert_eq!(btc["shardClass"], "Dedicated");
        assert_eq!(btc["validation"]["errors"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn unknown_shard_is_404() {
        let (status, body) = get_json("/api/v1/shards/nope/manifests").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "shard_not_found");
    }

    #[tokio::test]
    async fn invalid_bundle_is_rejected_with_issues() {
        let catalog = Store::in_memory(true).snapshot();
        let mut bundle = crate::bundle::demo_bundles().remove(0);
        bundle.deployments[0].spec.placement.http_port = 21999;
        let (status, body) =
            post_json("/api/v1/bundles", serde_json::to_value(&bundle).unwrap()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["code"] == "port_convention"),
            "{body}"
        );
        assert!(catalog.shards.contains_key("fx-eu-01"));
    }

    #[tokio::test]
    async fn simulate_writes_route_status() {
        let (status, body) =
            post_json("/api/v1/shards/fx-eu-01/simulate?scenario=ready", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["routes"].as_array().unwrap().len(), 2);
        assert!(
            body["observation"]["processes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["bootstrapped"] == true)
        );
    }

    #[tokio::test]
    async fn audit_records_upserts() {
        let store = Arc::new(Store::in_memory(true));
        let app = router(store.clone());
        let bundle = crate::bundle::demo_bundles().remove(0);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/bundles")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&bundle).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = router(store.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/audit?limit=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            body["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["action"] == "upsert-bundle")
        );
    }

    #[tokio::test]
    async fn create_symbol_endpoint_places_on_a_reserved_shard() {
        let store = Arc::new(Store::in_memory(true));
        let app = router(store.clone());
        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/symbols",
            json!({ "symbol": symbol_body("usdjpy", 10), "shard": "fx-eu-02" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["placement"]["shard"], "fx-eu-02");
        assert_eq!(body["placement"]["slot"], 0);
        assert_eq!(body["placement"]["httpPort"], 21041);
        assert_eq!(body["placement"]["deployment"]["name"], "usdjpy-g1");
        assert_eq!(
            body["placement"]["route"]["spec"]["activeDeploymentRef"],
            "usdjpy-g1"
        );
        assert_eq!(store.snapshot().shard_processes("fx-eu-02"), 1);

        let (status, detail) = get_json_from(app, "/api/v1/shards/fx-eu-02").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["deployments"].as_array().unwrap().len(), 1);
        assert_eq!(
            detail["deployments"][0]["spec"]["placement"]["httpPort"],
            21041
        );
    }

    #[tokio::test]
    async fn create_symbol_endpoint_rejects_a_full_shard() {
        let (status, body) = post_json(
            "/api/v1/symbols",
            json!({ "symbol": symbol_body("usdjpy", 10), "shard": "me-btcusdt" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "shard_full");
    }

    #[tokio::test]
    async fn create_empty_shard_endpoint_reserves_capacity() {
        let mut shard = crate::store::demo_catalog().shards["fx-eu-02"].clone();
        shard.name = "fx-eu-03".to_string();
        shard.spec.network.http_base_port = 21061;
        shard.status = Default::default();
        let (status, body) =
            post_json("/api/v1/shards", serde_json::to_value(shard).unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["shard"]["name"], "fx-eu-03");
        assert_eq!(body["shard"]["status"]["phase"], "Reserved");
        assert_eq!(body["shard"]["spec"]["network"]["httpBasePort"], 21061);
    }

    #[tokio::test]
    async fn migrate_endpoint_creates_a_shadow_plan() {
        let (status, body) = post_json(
            "/api/v1/symbols/eurusd/migrate",
            json!({ "targetShard": "fx-eu-02" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let migration = &body["migration"];
        assert_eq!(migration["sourceShard"], "fx-eu-01");
        assert_eq!(migration["targetShard"], "fx-eu-02");
        assert_eq!(migration["targetDeployment"], "eurusd-shadow");
        assert_eq!(migration["targetSlot"], 0);
        assert_eq!(migration["targetHttpPort"], 21041);
        let steps = migration["steps"].as_array().unwrap();
        assert!(steps.iter().any(|step| step["action"] == "cutover-route"));
        assert!(steps.iter().any(|step| step["action"] == "rollback"));
    }

    #[tokio::test]
    async fn create_symbol_endpoint_requires_opt_in_for_container_restarts() {
        let (status, body) = post_json(
            "/api/v1/symbols",
            json!({ "symbol": symbol_body("usdjpy", 10), "shard": "fx-eu-01" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "restart_required");

        let (status, body) = post_json(
            "/api/v1/symbols",
            json!({
                "symbol": symbol_body("usdjpy", 10),
                "shard": "fx-eu-01",
                "allowRestart": true,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["placement"]["shard"], "fx-eu-01");
        assert_eq!(body["placement"]["slot"], 2);
        assert_eq!(body["placement"]["httpPort"], 21003);
    }

    #[tokio::test]
    async fn shutdown_endpoint_guards_containers_and_retains_the_slot() {
        let store = Arc::new(Store::in_memory(true));
        let app = router(store.clone());
        let (status, body) =
            post_json_to(app.clone(), "/api/v1/symbols/eurusd/shutdown", json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "restart_required");
        assert_eq!(
            store.snapshot().deployments["eurusd-g1"].spec.desired_state,
            DeploymentState::Active
        );

        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/symbols/eurusd/shutdown",
            json!({ "allowRestart": true }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let shutdown = &body["shutdown"];
        assert_eq!(shutdown["mode"], "containers");
        assert_eq!(shutdown["zeroImpact"], false);
        assert!(
            shutdown["steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step["action"] == "rolling-remove")
        );

        let (status, detail) = get_json_from(app.clone(), "/api/v1/shards/fx-eu-01").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["deployments"].as_array().unwrap().len(), 2);
        assert_eq!(detail["activeProcesses"], 1);
        assert_eq!(detail["disabledProcesses"], 1);
        assert_eq!(detail["engineConsoles"].as_array().unwrap().len(), 1);
        // The disabled record still owns slot 0, so free capacity does not grow.
        assert_eq!(detail["freeSlots"], 18);
        assert_eq!(detail["deployments"][0]["spec"]["desiredState"], "Disabled");

        let (status, list) = get_json_from(app.clone(), "/api/v1/shards").await;
        assert_eq!(status, StatusCode::OK);
        let shard = list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == "fx-eu-01")
            .unwrap();
        assert_eq!(shard["processes"], 1);
        assert_eq!(shard["disabledProcesses"], 1);
        assert_eq!(shard["freeSlots"], 18);
        assert_eq!(shard["symbols"], json!(["gbpusd"]));

        let (status, manifests) =
            get_json_from(app, "/api/v1/shards/fx-eu-01/manifests?profile=local").await;
        assert_eq!(status, StatusCode::OK);
        let items = manifests["items"].as_array().unwrap();
        let config = items
            .iter()
            .find(|item| item["kind"] == "ConfigMap")
            .unwrap();
        let processes = config["data"]["processes.json"].as_str().unwrap();
        assert!(processes.contains("gbpusd"));
        assert!(!processes.contains("eurusd"));
    }

    #[tokio::test]
    async fn shutdown_endpoint_marks_a_supervisor_child_disabled_without_restart() {
        let mut bundle = crate::bundle::demo_bundles().remove(0);
        bundle.shard.name = "fx-sup-01".to_string();
        bundle.shard.spec.network.http_base_port = 21101;
        bundle.shard.spec.packing.mode = crate::model::PackingMode::Supervisor;
        for (index, deployment) in bundle.deployments.iter_mut().enumerate() {
            deployment.spec.shard_ref = "fx-sup-01".to_string();
            deployment.spec.placement.http_port = 21101 + index as u16;
        }

        let store = Arc::new(Store::in_memory(false));
        let app = router(store.clone());
        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/bundles",
            serde_json::to_value(&bundle).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, body) =
            post_json_to(app.clone(), "/api/v1/symbols/eurusd/shutdown", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let shutdown = &body["shutdown"];
        assert_eq!(shutdown["mode"], "supervisor");
        assert_eq!(shutdown["zeroImpact"], true);
        assert!(
            shutdown["steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step["action"] == "stop-process")
        );

        let (status, detail) = get_json_from(app.clone(), "/api/v1/shards/fx-sup-01").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["activeProcesses"], 1);
        assert_eq!(detail["disabledProcesses"], 1);
        assert_eq!(detail["freeSlots"], 18);
        assert_eq!(detail["deployments"][0]["spec"]["desiredState"], "Disabled");

        let (status, plan) =
            get_json_from(app.clone(), "/api/v1/shards/fx-sup-01/plan?scenario=ready").await;
        assert_eq!(status, StatusCode::OK);
        let targets: Vec<&str> = plan["plan"]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| step["target"].as_str().unwrap())
            .collect();
        assert!(!targets.contains(&"eurusd"), "{plan}");
    }

    #[tokio::test]
    async fn start_endpoint_reactivates_a_supervisor_child_from_retained_state() {
        let mut bundle = crate::bundle::demo_bundles().remove(0);
        bundle.shard.name = "fx-sup-01".to_string();
        bundle.shard.spec.network.http_base_port = 21101;
        bundle.shard.spec.packing.mode = crate::model::PackingMode::Supervisor;
        for (index, deployment) in bundle.deployments.iter_mut().enumerate() {
            deployment.spec.shard_ref = "fx-sup-01".to_string();
            deployment.spec.placement.http_port = 21101 + index as u16;
        }

        let store = Arc::new(Store::in_memory(false));
        let app = router(store.clone());
        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/bundles",
            serde_json::to_value(&bundle).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) =
            post_json_to(app.clone(), "/api/v1/symbols/eurusd/shutdown", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, body) =
            post_json_to(app.clone(), "/api/v1/symbols/eurusd/start", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let start = &body["start"];
        assert_eq!(start["mode"], "supervisor");
        assert_eq!(start["zeroImpact"], true);
        assert!(
            start["steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step["action"] == "start-process")
        );

        let (status, detail) = get_json_from(app.clone(), "/api/v1/shards/fx-sup-01").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["activeProcesses"], 2);
        assert_eq!(detail["disabledProcesses"], 0);
        assert_eq!(detail["engineConsoles"].as_array().unwrap().len(), 2);
        assert_eq!(detail["deployments"][0]["spec"]["desiredState"], "Active");
        assert_eq!(detail["routes"][0]["spec"]["state"], "Active");

        let (status, manifests) =
            get_json_from(app, "/api/v1/shards/fx-sup-01/manifests?profile=local").await;
        assert_eq!(status, StatusCode::OK);
        let config = manifests["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["kind"] == "ConfigMap")
            .unwrap();
        let processes = config["data"]["processes.json"].as_str().unwrap();
        assert!(processes.contains("eurusd"), "{processes}");
        assert!(processes.contains("gbpusd"), "{processes}");
    }

    #[tokio::test]
    async fn start_endpoint_keeps_the_containers_restart_guard() {
        let store = Arc::new(Store::in_memory(true));
        let app = router(store.clone());
        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/symbols/eurusd/shutdown",
            json!({ "allowRestart": true }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, body) =
            post_json_to(app.clone(), "/api/v1/symbols/eurusd/start", json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "restart_required");

        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/symbols/eurusd/start",
            json!({ "allowRestart": true }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["start"]["mode"], "containers");
        assert_eq!(body["start"]["zeroImpact"], false);
        assert!(
            body["start"]["steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step["action"] == "rolling-add")
        );

        let (status, body) =
            post_json_to(app.clone(), "/api/v1/symbols/eurusd/start", json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "already_active");

        let (status, detail) = get_json_from(app, "/api/v1/shards/fx-eu-01").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["activeProcesses"], 2);
        assert_eq!(detail["disabledProcesses"], 0);
    }

    #[tokio::test]
    async fn drop_endpoint_defaults_to_retaining_storage() {
        let store = Arc::new(Store::in_memory(true));
        let app = router(store);
        let (status, body) =
            post_json_to(app.clone(), "/api/v1/shards/fx-eu-02/drop", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let drop = &body["drop"];
        assert_eq!(drop["shard"], "fx-eu-02");
        assert_eq!(drop["retainStorage"], true);
        assert_eq!(drop["mode"], "reserved");
        assert_eq!(drop["removedDeployments"].as_array().unwrap().len(), 0);

        let (status, list) = get_json_from(app.clone(), "/api/v1/shards").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list["items"].as_array().unwrap().len(), 2);
        let (status, body) = get_json_from(app, "/api/v1/shards/fx-eu-02").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "shard_not_found");
    }

    #[tokio::test]
    async fn drop_endpoint_rejects_a_shard_with_active_engines() {
        let store = Arc::new(Store::in_memory(true));
        let app = router(store);
        let (status, body) =
            post_json_to(app.clone(), "/api/v1/shards/fx-eu-01/drop", json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "shard_not_idle");
        assert!(body["message"].as_str().unwrap().contains("eurusd"));
        let (status, _) = get_json_from(app, "/api/v1/shards/fx-eu-01").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn drop_endpoint_reports_the_pvcs_for_each_storage_choice() {
        let store = Arc::new(Store::in_memory(true));
        let app = router(store);
        let (status, body) =
            post_json_to(app.clone(), "/api/v1/symbols/btcusdt/shutdown", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, body) = post_json_to(
            app.clone(),
            "/api/v1/shards/me-btcusdt/drop",
            json!({ "retainStorage": false }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let drop = &body["drop"];
        assert_eq!(drop["retainStorage"], false);
        assert_eq!(drop["mode"], "containers");
        assert_eq!(drop["pvcs"].as_array().unwrap().len(), 4);
        assert!(
            drop["steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step["action"] == "delete-storage")
        );
        assert!(
            drop["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|note| note.as_str().unwrap().contains("irreversible"))
        );

        let (status, symbols) = get_json_from(app, "/api/v1/symbols").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            symbols["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["symbol"]["name"] != "btcusdt")
        );
    }

    #[tokio::test]
    async fn all_disabled_processes_render_no_workload() {
        let store = Arc::new(Store::in_memory(true));
        let app = router(store.clone());
        for (symbol, allow_restart) in [("eurusd", true), ("gbpusd", false)] {
            let (status, body) = post_json_to(
                app.clone(),
                &format!("/api/v1/symbols/{symbol}/shutdown"),
                json!({ "allowRestart": allow_restart }),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let (status, manifest) =
            get_json_from(app, "/api/v1/shards/fx-eu-01/manifests?profile=local").await;
        assert_eq!(status, StatusCode::OK);
        let kinds: Vec<&str> = manifest["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["kind"].as_str().unwrap())
            .collect();
        assert!(!kinds.contains(&"StatefulSet"), "{kinds:?}");
        assert!(!kinds.contains(&"Service"), "{kinds:?}");
        assert!(kinds.contains(&"ConfigMap"), "{kinds:?}");
    }

    #[test]
    fn quantity_formatting() {
        assert_eq!(format_cpu(500), "500m");
        assert_eq!(format_cpu(2000), "2");
        assert_eq!(format_bytes(1024 * 1024), "1Mi");
        assert_eq!(format_bytes(1536), "1536");
    }
}

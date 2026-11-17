//! Cell operator: Lease election, CRD watches, workload reconciliation and status.
//!
//! The operator is the only component that applies rendered workloads. It watches the six
//! catalog CRDs, renders each shard and market-data service through the same deterministic
//! renderer as the manager, server-side-applies the desired StatefulSet/Deployment/Services/
//! PDB/NetworkPolicy/ConfigMap, prunes managed objects that are no longer desired, labels the
//! dedicated-shard leader pod for the leader Service, and writes status/route conditions back
//! through the CRD status subresource.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::api::core::v1::{Node, PersistentVolumeClaim, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::jiff::Timestamp;
use kube::Client;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, ListParams, Patch, PatchParams, PostParams,
};
use kube::runtime::watcher::{self, Event};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::api::{format_bytes, format_cpu};
use crate::crd::{self, CrdKind, DEFAULT_NAMESPACE, ObjectKey};
use crate::model::{
    CapacityBlock, Condition, DeploymentState, DeploymentStatus, MarketDataService,
    MarketDataSymbolStatus, OrderSystemShardStatus, OrderSystemStatus, PackingMode, Phase, Release,
    ReleaseBlock, RouteState, RouteStatus, ShardClass, ShardNodeStatus, ShardRolloutStatus,
    ShardStatus, ShardSymbolStatus, SymbolDeployment, SymbolRoute, SymbolShard, TradingSymbol,
    TradingSymbolStatus, desired_config_hash, dns_label,
};
use crate::render::{
    MANAGED_BY, MARKET_DATA_LABEL, ORDER_SYSTEM_LABEL, Profile, RenderOptions,
    render_market_data_service, render_order_system, render_shard,
};
use crate::store::Catalog;
use crate::validate::{parse_cpu_millis, parse_memory_bytes, per_pod_totals};

const FIELD_MANAGER: &str = "match-engine-operator";
const LEADER_LABEL: &str = "matching.io/role";
const RETAIN_UNTIL_ANNOTATION: &str = "matching.io/retain-until";
const RETAINED_AT_ANNOTATION: &str = "matching.io/retained-at";
const RELEASE_WIRE_ANNOTATION: &str = "matching.io/release-wire";
const RELEASE_SNAPSHOT_ANNOTATION: &str = "matching.io/release-snapshot";
/// Maximum applied-index gap tolerated before promoting new voters.
const MEMBERSHIP_CATCH_UP_LAG: u64 = 128;

/// Runtime options for the cell operator.
#[derive(Debug, Clone)]
pub struct OperatorOptions {
    pub profile: Profile,
    pub resync: Duration,
    pub observe_timeout: Duration,
    pub lease_namespace: String,
    pub lease_name: String,
    pub identity: String,
    pub lease_duration: Duration,
    pub lease_renew: Duration,
    /// Restrict CRD/workload watches and pruning to one namespace. `None` is cell-wide.
    pub namespace: Option<String>,
    /// How long orphaned shard PVCs are retained before deletion. `0` marks them for
    /// manual deletion instead of starting a timer.
    pub retention_seconds: u64,
    pub dry_run: bool,
}

impl OperatorOptions {
    pub fn validate(&self) -> Result<()> {
        if self.lease_duration.is_zero() {
            bail!("lease duration must be greater than zero");
        }
        if self.lease_renew.is_zero() {
            bail!("lease renew interval must be greater than zero");
        }
        if self.lease_renew >= self.lease_duration {
            bail!("lease renew interval must be shorter than the lease duration");
        }
        if self.observe_timeout.is_zero() {
            bail!("observe timeout must be greater than zero");
        }
        Ok(())
    }
}

/// Run the operator until cancellation or leadership loss.
pub async fn run(client: Client, options: OperatorOptions) -> Result<()> {
    options.validate()?;
    info!(
        identity = %options.identity,
        lease = %format!("{}/{}", options.lease_namespace, options.lease_name),
        "cell operator starting"
    );
    let mut keeper = LeaseKeeper::acquire(client.clone(), &options).await?;
    info!("leader lease acquired; starting CRD watches");

    let (tx, mut rx) = mpsc::channel(1);
    spawn_watchers(client.clone(), tx, options.namespace.clone());
    let mut ticker = tokio::time::interval(options.resync.max(Duration::from_secs(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            Some(()) = rx.recv() => {}
            _ = keeper.lost() => bail!("leader lease lost; stopping reconciliation"),
        }
        match reconcile_once(&client, &options).await {
            Ok(stats) => info!(
                shards = stats.shards,
                market_data_services = stats.market_data_services,
                order_systems = stats.order_systems,
                desired_objects = stats.desired_objects,
                applied = stats.applied,
                pruned = stats.pruned,
                status_writes = stats.status_writes,
                bootstraps = stats.bootstraps,
                membership_changes = stats.membership_changes,
                config_writes = stats.config_writes,
                rollouts = stats.rollouts,
                capacity_blocks = stats.capacity_blocks,
                retention_marks = stats.retention_marks,
                retention_deletes = stats.retention_deletes,
                pvc_labels = stats.pvc_labels,
                migration_actions = stats.migration_actions,
                action_failures = stats.action_failures,
                "reconcile complete"
            ),
            Err(error) => warn!(
                error = %format!("{error:#}"),
                "reconcile failed; retrying on the next watch/resync"
            ),
        }
    }
}

struct LeaseKeeper {
    lost: watch::Receiver<bool>,
}

impl LeaseKeeper {
    async fn acquire(client: Client, options: &OperatorOptions) -> Result<Self> {
        let api = Api::<Lease>::namespaced(client.clone(), &options.lease_namespace);
        loop {
            match try_acquire(&api, options).await {
                Ok(Some(lease)) => {
                    info!(
                        holder = options.identity,
                        resource_version = ?lease.metadata.resource_version,
                        "leader lease acquired"
                    );
                    return Ok(Self::spawn_renewal(api, lease, options.clone()));
                }
                Ok(None) => {
                    debug!("leader lease held by another operator; waiting");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(error) => {
                    warn!(%error, "lease acquire failed; retrying");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    fn spawn_renewal(api: Api<Lease>, _initial: Lease, options: OperatorOptions) -> Self {
        let (tx, rx) = watch::channel(false);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(options.lease_renew);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match renew_once(
                    &api,
                    &options.lease_name,
                    &options.identity,
                    options.lease_duration,
                )
                .await
                {
                    Ok(true) => debug!("leader lease renewed"),
                    Ok(false) => {
                        warn!("leader lease moved to another holder");
                        let _ = tx.send(true);
                        return;
                    }
                    Err(error) => {
                        warn!(%error, "leader lease renewal failed");
                        let _ = tx.send(true);
                        return;
                    }
                }
            }
        });
        Self { lost: rx }
    }

    async fn lost(&mut self) {
        while !*self.lost.borrow() {
            if self.lost.changed().await.is_err() {
                return;
            }
        }
    }
}

async fn try_acquire(api: &Api<Lease>, options: &OperatorOptions) -> Result<Option<Lease>> {
    let now = Timestamp::now();
    let duration = lease_seconds(options.lease_duration);
    match api.get(&options.lease_name).await {
        Ok(mut lease) => {
            let spec = lease.spec.get_or_insert_with(LeaseSpec::default);
            let same_holder = spec.holder_identity.as_deref() == Some(options.identity.as_str());
            let expired = lease_expired(spec, now.as_second());
            if !same_holder && !expired {
                return Ok(None);
            }
            let previous_holder = spec.holder_identity.clone();
            if !same_holder {
                spec.acquire_time = Some(MicroTime(now));
                spec.lease_transitions = Some(spec.lease_transitions.unwrap_or(0) + 1);
            }
            spec.holder_identity = Some(options.identity.clone());
            spec.lease_duration_seconds = Some(duration);
            spec.renew_time = Some(MicroTime(now));
            let updated = api
                .replace(&options.lease_name, &PostParams::default(), &lease)
                .await
                .context("replace leader lease")?;
            info!(previous_holder = ?previous_holder, "leader lease acquired or renewed");
            Ok(Some(updated))
        }
        Err(kube::Error::Api(error)) if error.code == 404 => {
            let lease = Lease {
                metadata: ObjectMeta {
                    name: Some(options.lease_name.clone()),
                    namespace: Some(options.lease_namespace.clone()),
                    ..ObjectMeta::default()
                },
                spec: Some(LeaseSpec {
                    holder_identity: Some(options.identity.clone()),
                    lease_duration_seconds: Some(duration),
                    acquire_time: Some(MicroTime(now)),
                    renew_time: Some(MicroTime(now)),
                    lease_transitions: Some(0),
                    ..LeaseSpec::default()
                }),
            };
            match api.create(&PostParams::default(), &lease).await {
                Ok(lease) => Ok(Some(lease)),
                Err(kube::Error::Api(error)) if error.code == 409 => Ok(None),
                Err(error) => Err(error).context("create leader lease"),
            }
        }
        Err(error) => Err(error).context("get leader lease"),
    }
}

async fn renew_once(
    api: &Api<Lease>,
    lease_name: &str,
    identity: &str,
    duration: Duration,
) -> Result<bool> {
    let mut lease = api.get(lease_name).await.context("get leader lease")?;
    let spec = lease.spec.get_or_insert_with(LeaseSpec::default);
    if spec.holder_identity.as_deref() != Some(identity) {
        return Ok(false);
    }
    spec.renew_time = Some(MicroTime(Timestamp::now()));
    spec.lease_duration_seconds = Some(lease_seconds(duration));
    api.replace(lease_name, &PostParams::default(), &lease)
        .await
        .context("renew leader lease")?;
    Ok(true)
}

fn lease_seconds(duration: Duration) -> i32 {
    i32::try_from(duration.as_secs()).unwrap_or(i32::MAX)
}

fn lease_expired(spec: &LeaseSpec, now_secs: i64) -> bool {
    let Some(renew_time) = spec.renew_time.as_ref() else {
        return true;
    };
    let duration = spec.lease_duration_seconds.unwrap_or(0).max(0) as i64;
    now_secs.saturating_sub(renew_time.0.as_second()) >= duration
}

fn spawn_watchers(client: Client, tx: mpsc::Sender<()>, namespace: Option<String>) {
    for kind in CrdKind::ALL {
        let client = client.clone();
        let tx = tx.clone();
        let namespace = namespace.clone();
        tokio::spawn(async move {
            loop {
                let api = match namespace.as_deref() {
                    Some(namespace) => Api::<DynamicObject>::namespaced_with(
                        client.clone(),
                        namespace,
                        &kind.api_resource(),
                    ),
                    None => Api::<DynamicObject>::all_with(client.clone(), &kind.api_resource()),
                };
                let mut stream = std::pin::pin!(watcher::watcher(api, watcher::Config::default()));
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(Event::Apply(_))
                        | Ok(Event::Delete(_))
                        | Ok(Event::InitApply(_))
                        | Ok(Event::InitDone)
                        | Ok(Event::Init) => {
                            let _ = tx.try_send(());
                        }
                        Err(error) => {
                            warn!(?kind, %error, "CRD watch failed; reconnecting");
                            break;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkloadKind {
    ConfigMap,
    Service,
    PodDisruptionBudget,
    NetworkPolicy,
    StatefulSet,
    Deployment,
    HorizontalPodAutoscaler,
    Ingress,
}

impl WorkloadKind {
    const ALL: [Self; 8] = [
        Self::ConfigMap,
        Self::Service,
        Self::PodDisruptionBudget,
        Self::NetworkPolicy,
        Self::StatefulSet,
        Self::Deployment,
        Self::HorizontalPodAutoscaler,
        Self::Ingress,
    ];

    fn from_kind(kind: &str) -> Option<Self> {
        match kind {
            "ConfigMap" => Some(Self::ConfigMap),
            "Service" => Some(Self::Service),
            "PodDisruptionBudget" => Some(Self::PodDisruptionBudget),
            "NetworkPolicy" => Some(Self::NetworkPolicy),
            "StatefulSet" => Some(Self::StatefulSet),
            "Deployment" => Some(Self::Deployment),
            "HorizontalPodAutoscaler" => Some(Self::HorizontalPodAutoscaler),
            "Ingress" => Some(Self::Ingress),
            _ => None,
        }
    }

    fn api_resource(self) -> ApiResource {
        let (group, version, kind, plural) = match self {
            Self::ConfigMap => ("", "v1", "ConfigMap", "configmaps"),
            Self::Service => ("", "v1", "Service", "services"),
            Self::PodDisruptionBudget => (
                "policy",
                "v1",
                "PodDisruptionBudget",
                "poddisruptionbudgets",
            ),
            Self::NetworkPolicy => (
                "networking.k8s.io",
                "v1",
                "NetworkPolicy",
                "networkpolicies",
            ),
            Self::StatefulSet => ("apps", "v1", "StatefulSet", "statefulsets"),
            Self::Deployment => ("apps", "v1", "Deployment", "deployments"),
            Self::HorizontalPodAutoscaler => (
                "autoscaling",
                "v2",
                "HorizontalPodAutoscaler",
                "horizontalpodautoscalers",
            ),
            Self::Ingress => ("networking.k8s.io", "v1", "Ingress", "ingresses"),
        };
        ApiResource {
            group: group.to_string(),
            version: version.to_string(),
            api_version: if group.is_empty() {
                version.to_string()
            } else {
                format!("{group}/{version}")
            },
            kind: kind.to_string(),
            plural: plural.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkloadKey {
    pub kind: WorkloadKind,
    pub namespace: String,
    pub name: String,
}

fn workload_key(value: &Value) -> Result<WorkloadKey> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .and_then(WorkloadKind::from_kind)
        .ok_or_else(|| anyhow!("rendered object has no supported kind"))?;
    let name = value
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("rendered {kind:?} has no metadata.name"))?;
    let namespace = value
        .pointer("/metadata/namespace")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("rendered {kind:?} {name} has no metadata.namespace"))?;
    Ok(WorkloadKey {
        kind,
        namespace: namespace.to_string(),
        name: name.to_string(),
    })
}

#[derive(Debug, Default)]
struct ReconcileStats {
    shards: usize,
    market_data_services: usize,
    order_systems: usize,
    desired_objects: usize,
    applied: usize,
    pruned: usize,
    status_writes: usize,
    bootstraps: usize,
    membership_changes: usize,
    config_writes: usize,
    rollouts: usize,
    capacity_blocks: usize,
    retention_marks: usize,
    retention_deletes: usize,
    pvc_labels: usize,
    migration_actions: usize,
    action_failures: usize,
}

struct CrdView {
    catalog: Catalog,
    objects: BTreeMap<ObjectKey, DynamicObject>,
}

impl CrdView {
    fn object(&self, kind: CrdKind, namespace: &str, name: &str) -> Option<&DynamicObject> {
        self.objects.get(&ObjectKey {
            kind,
            namespace: namespace_or_default(namespace).to_string(),
            name: name.to_string(),
        })
    }
}

async fn load_view(client: &Client, namespace: Option<&str>) -> Result<CrdView> {
    let mut objects = Vec::new();
    for kind in CrdKind::ALL {
        let api = match namespace {
            Some(namespace) => Api::<DynamicObject>::namespaced_with(
                client.clone(),
                namespace,
                &kind.api_resource(),
            ),
            None => Api::<DynamicObject>::all_with(client.clone(), &kind.api_resource()),
        };
        let list = api
            .list(&ListParams::default())
            .await
            .with_context(|| format!("list {}", kind.plural()))?;
        objects.extend(list.items);
    }
    let catalog = crd::catalog_from_objects(objects.iter().cloned(), DEFAULT_NAMESPACE);
    let mut by_key = BTreeMap::new();
    for object in objects {
        let Some(kind) = crd::object_kind(&object) else {
            continue;
        };
        if let Some(key) = ObjectKey::from_object(kind, &object, DEFAULT_NAMESPACE) {
            by_key.insert(key, object);
        }
    }
    Ok(CrdView {
        catalog,
        objects: by_key,
    })
}

fn namespace_or_default(namespace: &str) -> &str {
    if namespace.is_empty() {
        DEFAULT_NAMESPACE
    } else {
        namespace
    }
}

fn render_options(shard: &SymbolShard, options: &OperatorOptions) -> RenderOptions {
    RenderOptions {
        profile: options.profile,
        namespace: Some(namespace_or_default(&shard.namespace).to_string()),
        image_pull_policy: None,
        storage_class: None,
        replicas: None,
        non_root: Some(true),
    }
}

async fn reconcile_once(client: &Client, options: &OperatorOptions) -> Result<ReconcileStats> {
    let view = load_view(client, options.namespace.as_deref()).await?;
    let mut stats = ReconcileStats {
        shards: view.catalog.shards.len(),
        market_data_services: view.catalog.market_data_services.len(),
        order_systems: view.catalog.order_systems.len(),
        ..ReconcileStats::default()
    };
    let mut desired = BTreeMap::<WorkloadKey, Value>::new();
    let mut failed_shards = BTreeSet::new();
    let mut failed_market_data = BTreeSet::new();
    let mut failed_order_systems = BTreeSet::new();

    for shard in view.catalog.shards.values() {
        match render_shard(&view.catalog, &shard.name, &render_options(shard, options)) {
            Ok(objects) => {
                for object in objects.into_iter().filter(|object| !object.is_null()) {
                    let key = workload_key(&object)?;
                    desired.insert(key, object);
                }
            }
            Err(error) => {
                warn!(shard = %shard.name, %error, "cannot render shard; skipping apply/prune");
                failed_shards.insert(shard.name.clone());
            }
        }
    }
    for service in view.catalog.market_data_services.values() {
        match render_market_data_service(&view.catalog, &service.name, None) {
            Ok(objects) => {
                for object in objects.into_iter().filter(|object| !object.is_null()) {
                    let key = workload_key(&object)?;
                    desired.insert(key, object);
                }
            }
            Err(error) => {
                warn!(
                    service = %service.name,
                    error = %format!("{error:?}"),
                    "cannot render market-data service; skipping apply/prune"
                );
                failed_market_data.insert(service.name.clone());
            }
        }
    }
    for system in view.catalog.order_systems.values() {
        match render_order_system(&view.catalog, &system.name, None) {
            Ok(objects) => {
                for object in objects.into_iter().filter(|object| !object.is_null()) {
                    let key = workload_key(&object)?;
                    desired.insert(key, object);
                }
            }
            Err(error) => {
                warn!(
                    system = %system.name,
                    error = %format!("{error:?}"),
                    "cannot render order system; skipping apply/prune"
                );
                failed_order_systems.insert(system.name.clone());
            }
        }
    }
    stats.desired_objects = desired.len();
    let capacity_blocks = if options.dry_run {
        BTreeMap::new()
    } else {
        capacity_preflight(client, &desired, &view.catalog).await?
    };
    stats.capacity_blocks = capacity_blocks.len();

    if !options.dry_run {
        for (key, object) in &desired {
            if key.kind == WorkloadKind::StatefulSet && capacity_blocks.contains_key(&key.name) {
                continue;
            }
            apply_object(client, key, object).await?;
            stats.applied += 1;
        }
        stats.pruned = prune_objects(
            client,
            &desired,
            &failed_shards,
            &failed_market_data,
            &failed_order_systems,
            options.namespace.as_deref(),
        )
        .await?;
        stats.pvc_labels = label_shard_pvcs(client, &view).await?;
        let (marks, deletes) = reconcile_retention(client, &view, options).await?;
        stats.retention_marks = marks;
        stats.retention_deletes = deletes;
    } else {
        info!(
            desired = stats.desired_objects,
            "dry-run: skipping workload apply/prune"
        );
    }

    let observations = observe_catalog(&view.catalog, options).await;
    let market_data_observations = observe_market_data(client, &view.catalog, options).await;
    let order_system_statuses = observe_order_systems(client, &view.catalog).await;
    if !options.dry_run {
        let actions = actuate_catalog(&view.catalog, &observations, options).await;
        stats.bootstraps = actions.bootstraps;
        stats.membership_changes = actions.membership_changes;
        stats.config_writes = actions.config_writes;
        stats.action_failures = actions.failures;
        stats.migration_actions = execute_migrations(client, &view, &observations, options).await?;
        let rollout = rollout_shards(client, &view.catalog, &observations).await?;
        stats.rollouts = rollout.actions;
        stats.status_writes = write_statuses(
            client,
            &view,
            &observations,
            &rollout.release_blocks,
            &capacity_blocks,
        )
        .await?;
        stats.status_writes += write_market_data_statuses(
            client,
            &view,
            &market_data_observations,
            &failed_market_data,
        )
        .await?;
        stats.status_writes += write_order_system_statuses(
            client,
            &view,
            &order_system_statuses,
            &failed_order_systems,
        )
        .await?;
        label_leaders(client, &view.catalog, &observations).await?;
    } else {
        info!(
            deployments = observations.len(),
            market_data = market_data_observations.len(),
            order_systems = order_system_statuses.len(),
            "dry-run: skipping status writes, leader labels and bootstrap/config actions"
        );
    }
    Ok(stats)
}

async fn apply_object(client: &Client, key: &WorkloadKey, object: &Value) -> Result<()> {
    let api = Api::<DynamicObject>::namespaced_with(
        client.clone(),
        &key.namespace,
        &key.kind.api_resource(),
    );
    let params = PatchParams::apply(FIELD_MANAGER).force();
    match api.patch(&key.name, &params, &Patch::Apply(object)).await {
        Ok(_) => Ok(()),
        Err(error) if key.kind == WorkloadKind::StatefulSet && is_validation_error(&error) => {
            // A fleet adopted from `kubectl apply` can leave list entries (containers, ports,
            // env) co-owned by `kubectl-client-side-apply`. Removing a container with SSA then
            // strips the operator-owned fields but leaves a partial item behind, so the API
            // rejects the pod template (for example "containers[0].image: Required value").
            // Merge patch replaces arrays as a whole and migrates the StatefulSet cleanly.
            warn!(
                kind = ?key.kind,
                namespace = %key.namespace,
                name = %key.name,
                error = %format!("{error:#}"),
                "server-side apply failed; retrying StatefulSet with a merge patch"
            );
            let params = PatchParams::default();
            api.patch(&key.name, &params, &Patch::Merge(object))
                .await
                .with_context(|| {
                    format!(
                        "merge patch {:?} {}/{} after server-side apply failed: {error:#}",
                        key.kind, key.namespace, key.name
                    )
                })?;
            Ok(())
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "server-side apply {:?} {}/{}",
                key.kind, key.namespace, key.name
            )
        }),
    }
}

fn is_validation_error(error: &kube::Error) -> bool {
    matches!(error, kube::Error::Api(response) if response.code == 422)
}

async fn prune_objects(
    client: &Client,
    desired: &BTreeMap<WorkloadKey, Value>,
    failed_shards: &BTreeSet<String>,
    failed_market_data: &BTreeSet<String>,
    failed_order_systems: &BTreeSet<String>,
    namespace: Option<&str>,
) -> Result<usize> {
    let mut pruned = 0;
    let selector = format!("app.kubernetes.io/managed-by={MANAGED_BY}");
    for kind in WorkloadKind::ALL {
        let list_api = match namespace {
            Some(namespace) => Api::<DynamicObject>::namespaced_with(
                client.clone(),
                namespace,
                &kind.api_resource(),
            ),
            None => Api::<DynamicObject>::all_with(client.clone(), &kind.api_resource()),
        };
        let list = list_api
            .list(&ListParams::default().labels(&selector))
            .await
            .with_context(|| format!("list managed {:?}", kind))?;
        for object in list.items {
            let Some(name) = object.metadata.name.clone() else {
                continue;
            };
            let Some(namespace) = object.metadata.namespace.clone() else {
                continue;
            };
            let key = WorkloadKey {
                kind,
                namespace: namespace.clone(),
                name: name.clone(),
            };
            if desired.contains_key(&key) {
                continue;
            }
            if object
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("matching.io/shard"))
                .is_some_and(|shard| failed_shards.contains(shard))
            {
                continue;
            }
            if object
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get(MARKET_DATA_LABEL))
                .is_some_and(|service| failed_market_data.contains(service))
            {
                continue;
            }
            if object
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get(ORDER_SYSTEM_LABEL))
                .is_some_and(|system| failed_order_systems.contains(system))
            {
                continue;
            }
            let namespaced = Api::<DynamicObject>::namespaced_with(
                client.clone(),
                &namespace,
                &kind.api_resource(),
            );
            match namespaced.delete(&name, &DeleteParams::default()).await {
                Ok(_) => {
                    info!(?kind, namespace, name, "pruned managed object");
                    pruned += 1;
                }
                Err(kube::Error::Api(error)) if error.code == 404 => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("delete managed {:?} {namespace}/{name}", kind));
                }
            }
        }
    }
    Ok(pruned)
}

#[derive(Debug, Clone, Default)]
pub struct PodObservation {
    pub pod: u8,
    /// Raft node ID reported by `/health`; defaults to the pod ordinal when absent.
    pub raft_id: Option<u64>,
    pub healthy: bool,
    pub ready: bool,
    pub is_leader: bool,
    pub leader_id: Option<u64>,
    pub applied_index: Option<u64>,
    pub last_log_index: Option<u64>,
    pub voters: Vec<u64>,
    pub learners: Vec<u64>,
    pub symbol_config: Option<Value>,
    pub config_locked: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DeploymentObservation {
    pub pods: Vec<PodObservation>,
    pub leader_pod: Option<u8>,
    pub leader_pod_name: Option<String>,
    pub leader_addr: Option<String>,
    pub ready: bool,
    pub phase: Phase,
    pub applied_index: Option<u64>,
    pub lag: Option<u64>,
    pub voters: Vec<u64>,
    pub learners: Vec<u64>,
    pub configured: bool,
    pub config_locked: bool,
}

pub type Observations = BTreeMap<String, DeploymentObservation>;

#[derive(Debug, Deserialize)]
struct HealthProbe {
    #[serde(default)]
    healthy: bool,
    #[serde(default)]
    ready: bool,
    #[serde(default)]
    node_id: Option<u64>,
    #[serde(default)]
    leadership: LeadershipProbe,
    #[serde(default)]
    storage: StorageProbe,
}

#[derive(Debug, Default, Deserialize)]
struct LeadershipProbe {
    #[serde(default)]
    leader_id: Option<u64>,
    #[serde(default)]
    is_leader: bool,
}

#[derive(Debug, Default, Deserialize)]
struct StorageProbe {
    #[serde(default)]
    wal: Option<WalProbe>,
    #[serde(default)]
    state_machine: Option<StateMachineProbe>,
}

#[derive(Debug, Default, Deserialize)]
struct WalProbe {
    #[serde(default)]
    last_log_id: Option<LogIdProbe>,
}

#[derive(Debug, Default, Deserialize)]
struct StateMachineProbe {
    #[serde(default)]
    last_applied: Option<LogIdProbe>,
}

#[derive(Debug, Default, Deserialize)]
struct LogIdProbe {
    #[serde(default)]
    index: u64,
}

async fn observe_catalog(catalog: &Catalog, options: &OperatorOptions) -> Observations {
    let http = reqwest::Client::new();
    let mut observations = BTreeMap::new();
    for shard in catalog.shards.values() {
        for deployment in catalog.shard_deployments(&shard.name) {
            let deployment = deployment.clone();
            if deployment.spec.desired_state == DeploymentState::Disabled {
                observations.insert(
                    deployment.name.clone(),
                    DeploymentObservation {
                        phase: Phase::Disabled,
                        ..DeploymentObservation::default()
                    },
                );
                continue;
            }
            let desired_config = catalog
                .symbols
                .get(&deployment.spec.symbol_ref)
                .map(desired_symbol_config);
            let futures = (0..shard.spec.replicas).map(|pod| {
                let http = http.clone();
                let shard = shard.clone();
                let deployment = deployment.clone();
                let timeout = options.observe_timeout;
                async move { observe_pod(&http, &shard, &deployment, pod, timeout).await }
            });
            let pods: Vec<PodObservation> = futures::stream::iter(futures)
                .buffer_unordered(16)
                .collect()
                .await;
            observations.insert(
                deployment.name.clone(),
                deployment_observation(shard, &deployment, pods, desired_config.as_ref()),
            );
        }
    }
    observations
}

#[derive(Debug, Clone, Default)]
pub struct MarketDataObservation {
    pub replicas: u16,
    pub ready_replicas: u16,
    pub updated_replicas: u16,
    pub available: bool,
    pub symbols: Vec<MarketDataSymbolStatus>,
    pub feed_ready: bool,
    pub message: String,
    pub error: Option<String>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Coarse last-frame age so an idle service does not rewrite CR status every reconcile.
fn quantize_frame_age(age_ms: u64) -> u64 {
    match age_ms {
        0..=999 => 0,
        1_000..=4_999 => 1_000,
        5_000..=29_999 => 5_000,
        30_000..=59_999 => 30_000,
        _ => 60_000,
    }
}

async fn observe_market_data(
    client: &Client,
    catalog: &Catalog,
    options: &OperatorOptions,
) -> BTreeMap<String, MarketDataObservation> {
    let http = reqwest::Client::new();
    let mut observations = BTreeMap::new();
    for service in catalog.market_data_services.values() {
        let namespace = namespace_or_default(&service.namespace);
        let mut observation = MarketDataObservation::default();
        let deployment_api = Api::<Deployment>::namespaced(client.clone(), namespace);
        match deployment_api.get(&service.name).await {
            Ok(deployment) => {
                if let Some(status) = deployment.status {
                    observation.replicas = status.replicas.unwrap_or(0).max(0) as u16;
                    observation.ready_replicas = status.ready_replicas.unwrap_or(0).max(0) as u16;
                    observation.updated_replicas =
                        status.updated_replicas.unwrap_or(0).max(0) as u16;
                    observation.available = status.conditions.as_ref().is_some_and(|conditions| {
                        conditions.iter().any(|condition| {
                            condition.type_.as_deref() == Some("Available")
                                && condition.status.as_deref() == Some("True")
                        })
                    });
                }
            }
            Err(kube::Error::Api(error)) if error.code == 404 => {
                observation.message = "Deployment has not been created yet".to_string();
                observations.insert(service.name.clone(), observation);
                continue;
            }
            Err(error) => {
                observation.error = Some(format!(
                    "get Deployment {namespace}/{}: {error}",
                    service.name
                ));
                observations.insert(service.name.clone(), observation);
                continue;
            }
        }

        let pod_api = Api::<Pod>::namespaced(client.clone(), namespace);
        let selector = format!("{MARKET_DATA_LABEL}={}", service.name);
        let pod = match pod_api.list(&ListParams::default().labels(&selector)).await {
            Ok(mut list) => {
                let has_ip = |pod: &Pod| {
                    pod.status
                        .as_ref()
                        .and_then(|status| status.pod_ip.as_ref())
                        .is_some()
                };
                let ready = list.items.iter().position(|pod| {
                    has_ip(pod)
                        && pod
                            .status
                            .as_ref()
                            .and_then(|status| status.conditions.as_ref())
                            .is_some_and(|conditions| {
                                conditions.iter().any(|condition| {
                                    condition.type_ == "Ready" && condition.status == "True"
                                })
                            })
                });
                // A not-ready pod is still the best diagnostic source: it reports which
                // symbol has not seen a snapshot/heartbeat yet.
                let running = list.items.iter().position(|pod| {
                    has_ip(pod)
                        && pod
                            .status
                            .as_ref()
                            .and_then(|status| status.phase.as_deref())
                            == Some("Running")
                });
                ready.or(running).map(|index| list.items.remove(index))
            }
            Err(error) => {
                observation.error = Some(format!("list market-data pods in {namespace}: {error}"));
                observations.insert(service.name.clone(), observation);
                continue;
            }
        };

        let Some(pod) = pod else {
            observation.message = "no running market-data pod".to_string();
            observations.insert(service.name.clone(), observation);
            continue;
        };
        let Some(ip) = pod
            .status
            .as_ref()
            .and_then(|status| status.pod_ip.as_deref())
        else {
            observation.message = "market-data pod has no IP".to_string();
            observations.insert(service.name.clone(), observation);
            continue;
        };
        let port = service.spec.websocket.port;
        let ready = fetch_json(
            &http,
            &format!("http://{ip}:{port}/ready"),
            options.observe_timeout,
        )
        .await;
        let metrics = fetch_json(
            &http,
            &format!("http://{ip}:{port}/metrics"),
            options.observe_timeout,
        )
        .await;
        let now = now_ms();
        let mut not_ready = Vec::new();
        let mut symbols = Vec::new();
        for name in service.spec.symbol_names() {
            let ready_symbol = ready
                .as_ref()
                .and_then(|value| value.get("symbols"))
                .and_then(|symbols| symbols.get(&name));
            let ready = ready_symbol
                .and_then(|value| value.get("ready"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let last_ts = ready_symbol
                .and_then(|value| value.get("last_ts_ms"))
                .and_then(Value::as_u64)
                .or_else(|| {
                    metrics
                        .as_ref()
                        .and_then(|value| value.get("symbols"))
                        .and_then(|symbols| symbols.get(&name))
                        .and_then(|value| value.get("last_ts_ms"))
                        .and_then(Value::as_u64)
                });
            let age = last_ts
                .filter(|ts| *ts > 0)
                .map(|ts| quantize_frame_age(now.saturating_sub(ts)))
                .unwrap_or(0);
            let subscribers = metrics
                .as_ref()
                .and_then(|value| value.get("symbols"))
                .and_then(|symbols| symbols.get(&name))
                .and_then(|value| value.get("subscribers"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let drops = metrics
                .as_ref()
                .and_then(|value| value.get("gaps"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if !ready {
                not_ready.push(name.clone());
            }
            symbols.push(MarketDataSymbolStatus {
                name,
                ready,
                feed_lag: age,
                last_frame_age_ms: age,
                subscribers,
                drops,
            });
        }
        observation.feed_ready = !symbols.is_empty() && symbols.iter().all(|symbol| symbol.ready);
        observation.symbols = symbols;
        if !observation.feed_ready {
            observation.message = if not_ready.is_empty() {
                "feed not ready".to_string()
            } else {
                format!("feed not ready for {}", not_ready.join(", "))
            };
        }
        observations.insert(service.name.clone(), observation);
    }
    observations
}

fn desired_symbol_config(symbol: &TradingSymbol) -> Value {
    json!({
        "symbol_id": symbol.spec.symbol_id,
        "base_asset": symbol.spec.base,
        "quote_asset": symbol.spec.quote,
        "price_decimals": symbol.spec.price_decimals,
        "price_tick": symbol.spec.price_tick,
        "base_lot": symbol.spec.base_lot,
        "fee_asset_policy": symbol.spec.fee_asset_policy,
        "stp": symbol.spec.stp,
    })
}

async fn fetch_json(http: &reqwest::Client, url: &str, timeout: Duration) -> Option<Value> {
    http.get(url)
        .timeout(timeout)
        .send()
        .await
        .ok()?
        .json::<Value>()
        .await
        .ok()
}

fn membership_ids(value: &Value, pointer: &str) -> Vec<u64> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

fn membership_nodes(value: &Value) -> Vec<u64> {
    value
        .pointer("/Ok/membership_config/membership/nodes")
        .and_then(Value::as_object)
        .map(|nodes| {
            nodes
                .keys()
                .filter_map(|id| id.parse::<u64>().ok())
                .collect()
        })
        .unwrap_or_default()
}

async fn observe_pod(
    http: &reqwest::Client,
    shard: &SymbolShard,
    deployment: &SymbolDeployment,
    pod: u8,
    timeout: Duration,
) -> PodObservation {
    let namespace = namespace_or_default(&shard.namespace);
    let base = format!(
        "http://{}-{pod}.{}-headless.{namespace}.svc.cluster.local:{}",
        shard.name, shard.name, deployment.spec.placement.http_port
    );
    let health_url = format!("{base}/health");
    let metrics_url = format!("{base}/metrics");
    let pretrade_url = format!("{base}/pretrade/status");
    let (health, metrics, pretrade) = tokio::join!(
        fetch_json(http, &health_url, timeout),
        fetch_json(http, &metrics_url, timeout),
        fetch_json(http, &pretrade_url, timeout),
    );
    let Some(health) = health else {
        return PodObservation {
            pod,
            error: Some(format!("{base}/health unavailable")),
            ..PodObservation::default()
        };
    };
    let probe: HealthProbe = match serde_json::from_value(health) {
        Ok(probe) => probe,
        Err(error) => {
            return PodObservation {
                pod,
                error: Some(format!("decode /health: {error}")),
                ..PodObservation::default()
            };
        }
    };
    let voters = metrics
        .as_ref()
        .map(|value| membership_ids(value, "/Ok/membership_config/membership/configs/0"))
        .unwrap_or_default();
    let nodes = metrics.as_ref().map(membership_nodes).unwrap_or_default();
    let learners: Vec<u64> = nodes
        .into_iter()
        .filter(|id| !voters.contains(id))
        .collect();
    let symbol_config = pretrade
        .as_ref()
        .and_then(|value| value.get("symbol_config").cloned());
    let config_locked = pretrade
        .as_ref()
        .and_then(|value| value.get("symbol_locked"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    PodObservation {
        pod,
        raft_id: probe.node_id.or(Some(u64::from(pod))),
        healthy: probe.healthy,
        ready: probe.healthy && probe.ready,
        is_leader: probe.leadership.is_leader,
        leader_id: probe.leadership.leader_id,
        applied_index: probe
            .storage
            .state_machine
            .and_then(|state| state.last_applied)
            .map(|log| log.index),
        last_log_index: probe
            .storage
            .wal
            .and_then(|wal| wal.last_log_id)
            .map(|log| log.index),
        voters,
        learners,
        symbol_config,
        config_locked,
        error: None,
    }
}

pub fn deployment_observation(
    shard: &SymbolShard,
    deployment: &SymbolDeployment,
    mut pods: Vec<PodObservation>,
    desired_config: Option<&Value>,
) -> DeploymentObservation {
    pods.sort_by_key(|pod| pod.pod);
    let leader_pod = pods
        .iter()
        .find(|pod| pod.is_leader)
        .map(|pod| pod.pod)
        .or_else(|| {
            pods.iter()
                .find(|pod| pod.leader_id == pod.raft_id.or(Some(u64::from(pod.pod))))
                .map(|pod| pod.pod)
        });
    let leader = leader_pod.and_then(|leader| pods.iter().find(|pod| pod.pod == leader));
    let leader_addr = leader_pod.map(|pod| {
        format!(
            "{}-{pod}.{}-headless.{}.svc.cluster.local:{}",
            shard.name,
            shard.name,
            namespace_or_default(&shard.namespace),
            deployment.spec.placement.http_port
        )
    });
    let replica_count = shard.spec.replicas as usize;
    let ready =
        pods.len() == replica_count && pods.iter().all(|pod| pod.ready) && leader_pod.is_some();
    let phase = if deployment.spec.desired_state == DeploymentState::Disabled {
        Phase::Disabled
    } else if ready {
        Phase::Ready
    } else if pods.iter().any(|pod| pod.ready) {
        Phase::Degraded
    } else {
        Phase::Pending
    };
    let applied_index = pods.iter().filter_map(|pod| pod.applied_index).max();
    let lag = {
        let last_log = pods.iter().filter_map(|pod| pod.last_log_index).max();
        let min_applied = pods.iter().filter_map(|pod| pod.applied_index).min();
        last_log
            .zip(min_applied)
            .map(|(last, applied)| last.saturating_sub(applied))
    };
    let configured = desired_config.is_some_and(|desired| {
        leader
            .and_then(|pod| pod.symbol_config.as_ref())
            .is_some_and(|observed| observed == desired)
    });
    let config_locked =
        leader.is_some_and(|pod| pod.config_locked) || pods.iter().any(|pod| pod.config_locked);
    let voters = pods
        .iter()
        .find(|pod| !pod.voters.is_empty())
        .map(|pod| pod.voters.clone())
        .unwrap_or_default();
    let learners = pods
        .iter()
        .find(|pod| !pod.learners.is_empty())
        .map(|pod| pod.learners.clone())
        .unwrap_or_default();
    DeploymentObservation {
        pods,
        leader_pod,
        leader_pod_name: leader_pod.map(|pod| format!("{}-{pod}", shard.name)),
        leader_addr,
        ready,
        phase,
        applied_index,
        lag,
        voters,
        learners,
        configured,
        config_locked,
    }
}

#[derive(Debug, Default)]
struct ActuationStats {
    bootstraps: usize,
    membership_changes: usize,
    config_writes: usize,
    failures: usize,
}

fn process_url(shard: &SymbolShard, deployment: &SymbolDeployment, pod: u8, path: &str) -> String {
    format!(
        "http://{}-{pod}.{}-headless.{}.svc.cluster.local:{}/{path}",
        shard.name,
        shard.name,
        namespace_or_default(&shard.namespace),
        deployment.spec.placement.http_port
    )
}

fn voter_endpoints(shard: &SymbolShard, deployment: &SymbolDeployment) -> Vec<(u64, String)> {
    let offset = deployment.spec.raft_id_offset;
    (0..shard.spec.replicas)
        .map(|pod| {
            (
                offset.saturating_add(u64::from(pod)),
                format!(
                    "{}-{pod}.{}-headless.{}.svc.cluster.local:{}",
                    shard.name,
                    shard.name,
                    namespace_or_default(&shard.namespace),
                    deployment.spec.placement.http_port
                ),
            )
        })
        .collect()
}

async fn post_json(
    http: &reqwest::Client,
    url: &str,
    body: &Value,
    timeout: Duration,
) -> Result<Value> {
    post_json_with_header(http, url, body, timeout, None).await
}

async fn post_json_with_header(
    http: &reqwest::Client,
    url: &str,
    body: &Value,
    timeout: Duration,
    header: Option<(&str, &str)>,
) -> Result<Value> {
    let mut request = http.post(url).timeout(timeout).json(body);
    if let Some((name, value)) = header {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() || value.get("Err").is_some() {
        bail!("POST {url} returned {status}: {value}");
    }
    Ok(value)
}

async fn actuate_catalog(
    catalog: &Catalog,
    observations: &Observations,
    options: &OperatorOptions,
) -> ActuationStats {
    let http = reqwest::Client::new();
    let mut stats = ActuationStats::default();
    for shard in catalog.shards.values() {
        for deployment in catalog.shard_deployments(&shard.name) {
            if deployment.spec.desired_state != DeploymentState::Active {
                continue;
            }
            let Some(observation) = observations.get(&deployment.name) else {
                continue;
            };
            if observation.pods.len() != shard.spec.replicas as usize
                || !observation.pods.iter().all(|pod| pod.healthy)
            {
                continue;
            }
            // An initialised source in an in-flight migration must not be re-actuated:
            // the migration controller temporarily moves its voter set to the target.
            // Before the source has voters/configuration, still allow /init and
            // ConfigureSymbol so a shadow can be created concurrently.
            if migration_source(catalog, deployment)
                && (!observation.voters.is_empty() || !observation.learners.is_empty())
                && observation.configured
            {
                continue;
            }
            let offset = deployment.spec.raft_id_offset;
            let desired: BTreeSet<u64> = (0..shard.spec.replicas)
                .map(|pod| offset.saturating_add(u64::from(pod)))
                .collect();
            let voters: BTreeSet<u64> = observation.voters.iter().copied().collect();
            let learners: BTreeSet<u64> = observation.learners.iter().copied().collect();
            let known: BTreeSet<u64> = voters.union(&learners).copied().collect();

            if voters.is_empty() && known.is_empty() {
                let endpoints = voter_endpoints(shard, deployment);
                match post_json(
                    &http,
                    &process_url(shard, deployment, 0, "init"),
                    &json!(endpoints),
                    options.observe_timeout,
                )
                .await
                {
                    Ok(_) => {
                        stats.bootstraps += 1;
                        info!(shard = %shard.name, deployment = %deployment.name, "bootstrapped Raft group");
                    }
                    Err(error) => {
                        stats.failures += 1;
                        warn!(shard = %shard.name, deployment = %deployment.name, %error, "bootstrap failed");
                    }
                }
                continue;
            }

            if voters != desired {
                let Some(leader) = observation.leader_pod else {
                    continue;
                };
                let missing_known: Vec<u64> = desired.difference(&known).copied().collect();
                if !missing_known.is_empty() {
                    for id in missing_known {
                        let address = voter_endpoints(shard, deployment)
                            .into_iter()
                            .find(|(candidate, _)| *candidate == id)
                            .map(|(_, address)| address)
                            .unwrap_or_default();
                        match post_json(
                            &http,
                            &process_url(shard, deployment, leader, "add-learner"),
                            &json!([id, address]),
                            options.observe_timeout,
                        )
                        .await
                        {
                            Ok(_) => {
                                stats.membership_changes += 1;
                                info!(shard = %shard.name, deployment = %deployment.name, node = id, "added learner");
                            }
                            Err(error) => {
                                stats.failures += 1;
                                warn!(shard = %shard.name, deployment = %deployment.name, node = id, %error, "add learner failed");
                            }
                        }
                    }
                    continue;
                }
                let desired_ids: Vec<u64> = desired.iter().copied().collect();
                // Do not promote a voter that is still replaying/installing a snapshot.
                // The index gap is a conservative proxy; wait until every desired pod
                // reports applied/last-log state and the slowest applied index is close
                // to the newest log index.
                let mut catch_up = Some((u64::MAX, 0u64));
                for pod in observation
                    .pods
                    .iter()
                    .filter(|pod| desired.contains(&pod.raft_id.unwrap_or(u64::from(pod.pod))))
                {
                    let (Some(applied), Some(last)) = (pod.applied_index, pod.last_log_index)
                    else {
                        catch_up = None;
                        break;
                    };
                    let Some((min_applied, max_log)) = catch_up else {
                        break;
                    };
                    catch_up = Some((min_applied.min(applied), max_log.max(last)));
                }
                match catch_up {
                    None => {
                        info!(
                            shard = %shard.name,
                            deployment = %deployment.name,
                            "waiting for membership status before changing voters"
                        );
                        continue;
                    }
                    Some((min_applied, max_log))
                        if max_log.saturating_sub(min_applied) > MEMBERSHIP_CATCH_UP_LAG =>
                    {
                        info!(
                            shard = %shard.name,
                            deployment = %deployment.name,
                            min_applied,
                            max_log,
                            "waiting for desired voters to catch up before membership change"
                        );
                        continue;
                    }
                    Some(_) => {}
                }
                match post_json(
                    &http,
                    &process_url(shard, deployment, leader, "change-membership"),
                    &json!(desired_ids),
                    options.observe_timeout,
                )
                .await
                {
                    Ok(_) => {
                        stats.membership_changes += 1;
                        info!(shard = %shard.name, deployment = %deployment.name, "changed Raft membership");
                    }
                    Err(error) => {
                        stats.failures += 1;
                        warn!(shard = %shard.name, deployment = %deployment.name, %error, "change membership failed");
                    }
                }
                continue;
            }

            if observation.leader_pod.is_none() || observation.configured {
                continue;
            }
            if observation.config_locked {
                continue;
            }
            let Some(symbol) = catalog.symbols.get(&deployment.spec.symbol_ref) else {
                continue;
            };
            let body = json!({
                "ConfigureSymbol": {
                    "config": desired_symbol_config(symbol),
                }
            });
            match post_json(
                &http,
                &process_url(
                    shard,
                    deployment,
                    observation.leader_pod.unwrap_or(0),
                    "write",
                ),
                &body,
                options.observe_timeout,
            )
            .await
            {
                Ok(_) => {
                    stats.config_writes += 1;
                    info!(shard = %shard.name, deployment = %deployment.name, "applied symbol configuration");
                }
                Err(error) => {
                    stats.failures += 1;
                    warn!(shard = %shard.name, deployment = %deployment.name, %error, "configure symbol failed");
                }
            }
        }
    }
    stats
}

/// Executes a single migration transition per shard/symbol per reconcile.
///
/// Forward: add the target's offset IDs as learners to the source leader, wait for catch-up,
/// change membership, then switch the route and drain the source. Reverse (`state: Rollback`):
/// add the source IDs back to the target leader, wait, change membership back, restore the route
/// and disable the target. The source PVCs are retained throughout.
async fn execute_migrations(
    client: &Client,
    view: &CrdView,
    observations: &Observations,
    options: &OperatorOptions,
) -> Result<usize> {
    let http = reqwest::Client::new();
    let mut actions = finish_cutovers(client, view, observations).await?;
    actions += rollback_migrations(client, view, observations, options).await?;

    for deployment in view.catalog.deployments.values() {
        if deployment.spec.desired_state != DeploymentState::Shadow {
            continue;
        }
        let Some(route) = view.catalog.routes.values().find(|route| {
            route.spec.symbol_ref == deployment.spec.symbol_ref
                && route.spec.active_deployment_ref != deployment.name
        }) else {
            continue;
        };
        if route.spec.state == RouteState::Rollback {
            continue;
        }
        let Some(source) = view
            .catalog
            .deployments
            .get(&route.spec.active_deployment_ref)
        else {
            continue;
        };
        if !matches!(
            source.spec.desired_state,
            DeploymentState::Active | DeploymentState::Draining
        ) || source.spec.shard_ref == deployment.spec.shard_ref
        {
            continue;
        }
        let (Some(source_shard), Some(target_shard)) = (
            view.catalog.shards.get(&source.spec.shard_ref),
            view.catalog.shards.get(&deployment.spec.shard_ref),
        ) else {
            continue;
        };
        if source_shard.spec.cell != target_shard.spec.cell {
            warn!(
                symbol = %route.spec.symbol_ref,
                "cross-cell migration is not supported by the Raft membership controller"
            );
            continue;
        }
        let (Some(source_obs), Some(target_obs)) = (
            observations.get(&source.name),
            observations.get(&deployment.name),
        ) else {
            continue;
        };
        if target_obs.pods.len() != target_shard.spec.replicas as usize
            || !target_obs.pods.iter().all(|pod| pod.healthy)
        {
            continue;
        }
        let source_ids = deployment_ids(source, source_shard.spec.replicas);
        let target_ids = deployment_ids(deployment, target_shard.spec.replicas);
        let source_set: BTreeSet<u64> = source_ids.iter().copied().collect();
        let target_set: BTreeSet<u64> = target_ids.iter().copied().collect();
        let source_voters: BTreeSet<u64> = source_obs.voters.iter().copied().collect();
        let target_voters: BTreeSet<u64> = target_obs.voters.iter().copied().collect();

        if target_voters == target_set && target_obs.leader_pod.is_some() {
            patch_spec(
                client,
                CrdKind::Route,
                &route.namespace,
                &route.name,
                json!({
                    "activeDeploymentRef": deployment.name,
                    "previousDeploymentRef": source.name,
                    "state": "Cutover"
                }),
            )
            .await?;
            actions += 1;
            info!(
                symbol = %route.spec.symbol_ref,
                source = %source.name,
                target = %deployment.name,
                "migration target is caught up; route moved to Cutover"
            );
            continue;
        }
        if source_voters == target_set {
            info!(
                symbol = %route.spec.symbol_ref,
                target = %deployment.name,
                "waiting for the target group to elect a leader after membership change"
            );
            continue;
        }
        if !source_voters.is_empty() && source_voters != source_set {
            warn!(
                symbol = %route.spec.symbol_ref,
                source = %source.name,
                voters = ?source_voters,
                "source membership is not the source generation; refusing automatic migration"
            );
            continue;
        }
        let Some(leader) = source_obs.leader_pod else {
            continue;
        };
        let known: BTreeSet<u64> = source_obs
            .voters
            .iter()
            .chain(source_obs.learners.iter())
            .copied()
            .collect();
        let missing: Vec<u64> = target_set.difference(&known).copied().collect();
        if !missing.is_empty() {
            let endpoints = voter_endpoints(target_shard, deployment);
            for id in missing {
                let Some((_, address)) = endpoints.iter().find(|(candidate, _)| *candidate == id)
                else {
                    continue;
                };
                match post_json(
                    &http,
                    &process_url(source_shard, source, leader, "add-learner"),
                    &json!([id, address]),
                    options.observe_timeout,
                )
                .await
                {
                    Ok(_) => {
                        actions += 1;
                        info!(
                            symbol = %route.spec.symbol_ref,
                            source = %source.name,
                            target = %deployment.name,
                            node = id,
                            "added migration target as a learner"
                        );
                    }
                    Err(error) => warn!(
                        symbol = %route.spec.symbol_ref,
                        node = id,
                        %error,
                        "add migration learner failed"
                    ),
                }
            }
            continue;
        }

        let Some(symbol) = view.catalog.symbols.get(&deployment.spec.symbol_ref) else {
            continue;
        };
        let desired = desired_symbol_config(symbol);
        if !target_obs
            .pods
            .iter()
            .all(|pod| pod.symbol_config.as_ref() == Some(&desired))
        {
            info!(
                symbol = %route.spec.symbol_ref,
                target = %deployment.name,
                "waiting for the migration target to apply the symbol configuration"
            );
            continue;
        }
        let Some((min_applied, max_log)) = observed_indices(target_obs, &target_ids) else {
            continue;
        };
        if max_log.saturating_sub(min_applied) > MEMBERSHIP_CATCH_UP_LAG {
            info!(
                symbol = %route.spec.symbol_ref,
                target = %deployment.name,
                min_applied,
                max_log,
                "waiting for migration target catch-up"
            );
            continue;
        }
        match post_json(
            &http,
            &process_url(source_shard, source, leader, "change-membership"),
            &json!(target_ids),
            options.observe_timeout,
        )
        .await
        {
            Ok(_) => {
                actions += 1;
                info!(
                    symbol = %route.spec.symbol_ref,
                    source = %source.name,
                    target = %deployment.name,
                    "changed membership to migration target voters"
                );
            }
            Err(error) => warn!(
                symbol = %route.spec.symbol_ref,
                target = %deployment.name,
                %error,
                "migration membership change failed"
            ),
        }
    }
    Ok(actions)
}

async fn finish_cutovers(
    client: &Client,
    view: &CrdView,
    observations: &Observations,
) -> Result<usize> {
    let mut actions = 0;
    for route in view.catalog.routes.values() {
        if route.spec.state != RouteState::Cutover {
            continue;
        }
        let Some(previous) = route.spec.previous_deployment_ref.as_deref() else {
            warn!(route = %route.name, "cutover route has no previousDeploymentRef");
            continue;
        };
        let (Some(source), Some(target)) = (
            view.catalog.deployments.get(previous),
            view.catalog
                .deployments
                .get(&route.spec.active_deployment_ref),
        ) else {
            continue;
        };
        let Some(target_obs) = observations.get(&target.name) else {
            continue;
        };
        if target_obs.leader_pod.is_none() {
            info!(target = %target.name, "waiting for the cutover target leader");
            continue;
        }
        if target.spec.desired_state != DeploymentState::Active {
            patch_spec(
                client,
                CrdKind::Deployment,
                &target.namespace,
                &target.name,
                json!({ "desiredState": "Active" }),
            )
            .await?;
            actions += 1;
        }
        if source.spec.desired_state != DeploymentState::Draining {
            patch_spec(
                client,
                CrdKind::Deployment,
                &source.namespace,
                &source.name,
                json!({ "desiredState": "Draining" }),
            )
            .await?;
            actions += 1;
        }
        patch_spec(
            client,
            CrdKind::Route,
            &route.namespace,
            &route.name,
            json!({ "state": "Active" }),
        )
        .await?;
        actions += 1;
        info!(
            symbol = %route.spec.symbol_ref,
            source = %source.name,
            target = %target.name,
            "migration cutover complete; source is draining"
        );
    }
    Ok(actions)
}

async fn rollback_migrations(
    client: &Client,
    view: &CrdView,
    observations: &Observations,
    options: &OperatorOptions,
) -> Result<usize> {
    let http = reqwest::Client::new();
    let mut actions = 0;
    for route in view.catalog.routes.values() {
        if route.spec.state != RouteState::Rollback {
            continue;
        }
        let Some(previous) = route.spec.previous_deployment_ref.as_deref() else {
            warn!(route = %route.name, "rollback requested without previousDeploymentRef");
            continue;
        };
        let (Some(source), Some(target)) = (
            view.catalog.deployments.get(previous),
            view.catalog
                .deployments
                .get(&route.spec.active_deployment_ref),
        ) else {
            continue;
        };
        let (Some(source_shard), Some(target_shard)) = (
            view.catalog.shards.get(&source.spec.shard_ref),
            view.catalog.shards.get(&target.spec.shard_ref),
        ) else {
            continue;
        };
        let source_ids = deployment_ids(source, source_shard.spec.replicas);
        let target_ids = deployment_ids(target, target_shard.spec.replicas);
        let source_set: BTreeSet<u64> = source_ids.iter().copied().collect();
        let target_set: BTreeSet<u64> = target_ids.iter().copied().collect();

        if source.spec.desired_state == DeploymentState::Disabled {
            patch_spec(
                client,
                CrdKind::Deployment,
                &source.namespace,
                &source.name,
                json!({
                    "desiredState": "Active"
                }),
            )
            .await?;
            actions += 1;
            info!(source = %source.name, "starting the rollback source deployment");
            continue;
        }
        let (Some(source_obs), Some(target_obs)) = (
            observations.get(&source.name),
            observations.get(&target.name),
        ) else {
            continue;
        };
        let source_voters: BTreeSet<u64> = source_obs.voters.iter().copied().collect();
        let target_voters: BTreeSet<u64> = target_obs.voters.iter().copied().collect();
        if source_voters == source_set && source_obs.leader_pod.is_some() {
            patch_spec(
                client,
                CrdKind::Route,
                &route.namespace,
                &route.name,
                json!({
                    "activeDeploymentRef": previous,
                    "previousDeploymentRef": null,
                    "state": "Active"
                }),
            )
            .await?;
            patch_spec(
                client,
                CrdKind::Deployment,
                &source.namespace,
                &source.name,
                json!({
                    "desiredState": "Active"
                }),
            )
            .await?;
            patch_spec(
                client,
                CrdKind::Deployment,
                &target.namespace,
                &target.name,
                json!({
                    "desiredState": "Disabled"
                }),
            )
            .await?;
            actions += 3;
            info!(
                symbol = %route.spec.symbol_ref,
                source = %source.name,
                target = %target.name,
                "migration rollback complete"
            );
            continue;
        }
        if target_voters == source_set {
            info!(
                symbol = %route.spec.symbol_ref,
                source = %source.name,
                "waiting for the rollback source group to elect a leader"
            );
            continue;
        }
        if !target_voters.is_empty() && target_voters != target_set {
            warn!(
                symbol = %route.spec.symbol_ref,
                target = %target.name,
                voters = ?target_voters,
                "target membership is not the target generation; refusing rollback"
            );
            continue;
        }
        let Some(leader) = target_obs.leader_pod else {
            continue;
        };
        let known: BTreeSet<u64> = target_obs
            .voters
            .iter()
            .chain(target_obs.learners.iter())
            .copied()
            .collect();
        let missing: Vec<u64> = source_set.difference(&known).copied().collect();
        if !missing.is_empty() {
            let endpoints = voter_endpoints(source_shard, source);
            for id in missing {
                let Some((_, address)) = endpoints.iter().find(|(candidate, _)| *candidate == id)
                else {
                    continue;
                };
                match post_json(
                    &http,
                    &process_url(target_shard, target, leader, "add-learner"),
                    &json!([id, address]),
                    options.observe_timeout,
                )
                .await
                {
                    Ok(_) => {
                        actions += 1;
                        info!(
                            symbol = %route.spec.symbol_ref,
                            target = %target.name,
                            node = id,
                            "added rollback source as a learner"
                        );
                    }
                    Err(error) => warn!(
                        symbol = %route.spec.symbol_ref,
                        node = id,
                        %error,
                        "add rollback learner failed"
                    ),
                }
            }
            continue;
        }
        let Some((min_applied, max_log)) = observed_indices(source_obs, &source_ids) else {
            continue;
        };
        if max_log.saturating_sub(min_applied) > MEMBERSHIP_CATCH_UP_LAG {
            info!(
                symbol = %route.spec.symbol_ref,
                source = %source.name,
                min_applied,
                max_log,
                "waiting for rollback source catch-up"
            );
            continue;
        }
        match post_json(
            &http,
            &process_url(target_shard, target, leader, "change-membership"),
            &json!(source_ids),
            options.observe_timeout,
        )
        .await
        {
            Ok(_) => {
                actions += 1;
                info!(
                    symbol = %route.spec.symbol_ref,
                    source = %source.name,
                    target = %target.name,
                    "changed membership back to rollback source voters"
                );
            }
            Err(error) => warn!(
                symbol = %route.spec.symbol_ref,
                %error,
                "rollback membership change failed"
            ),
        }
    }
    Ok(actions)
}

fn migration_source(catalog: &Catalog, deployment: &SymbolDeployment) -> bool {
    let previous = catalog.routes.values().any(|route| {
        route.spec.previous_deployment_ref.as_deref() == Some(deployment.name.as_str())
            && route.spec.active_deployment_ref != deployment.name
    });
    previous
        || catalog.deployments.values().any(|candidate| {
            candidate.spec.desired_state == DeploymentState::Shadow
                && candidate.spec.symbol_ref == deployment.spec.symbol_ref
                && candidate.spec.shard_ref != deployment.spec.shard_ref
        })
}

fn deployment_ids(deployment: &SymbolDeployment, replicas: u8) -> Vec<u64> {
    let offset = deployment.spec.raft_id_offset;
    (0..replicas)
        .map(|pod| offset.saturating_add(u64::from(pod)))
        .collect()
}

fn observed_indices(observation: &DeploymentObservation, ids: &[u64]) -> Option<(u64, u64)> {
    let wanted: BTreeSet<u64> = ids.iter().copied().collect();
    let mut min_applied = u64::MAX;
    let mut max_log = 0;
    let mut found = 0;
    for pod in &observation.pods {
        let id = pod.raft_id.unwrap_or(u64::from(pod.pod));
        if !wanted.contains(&id) {
            continue;
        }
        let applied = pod.applied_index?;
        let last = pod.last_log_index?;
        min_applied = min_applied.min(applied);
        max_log = max_log.max(last);
        found += 1;
    }
    (found == wanted.len()).then_some((min_applied, max_log))
}

async fn patch_spec(
    client: &Client,
    kind: CrdKind,
    namespace: &str,
    name: &str,
    spec: Value,
) -> Result<()> {
    let namespace = namespace_or_default(namespace);
    let api =
        Api::<DynamicObject>::namespaced_with(client.clone(), namespace, &kind.api_resource());
    api.patch(
        name,
        &PatchParams::default(),
        &Patch::Merge(json!({ "spec": spec })),
    )
    .await
    .with_context(|| format!("patch {:?} spec {namespace}/{name}", kind))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetentionAction {
    /// Mark the PVC for manual deletion instead of starting a timer.
    MarkManual,
    /// Start or continue the retention timer at this Unix-second deadline.
    MarkAt(u64),
    /// Still inside the window or intentionally retained.
    Keep,
    /// The window has elapsed; delete the PVC.
    Delete,
}

fn retention_action(annotation: Option<&str>, now: u64, window: u64) -> RetentionAction {
    match annotation {
        Some("manual") => RetentionAction::Keep,
        Some(value) => match value.parse::<u64>() {
            Ok(deadline) if now >= deadline => RetentionAction::Delete,
            Ok(_) => RetentionAction::Keep,
            Err(_) => RetentionAction::Keep,
        },
        None if window == 0 => RetentionAction::MarkManual,
        None => RetentionAction::MarkAt(now.saturating_add(window)),
    }
}

/// Applies the standard shard labels to PVCs created by the StatefulSet.
///
/// The PVC template deliberately carries no labels so an existing StatefulSet can be adopted
/// without an immutable `volumeClaimTemplates` update. The operator labels the PVCs after
/// creation; retention then has the durable `matching.io/shard` selector it needs.
async fn label_shard_pvcs(client: &Client, view: &CrdView) -> Result<usize> {
    let mut writes = 0;
    for shard in view.catalog.shards.values() {
        let namespace = namespace_or_default(&shard.namespace);
        let api = Api::<PersistentVolumeClaim>::namespaced(client.clone(), namespace);
        let cluster_id = view
            .catalog
            .cells
            .get(&shard.spec.cell)
            .map(|cell| cell.spec.cluster_id.clone())
            .unwrap_or_default();
        let desired = BTreeMap::from([
            (
                "app.kubernetes.io/name".to_string(),
                "match-engine".to_string(),
            ),
            ("app.kubernetes.io/instance".to_string(), shard.name.clone()),
            (
                "app.kubernetes.io/managed-by".to_string(),
                MANAGED_BY.to_string(),
            ),
            ("matching.io/shard".to_string(), shard.name.clone()),
            ("matching.io/cluster".to_string(), shard.name.clone()),
            ("matching.io/cell".to_string(), shard.spec.cell.clone()),
            ("matching.io/cluster-id".to_string(), cluster_id),
            (
                "matching.io/environment".to_string(),
                shard.spec.environment.clone(),
            ),
        ]);
        for pod in 0..shard.spec.replicas {
            let name = format!("data-{}-{pod}", shard.name);
            let pvc = match api.get(&name).await {
                Ok(pvc) => pvc,
                Err(kube::Error::Api(error)) if error.code == 404 => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("get PVC {namespace}/{name} for labeling"));
                }
            };
            let labels = pvc.metadata.labels.clone().unwrap_or_default();
            if desired
                .iter()
                .all(|(key, value)| labels.get(key) == Some(value))
            {
                continue;
            }
            let patch = json!({ "metadata": { "labels": &desired } });
            api.patch(&name, &PatchParams::default(), &Patch::Merge(patch))
                .await
                .with_context(|| format!("label PVC {namespace}/{name}"))?;
            writes += 1;
        }
    }
    Ok(writes)
}

/// Marks orphaned shard PVCs and deletes them after the configured retention window.
///
/// The PVC annotation is the durable retention record: deleting the `SymbolShard` CR removes the
/// only catalog pointer, but the PVCs remain labeled and carry their own deadline. A shard that
/// reappears clears the marker, so a transient list/read failure cannot delete live data.
async fn reconcile_retention(
    client: &Client,
    view: &CrdView,
    options: &OperatorOptions,
) -> Result<(usize, usize)> {
    let api = match options.namespace.as_deref() {
        Some(namespace) => Api::<PersistentVolumeClaim>::namespaced(client.clone(), namespace),
        None => Api::<PersistentVolumeClaim>::all(client.clone()),
    };
    let pvcs = api
        .list(&ListParams::default().labels("app.kubernetes.io/managed-by=match-engine-operator"))
        .await
        .context("list retained PVCs")?;
    let now = crate::store::now_epoch();
    let mut marks = 0;
    let mut deletes = 0;
    for pvc in pvcs.items {
        let Some(name) = pvc.metadata.name.as_deref() else {
            continue;
        };
        let Some(shard) = pvc
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get("matching.io/shard"))
        else {
            continue;
        };
        let namespace = pvc
            .metadata
            .namespace
            .as_deref()
            .unwrap_or(DEFAULT_NAMESPACE);
        let api = Api::<PersistentVolumeClaim>::namespaced(client.clone(), namespace);
        let annotations = pvc.metadata.annotations.as_ref();
        let retained = annotations.and_then(|values| values.get(RETAIN_UNTIL_ANNOTATION));

        if view.catalog.shards.contains_key(shard) {
            if retained.is_some() {
                let patch = json!({
                    "metadata": {
                        "annotations": {
                            RETAIN_UNTIL_ANNOTATION: null,
                            RETAINED_AT_ANNOTATION: null
                        }
                    }
                });
                api.patch(name, &PatchParams::default(), &Patch::Merge(patch))
                    .await
                    .with_context(|| format!("clear PVC retention on {namespace}/{name}"))?;
                marks += 1;
                info!(
                    namespace,
                    name, shard, "cancelled PVC retention because the shard exists again"
                );
            }
            continue;
        }
        if pvc.metadata.deletion_timestamp.is_some() {
            continue;
        }
        // Do not start the retention clock while the workload still exists. This
        // protects against a transient CR-list gap and lets the operator prune the
        // StatefulSet (and release its pods) before touching its PVCs.
        match Api::<StatefulSet>::namespaced(client.clone(), namespace)
            .get(shard)
            .await
        {
            Ok(_) => continue,
            Err(kube::Error::Api(error)) if error.code == 404 => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("get StatefulSet {namespace}/{shard} for retention"));
            }
        }

        match retention_action(retained.map(String::as_str), now, options.retention_seconds) {
            RetentionAction::Keep => {}
            RetentionAction::MarkManual => {
                let patch = json!({
                    "metadata": {
                        "annotations": {
                            RETAIN_UNTIL_ANNOTATION: "manual",
                            RETAINED_AT_ANNOTATION: now.to_string()
                        }
                    }
                });
                api.patch(name, &PatchParams::default(), &Patch::Merge(patch))
                    .await
                    .with_context(|| format!("mark PVC retention on {namespace}/{name}"))?;
                marks += 1;
                info!(
                    namespace,
                    name, shard, "marked orphaned PVC for manual retention"
                );
            }
            RetentionAction::MarkAt(deadline) => {
                let patch = json!({
                    "metadata": {
                        "annotations": {
                            RETAIN_UNTIL_ANNOTATION: deadline.to_string(),
                            RETAINED_AT_ANNOTATION: now.to_string()
                        }
                    }
                });
                api.patch(name, &PatchParams::default(), &Patch::Merge(patch))
                    .await
                    .with_context(|| format!("mark PVC retention on {namespace}/{name}"))?;
                marks += 1;
                info!(
                    namespace,
                    name,
                    shard,
                    retain_until = deadline,
                    "marked orphaned PVC for retention"
                );
            }
            RetentionAction::Delete => match api.delete(name, &DeleteParams::default()).await {
                Ok(_) => {
                    deletes += 1;
                    info!(
                        namespace,
                        name, shard, "deleted PVC after the retention window"
                    );
                }
                Err(kube::Error::Api(error)) if error.code == 404 => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("delete retained PVC {namespace}/{name}"));
                }
            },
        }
    }
    Ok((marks, deletes))
}

#[derive(Default)]
struct RolloutOutcome {
    actions: usize,
    release_blocks: BTreeMap<String, ReleaseBlock>,
}

async fn capacity_preflight(
    client: &Client,
    desired: &BTreeMap<WorkloadKey, Value>,
    catalog: &Catalog,
) -> Result<BTreeMap<String, CapacityBlock>> {
    let nodes = Api::<Node>::all(client.clone())
        .list(&ListParams::default())
        .await
        .context("list nodes for capacity preflight")?;
    let pods = Api::<Pod>::all(client.clone())
        .list(&ListParams::default())
        .await
        .context("list pods for capacity preflight")?;

    let mut allocatable = BTreeMap::<String, (u64, u64)>::new();
    for node in &nodes.items {
        if node.spec.as_ref().and_then(|spec| spec.unschedulable) == Some(true) {
            continue;
        }
        let Some(name) = node.metadata.name.as_deref() else {
            continue;
        };
        let Some(quantities) = node
            .status
            .as_ref()
            .and_then(|status| status.allocatable.as_ref())
        else {
            continue;
        };
        let cpu = quantities
            .get("cpu")
            .and_then(|quantity| parse_cpu_millis(&quantity.0).ok())
            .unwrap_or(0);
        let memory = quantities
            .get("memory")
            .and_then(|quantity| parse_memory_bytes(&quantity.0).ok())
            .unwrap_or(0);
        if cpu > 0 || memory > 0 {
            allocatable.insert(name.to_string(), (cpu, memory));
        }
    }
    if allocatable.is_empty() {
        return Ok(BTreeMap::new());
    }

    let mut node_used = BTreeMap::<String, (u64, u64)>::new();
    let mut shard_used = BTreeMap::<String, BTreeMap<String, (u64, u64)>>::new();
    for pod in &pods.items {
        if pod
            .status
            .as_ref()
            .and_then(|status| status.phase.as_deref())
            .is_some_and(|phase| phase == "Succeeded" || phase == "Failed")
        {
            continue;
        }
        let Some(node) = pod.spec.as_ref().and_then(|spec| spec.node_name.as_deref()) else {
            continue;
        };
        let (cpu, memory) = pod_requests(pod);
        if cpu == 0 && memory == 0 {
            continue;
        }
        let used = node_used.entry(node.to_string()).or_default();
        used.0 = used.0.saturating_add(cpu);
        used.1 = used.1.saturating_add(memory);
        if let Some(shard) = pod
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get("matching.io/cluster"))
        {
            let per_shard = shard_used.entry(shard.clone()).or_default();
            let shard_node = per_shard.entry(node.to_string()).or_default();
            shard_node.0 = shard_node.0.saturating_add(cpu);
            shard_node.1 = shard_node.1.saturating_add(memory);
        }
    }

    let blocks = capacity_blocks_for(desired, catalog, &allocatable, &node_used, &shard_used);
    if !blocks.is_empty() {
        warn!(
            shards = blocks.len(),
            "capacity preflight blocked StatefulSet apply"
        );
    }
    Ok(blocks)
}

fn capacity_blocks_for(
    desired: &BTreeMap<WorkloadKey, Value>,
    catalog: &Catalog,
    allocatable: &BTreeMap<String, (u64, u64)>,
    node_used: &BTreeMap<String, (u64, u64)>,
    shard_used: &BTreeMap<String, BTreeMap<String, (u64, u64)>>,
) -> BTreeMap<String, CapacityBlock> {
    let mut blocks = BTreeMap::new();
    for (key, object) in desired {
        if key.kind != WorkloadKind::StatefulSet {
            continue;
        }
        let Some(shard) = catalog.shards.get(&key.name) else {
            continue;
        };
        let (per_pod_cpu, per_pod_memory) = desired_pod_requests(object);
        if per_pod_cpu == 0 && per_pod_memory == 0 {
            continue;
        }
        let replicas = object
            .pointer("/spec/replicas")
            .and_then(Value::as_u64)
            .unwrap_or(u64::from(shard.spec.replicas));
        let cluster_cpu = per_pod_cpu.saturating_mul(replicas);
        let cluster_memory = per_pod_memory.saturating_mul(replicas);
        let empty = BTreeMap::new();
        let current = shard_used.get(&key.name).unwrap_or(&empty);

        let mut free_cpu = 0u64;
        let mut free_memory = 0u64;
        let mut max_free_cpu = 0u64;
        let mut max_free_memory = 0u64;
        for (node, (alloc_cpu, alloc_memory)) in allocatable {
            let used = node_used.get(node).copied().unwrap_or_default();
            let shard_used = current.get(node).copied().unwrap_or_default();
            let other_cpu = used.0.saturating_sub(shard_used.0);
            let other_memory = used.1.saturating_sub(shard_used.1);
            let available_cpu = alloc_cpu.saturating_sub(other_cpu);
            let available_memory = alloc_memory.saturating_sub(other_memory);
            free_cpu = free_cpu.saturating_add(available_cpu);
            free_memory = free_memory.saturating_add(available_memory);
            max_free_cpu = max_free_cpu.max(available_cpu);
            max_free_memory = max_free_memory.max(available_memory);
        }

        let block = |reason: &str, message: String| CapacityBlock {
            reason: reason.to_string(),
            message,
            per_pod_cpu_millis: per_pod_cpu,
            per_pod_memory_bytes: per_pod_memory,
            cluster_cpu_millis: cluster_cpu,
            cluster_memory_bytes: cluster_memory,
            free_cpu_millis: free_cpu,
            free_memory_bytes: free_memory,
        };
        if per_pod_cpu > max_free_cpu || per_pod_memory > max_free_memory {
            blocks.insert(
                key.name.clone(),
                block(
                    "pod_does_not_fit",
                    format!(
                        "one rendered pod requests {} CPU / {} memory, but the largest free node capacity is {} CPU / {} memory after excluding this shard",
                        format_cpu(per_pod_cpu),
                        format_bytes(per_pod_memory),
                        format_cpu(max_free_cpu),
                        format_bytes(max_free_memory)
                    ),
                ),
            );
            continue;
        }
        if cluster_cpu > free_cpu || cluster_memory > free_memory {
            blocks.insert(
                key.name.clone(),
                block(
                    "aggregate_capacity",
                    format!(
                        "{replicas} rendered pods request {} CPU / {} memory, but cluster free capacity is {} CPU / {} memory after excluding this shard",
                        format_cpu(cluster_cpu),
                        format_bytes(cluster_memory),
                        format_cpu(free_cpu),
                        format_bytes(free_memory)
                    ),
                ),
            );
        }
    }
    blocks
}

fn desired_pod_requests(object: &Value) -> (u64, u64) {
    object
        .pointer("/spec/template/spec/containers")
        .and_then(Value::as_array)
        .map(|containers| {
            containers
                .iter()
                .fold((0u64, 0u64), |(cpu, memory), container| {
                    let requests = container.pointer("/resources/requests");
                    (
                        cpu.saturating_add(
                            requests
                                .and_then(|requests| requests.get("cpu"))
                                .and_then(Value::as_str)
                                .and_then(|value| parse_cpu_millis(value).ok())
                                .unwrap_or(0),
                        ),
                        memory.saturating_add(
                            requests
                                .and_then(|requests| requests.get("memory"))
                                .and_then(Value::as_str)
                                .and_then(|value| parse_memory_bytes(value).ok())
                                .unwrap_or(0),
                        ),
                    )
                })
        })
        .unwrap_or_default()
}

fn pod_requests(pod: &Pod) -> (u64, u64) {
    pod.spec
        .as_ref()
        .map(|spec| {
            spec.containers
                .iter()
                .fold((0u64, 0u64), |(cpu, memory), container| {
                    let requests = container
                        .resources
                        .as_ref()
                        .and_then(|resources| resources.requests.as_ref());
                    (
                        cpu.saturating_add(
                            requests
                                .and_then(|requests| requests.get("cpu"))
                                .and_then(|quantity| parse_cpu_millis(&quantity.0).ok())
                                .unwrap_or(0),
                        ),
                        memory.saturating_add(
                            requests
                                .and_then(|requests| requests.get("memory"))
                                .and_then(|quantity| parse_memory_bytes(&quantity.0).ok())
                                .unwrap_or(0),
                        ),
                    )
                })
        })
        .unwrap_or_default()
}

async fn rollout_shards(
    client: &Client,
    catalog: &Catalog,
    observations: &Observations,
) -> Result<RolloutOutcome> {
    let http = reqwest::Client::new();
    let mut outcome = RolloutOutcome::default();
    for shard in catalog.shards.values() {
        if shard.spec.paused {
            continue;
        }
        let active: Vec<&SymbolDeployment> = catalog
            .shard_deployments(&shard.name)
            .into_iter()
            .filter(|deployment| deployment.spec.desired_state != DeploymentState::Disabled)
            .collect();
        if active.is_empty() {
            continue;
        }
        let namespace = namespace_or_default(&shard.namespace);
        let api = Api::<StatefulSet>::namespaced(client.clone(), namespace);
        let stateful_set = match api.get(&shard.name).await {
            Ok(stateful_set) => stateful_set,
            Err(kube::Error::Api(error)) if error.code == 404 => continue,
            Err(error) => return Err(error).context("get StatefulSet for rollout"),
        };
        let update_revision = stateful_set
            .status
            .as_ref()
            .and_then(|status| status.update_revision.clone());
        let current_revision = stateful_set
            .status
            .as_ref()
            .and_then(|status| status.current_revision.clone());
        let Some(update_revision) = update_revision else {
            continue;
        };
        if Some(&update_revision) == current_revision.as_ref() {
            continue;
        }
        let pods = Api::<Pod>::namespaced(client.clone(), namespace)
            .list(&ListParams::default().labels(&format!("matching.io/cluster={}", shard.name)))
            .await
            .with_context(|| format!("list pods for rollout {}", shard.name))?;
        if pods.items.is_empty() {
            continue;
        }
        let mut outdated: Vec<&Pod> = pods
            .items
            .iter()
            .filter(|pod| {
                pod.metadata.deletion_timestamp.is_none()
                    && pod
                        .metadata
                        .labels
                        .as_ref()
                        .and_then(|labels| labels.get("controller-revision-hash"))
                        .is_none_or(|revision| revision != &update_revision)
            })
            .collect();
        if outdated.is_empty() {
            continue;
        }
        if let Some(block) = release_block_for_pods(shard, &stateful_set, &pods.items) {
            warn!(
                shard = %shard.name,
                reason = %block.reason,
                message = %block.message,
                "containers rollout blocked by release compatibility"
            );
            outcome.release_blocks.insert(shard.name.clone(), block);
            continue;
        }
        let leader_pods: BTreeSet<&str> = active
            .iter()
            .filter_map(|deployment| observations.get(&deployment.name))
            .filter_map(|observation| observation.leader_pod_name.as_deref())
            .collect();
        // Replace pods that are not running first: they hold no live process and a Pending
        // old-revision pod otherwise blocks the StatefulSet from ever starting the new one.
        // Among running pods, prefer non-leaders and then higher ordinals (StatefulSet order).
        outdated.sort_by_key(|pod| {
            let running = pod_running(pod);
            let leader = pod
                .metadata
                .name
                .as_deref()
                .is_some_and(|name| leader_pods.contains(name));
            (
                running,
                leader,
                std::cmp::Reverse(pod_index(pod).unwrap_or(0)),
            )
        });
        let mut candidate = None;
        let mut transferred = 0;
        for pod in &outdated {
            let Some(name) = pod.metadata.name.as_deref() else {
                continue;
            };
            // A non-running pod holds no live engine, so replacing it cannot reduce a
            // healthy count. It must be replaced even when a newly added process has not
            // reached quorum anywhere yet; otherwise the add roll deadlocks on a Pending pod.
            if !pod_running(pod) {
                candidate = Some((pod, name));
                break;
            }
            if !processes_tolerate_removal(shard, &active, observations, pod) {
                continue;
            }
            if leader_pods.contains(name) {
                // Hand off only a pod whose removal is already quorum-safe; deleting it
                // happens on the next reconcile so the new leader can settle first.
                transferred += transfer_leaders(&http, shard, &active, observations, name).await;
                break;
            }
            candidate = Some((pod, name));
            break;
        }
        if transferred > 0 {
            outcome.actions += transferred;
        }
        let Some((_, candidate_name)) = candidate else {
            continue;
        };

        match Api::<Pod>::namespaced(client.clone(), namespace)
            .delete(candidate_name, &DeleteParams::default())
            .await
        {
            Ok(_) => {
                outcome.actions += 1;
                info!(
                    shard = %shard.name,
                    pod = %candidate_name,
                    revision = %update_revision,
                    "deleted one outdated pod for shard rollout"
                );
            }
            Err(kube::Error::Api(error)) if error.code == 404 => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("delete outdated pod {namespace}/{candidate_name}"));
            }
        }
    }
    Ok(outcome)
}

fn release_block_for_pods(
    shard: &SymbolShard,
    stateful_set: &StatefulSet,
    pods: &[Pod],
) -> Option<ReleaseBlock> {
    let target_images: BTreeMap<&str, &str> = stateful_set
        .spec
        .template
        .spec
        .as_ref()
        .map(|spec| {
            spec.containers
                .iter()
                .filter_map(|container| {
                    Some((
                        container.name.as_str(),
                        container
                            .image
                            .as_deref()
                            .filter(|image| !image.is_empty())?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut image_changed = false;
    for pod in pods
        .iter()
        .filter(|pod| pod.metadata.deletion_timestamp.is_none())
    {
        for container in pod
            .spec
            .as_ref()
            .map(|spec| spec.containers.as_slice())
            .unwrap_or_default()
        {
            if let Some(target_image) = target_images.get(container.name.as_str())
                && container.image.as_deref() != Some(*target_image)
            {
                image_changed = true;
            }
        }
    }
    if !image_changed {
        return None;
    }

    let target = &shard.spec.release;
    let mut current: Option<(u16, u16)> = None;
    let mut missing = false;
    let mut mixed = false;
    for pod in pods
        .iter()
        .filter(|pod| pod.metadata.deletion_timestamp.is_none())
    {
        let annotations = pod.metadata.annotations.as_ref();
        let wire = annotations
            .and_then(|annotations| annotations.get(RELEASE_WIRE_ANNOTATION))
            .and_then(|value| value.parse::<u16>().ok());
        let snapshot = annotations
            .and_then(|annotations| annotations.get(RELEASE_SNAPSHOT_ANNOTATION))
            .and_then(|value| value.parse::<u16>().ok());
        let (Some(wire), Some(snapshot)) = (wire, snapshot) else {
            missing = true;
            continue;
        };
        match current {
            Some(existing) if existing != (wire, snapshot) => mixed = true,
            None => current = Some((wire, snapshot)),
            Some(_) => {}
        }
    }

    let block = |reason: &str, message: String| ReleaseBlock {
        reason: reason.to_string(),
        message,
        current_wire_version: current.map(|(wire, _)| wire),
        current_snapshot_version: current.map(|(_, snapshot)| snapshot),
        target_wire_version: target.wire_version,
        target_snapshot_version: target.snapshot_version,
    };
    if missing {
        return Some(block(
            "release_metadata_missing",
            "one or more existing pods have no applied wire/snapshot annotations; cannot prove the image roll is compatible".to_string(),
        ));
    }
    if mixed {
        return Some(block(
            "mixed_release",
            "existing pods report more than one wire/snapshot release; inspect before rolling"
                .to_string(),
        ));
    }
    let (current_wire, current_snapshot) =
        current.unwrap_or((target.wire_version, target.snapshot_version));
    let current_release = Release {
        wire_version: current_wire,
        snapshot_version: current_snapshot,
        mixed_version_safe: false,
    };
    if !current_release.can_roll_to(target) {
        return Some(block(
            "stop_the_world_required",
            format!(
                "current wire v{current_wire}/snapshot v{current_snapshot} is incompatible with target wire v{}/snapshot v{}; a one-pod-at-a-time roll would mix incompatible peers",
                target.wire_version, target.snapshot_version
            ),
        ));
    }
    None
}

fn pod_running(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|status| status.phase.as_deref())
        .is_some_and(|phase| phase == "Running")
}

fn pod_index(pod: &Pod) -> Option<u8> {
    pod.metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get("apps.kubernetes.io/pod-index"))
        .and_then(|index| index.parse().ok())
        .or_else(|| {
            pod.metadata
                .name
                .as_deref()
                .and_then(|name| name.rsplit_once('-')?.1.parse().ok())
        })
}

fn pod_has_container(pod: &Pod, name: &str) -> bool {
    pod.spec.as_ref().is_some_and(|spec| {
        spec.containers
            .iter()
            .any(|container| container.name == name)
    })
}

/// Returns true when removing `pod` leaves a healthy quorum for every active process it hosts.
///
/// In containers mode, processes whose container is absent from the pod (for example a symbol
/// that is still being added) are unaffected by the removal. In supervisor mode every active
/// child is hosted by the supervisor container, so all of them must remain quorate. A process
/// that is already below quorum blocks the roll because replacing one of its remaining members
/// would make the loss worse.
fn processes_tolerate_removal(
    shard: &SymbolShard,
    active: &[&SymbolDeployment],
    observations: &Observations,
    pod: &Pod,
) -> bool {
    let quorum = usize::from(shard.spec.replicas) / 2 + 1;
    let supervisor = shard.spec.packing.mode == PackingMode::Supervisor;
    let Some(index) = pod_index(pod) else {
        return false;
    };
    for deployment in active {
        if !supervisor && !pod_has_container(pod, &deployment.spec.symbol_ref) {
            continue;
        }
        let Some(observation) = observations.get(&deployment.name) else {
            return false;
        };
        let healthy = observation.pods.iter().filter(|row| row.ready).count();
        let candidate_healthy = observation
            .pods
            .iter()
            .any(|row| row.pod == index && row.ready);
        let remaining = healthy.saturating_sub(usize::from(candidate_healthy));
        if remaining < quorum {
            info!(
                shard = %shard.name,
                deployment = %deployment.name,
                pod = index,
                healthy,
                remaining,
                quorum,
                "holding rollout until the process can survive the pod removal"
            );
            return false;
        }
    }
    true
}

async fn transfer_leaders(
    http: &reqwest::Client,
    shard: &SymbolShard,
    active: &[&SymbolDeployment],
    observations: &Observations,
    candidate_name: &str,
) -> usize {
    let mut transferred = 0;
    for deployment in active {
        let Some(observation) = observations.get(&deployment.name) else {
            continue;
        };
        if observation.leader_pod_name.as_deref() != Some(candidate_name) {
            continue;
        }
        let Some(follower) = observation
            .pods
            .iter()
            .find(|pod| pod.ready && format!("{}-{}", shard.name, pod.pod) != candidate_name)
            .map(|pod| pod.pod)
        else {
            warn!(
                shard = %shard.name,
                deployment = %deployment.name,
                pod = %candidate_name,
                "no ready follower for leader transfer; skipping rollout"
            );
            continue;
        };
        let leader = observation.leader_pod.unwrap_or(0);
        let url = process_url(
            shard,
            deployment,
            leader,
            "console/api/actions/transfer-leader",
        );
        match post_json_with_header(
            http,
            &url,
            &json!({ "target": follower }),
            Duration::from_millis(1_500),
            Some(("x-console-action", "1")),
        )
        .await
        {
            Ok(_) => {
                transferred += 1;
                info!(
                    shard = %shard.name,
                    deployment = %deployment.name,
                    from = leader,
                    to = follower,
                    "transferred leadership before rollout"
                );
            }
            Err(error) => {
                warn!(
                    shard = %shard.name,
                    deployment = %deployment.name,
                    %error,
                    "leader transfer failed; skipping rollout"
                );
            }
        }
    }
    transferred
}

fn deployment_status(
    existing: &DeploymentStatus,
    observation: Option<&DeploymentObservation>,
) -> DeploymentStatus {
    let mut status = existing.clone();
    let Some(observation) = observation else {
        status.phase = Phase::Pending;
        status.ready = false;
        status.leader_pod = None;
        status.leader_addr = None;
        status.conditions = vec![Condition::false_("Ready", "NoObservation")];
        return status;
    };
    status.phase = observation.phase;
    status.ready = observation.ready;
    status.leader_pod = observation.leader_pod_name.clone();
    status.leader_addr = observation.leader_addr.clone();
    status.applied_index = observation.applied_index;
    status.lag = observation.lag;
    if observation.phase == Phase::Disabled {
        status.leader_pod = None;
        status.leader_addr = None;
        status.conditions = vec![
            Condition::false_("Ready", "DeploymentDisabled"),
            Condition::false_("Configured", "DeploymentDisabled"),
        ];
        return status;
    }
    let leader = if observation.leader_pod.is_some() {
        Condition::true_("LeaderElected")
    } else {
        Condition::false_("LeaderElected", "NoLeader")
    };
    let configured = if observation.configured {
        Condition::true_("Configured")
    } else if observation.config_locked {
        Condition::false_("Configured", "LockedWithDifferentConfig")
    } else {
        Condition::false_("Configured", "Pending")
    };
    let ready = if observation.ready {
        Condition::true_("Ready")
    } else if observation.pods.iter().any(|pod| pod.ready) && !observation.configured {
        Condition::false_("Ready", "Configuring")
    } else if observation.pods.iter().any(|pod| pod.ready) {
        Condition::false_("Ready", "PodsNotReady")
    } else {
        Condition::false_("Ready", "PodsUnavailable")
    };
    status.conditions = vec![ready, leader, configured];
    status
}

fn route_status(
    existing: &RouteStatus,
    route: &SymbolRoute,
    deployment: &SymbolDeployment,
    deployment_status: &DeploymentStatus,
) -> RouteStatus {
    let mut status = existing.clone();
    status.observed_generation = u64::from(deployment.spec.generation);
    status.active_deployment_ref = route.spec.active_deployment_ref.clone();
    let configured = deployment_status
        .conditions
        .iter()
        .any(|condition| condition.kind == "Configured" && condition.status == "True");
    let ready = deployment_status.phase == Phase::Ready && configured;
    status.phase = if deployment.spec.desired_state == DeploymentState::Disabled {
        Phase::Disabled
    } else if ready {
        Phase::Ready
    } else if deployment_status.phase == Phase::Degraded {
        Phase::Degraded
    } else {
        Phase::Pending
    };
    status.leader_addr = if ready {
        deployment_status.leader_addr.clone()
    } else {
        None
    };
    status
}

async fn rollout_statuses(
    client: &Client,
    catalog: &Catalog,
) -> Result<BTreeMap<String, ShardRolloutStatus>> {
    let mut statuses = BTreeMap::new();
    for shard in catalog.shards.values() {
        let namespace = namespace_or_default(&shard.namespace);
        let api = Api::<StatefulSet>::namespaced(client.clone(), namespace);
        let stateful_set = match api.get(&shard.name).await {
            Ok(stateful_set) => stateful_set,
            Err(kube::Error::Api(error)) if error.code == 404 => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "get StatefulSet {namespace}/{} for rollout status",
                        shard.name
                    )
                });
            }
        };
        let updated = stateful_set
            .status
            .as_ref()
            .and_then(|status| status.updated_replicas)
            .unwrap_or(0);
        let ready = stateful_set
            .status
            .as_ref()
            .and_then(|status| status.ready_replicas)
            .unwrap_or(0);
        statuses.insert(
            shard.name.clone(),
            shard_rollout_status(
                u16::from(shard.spec.replicas),
                updated.max(0) as u32,
                ready.max(0) as u32,
            ),
        );
    }
    Ok(statuses)
}

fn shard_rollout_status(replicas: u16, updated: u32, ready: u32) -> ShardRolloutStatus {
    let replicas = u32::from(replicas);
    let updated = updated.min(replicas);
    let ready = ready.min(replicas);
    let percent = (updated * 100).checked_div(replicas).unwrap_or(100) as u8;
    ShardRolloutStatus {
        updated_replicas: updated,
        ready_replicas: ready,
        replicas,
        percent,
        complete: updated == replicas && ready == replicas,
    }
}

fn shard_status(
    shard: &SymbolShard,
    catalog: &Catalog,
    observations: &Observations,
    observed_generation: u64,
    rollout: Option<&ShardRolloutStatus>,
    release_block: Option<&ReleaseBlock>,
    capacity_block: Option<&CapacityBlock>,
) -> ShardStatus {
    let deployments = catalog.shard_deployments(&shard.name);
    let active: Vec<&SymbolDeployment> = deployments
        .iter()
        .copied()
        .filter(|deployment| deployment.spec.desired_state != DeploymentState::Disabled)
        .collect();
    let totals = per_pod_totals(shard, &active);
    let mut symbols: Vec<ShardSymbolStatus> = deployments
        .iter()
        .filter_map(|deployment| {
            let symbol = catalog.symbols.get(&deployment.spec.symbol_ref)?;
            let observation = observations.get(&deployment.name);
            Some(ShardSymbolStatus {
                name: symbol.name.clone(),
                deployment_ref: deployment.name.clone(),
                generation: deployment.spec.generation,
                slot: deployment.spec.placement.slot,
                http_port: deployment.spec.placement.http_port,
                leader_pod: observation.and_then(|observation| {
                    observation
                        .leader_pod
                        .map(|pod| format!("{}-{pod}", shard.name))
                }),
                leader_addr: observation.and_then(|observation| observation.leader_addr.clone()),
                ready: observation
                    .is_some_and(|observation| observation.ready && observation.configured),
                applied_index: observation.and_then(|observation| observation.applied_index),
                lag: observation.and_then(|observation| observation.lag),
                config_hash: desired_config_hash(deployment, symbol),
            })
        })
        .collect();
    symbols.sort_by_key(|symbol| symbol.slot);

    let nodes: Vec<ShardNodeStatus> = (0..shard.spec.replicas)
        .map(|pod| ShardNodeStatus {
            name: format!("{}-{pod}", shard.name),
            id: pod,
            ready: !active.is_empty()
                && active.iter().all(|deployment| {
                    observations
                        .get(&deployment.name)
                        .and_then(|observation| observation.pods.iter().find(|row| row.pod == pod))
                        .is_some_and(|row| row.ready)
                }),
            processes: active.len() as u16,
        })
        .collect();

    let ready_count = active
        .iter()
        .filter(|deployment| {
            observations
                .get(&deployment.name)
                .is_some_and(|observation| observation.ready && observation.configured)
        })
        .count();
    let any_pod_ready = active.iter().any(|deployment| {
        observations
            .get(&deployment.name)
            .is_some_and(|observation| observation.pods.iter().any(|pod| pod.ready))
    });
    let rollout_active = rollout.is_some_and(|rollout| !rollout.complete);
    let phase = if active.is_empty() {
        Phase::Reserved
    } else if ready_count == active.len() {
        Phase::Ready
    } else if rollout_active && any_pod_ready {
        Phase::Progressing
    } else if ready_count > 0 || any_pod_ready {
        Phase::Degraded
    } else {
        Phase::Pending
    };
    let ready = if phase == Phase::Ready {
        Condition::true_("Ready")
    } else {
        Condition::false_(
            "Ready",
            if phase == Phase::Reserved {
                "Reserved"
            } else {
                "PodsNotReady"
            },
        )
    };
    let progress = if release_block.is_some() {
        Condition::false_("Progressing", "ReleaseBlocked")
    } else if capacity_block.is_some() {
        Condition::false_("Progressing", "CapacityBlocked")
    } else if phase == Phase::Ready && rollout.is_none_or(|rollout| rollout.complete) {
        Condition::false_("Progressing", "Settled")
    } else {
        Condition::true_("Progressing")
    };
    let release = match release_block {
        Some(block) => Condition {
            kind: "ReleaseCompatible".to_string(),
            status: "False".to_string(),
            reason: Some(block.reason.clone()),
            message: Some(block.message.clone()),
        },
        None => Condition::true_("ReleaseCompatible"),
    };
    let capacity = match capacity_block {
        Some(block) => Condition {
            kind: "CapacityAvailable".to_string(),
            status: "False".to_string(),
            reason: Some(block.reason.clone()),
            message: Some(block.message.clone()),
        },
        None => Condition::true_("CapacityAvailable"),
    };
    ShardStatus {
        observed_generation,
        phase,
        members_per_process: shard.spec.replicas,
        capacity: crate::model::ShardCapacity {
            processes: totals.processes as u16,
            cpu_request: format_cpu(totals.cpu_request_millis),
            memory_request: format_bytes(totals.memory_request_bytes),
            storage_request: shard.spec.storage.size.clone(),
        },
        symbols,
        nodes,
        rollout: rollout.cloned(),
        release_block: release_block.cloned(),
        capacity_block: capacity_block.cloned(),
        conditions: vec![ready, progress, release, capacity],
    }
}

fn symbol_status(
    existing: &TradingSymbolStatus,
    deployment: Option<&SymbolDeployment>,
    deployment_status: Option<&DeploymentStatus>,
) -> TradingSymbolStatus {
    let mut status = existing.clone();
    if let (Some(deployment), Some(deployment_status)) = (deployment, deployment_status) {
        status.phase = if deployment.spec.desired_state == DeploymentState::Disabled {
            Phase::Disabled
        } else {
            deployment_status.phase
        };
        status.config_hash = deployment.spec.config_hash.clone();
        status.applied_by = "operator".to_string();
    } else {
        status.phase = Phase::Pending;
    }
    status
}

fn market_data_status(
    service: &MarketDataService,
    catalog: &Catalog,
    observation: Option<&MarketDataObservation>,
    render_failed: bool,
) -> crate::model::MarketDataServiceStatus {
    use crate::model::MarketDataServiceStatus;

    let observation = observation.cloned().unwrap_or_default();
    // HPA owns the live replica count; `spec.replicas` is only the upper bound
    // when `autoscaling.maxReplicas` is unset. Use the Deployment's observed
    // desired count so scaling to the HPA minimum of one reads as Ready.
    let desired = observation.replicas.max(if service.spec.paused {
        0
    } else {
        service.spec.min_replicas()
    });
    let image_pinned = service.spec.image.contains("@sha256:");
    let release_compatible = service.spec.symbol_names().iter().all(|name| {
        catalog
            .deployments
            .values()
            .find(|deployment| deployment.spec.symbol_ref == *name)
            .and_then(|deployment| catalog.shards.get(&deployment.spec.shard_ref))
            .is_none_or(|shard| {
                shard.spec.release.wire_version == service.spec.release.wire_version
            })
    });
    let all_symbols_ready = observation.feed_ready;
    let ready_replicas = observation.ready_replicas;
    let mut conditions = Vec::new();
    conditions.push(if ready_replicas > 0 || observation.available {
        Condition::true_("Available")
    } else {
        Condition::false_("DeploymentUnavailable", "NoReadyReplicas")
    });
    conditions.push(if all_symbols_ready {
        Condition::true_("FeedReady")
    } else {
        Condition::false_("FeedNotReady", "SymbolFeedUnavailable")
    });
    conditions.push(if image_pinned {
        Condition::true_("ImagePinned")
    } else {
        Condition::false_("MutableImageTag", "ImageTagNotDigest")
    });
    conditions.push(if release_compatible {
        Condition::true_("ReleaseCompatible")
    } else {
        Condition::false_("IncompatibleRelease", "WireVersionMismatch")
    });
    if service.spec.paused {
        conditions.push(Condition::true_("Paused"));
    }

    let phase = if service.spec.paused {
        Phase::Disabled
    } else if render_failed || !release_compatible {
        Phase::Failed
    } else if ready_replicas == 0 {
        if observation.replicas > 0 {
            Phase::Degraded
        } else {
            Phase::Pending
        }
    } else if ready_replicas < desired {
        Phase::Progressing
    } else if !all_symbols_ready {
        Phase::Degraded
    } else {
        Phase::Ready
    };
    let message = if service.spec.paused {
        "paused by spec".to_string()
    } else if render_failed {
        "render rejected; see operator logs for the validation error".to_string()
    } else if !release_compatible {
        "market-data release does not match every engine shard wire version".to_string()
    } else if let Some(error) = observation.error.as_deref() {
        error.to_string()
    } else if !observation.message.is_empty() {
        observation.message.clone()
    } else {
        format!(
            "{ready_replicas}/{desired} replicas ready; {} symbols ready",
            observation
                .symbols
                .iter()
                .filter(|symbol| symbol.ready)
                .count()
        )
    };

    MarketDataServiceStatus {
        phase,
        conditions,
        replicas: observation.replicas,
        ready_replicas,
        symbols: observation.symbols,
        message,
    }
}

async fn observe_order_systems(
    client: &Client,
    catalog: &Catalog,
) -> BTreeMap<String, OrderSystemStatus> {
    let mut statuses = BTreeMap::new();
    for system in catalog.order_systems.values() {
        let namespace = namespace_or_default(&system.namespace);
        let mut status = OrderSystemStatus {
            phase: if system.spec.paused {
                Phase::Disabled
            } else {
                Phase::Pending
            },
            message: String::new(),
            ..OrderSystemStatus::default()
        };
        let mut desired = 0u16;
        let mut ready = 0u16;
        let mut pending = false;
        let mut errors = Vec::new();
        for shard in &system.spec.shards {
            let name = dns_label(&format!("{}-{}", system.name, shard.name));
            let api = Api::<Deployment>::namespaced(client.clone(), namespace);
            let observation = match api.get(&name).await {
                Ok(deployment) => {
                    let shard_status = deployment.status.unwrap_or_default();
                    let replicas = shard_status.replicas.unwrap_or(0).max(0) as u16;
                    let ready_replicas = shard_status.ready_replicas.unwrap_or(0).max(0) as u16;
                    desired = desired.saturating_add(replicas);
                    ready = ready.saturating_add(ready_replicas);
                    OrderSystemShardStatus {
                        name: shard.name.clone(),
                        replicas,
                        ready_replicas,
                    }
                }
                Err(kube::Error::Api(error)) if error.code == 404 => {
                    pending = true;
                    OrderSystemShardStatus {
                        name: shard.name.clone(),
                        replicas: 0,
                        ready_replicas: 0,
                    }
                }
                Err(error) => {
                    errors.push(format!("{name}: {error}"));
                    OrderSystemShardStatus {
                        name: shard.name.clone(),
                        replicas: 0,
                        ready_replicas: 0,
                    }
                }
            };
            status.shards.push(observation);
        }
        status.replicas = desired;
        status.ready_replicas = ready;
        status.phase = if system.spec.paused {
            Phase::Disabled
        } else if !errors.is_empty() {
            Phase::Degraded
        } else if pending || desired == 0 {
            Phase::Pending
        } else if ready == desired {
            Phase::Ready
        } else if ready > 0 {
            Phase::Progressing
        } else {
            Phase::Degraded
        };
        status.conditions = vec![Condition {
            kind: "Available".to_string(),
            status: if status.phase == Phase::Ready {
                "True"
            } else {
                "False"
            }
            .to_string(),
            reason: (status.phase != Phase::Ready).then(|| "ReplicasNotReady".to_string()),
            message: Some(format!("{ready}/{desired} shard replicas ready")),
        }];
        status.message = if system.spec.paused {
            "paused by spec".to_string()
        } else if !errors.is_empty() {
            errors.join("; ")
        } else {
            format!("{ready}/{desired} shard replicas ready")
        };
        statuses.insert(system.name.clone(), status);
    }
    statuses
}

async fn write_order_system_statuses(
    client: &Client,
    view: &CrdView,
    statuses: &BTreeMap<String, OrderSystemStatus>,
    failed: &BTreeSet<String>,
) -> Result<usize> {
    let mut writes = 0;
    for system in view.catalog.order_systems.values() {
        let mut status = statuses.get(&system.name).cloned().unwrap_or_default();
        if failed.contains(&system.name) {
            status.phase = Phase::Failed;
            status.message =
                "render rejected; see operator logs for the validation error".to_string();
        }
        writes += write_status(
            client,
            view,
            CrdKind::OrderSystem,
            &system.namespace,
            &system.name,
            &status,
        )
        .await?;
    }
    Ok(writes)
}

async fn write_market_data_statuses(
    client: &Client,
    view: &CrdView,
    observations: &BTreeMap<String, MarketDataObservation>,
    failed: &BTreeSet<String>,
) -> Result<usize> {
    let mut writes = 0;
    for service in view.catalog.market_data_services.values() {
        let status = market_data_status(
            service,
            &view.catalog,
            observations.get(&service.name),
            failed.contains(&service.name),
        );
        writes += write_status(
            client,
            view,
            CrdKind::MarketData,
            &service.namespace,
            &service.name,
            &status,
        )
        .await?;
    }
    Ok(writes)
}

async fn write_statuses(
    client: &Client,
    view: &CrdView,
    observations: &Observations,
    release_blocks: &BTreeMap<String, ReleaseBlock>,
    capacity_blocks: &BTreeMap<String, CapacityBlock>,
) -> Result<usize> {
    let catalog = &view.catalog;
    let rollouts = rollout_statuses(client, catalog).await?;
    let mut writes = 0;
    for shard in catalog.shards.values() {
        let generation = view
            .object(CrdKind::Shard, &shard.namespace, &shard.name)
            .and_then(|object| object.metadata.generation)
            .unwrap_or(0) as u64;
        let status = shard_status(
            shard,
            catalog,
            observations,
            generation,
            rollouts.get(&shard.name),
            release_blocks.get(&shard.name),
            capacity_blocks.get(&shard.name),
        );
        writes += write_status(
            client,
            view,
            CrdKind::Shard,
            &shard.namespace,
            &shard.name,
            &status,
        )
        .await?;

        for deployment in catalog.shard_deployments(&shard.name) {
            let dstatus = deployment_status(&deployment.status, observations.get(&deployment.name));
            writes += write_status(
                client,
                view,
                CrdKind::Deployment,
                &deployment.namespace,
                &deployment.name,
                &dstatus,
            )
            .await?;
        }
    }

    for route in catalog.routes.values() {
        let deployment = catalog.deployments.get(&route.spec.active_deployment_ref);
        let Some(deployment) = deployment else {
            continue;
        };
        let dstatus = deployment_status(&deployment.status, observations.get(&deployment.name));
        let status = route_status(&route.status, route, deployment, &dstatus);
        writes += write_status(
            client,
            view,
            CrdKind::Route,
            &route.namespace,
            &route.name,
            &status,
        )
        .await?;
    }

    for symbol in catalog.symbols.values() {
        let deployment = catalog
            .deployments
            .values()
            .find(|deployment| deployment.spec.symbol_ref == symbol.name);
        let dstatus = deployment.map(|deployment| {
            deployment_status(&deployment.status, observations.get(&deployment.name))
        });
        let status = symbol_status(&symbol.status, deployment, dstatus.as_ref());
        writes += write_status(
            client,
            view,
            CrdKind::Symbol,
            &symbol.namespace,
            &symbol.name,
            &status,
        )
        .await?;
    }
    Ok(writes)
}

async fn write_status<T: serde::Serialize>(
    client: &Client,
    view: &CrdView,
    kind: CrdKind,
    namespace: &str,
    name: &str,
    status: &T,
) -> Result<usize> {
    let namespace = namespace_or_default(namespace);
    let Some(object) = view.object(kind, namespace, name) else {
        return Ok(0);
    };
    let value = serde_json::to_value(status).context("encode status")?;
    if object.data.get("status") == Some(&value) {
        return Ok(0);
    }
    let mut updated = object.clone();
    updated.data["status"] = value;
    let api =
        Api::<DynamicObject>::namespaced_with(client.clone(), namespace, &kind.api_resource());
    match api
        .replace_status(name, &PostParams::default(), &updated)
        .await
    {
        Ok(_) => Ok(1),
        Err(kube::Error::Api(error)) if error.code == 409 => {
            // A spec patch in this reconcile changed the resourceVersion; the next
            // reconcile writes the status against the fresh object.
            debug!(
                ?kind,
                namespace,
                name,
                "status update conflicts with a spec change; retrying next reconcile"
            );
            Ok(0)
        }
        Err(error) => {
            warn!(?kind, namespace, name, %error, "status update failed");
            Ok(0)
        }
    }
}

fn leader_label(leader_pod: Option<&str>, pod_name: &str) -> Option<&'static str> {
    if leader_pod == Some(pod_name) {
        Some("leader")
    } else {
        None
    }
}

async fn label_leaders(
    client: &Client,
    catalog: &Catalog,
    observations: &Observations,
) -> Result<()> {
    for shard in catalog.shards.values() {
        if shard.spec.class != ShardClass::Dedicated {
            continue;
        }
        let deployments = catalog.shard_deployments(&shard.name);
        let leader_pod = deployments
            .first()
            .and_then(|deployment| observations.get(&deployment.name))
            .and_then(|observation| observation.leader_pod)
            .map(|pod| format!("{}-{pod}", shard.name));
        let namespace = namespace_or_default(&shard.namespace);
        let api = Api::<Pod>::namespaced(client.clone(), namespace);
        let pods = api
            .list(&ListParams::default().labels(&format!("matching.io/cluster={}", shard.name)))
            .await
            .with_context(|| format!("list pods for shard {}", shard.name))?;
        for pod in pods.items {
            let Some(name) = pod.metadata.name.clone() else {
                continue;
            };
            let desired = leader_label(leader_pod.as_deref(), &name);
            let current = pod
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get(LEADER_LABEL))
                .map(String::as_str);
            if current == desired {
                continue;
            }
            let patch = json!({
                "metadata": {
                    "labels": {
                        "matching.io/role": desired
                    }
                }
            });
            api.patch(&name, &PatchParams::default(), &Patch::Merge(patch))
                .await
                .with_context(|| format!("patch leader label on pod {namespace}/{name}"))?;
            debug!(namespace, name, ?desired, "updated leader label");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ShardNodeStatus, SymbolDeployment};
    use crate::store::demo_catalog;

    fn observation(ready: bool, leader_pod: Option<u8>) -> DeploymentObservation {
        DeploymentObservation {
            pods: Vec::new(),
            leader_pod,
            leader_pod_name: leader_pod.map(|pod| format!("eurusd-{pod}")),
            leader_addr: leader_pod.map(|pod| format!("eurusd-{pod}:21001")),
            ready,
            phase: if ready { Phase::Ready } else { Phase::Pending },
            applied_index: Some(10),
            lag: Some(0),
            voters: vec![0, 1, 2, 3],
            learners: Vec::new(),
            configured: true,
            config_locked: false,
        }
    }

    #[test]
    fn deployment_status_ready_and_route_publish() {
        let catalog = demo_catalog();
        let deployment = catalog.deployments["eurusd-g1"].clone();
        let route = catalog.routes["eurusd"].clone();
        let status = deployment_status(&deployment.status, Some(&observation(true, Some(0))));
        assert_eq!(status.phase, Phase::Ready);
        assert!(status.ready);
        assert_eq!(status.leader_pod.as_deref(), Some("eurusd-0"));
        let route_status = route_status(&route.status, &route, &deployment, &status);
        assert_eq!(route_status.phase, Phase::Ready);
        assert_eq!(route_status.leader_addr.as_deref(), Some("eurusd-0:21001"));
        assert_eq!(route_status.observed_generation, 1);
    }

    #[test]
    fn deployment_status_is_degraded_when_some_pods_report() {
        let catalog = demo_catalog();
        let deployment = catalog.deployments["eurusd-g1"].clone();
        let mut observation = observation(false, None);
        observation.phase = Phase::Degraded;
        observation.pods = vec![PodObservation {
            pod: 0,
            ready: true,
            healthy: true,
            ..PodObservation::default()
        }];
        let status = deployment_status(&deployment.status, Some(&observation));
        assert_eq!(status.phase, Phase::Degraded);
        assert!(!status.ready);
        assert!(
            status
                .conditions
                .iter()
                .any(|condition| condition.kind == "Ready")
        );
    }

    #[test]
    fn disabled_deployment_has_no_leader_route() {
        let catalog = demo_catalog();
        let mut deployment = catalog.deployments["eurusd-g1"].clone();
        deployment.spec.desired_state = DeploymentState::Disabled;
        let route = catalog.routes["eurusd"].clone();
        let observation = DeploymentObservation {
            phase: Phase::Disabled,
            ..DeploymentObservation::default()
        };
        let dstatus = deployment_status(&deployment.status, Some(&observation));
        assert_eq!(dstatus.phase, Phase::Disabled);
        assert!(dstatus.leader_addr.is_none());
        let rstatus = route_status(&route.status, &route, &deployment, &dstatus);
        assert_eq!(rstatus.phase, Phase::Disabled);
        assert!(rstatus.leader_addr.is_none());
    }

    #[test]
    fn shard_status_lists_symbols_and_nodes() {
        let catalog = demo_catalog();
        let shard = catalog.shards["fx-eu-01"].clone();
        let observations = Observations::from([
            ("eurusd-g1".to_string(), observation(true, Some(0))),
            ("gbpusd-g1".to_string(), observation(true, Some(1))),
        ]);
        let status = shard_status(&shard, &catalog, &observations, 3, None, None, None);
        assert_eq!(status.observed_generation, 3);
        assert_eq!(status.phase, Phase::Ready);
        assert_eq!(status.members_per_process, 4);
        assert_eq!(status.symbols.len(), 2);
        assert_eq!(status.capacity.processes, 2);
        assert_eq!(
            status.nodes.iter().filter(|node| node.ready).count(),
            0,
            "no per-pod rows in the synthetic observation"
        );
        assert_eq!(status.nodes.len(), 4);
    }

    #[test]
    fn rollout_percentage_tracks_updated_pods_and_readiness() {
        let half = shard_rollout_status(4, 2, 2);
        assert_eq!(half.percent, 50);
        assert_eq!(half.updated_replicas, 2);
        assert!(!half.complete);

        let finalizing = shard_rollout_status(4, 4, 3);
        assert_eq!(finalizing.percent, 100);
        assert_eq!(finalizing.ready_replicas, 3);
        assert!(!finalizing.complete);

        let done = shard_rollout_status(4, 4, 4);
        assert_eq!(done.percent, 100);
        assert!(done.complete);

        let reserved = shard_rollout_status(0, 0, 0);
        assert_eq!(reserved.percent, 100);
        assert!(reserved.complete);
    }

    #[test]
    fn leader_label_only_marks_the_leader_pod() {
        assert_eq!(
            leader_label(Some("me-btcusdt-2"), "me-btcusdt-2"),
            Some("leader")
        );
        assert_eq!(leader_label(Some("me-btcusdt-2"), "me-btcusdt-1"), None);
        assert_eq!(leader_label(None, "me-btcusdt-2"), None);
    }

    #[test]
    fn shard_rollout_reports_progressing_while_not_all_pods_are_ready() {
        let catalog = demo_catalog();
        let shard = catalog.shards["fx-eu-01"].clone();
        let mut observations = Observations::new();
        for name in ["eurusd-g1", "gbpusd-g1"] {
            let deployment = catalog.deployments[name].clone();
            let symbol = catalog.symbols[&deployment.spec.symbol_ref].clone();
            let desired = desired_symbol_config(&symbol);
            let pods = (0..shard.spec.replicas)
                .map(|pod| {
                    let up = pod != 3;
                    PodObservation {
                        pod,
                        healthy: up,
                        ready: up,
                        is_leader: pod == 0,
                        leader_id: Some(0),
                        applied_index: Some(10),
                        last_log_index: Some(10),
                        symbol_config: Some(desired.clone()),
                        error: None,
                        ..PodObservation::default()
                    }
                })
                .collect();
            observations.insert(
                name.to_string(),
                deployment_observation(&shard, &deployment, pods, Some(&desired)),
            );
        }

        let rolling = shard_rollout_status(4, 2, 2);
        let status = shard_status(
            &shard,
            &catalog,
            &observations,
            1,
            Some(&rolling),
            None,
            None,
        );
        assert_eq!(status.phase, Phase::Progressing);
        assert!(
            status
                .conditions
                .iter()
                .any(|condition| { condition.kind == "Progressing" && condition.status == "True" })
        );

        let status = shard_status(&shard, &catalog, &observations, 1, None, None, None);
        assert_eq!(status.phase, Phase::Degraded);
    }

    #[test]
    fn shard_nodes_are_ready_when_every_process_reports() {
        let catalog = demo_catalog();
        let shard = catalog.shards["fx-eu-01"].clone();
        let mut observations = Observations::new();
        for name in ["eurusd-g1", "gbpusd-g1"] {
            let deployment = catalog.deployments[name].clone();
            let symbol = catalog.symbols[&deployment.spec.symbol_ref].clone();
            let desired = desired_symbol_config(&symbol);
            let pods = (0..shard.spec.replicas)
                .map(|pod| PodObservation {
                    pod,
                    healthy: true,
                    ready: true,
                    is_leader: pod == 0,
                    leader_id: Some(0),
                    applied_index: Some(10),
                    last_log_index: Some(10),
                    symbol_config: Some(desired.clone()),
                    error: None,
                    ..PodObservation::default()
                })
                .collect();
            observations.insert(
                name.to_string(),
                deployment_observation(&shard, &deployment, pods, Some(&desired)),
            );
        }
        let status = shard_status(&shard, &catalog, &observations, 1, None, None, None);
        assert_eq!(status.phase, Phase::Ready);
        assert_eq!(status.nodes.len(), 4);
        assert!(status.nodes.iter().all(|node| node.ready));
        assert_eq!(status.nodes[0].processes, 2);

        let rolling = shard_rollout_status(4, 2, 2);
        let status = shard_status(
            &shard,
            &catalog,
            &observations,
            1,
            Some(&rolling),
            None,
            None,
        );
        assert_eq!(status.phase, Phase::Ready);
        assert!(
            status
                .conditions
                .iter()
                .any(|condition| condition.kind == "Progressing" && condition.status == "True"),
            "an active engines-ready shard is still progressing while pods update"
        );

        let done = shard_rollout_status(4, 4, 4);
        let status = shard_status(&shard, &catalog, &observations, 1, Some(&done), None, None);
        assert!(
            status
                .conditions
                .iter()
                .any(|condition| condition.kind == "Progressing" && condition.status == "False"),
            "the rollout settles only when every pod is updated and ready"
        );
    }

    #[test]
    fn migration_ids_and_catch_up_use_raft_ids() {
        let catalog = demo_catalog();
        let mut deployment = catalog.deployments["eurusd-g1"].clone();
        deployment.spec.raft_id_offset = 4;
        assert_eq!(deployment_ids(&deployment, 4), vec![4, 5, 6, 7]);

        let observation = DeploymentObservation {
            pods: (0..4)
                .map(|pod| PodObservation {
                    pod,
                    raft_id: Some(4 + u64::from(pod)),
                    applied_index: Some(10),
                    last_log_index: Some(10),
                    ..PodObservation::default()
                })
                .collect(),
            ..DeploymentObservation::default()
        };
        assert_eq!(
            observed_indices(&observation, &[4, 5, 6, 7]),
            Some((10, 10))
        );
        let mut lagging = observation.clone();
        lagging.pods[0].applied_index = Some(1);
        assert_eq!(observed_indices(&lagging, &[4, 5, 6, 7]), Some((1, 10)));
        assert_eq!(observed_indices(&observation, &[4, 5, 8]), None);
    }

    #[test]
    fn retention_window_marks_then_deletes() {
        assert_eq!(retention_action(None, 100, 0), RetentionAction::MarkManual);
        assert_eq!(
            retention_action(None, 100, 60),
            RetentionAction::MarkAt(160)
        );
        assert_eq!(retention_action(Some("160"), 100, 0), RetentionAction::Keep);
        assert_eq!(
            retention_action(Some("100"), 100, 0),
            RetentionAction::Delete
        );
        assert_eq!(
            retention_action(Some("manual"), 100, 60),
            RetentionAction::Keep
        );
        assert_eq!(
            retention_action(Some("bad"), 100, 60),
            RetentionAction::Keep
        );
    }

    #[test]
    fn operator_options_reject_bad_lease_intervals() {
        let base = OperatorOptions {
            profile: Profile::Local,
            resync: Duration::from_secs(10),
            observe_timeout: Duration::from_millis(500),
            lease_namespace: "match-control".to_string(),
            lease_name: "operator".to_string(),
            identity: "test".to_string(),
            lease_duration: Duration::from_secs(15),
            lease_renew: Duration::from_secs(5),
            namespace: None,
            retention_seconds: 0,
            dry_run: false,
        };
        assert!(base.validate().is_ok());
        let bad = OperatorOptions {
            lease_renew: base.lease_duration,
            ..base
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn lease_expiry_uses_renew_time_and_duration() {
        let spec = LeaseSpec {
            renew_time: Some(MicroTime(Timestamp::now())),
            lease_duration_seconds: Some(15),
            ..LeaseSpec::default()
        };
        let now = Timestamp::now().as_second();
        assert!(!lease_expired(&spec, now));
        assert!(lease_expired(&spec, now + 16));
        assert!(lease_expired(&LeaseSpec::default(), now));
    }

    #[test]
    fn workload_key_reads_rendered_metadata() {
        let catalog = demo_catalog();
        let objects = render_shard(
            &catalog,
            "fx-eu-01",
            &RenderOptions {
                profile: Profile::Local,
                namespace: Some("match-eu-1".to_string()),
                non_root: Some(true),
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let keys: Vec<WorkloadKey> = objects
            .iter()
            .map(workload_key)
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(keys.iter().any(|key| key.kind == WorkloadKind::StatefulSet));
        assert!(keys.iter().any(|key| key.kind == WorkloadKind::ConfigMap));
    }

    #[test]
    fn dedicated_shard_leader_name_matches_pod_ordinal() {
        let pod = ShardNodeStatus {
            name: "me-btcusdt-2".to_string(),
            id: 2,
            ready: true,
            processes: 1,
        };
        assert_eq!(pod.name, format!("me-btcusdt-{}", pod.id));
    }

    #[test]
    fn only_validation_errors_trigger_the_statefulset_merge_patch_fallback() {
        let invalid = kube::Error::Api(Box::new(kube::core::Status {
            message: "StatefulSet.apps \"fx-eu-01\" is invalid".to_string(),
            reason: "Invalid".to_string(),
            code: 422,
            ..Default::default()
        }));
        assert!(is_validation_error(&invalid));
        let conflict = kube::Error::Api(Box::new(kube::core::Status {
            message: "conflict".to_string(),
            reason: "Conflict".to_string(),
            code: 409,
            ..Default::default()
        }));
        assert!(!is_validation_error(&conflict));
    }

    #[test]
    fn deployed_shard_has_one_active_deployment() {
        let catalog = demo_catalog();
        let deployments: Vec<&SymbolDeployment> = catalog.shard_deployments("me-btcusdt");
        assert_eq!(deployments.len(), 1);
    }

    fn release_stateful_set(image: &str) -> StatefulSet {
        StatefulSet {
            spec: k8s_openapi::api::apps::v1::StatefulSetSpec {
                selector: k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector::default(),
                service_name: Some("fx-eu-01-headless".to_string()),
                template: k8s_openapi::api::core::v1::PodTemplateSpec {
                    spec: Some(k8s_openapi::api::core::v1::PodSpec {
                        containers: vec![k8s_openapi::api::core::v1::Container {
                            name: "eurusd".to_string(),
                            image: Some(image.to_string()),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn release_pod(name: &str, image: &str, wire: Option<u16>, snapshot: Option<u16>) -> Pod {
        let mut annotations = std::collections::BTreeMap::new();
        if let Some(wire) = wire {
            annotations.insert(RELEASE_WIRE_ANNOTATION.to_string(), wire.to_string());
        }
        if let Some(snapshot) = snapshot {
            annotations.insert(
                RELEASE_SNAPSHOT_ANNOTATION.to_string(),
                snapshot.to_string(),
            );
        }
        Pod {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                annotations: (!annotations.is_empty()).then_some(annotations),
                ..Default::default()
            },
            spec: Some(k8s_openapi::api::core::v1::PodSpec {
                containers: vec![k8s_openapi::api::core::v1::Container {
                    name: "eurusd".to_string(),
                    image: Some(image.to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn release_gate_blocks_incompatible_image_rolls() {
        let mut shard = demo_catalog().shards["fx-eu-01"].clone();
        let old_image = "matchengine-raft:old";
        let new_image = "matchengine-raft:new";
        let set = release_stateful_set(new_image);

        assert!(
            release_block_for_pods(
                &shard,
                &release_stateful_set(old_image),
                &[release_pod("fx-eu-01-0", old_image, None, None)]
            )
            .is_none(),
            "a metadata-only template change does not need a release gate"
        );
        assert!(
            release_block_for_pods(
                &shard,
                &set,
                &[release_pod("fx-eu-01-0", old_image, Some(7), Some(11))]
            )
            .is_none(),
            "same-version image rolls are allowed"
        );

        shard.spec.release.wire_version = 8;
        let block = release_block_for_pods(
            &shard,
            &set,
            &[release_pod("fx-eu-01-0", old_image, Some(7), Some(11))],
        )
        .expect("incompatible wire versions must block");
        assert_eq!(block.reason, "stop_the_world_required");
        assert_eq!(block.current_wire_version, Some(7));
        assert_eq!(block.target_wire_version, 8);

        shard.spec.release.mixed_version_safe = true;
        assert!(
            release_block_for_pods(
                &shard,
                &set,
                &[release_pod("fx-eu-01-0", old_image, Some(7), Some(11))]
            )
            .is_none(),
            "a target that declares mixed-version support may roll"
        );
    }

    #[test]
    fn release_gate_requires_metadata_for_image_changes() {
        let mut shard = demo_catalog().shards["fx-eu-01"].clone();
        shard.spec.release.wire_version = 8;
        let set = release_stateful_set("matchengine-raft:new");
        let block = release_block_for_pods(
            &shard,
            &set,
            &[release_pod(
                "fx-eu-01-0",
                "matchengine-raft:old",
                None,
                None,
            )],
        )
        .expect("missing release annotations must block an image change");
        assert_eq!(block.reason, "release_metadata_missing");

        let mixed = release_block_for_pods(
            &shard,
            &set,
            &[
                release_pod("fx-eu-01-0", "matchengine-raft:old", Some(7), Some(11)),
                release_pod("fx-eu-01-1", "matchengine-raft:old", Some(6), Some(11)),
            ],
        )
        .expect("mixed current releases must block");
        assert_eq!(mixed.reason, "mixed_release");
    }

    #[test]
    fn shard_status_surfaces_the_release_block() {
        let catalog = demo_catalog();
        let shard = catalog.shards["fx-eu-01"].clone();
        let block = ReleaseBlock {
            reason: "stop_the_world_required".to_string(),
            message: "cannot mix wire versions".to_string(),
            current_wire_version: Some(7),
            current_snapshot_version: Some(11),
            target_wire_version: 8,
            target_snapshot_version: 12,
        };
        let status = shard_status(
            &shard,
            &catalog,
            &Observations::new(),
            1,
            None,
            Some(&block),
            None,
        );
        assert_eq!(status.release_block, Some(block));
        assert!(status.conditions.iter().any(|condition| {
            condition.kind == "ReleaseCompatible"
                && condition.status == "False"
                && condition.reason.as_deref() == Some("stop_the_world_required")
        }));
        assert!(status.conditions.iter().any(|condition| {
            condition.kind == "Progressing"
                && condition.status == "False"
                && condition.reason.as_deref() == Some("ReleaseBlocked")
        }));
    }

    #[test]
    fn shard_status_surfaces_the_capacity_block() {
        let catalog = demo_catalog();
        let shard = catalog.shards["fx-eu-01"].clone();
        let block = CapacityBlock {
            reason: "pod_does_not_fit".to_string(),
            message: "one rendered pod requests 1 CPU / 2Gi".to_string(),
            per_pod_cpu_millis: 1000,
            per_pod_memory_bytes: 2 * 1024 * 1024 * 1024,
            cluster_cpu_millis: 4000,
            cluster_memory_bytes: 8 * 1024 * 1024 * 1024,
            free_cpu_millis: 500,
            free_memory_bytes: 1024 * 1024 * 1024,
        };
        let status = shard_status(
            &shard,
            &catalog,
            &Observations::new(),
            1,
            None,
            None,
            Some(&block),
        );
        assert_eq!(status.capacity_block, Some(block));
        assert!(status.conditions.iter().any(|condition| {
            condition.kind == "CapacityAvailable"
                && condition.status == "False"
                && condition.reason.as_deref() == Some("pod_does_not_fit")
        }));
        assert!(status.conditions.iter().any(|condition| {
            condition.kind == "Progressing"
                && condition.status == "False"
                && condition.reason.as_deref() == Some("CapacityBlocked")
        }));
    }

    #[test]
    fn supervisor_rollout_waits_for_every_child_quorum() {
        let catalog = demo_catalog();
        let mut shard = catalog.shards["fx-eu-01"].clone();
        shard.spec.packing.mode = PackingMode::Supervisor;
        let eurgbp = catalog.deployments["eurusd-g1"].clone();
        let nzdusd = catalog.deployments["gbpusd-g1"].clone();
        let active = vec![&eurgbp, &nzdusd];
        let pod = rollout_pod("fx-eu-01-0", "supervisor");

        let healthy: Observations = rollout_observations(&eurgbp, [true; 4])
            .into_iter()
            .chain(rollout_observations(&nzdusd, [true; 4]))
            .collect();
        assert!(processes_tolerate_removal(&shard, &active, &healthy, &pod));

        let one_child_unready: Observations =
            rollout_observations(&eurgbp, [true, true, true, false])
                .into_iter()
                .chain(rollout_observations(&nzdusd, [true; 4]))
                .collect();
        assert!(
            !processes_tolerate_removal(&shard, &active, &one_child_unready, &pod),
            "a supervisor pod must not be replaced while any child would lose quorum"
        );
    }

    #[test]
    fn capacity_preflight_blocks_aggregate_and_per_pod_overcommit() {
        let catalog = demo_catalog();
        let objects = render_shard(&catalog, "fx-eu-01", &RenderOptions::default()).unwrap();
        let stateful_set = objects
            .into_iter()
            .find(|object| object["kind"] == "StatefulSet")
            .expect("rendered StatefulSet");
        let key = workload_key(&stateful_set).expect("workload key");
        let desired = BTreeMap::from([(key, stateful_set.clone())]);
        let (cpu, memory) = desired_pod_requests(&stateful_set);
        assert!(cpu > 0 && memory > 0);
        let node = "node".to_string();
        let full = BTreeMap::from([(node.clone(), (cpu * 4, memory * 4))]);

        let existing = BTreeMap::from([(node.clone(), (cpu * 4, memory * 4))]);
        let shard_existing = BTreeMap::from([("fx-eu-01".to_string(), existing.clone())]);
        assert!(
            capacity_blocks_for(&desired, &catalog, &full, &existing, &shard_existing).is_empty(),
            "the shard's own current pods must not count against its replacement capacity"
        );

        let other = BTreeMap::from([(node.clone(), (cpu * 3, memory * 3 + 1))]);
        let block = capacity_blocks_for(&desired, &catalog, &full, &other, &BTreeMap::new())
            .remove("fx-eu-01")
            .expect("per-pod overcommit must block");
        assert_eq!(block.reason, "pod_does_not_fit");
        assert_eq!(block.per_pod_memory_bytes, memory);

        let other = BTreeMap::from([(node, (cpu * 3, memory * 3))]);
        let block = capacity_blocks_for(&desired, &catalog, &full, &other, &BTreeMap::new())
            .remove("fx-eu-01")
            .expect("aggregate overcommit must block");
        assert_eq!(block.reason, "aggregate_capacity");
        assert_eq!(block.cluster_memory_bytes, memory * 4);
    }

    fn rollout_pod(name: &str, container: &str) -> Pod {
        Pod {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            spec: Some(k8s_openapi::api::core::v1::PodSpec {
                containers: vec![k8s_openapi::api::core::v1::Container {
                    name: container.to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn rollout_observations(deployment: &SymbolDeployment, ready: [bool; 4]) -> Observations {
        Observations::from([(
            deployment.name.clone(),
            DeploymentObservation {
                pods: ready
                    .into_iter()
                    .enumerate()
                    .map(|(pod, ready)| PodObservation {
                        pod: pod as u8,
                        ready,
                        healthy: ready,
                        ..PodObservation::default()
                    })
                    .collect(),
                ..DeploymentObservation::default()
            },
        )])
    }

    #[test]
    fn rollout_removal_requires_a_healthy_quorum() {
        let catalog = demo_catalog();
        let shard = catalog.shards["fx-eu-01"].clone();
        let deployment = catalog.deployments["eurusd-g1"].clone();
        let active = vec![&deployment];
        let pod = rollout_pod("fx-eu-01-2", "eurusd");

        let observations = rollout_observations(&deployment, [true, true, true, true]);
        assert!(processes_tolerate_removal(
            &shard,
            &active,
            &observations,
            &pod
        ));

        let observations = rollout_observations(&deployment, [true, true, true, false]);
        assert!(!processes_tolerate_removal(
            &shard,
            &active,
            &observations,
            &pod
        ));

        let pending = rollout_pod("fx-eu-01-3", "eurusd");
        assert!(processes_tolerate_removal(
            &shard,
            &active,
            &observations,
            &pending
        ));

        let unrelated = rollout_pod("fx-eu-01-2", "gbpusd");
        assert!(processes_tolerate_removal(
            &shard,
            &active,
            &observations,
            &unrelated
        ));
    }

    #[test]
    fn pod_index_prefers_the_statefulset_label() {
        let mut pod = rollout_pod("fx-eu-01-9", "eurusd");
        assert_eq!(pod_index(&pod), Some(9));
        pod.metadata.labels = Some(std::collections::BTreeMap::from([(
            "apps.kubernetes.io/pod-index".to_string(),
            "2".to_string(),
        )]));
        assert_eq!(pod_index(&pod), Some(2));
        assert!(!pod_running(&pod));
    }

    fn market_data_service() -> MarketDataService {
        use crate::model::{
            MarketDataFeedGroup, MarketDataServiceSpec, MarketDataTransport, MarketDataWebSocket,
        };
        MarketDataService {
            name: "btc-marketdata".to_string(),
            namespace: "match-eu-1".to_string(),
            spec: MarketDataServiceSpec {
                cell: "eu-1".to_string(),
                symbols: Vec::new(),
                feed_groups: vec![MarketDataFeedGroup {
                    name: "btc".to_string(),
                    symbols: vec!["btcusdt".to_string()],
                    stream_base: 2001,
                    channel: None,
                }],
                image: "registry.example.com/matchengine-marketdata@sha256:abc".to_string(),
                release: Default::default(),
                replicas: 2,
                transport: MarketDataTransport {
                    channel: Some(
                        "aeron:udp?endpoint=239.1.2.51:24001|interface={pod_ip}|ttl=1".to_string(),
                    ),
                    ..Default::default()
                },
                websocket: MarketDataWebSocket::default(),
                autoscaling: Default::default(),
                resources: None,
                paused: false,
            },
            status: Default::default(),
        }
    }

    fn market_data_observation(ready_replicas: u16, feed_ready: bool) -> MarketDataObservation {
        MarketDataObservation {
            replicas: 2,
            ready_replicas,
            updated_replicas: ready_replicas,
            available: ready_replicas > 0,
            feed_ready,
            symbols: vec![MarketDataSymbolStatus {
                name: "btcusdt".to_string(),
                ready: feed_ready,
                ..MarketDataSymbolStatus::default()
            }],
            ..MarketDataObservation::default()
        }
    }

    #[test]
    fn market_data_status_tracks_replicas_and_feed_readiness() {
        let catalog = demo_catalog();
        let service = market_data_service();
        let ready = market_data_status(
            &service,
            &catalog,
            Some(&market_data_observation(2, true)),
            false,
        );
        assert_eq!(ready.phase, Phase::Ready);
        assert_eq!(ready.ready_replicas, 2);
        assert!(
            ready
                .conditions
                .iter()
                .any(|condition| { condition.kind == "FeedReady" && condition.status == "True" })
        );

        let warming = market_data_status(
            &service,
            &catalog,
            Some(&market_data_observation(1, false)),
            false,
        );
        assert_eq!(warming.phase, Phase::Progressing);

        let degraded = market_data_status(
            &service,
            &catalog,
            Some(&market_data_observation(2, false)),
            false,
        );
        assert_eq!(degraded.phase, Phase::Degraded);

        let mut no_pods = market_data_observation(0, false);
        no_pods.replicas = 0;
        let pending = market_data_status(&service, &catalog, Some(&no_pods), false);
        assert_eq!(pending.phase, Phase::Pending);

        let degraded = market_data_status(
            &service,
            &catalog,
            Some(&market_data_observation(0, false)),
            false,
        );
        assert_eq!(degraded.phase, Phase::Degraded);

        let failed = market_data_status(&service, &catalog, None, true);
        assert_eq!(failed.phase, Phase::Failed);

        let mut paused = service.clone();
        paused.spec.paused = true;
        let disabled = market_data_status(&paused, &catalog, None, false);
        assert_eq!(disabled.phase, Phase::Disabled);

        let mut tagged = service;
        tagged.spec.image = "matchengine-marketdata:aeron-lab".to_string();
        let tagged = market_data_status(&tagged, &catalog, None, false);
        assert!(tagged.conditions.iter().any(|condition| {
            condition.kind == "MutableImageTag" && condition.status == "False"
        }));
    }
}

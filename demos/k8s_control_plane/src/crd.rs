//! CRD-backed desired state.
//!
//! The manager never talks to etcd. It writes the five catalog CRDs through the Kubernetes API
//! server, bootstraps its cache from list calls and keeps it fresh with one watcher per kind.
//! Status is written through the CRD status subresource and is rate-limited; audit history stays
//! in memory (an external append-only store in production) and is never persisted in etcd.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use kube::Client;
use kube::api::{Api, ApiResource, DeleteParams, DynamicObject, ListParams, PostParams};
use kube::runtime::watcher::{self, Event};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::error::ApiError;
use crate::model::{Cell, SymbolDeployment, SymbolRoute, SymbolShard, TradingSymbol};
use crate::store::{Catalog, CatalogStore};

/// CRD API group. The version is deliberately one shared version for all five kinds.
pub const GROUP: &str = "matching.io";
pub const VERSION: &str = "v1";
pub const DEFAULT_NAMESPACE: &str = "match-eu-1";

/// One of the five catalog CRDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CrdKind {
    Cell,
    Shard,
    Symbol,
    Deployment,
    Route,
}

impl CrdKind {
    pub const ALL: [Self; 5] = [
        Self::Cell,
        Self::Shard,
        Self::Symbol,
        Self::Deployment,
        Self::Route,
    ];

    pub fn kind(self) -> &'static str {
        match self {
            Self::Cell => "Cell",
            Self::Shard => "SymbolShard",
            Self::Symbol => "TradingSymbol",
            Self::Deployment => "SymbolDeployment",
            Self::Route => "SymbolRoute",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Self::Cell => "cells",
            Self::Shard => "symbolshards",
            Self::Symbol => "tradingsymbols",
            Self::Deployment => "symboldeployments",
            Self::Route => "symbolroutes",
        }
    }

    pub fn singular(self) -> &'static str {
        match self {
            Self::Cell => "cell",
            Self::Shard => "symbolshard",
            Self::Symbol => "tradingsymbol",
            Self::Deployment => "symboldeployment",
            Self::Route => "symbolroute",
        }
    }

    pub fn api_resource(self) -> ApiResource {
        ApiResource {
            group: GROUP.to_string(),
            version: VERSION.to_string(),
            api_version: format!("{GROUP}/{VERSION}"),
            kind: self.kind().to_string(),
            plural: self.plural().to_string(),
        }
    }

    fn from_kind(kind: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.kind() == kind)
    }
}

/// Generate the five CustomResourceDefinition manifests.
///
/// The schema deliberately preserves unknown fields inside `spec`/`status`: the manager already
/// validates the catalog before writing it, and the exact typed CRD schema is a follow-up. The
/// important contract here is the group/version, names, namespacing and the status subresource.
pub fn crd_manifests() -> Vec<Value> {
    CrdKind::ALL
        .into_iter()
        .map(|kind| {
            json!({
                "apiVersion": "apiextensions.k8s.io/v1",
                "kind": "CustomResourceDefinition",
                "metadata": {
                    "name": format!("{}.{GROUP}", kind.plural()),
                },
                "spec": {
                    "group": GROUP,
                    "scope": "Namespaced",
                    "names": {
                        "kind": kind.kind(),
                        "listKind": format!("{}List", kind.kind()),
                        "plural": kind.plural(),
                        "singular": kind.singular(),
                    },
                    "versions": [{
                        "name": VERSION,
                        "served": true,
                        "storage": true,
                        "additionalPrinterColumns": [
                            {
                                "name": "Phase",
                                "type": "string",
                                "jsonPath": ".status.phase"
                            },
                            {
                                "name": "Age",
                                "type": "date",
                                "jsonPath": ".metadata.creationTimestamp"
                            }
                        ],
                        "subresources": {
                            "status": {}
                        },
                        "schema": {
                            "openAPIV3Schema": {
                                "type": "object",
                                "properties": {
                                    "spec": {
                                        "type": "object",
                                        "x-kubernetes-preserve-unknown-fields": true
                                    },
                                    "status": {
                                        "type": "object",
                                        "x-kubernetes-preserve-unknown-fields": true
                                    }
                                }
                            }
                        }
                    }]
                }
            })
        })
        .collect()
}

/// Render the CRDs as a multi-document YAML file suitable for `kubectl apply -f`.
pub fn crd_manifest_yaml() -> Result<String, ApiError> {
    let mut out = String::new();
    for (index, manifest) in crd_manifests().iter().enumerate() {
        if index > 0 {
            out.push_str("---\n");
        }
        out.push_str(
            &serde_yaml::to_string(manifest)
                .map_err(|error| ApiError::internal(format!("encode CRD YAML: {error}")))?,
        );
    }
    Ok(out)
}

fn namespace_or_default(namespace: &str, default_namespace: &str) -> String {
    if namespace.trim().is_empty() {
        default_namespace.to_string()
    } else {
        namespace.to_string()
    }
}

fn object_with_spec_status<T: Serialize, S: Serialize>(
    kind: CrdKind,
    name: &str,
    namespace: &str,
    spec: &T,
    status: Option<&S>,
    default_namespace: &str,
) -> DynamicObject {
    let mut data = json!({ "spec": spec });
    if let Some(status) = status {
        data["status"] = json!(status);
    }
    DynamicObject::new(name, &kind.api_resource())
        .within(&namespace_or_default(namespace, default_namespace))
        .data(data)
}

pub fn cell_object(cell: &Cell, default_namespace: &str) -> DynamicObject {
    object_with_spec_status::<_, ()>(
        CrdKind::Cell,
        &cell.name,
        &cell.namespace,
        &cell.spec,
        None,
        default_namespace,
    )
}

pub fn shard_object(shard: &SymbolShard, default_namespace: &str) -> DynamicObject {
    object_with_spec_status(
        CrdKind::Shard,
        &shard.name,
        &shard.namespace,
        &shard.spec,
        Some(&shard.status),
        default_namespace,
    )
}

pub fn symbol_object(symbol: &TradingSymbol, default_namespace: &str) -> DynamicObject {
    object_with_spec_status(
        CrdKind::Symbol,
        &symbol.name,
        &symbol.namespace,
        &symbol.spec,
        Some(&symbol.status),
        default_namespace,
    )
}

pub fn deployment_object(deployment: &SymbolDeployment, default_namespace: &str) -> DynamicObject {
    object_with_spec_status(
        CrdKind::Deployment,
        &deployment.name,
        &deployment.namespace,
        &deployment.spec,
        Some(&deployment.status),
        default_namespace,
    )
}

pub fn route_object(route: &SymbolRoute, default_namespace: &str) -> DynamicObject {
    object_with_spec_status(
        CrdKind::Route,
        &route.name,
        &route.namespace,
        &route.spec,
        Some(&route.status),
        default_namespace,
    )
}

fn metadata_namespace(object: &DynamicObject, default_namespace: &str) -> String {
    object
        .metadata
        .namespace
        .clone()
        .filter(|namespace| !namespace.is_empty())
        .unwrap_or_else(|| default_namespace.to_string())
}

fn parse_spec<T: DeserializeOwned>(object: &DynamicObject) -> Option<T> {
    serde_json::from_value(object.data.get("spec")?.clone()).ok()
}

fn parse_status<T: DeserializeOwned + Default>(object: &DynamicObject) -> T {
    object
        .data
        .get("status")
        .and_then(|status| serde_json::from_value(status.clone()).ok())
        .unwrap_or_default()
}

fn parse_name(object: &DynamicObject) -> Option<String> {
    object.metadata.name.clone()
}

pub fn cell_from_object(object: &DynamicObject) -> Option<Cell> {
    Some(Cell {
        name: parse_name(object)?,
        namespace: metadata_namespace(object, DEFAULT_NAMESPACE),
        spec: parse_spec(object)?,
    })
}

pub fn shard_from_object(object: &DynamicObject) -> Option<SymbolShard> {
    Some(SymbolShard {
        name: parse_name(object)?,
        namespace: metadata_namespace(object, DEFAULT_NAMESPACE),
        spec: parse_spec(object)?,
        status: parse_status(object),
    })
}

pub fn symbol_from_object(object: &DynamicObject) -> Option<TradingSymbol> {
    Some(TradingSymbol {
        name: parse_name(object)?,
        namespace: metadata_namespace(object, DEFAULT_NAMESPACE),
        spec: parse_spec(object)?,
        status: parse_status(object),
    })
}

pub fn deployment_from_object(object: &DynamicObject) -> Option<SymbolDeployment> {
    Some(SymbolDeployment {
        name: parse_name(object)?,
        namespace: metadata_namespace(object, DEFAULT_NAMESPACE),
        spec: parse_spec(object)?,
        status: parse_status(object),
    })
}

pub fn route_from_object(object: &DynamicObject) -> Option<SymbolRoute> {
    Some(SymbolRoute {
        name: parse_name(object)?,
        namespace: metadata_namespace(object, DEFAULT_NAMESPACE),
        spec: parse_spec(object)?,
        status: parse_status(object),
    })
}

/// Convert a catalog to the five CRD object sets, used for CRD/etcd backup and restore tests.
pub fn catalog_to_objects(catalog: &Catalog, default_namespace: &str) -> Vec<DynamicObject> {
    let mut objects = Vec::new();
    objects.extend(
        catalog
            .cells
            .values()
            .map(|cell| cell_object(cell, default_namespace)),
    );
    objects.extend(
        catalog
            .shards
            .values()
            .map(|shard| shard_object(shard, default_namespace)),
    );
    objects.extend(
        catalog
            .symbols
            .values()
            .map(|symbol| symbol_object(symbol, default_namespace)),
    );
    objects.extend(
        catalog
            .deployments
            .values()
            .map(|deployment| deployment_object(deployment, default_namespace)),
    );
    objects.extend(
        catalog
            .routes
            .values()
            .map(|route| route_object(route, default_namespace)),
    );
    objects
}

/// Rebuild a catalog from CRD objects. Audit history is intentionally not part of CRDs.
pub fn catalog_from_objects(
    objects: impl IntoIterator<Item = DynamicObject>,
    default_namespace: &str,
) -> Catalog {
    let mut catalog = Catalog::default();
    for object in objects {
        let kind = object
            .types
            .as_ref()
            .and_then(|types| CrdKind::from_kind(&types.kind));
        let Some(kind) = kind else { continue };
        upsert_object(&mut catalog, kind, &object, default_namespace);
    }
    catalog
}

fn upsert_object(
    catalog: &mut Catalog,
    kind: CrdKind,
    object: &DynamicObject,
    default_namespace: &str,
) {
    let namespace = metadata_namespace(object, default_namespace);
    match kind {
        CrdKind::Cell => {
            if let Some(mut cell) = cell_from_object(object) {
                cell.namespace = namespace.clone();
                catalog.cells.insert(cell.name.clone(), cell);
            }
        }
        CrdKind::Shard => {
            if let Some(mut shard) = shard_from_object(object) {
                shard.namespace = namespace.clone();
                catalog.shards.insert(shard.name.clone(), shard);
            }
        }
        CrdKind::Symbol => {
            if let Some(mut symbol) = symbol_from_object(object) {
                symbol.namespace = namespace.clone();
                catalog.symbols.insert(symbol.name.clone(), symbol);
            }
        }
        CrdKind::Deployment => {
            if let Some(mut deployment) = deployment_from_object(object) {
                deployment.namespace = namespace.clone();
                catalog
                    .deployments
                    .insert(deployment.name.clone(), deployment);
            }
        }
        CrdKind::Route => {
            if let Some(mut route) = route_from_object(object) {
                route.namespace = namespace.clone();
                catalog.routes.insert(route.name.clone(), route);
            }
        }
    }
    debug!(?kind, name = ?object.metadata.name, namespace, "catalog cache upsert");
}

fn remove_object(catalog: &mut Catalog, kind: CrdKind, object: &DynamicObject) {
    let Some(name) = object.metadata.name.as_deref() else {
        return;
    };
    match kind {
        CrdKind::Cell => {
            catalog.cells.remove(name);
        }
        CrdKind::Shard => {
            catalog.shards.remove(name);
        }
        CrdKind::Symbol => {
            catalog.symbols.remove(name);
        }
        CrdKind::Deployment => {
            catalog.deployments.remove(name);
        }
        CrdKind::Route => {
            catalog.routes.remove(name);
        }
    }
}

fn remove_kind(catalog: &mut Catalog, kind: CrdKind) {
    match kind {
        CrdKind::Cell => catalog.cells.clear(),
        CrdKind::Shard => catalog.shards.clear(),
        CrdKind::Symbol => catalog.symbols.clear(),
        CrdKind::Deployment => catalog.deployments.clear(),
        CrdKind::Route => catalog.routes.clear(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectKey {
    pub kind: CrdKind,
    pub namespace: String,
    pub name: String,
}

impl ObjectKey {
    fn from_object(kind: CrdKind, object: &DynamicObject, default_namespace: &str) -> Option<Self> {
        Some(Self {
            kind,
            namespace: metadata_namespace(object, default_namespace),
            name: object.metadata.name.clone()?,
        })
    }
}

#[derive(Debug, Clone)]
pub enum ObjectChange {
    Create(DynamicObject),
    Update(DynamicObject),
    Status(DynamicObject),
    Delete(ObjectKey),
}

fn objects_by_key(
    catalog: &Catalog,
    default_namespace: &str,
) -> BTreeMap<ObjectKey, DynamicObject> {
    let mut objects = BTreeMap::new();
    for object in catalog_to_objects(catalog, default_namespace) {
        let Some(kind) = object
            .types
            .as_ref()
            .and_then(|types| CrdKind::from_kind(&types.kind))
        else {
            continue;
        };
        if let Some(key) = ObjectKey::from_object(kind, &object, default_namespace) {
            objects.insert(key, object);
        }
    }
    objects
}

/// Compute the minimal set of CRD writes between two catalog snapshots.
///
/// A spec change uses the normal resource; a status-only change uses the `/status` subresource.
/// This keeps the status write rate independent of spec writes and makes the diff testable without
/// a live API server.
pub fn diff_catalog(
    before: &Catalog,
    after: &Catalog,
    default_namespace: &str,
) -> Vec<ObjectChange> {
    let before = objects_by_key(before, default_namespace);
    let after = objects_by_key(after, default_namespace);
    let mut changes = Vec::new();
    for (key, object) in &after {
        match before.get(key) {
            None => changes.push(ObjectChange::Create(object.clone())),
            Some(previous) => {
                let spec_changed = previous.data.get("spec") != object.data.get("spec");
                let status_changed = previous.data.get("status") != object.data.get("status");
                if spec_changed {
                    changes.push(ObjectChange::Update(object.clone()));
                }
                if status_changed {
                    changes.push(ObjectChange::Status(object.clone()));
                }
            }
        }
    }
    for key in before.keys() {
        if !after.contains_key(key) {
            changes.push(ObjectChange::Delete(key.clone()));
        }
    }
    changes
}

fn kube_error(action: &str, error: kube::Error) -> ApiError {
    ApiError::new(
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "kubernetes_api",
        format!("{action}: {error}"),
    )
}

#[derive(Debug)]
struct StatusWriteThrottle {
    min_interval: Duration,
    last: AsyncMutex<BTreeMap<ObjectKey, Instant>>,
}

impl StatusWriteThrottle {
    fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last: AsyncMutex::new(BTreeMap::new()),
        }
    }

    async fn wait(&self, key: &ObjectKey) {
        if self.min_interval.is_zero() {
            return;
        }
        let last = self.last.lock().await.get(key).copied();
        if let Some(last) = last {
            let elapsed = last.elapsed();
            if elapsed < self.min_interval {
                tokio::time::sleep(self.min_interval - elapsed).await;
            }
        }
        self.last.lock().await.insert(key.clone(), Instant::now());
    }
}

/// CRD-backed catalog store.
pub struct CrdCatalogStore {
    client: Client,
    default_namespace: String,
    state: Arc<RwLock<Catalog>>,
    resource_versions: Arc<RwLock<BTreeMap<ObjectKey, String>>>,
    status_throttle: StatusWriteThrottle,
}

impl std::fmt::Debug for CrdCatalogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrdCatalogStore")
            .field("default_namespace", &self.default_namespace)
            .finish_non_exhaustive()
    }
}

impl CrdCatalogStore {
    /// Bootstrap the cache from the API server and spawn the informer tasks.
    pub async fn new(
        client: Client,
        state: Arc<RwLock<Catalog>>,
        default_namespace: String,
        status_interval: Duration,
    ) -> Result<Self, ApiError> {
        let store = Self {
            client,
            default_namespace,
            state,
            resource_versions: Arc::new(RwLock::new(BTreeMap::new())),
            status_throttle: StatusWriteThrottle::new(status_interval),
        };
        store.bootstrap().await?;
        store.spawn_watchers();
        Ok(store)
    }

    async fn bootstrap(&self) -> Result<(), ApiError> {
        let mut catalog = Catalog::default();
        for kind in CrdKind::ALL {
            let api = Api::<DynamicObject>::all_with(self.client.clone(), &kind.api_resource());
            let list = api
                .list(&ListParams::default())
                .await
                .map_err(|error| kube_error(&format!("list {}", kind.plural()), error))?;
            for object in list.items {
                if let Some(key) = ObjectKey::from_object(kind, &object, &self.default_namespace)
                    && let Some(rv) = object.metadata.resource_version.clone()
                {
                    self.resource_versions.write().unwrap().insert(key, rv);
                }
                upsert_object(&mut catalog, kind, &object, &self.default_namespace);
            }
        }
        let mut cache = self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        catalog.audit = cache.audit.clone();
        catalog.next_audit_id = cache.next_audit_id;
        *cache = catalog;
        Ok(())
    }

    fn spawn_watchers(&self) {
        for kind in CrdKind::ALL {
            let client = self.client.clone();
            let state = self.state.clone();
            let resource_versions = self.resource_versions.clone();
            let default_namespace = self.default_namespace.clone();
            tokio::spawn(async move {
                loop {
                    let api = Api::<DynamicObject>::all_with(client.clone(), &kind.api_resource());
                    let mut stream =
                        std::pin::pin!(watcher::watcher(api, watcher::Config::default()));
                    let mut init: Option<BTreeMap<String, DynamicObject>> = None;
                    while let Some(event) = stream.next().await {
                        match event {
                            Ok(Event::Init) => {
                                init = Some(BTreeMap::new());
                            }
                            Ok(Event::InitApply(object)) => {
                                if let Some(buffer) = init.as_mut() {
                                    if let Some(name) = object.metadata.name.clone() {
                                        buffer.insert(name, object);
                                    }
                                } else {
                                    apply_watch_object(
                                        &state,
                                        &resource_versions,
                                        kind,
                                        &object,
                                        &default_namespace,
                                    );
                                }
                            }
                            Ok(Event::InitDone) => {
                                if let Some(buffer) = init.take() {
                                    replace_kind(
                                        &state,
                                        &resource_versions,
                                        kind,
                                        buffer.into_values().collect(),
                                        &default_namespace,
                                    );
                                }
                            }
                            Ok(Event::Apply(object)) => {
                                apply_watch_object(
                                    &state,
                                    &resource_versions,
                                    kind,
                                    &object,
                                    &default_namespace,
                                );
                            }
                            Ok(Event::Delete(object)) => {
                                remove_watch_object(
                                    &state,
                                    &resource_versions,
                                    kind,
                                    &object,
                                    &default_namespace,
                                );
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

    fn api_for(&self, kind: CrdKind, namespace: &str) -> Api<DynamicObject> {
        Api::namespaced_with(self.client.clone(), namespace, &kind.api_resource())
    }

    async fn resource_version(&self, key: &ObjectKey) -> Result<String, ApiError> {
        if let Some(rv) = self.resource_versions.read().unwrap().get(key).cloned() {
            return Ok(rv);
        }
        let api = self.api_for(key.kind, &key.namespace);
        let object = api
            .get(&key.name)
            .await
            .map_err(|error| kube_error(&format!("get {} {}", key.kind.kind(), key.name), error))?;
        object
            .metadata
            .resource_version
            .ok_or_else(|| ApiError::internal(format!("{} has no resourceVersion", key.name)))
    }

    fn record_version(&self, key: &ObjectKey, object: &DynamicObject) {
        if let Some(rv) = object.metadata.resource_version.clone() {
            self.resource_versions
                .write()
                .unwrap()
                .insert(key.clone(), rv);
        }
    }

    async fn create_object(&self, object: DynamicObject) -> Result<(), ApiError> {
        let Some(key) = ObjectKey::from_object(
            object
                .types
                .as_ref()
                .and_then(|types| CrdKind::from_kind(&types.kind))
                .unwrap_or(CrdKind::Cell),
            &object,
            &self.default_namespace,
        ) else {
            return Ok(());
        };
        let api = self.api_for(key.kind, &key.namespace);
        let created = api
            .create(&PostParams::default(), &object)
            .await
            .map_err(|error| {
                kube_error(&format!("create {} {}", key.kind.kind(), key.name), error)
            })?;
        self.record_version(&key, &created);
        Ok(())
    }

    async fn update_object(&self, mut object: DynamicObject) -> Result<(), ApiError> {
        let Some(key) = ObjectKey::from_object(
            object
                .types
                .as_ref()
                .and_then(|types| CrdKind::from_kind(&types.kind))
                .unwrap_or(CrdKind::Cell),
            &object,
            &self.default_namespace,
        ) else {
            return Ok(());
        };
        object.metadata.resource_version = Some(self.resource_version(&key).await?);
        let api = self.api_for(key.kind, &key.namespace);
        let updated = api
            .replace(&key.name, &PostParams::default(), &object)
            .await
            .map_err(|error| {
                kube_error(&format!("replace {} {}", key.kind.kind(), key.name), error)
            })?;
        self.record_version(&key, &updated);
        Ok(())
    }

    async fn update_status(&self, mut object: DynamicObject) -> Result<(), ApiError> {
        let Some(key) = ObjectKey::from_object(
            object
                .types
                .as_ref()
                .and_then(|types| CrdKind::from_kind(&types.kind))
                .unwrap_or(CrdKind::Cell),
            &object,
            &self.default_namespace,
        ) else {
            return Ok(());
        };
        self.status_throttle.wait(&key).await;
        object.metadata.resource_version = Some(self.resource_version(&key).await?);
        let api = self.api_for(key.kind, &key.namespace);
        let updated = api
            .replace_status(&key.name, &PostParams::default(), &object)
            .await
            .map_err(|error| {
                kube_error(&format!("status {} {}", key.kind.kind(), key.name), error)
            })?;
        self.record_version(&key, &updated);
        Ok(())
    }

    async fn delete_object(&self, key: &ObjectKey) -> Result<(), ApiError> {
        let api = self.api_for(key.kind, &key.namespace);
        match api.delete(&key.name, &DeleteParams::default()).await {
            Ok(_) => {}
            Err(kube::Error::Api(error)) if error.code == 404 => {}
            Err(error) => {
                return Err(kube_error(
                    &format!("delete {} {}", key.kind.kind(), key.name),
                    error,
                ));
            }
        }
        self.resource_versions.write().unwrap().remove(key);
        Ok(())
    }
}

#[async_trait]
impl CatalogStore for CrdCatalogStore {
    fn mode(&self) -> &'static str {
        "crd"
    }

    async fn persist(&self, before: &Catalog, after: &Catalog) -> Result<(), ApiError> {
        for change in diff_catalog(before, after, &self.default_namespace) {
            match change {
                ObjectChange::Create(object) => self.create_object(object).await?,
                ObjectChange::Update(object) => self.update_object(object).await?,
                ObjectChange::Status(object) => self.update_status(object).await?,
                ObjectChange::Delete(key) => self.delete_object(&key).await?,
            }
        }
        Ok(())
    }
}

fn apply_watch_object(
    state: &Arc<RwLock<Catalog>>,
    resource_versions: &Arc<RwLock<BTreeMap<ObjectKey, String>>>,
    kind: CrdKind,
    object: &DynamicObject,
    default_namespace: &str,
) {
    if let Some(key) = ObjectKey::from_object(kind, object, default_namespace)
        && let Some(rv) = object.metadata.resource_version.clone()
    {
        resource_versions.write().unwrap().insert(key, rv);
    }
    let mut catalog = state.write().unwrap();
    upsert_object(&mut catalog, kind, object, default_namespace);
}

fn remove_watch_object(
    state: &Arc<RwLock<Catalog>>,
    resource_versions: &Arc<RwLock<BTreeMap<ObjectKey, String>>>,
    kind: CrdKind,
    object: &DynamicObject,
    default_namespace: &str,
) {
    if let Some(key) = ObjectKey::from_object(kind, object, default_namespace) {
        resource_versions.write().unwrap().remove(&key);
    }
    let mut catalog = state.write().unwrap();
    remove_object(&mut catalog, kind, object);
}

fn replace_kind(
    state: &Arc<RwLock<Catalog>>,
    resource_versions: &Arc<RwLock<BTreeMap<ObjectKey, String>>>,
    kind: CrdKind,
    objects: Vec<DynamicObject>,
    default_namespace: &str,
) {
    {
        let mut versions = resource_versions.write().unwrap();
        versions.retain(|key, _| key.kind != kind);
        for object in &objects {
            if let Some(key) = ObjectKey::from_object(kind, object, default_namespace)
                && let Some(rv) = object.metadata.resource_version.clone()
            {
                versions.insert(key, rv);
            }
        }
    }
    let mut catalog = state.write().unwrap();
    remove_kind(&mut catalog, kind);
    for object in &objects {
        upsert_object(&mut catalog, kind, object, default_namespace);
    }
}

/// Namespaced objects grouped by kind, useful for RBAC examples and tests.
pub fn object_names_by_kind(
    catalog: &Catalog,
    default_namespace: &str,
) -> BTreeMap<CrdKind, BTreeSet<String>> {
    let mut names: BTreeMap<CrdKind, BTreeSet<String>> = BTreeMap::new();
    for object in catalog_to_objects(catalog, default_namespace) {
        let Some(kind) = object
            .types
            .as_ref()
            .and_then(|types| CrdKind::from_kind(&types.kind))
        else {
            continue;
        };
        if let Some(name) = object.metadata.name {
            names.entry(kind).or_default().insert(name);
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::demo_catalog;

    #[test]
    fn manifests_have_status_subresources_and_namespacing() {
        let manifests = crd_manifests();
        assert_eq!(manifests.len(), 5);
        for manifest in manifests {
            assert_eq!(manifest["apiVersion"], "apiextensions.k8s.io/v1");
            assert_eq!(manifest["kind"], "CustomResourceDefinition");
            assert_eq!(manifest["spec"]["group"], GROUP);
            assert_eq!(manifest["spec"]["scope"], "Namespaced");
            assert_eq!(manifest["spec"]["versions"][0]["name"], VERSION);
            assert_eq!(
                manifest["spec"]["versions"][0]["subresources"]["status"],
                json!({})
            );
            assert_eq!(
                manifest["spec"]["versions"][0]["additionalPrinterColumns"][0]["name"],
                "Phase"
            );
            let kind = manifest["spec"]["names"]["kind"].as_str().unwrap();
            assert!(
                CrdKind::ALL
                    .iter()
                    .any(|candidate| candidate.kind() == kind),
                "{kind}"
            );
        }
        let yaml = crd_manifest_yaml().unwrap();
        assert_eq!(yaml.matches("kind: CustomResourceDefinition").count(), 5);
        assert!(yaml.contains("status: {}"));
    }

    #[test]
    fn checked_in_crds_match_the_generator() {
        assert_eq!(
            include_str!("../deploy/crds.yaml"),
            crd_manifest_yaml().expect("CRD YAML")
        );
    }

    #[test]
    fn catalog_backup_and_restore_round_trip_preserves_desired_state() {
        let catalog = demo_catalog();
        let objects = catalog_to_objects(&catalog, DEFAULT_NAMESPACE);
        let restored = catalog_from_objects(objects, DEFAULT_NAMESPACE);

        assert_eq!(restored.cells, catalog.cells);
        assert_eq!(restored.shards, catalog.shards);
        assert_eq!(restored.symbols, catalog.symbols);
        assert_eq!(restored.deployments, catalog.deployments);
        assert_eq!(restored.routes, catalog.routes);
        // Audit is deliberately outside the CRDs.
        assert!(restored.audit.is_empty());
        assert_eq!(restored.next_audit_id, 0);
    }

    #[test]
    fn diff_uses_status_subresource_for_status_only_changes() {
        let catalog = demo_catalog();
        let mut before = catalog.clone();
        let mut after = catalog.clone();
        let deployment = after.deployments.get_mut("eurusd-g1").unwrap();
        deployment.status.phase = crate::model::Phase::Ready;
        deployment.status.ready = true;
        let changes = diff_catalog(&before, &after, DEFAULT_NAMESPACE);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], ObjectChange::Status(_)));

        before
            .deployments
            .get_mut("eurusd-g1")
            .unwrap()
            .spec
            .config_hash = "old".into();
        after
            .deployments
            .get_mut("eurusd-g1")
            .unwrap()
            .spec
            .config_hash = "new".into();
        let changes = diff_catalog(&before, &after, DEFAULT_NAMESPACE);
        assert!(
            changes
                .iter()
                .any(|change| matches!(change, ObjectChange::Update(_)))
        );
    }

    #[test]
    fn diff_creates_and_deletes_objects() {
        let before = Catalog::default();
        let after = demo_catalog();
        let changes = diff_catalog(&before, &after, DEFAULT_NAMESPACE);
        assert!(
            changes
                .iter()
                .all(|change| matches!(change, ObjectChange::Create(_)))
        );
        assert_eq!(
            changes.len(),
            after.cells.len()
                + after.shards.len()
                + after.symbols.len()
                + after.deployments.len()
                + after.routes.len()
        );

        let changes = diff_catalog(&after, &before, DEFAULT_NAMESPACE);
        assert!(
            changes
                .iter()
                .all(|change| matches!(change, ObjectChange::Delete(_)))
        );
    }

    #[tokio::test]
    async fn status_throttle_allows_zero_interval() {
        let throttle = StatusWriteThrottle::new(Duration::ZERO);
        let key = ObjectKey {
            kind: CrdKind::Symbol,
            namespace: "ns".to_string(),
            name: "eurusd".to_string(),
        };
        throttle.wait(&key).await;
        throttle.wait(&key).await;
    }
}

//! In-memory catalog with optional JSON persistence.
//!
//! Production uses Postgres for the global catalog and Kubernetes CRs as the desired-state
//! contract (see the design doc §8). The demo keeps one JSON document so it can restart without
//! a database while preserving the same object model.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::bundle::ShardBundle;
use crate::error::ApiError;
use crate::model::{Cell, SymbolDeployment, SymbolRoute, SymbolShard, TradingSymbol};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    pub id: u64,
    /// Unix seconds; the GUI formats it.
    pub at: u64,
    pub actor: String,
    pub action: String,
    pub target: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    #[serde(default)]
    pub cells: BTreeMap<String, Cell>,
    #[serde(default)]
    pub shards: BTreeMap<String, SymbolShard>,
    #[serde(default)]
    pub symbols: BTreeMap<String, TradingSymbol>,
    #[serde(default)]
    pub deployments: BTreeMap<String, SymbolDeployment>,
    #[serde(default)]
    pub routes: BTreeMap<String, SymbolRoute>,
    #[serde(default)]
    pub audit: Vec<AuditEntry>,
    #[serde(default)]
    pub next_audit_id: u64,
}

impl Catalog {
    /// Upsert a bundle. Validation happens before this call.
    pub fn insert_bundle(&mut self, bundle: ShardBundle) {
        self.cells.insert(bundle.cell.name.clone(), bundle.cell);
        self.shards.insert(bundle.shard.name.clone(), bundle.shard);
        for symbol in bundle.symbols {
            self.symbols.insert(symbol.name.clone(), symbol);
        }
        for deployment in bundle.deployments {
            self.deployments.insert(deployment.name.clone(), deployment);
        }
        for route in bundle.routes {
            self.routes.insert(route.name.clone(), route);
        }
    }

    pub fn deployment(&self, name: &str) -> Option<&SymbolDeployment> {
        self.deployments.get(name)
    }

    pub fn route_for_symbol(&self, symbol: &str) -> Option<&SymbolRoute> {
        self.routes.values().find(|r| r.spec.symbol_ref == symbol)
    }

    /// Deployments bound to a shard, sorted by slot (that is the container order in the pod).
    pub fn shard_deployments(&self, shard: &str) -> Vec<&SymbolDeployment> {
        let mut deployments: Vec<&SymbolDeployment> = self
            .deployments
            .values()
            .filter(|d| d.spec.shard_ref == shard)
            .collect();
        deployments.sort_by_key(|d| d.spec.placement.slot);
        deployments
    }

    pub fn shard_processes(&self, shard: &str) -> usize {
        self.shard_deployments(shard).len()
    }

    pub fn audit(&mut self, actor: &str, action: &str, target: &str, detail: impl Into<String>) {
        self.next_audit_id += 1;
        self.audit.push(AuditEntry {
            id: self.next_audit_id,
            at: now_epoch(),
            actor: actor.to_string(),
            action: action.to_string(),
            target: target.to_string(),
            detail: detail.into(),
        });
    }

    pub fn recent_audit(&self, limit: usize) -> Vec<AuditEntry> {
        let mut entries = self.audit.clone();
        entries.reverse();
        entries.truncate(limit);
        entries
    }

    /// Strip runtime status so a restart starts from desired state only.
    pub fn without_status(&self) -> Self {
        let mut catalog = self.clone();
        for shard in catalog.shards.values_mut() {
            shard.status = Default::default();
        }
        for deployment in catalog.deployments.values_mut() {
            deployment.status = Default::default();
        }
        for route in catalog.routes.values_mut() {
            route.status = Default::default();
        }
        for symbol in catalog.symbols.values_mut() {
            symbol.status = Default::default();
        }
        catalog
    }
}

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Process-wide catalog with an optional JSON file.
#[derive(Debug)]
pub struct Store {
    state: RwLock<Catalog>,
    path: Option<PathBuf>,
}

impl Store {
    pub fn in_memory(seed_demo: bool) -> Self {
        let catalog = if seed_demo {
            demo_catalog()
        } else {
            Catalog::default()
        };
        Self {
            state: RwLock::new(catalog),
            path: None,
        }
    }

    /// Load a catalog from `path` when it exists; otherwise start from `seed_demo` and persist
    /// the seed so a restart is stable.
    pub fn open(path: Option<PathBuf>, seed_demo: bool) -> Result<Self, ApiError> {
        let catalog = match &path {
            Some(path) if path.exists() => {
                let bytes = std::fs::read(path)
                    .map_err(|e| ApiError::internal(format!("read {}: {e}", path.display())))?;
                serde_json::from_slice(&bytes)
                    .map_err(|e| ApiError::internal(format!("parse {}: {e}", path.display())))?
            }
            _ => {
                if seed_demo {
                    demo_catalog()
                } else {
                    Catalog::default()
                }
            }
        };
        let store = Self {
            state: RwLock::new(catalog),
            path,
        };
        if seed_demo && store.path.is_some() {
            store.persist(&store.snapshot())?;
        }
        Ok(store)
    }

    pub fn snapshot(&self) -> Catalog {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Apply a mutation and persist the result. The closure returns an `ApiError` to abort
    /// without changing state.
    pub fn mutate<T>(
        &self,
        f: impl FnOnce(&mut Catalog) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let mut guard = self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = guard.clone();
        let result = f(&mut guard)?;
        if let Some(path) = &self.path
            && let Err(error) = persist_catalog(path, &guard)
        {
            *guard = before;
            return Err(error);
        }
        Ok(result)
    }

    fn persist(&self, catalog: &Catalog) -> Result<(), ApiError> {
        match &self.path {
            Some(path) => persist_catalog(path, catalog),
            None => Ok(()),
        }
    }
}

fn persist_catalog(path: &Path, catalog: &Catalog) -> Result<(), ApiError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| ApiError::internal(format!("create {}: {e}", parent.display())))?;
    }
    let body = serde_json::to_vec_pretty(catalog)
        .map_err(|e| ApiError::internal(format!("encode catalog: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)
        .map_err(|e| ApiError::internal(format!("write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| ApiError::internal(format!("rename {}: {e}", path.display())))?;
    Ok(())
}

/// Demo catalog: one packed FX shard with two symbols and one dedicated crypto shard.
pub fn demo_catalog() -> Catalog {
    let mut catalog = Catalog::default();
    for bundle in crate::bundle::demo_bundles() {
        catalog.insert_bundle(bundle);
    }
    catalog.audit("system", "seed", "catalog", "loaded demo shards");
    catalog
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_catalog_has_packed_reserved_and_dedicated_shards() {
        let catalog = demo_catalog();
        assert_eq!(catalog.cells.len(), 1);
        assert_eq!(catalog.shards.len(), 3);
        assert_eq!(catalog.shard_processes("fx-eu-01"), 2);
        assert_eq!(catalog.shard_processes("fx-eu-02"), 0);
        assert_eq!(catalog.shard_processes("me-btcusdt"), 1);
        assert_eq!(catalog.audit.len(), 1);
    }

    #[test]
    fn persistence_round_trips() {
        let dir = std::env::temp_dir().join(format!(
            "k8s-control-plane-test-{}-{}",
            std::process::id(),
            now_epoch()
        ));
        let path = dir.join("catalog.json");
        let store = Store::open(Some(path.clone()), true).unwrap();
        let snapshot = store.snapshot();
        drop(store);
        let reopened = Store::open(Some(path.clone()), false).unwrap();
        assert_eq!(reopened.snapshot().shards.len(), snapshot.shards.len());
        let _ = std::fs::remove_dir_all(dir);
    }
}

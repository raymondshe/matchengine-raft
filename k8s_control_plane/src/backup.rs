//! Executable shard backups and selective restores.
//!
//! A backup bundle is a directory with:
//!
//! - `catalog.json` — the CRD/desired-state export,
//! - `backup.json` — the manifest (volume snapshots, checksums, counts),
//! - `SHA256SUMS` — checksums for every file in the bundle, and optionally
//! - `etcd.snapshot.db` — an externally produced etcd snapshot.
//!
//! Volume data is captured with the Kubernetes snapshot APIs. The CLI never connects to etcd
//! directly; an operator supplies an `etcdctl snapshot save` file when one is required.
//!
//! `--verify` restores each snapshot into a temporary PVC, hashes every file through a short-lived
//! pod and records the result in the manifest. `restore --drill` repeats that restore and compares
//! the hashes without touching desired state or the live PVCs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kube::Client;
use kube::api::{Api, ApiResource, DeleteParams, DynamicObject, ListParams, LogParams, PostParams};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub use crate::model::BackupMode;
use crate::model::SymbolShard;
use crate::store::{Catalog, now_epoch};
use crate::validate::validate_catalog;

pub const BACKUP_FORMAT: &str = "matchengine-backup/v1";
pub const MANIFEST_FILE: &str = "backup.json";
pub const CATALOG_FILE: &str = "catalog.json";
pub const CHECKSUMS_FILE: &str = "SHA256SUMS";
pub const ETCD_FILE: &str = "etcd.snapshot.db";

const BACKUP_LABEL: &str = "matching.io/backup";
const SHARD_LABEL: &str = "matching.io/shard";
const DRILL_LABEL: &str = "matching.io/restore-drill";

/// One file recorded in a backup bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

impl BackupFile {
    pub fn from_path(path: &Path, relative: &str) -> Result<Self> {
        let bytes = fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?
            .len();
        Ok(Self {
            path: relative.to_string(),
            sha256: sha256_file(path)?,
            bytes,
        })
    }
}

/// Object counts captured in the catalog export.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupCounts {
    #[serde(default)]
    pub cells: usize,
    #[serde(default)]
    pub shards: usize,
    #[serde(default)]
    pub symbols: usize,
    #[serde(default)]
    pub deployments: usize,
    #[serde(default)]
    pub routes: usize,
    #[serde(default)]
    pub market_data_services: usize,
    #[serde(default)]
    pub order_systems: usize,
}

impl BackupCounts {
    pub fn of(catalog: &Catalog) -> Self {
        Self {
            cells: catalog.cells.len(),
            shards: catalog.shards.len(),
            symbols: catalog.symbols.len(),
            deployments: catalog.deployments.len(),
            routes: catalog.routes.len(),
            market_data_services: catalog.market_data_services.len(),
            order_systems: catalog.order_systems.len(),
        }
    }
}

/// One PVC snapshot and its optional restored-content checksums.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeBackup {
    pub pvc: String,
    pub snapshot: String,
    #[serde(default)]
    pub ready: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub restore_size: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub requested_size: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub storage_class: String,
    #[serde(default)]
    pub access_modes: Vec<String>,
    /// Restored file checksums keyed by path relative to the mounted data root.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub file_sha256: BTreeMap<String, String>,
    /// Unix seconds when the snapshot was restored and hashed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<u64>,
}

/// Durable backup record stored as `backup.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupManifest {
    pub format: String,
    pub id: String,
    pub created_at: u64,
    pub namespace: String,
    pub shard: String,
    pub mode: BackupMode,
    #[serde(default)]
    pub quiesced: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub volume_group_snapshot: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub volume_group_snapshot_class: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub volume_snapshot_class: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub configured_destination: String,
    pub counts: BackupCounts,
    pub catalog: BackupFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etcd: Option<BackupFile>,
    #[serde(default)]
    pub volumes: Vec<VolumeBackup>,
}

impl BackupManifest {
    pub fn new(
        id: String,
        namespace: &str,
        shard: &SymbolShard,
        mode: BackupMode,
        catalog: &Catalog,
        volume_group_snapshot_class: &str,
        volume_snapshot_class: &str,
    ) -> Self {
        Self {
            format: BACKUP_FORMAT.to_string(),
            id,
            created_at: now_epoch(),
            namespace: namespace.to_string(),
            shard: shard.name.clone(),
            mode,
            quiesced: false,
            volume_group_snapshot: String::new(),
            volume_group_snapshot_class: volume_group_snapshot_class.to_string(),
            volume_snapshot_class: volume_snapshot_class.to_string(),
            configured_destination: shard
                .spec
                .backup
                .as_ref()
                .map(|backup| backup.destination.clone())
                .unwrap_or_default(),
            counts: BackupCounts::of(catalog),
            catalog: BackupFile {
                path: CATALOG_FILE.to_string(),
                sha256: String::new(),
                bytes: 0,
            },
            etcd: None,
            volumes: Vec::new(),
        }
    }
}

/// A loaded and checksum-verified bundle.
#[derive(Debug, Clone)]
pub struct LoadedBundle {
    pub root: PathBuf,
    pub manifest: BackupManifest,
    pub catalog: Catalog,
}

/// One parsed line in `SHA256SUMS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumEntry {
    pub sha256: String,
    pub path: String,
}

#[derive(Debug, Clone, Default)]
pub struct RestoreSelection {
    pub cells: BTreeSet<String>,
    pub shards: BTreeSet<String>,
    pub symbols: BTreeSet<String>,
    pub market_data_services: BTreeSet<String>,
    pub order_systems: BTreeSet<String>,
}

impl RestoreSelection {
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
            && self.shards.is_empty()
            && self.symbols.is_empty()
            && self.market_data_services.is_empty()
            && self.order_systems.is_empty()
    }
}

/// Snapshot and verification options for `backup`.
#[derive(Debug, Clone)]
pub struct SnapshotSet {
    pub volume_group_snapshot: Option<String>,
    pub volumes: Vec<VolumeBackup>,
}

pub struct SnapshotOptions {
    pub namespace: String,
    pub shard: String,
    pub backup_id: String,
    pub mode: BackupMode,
    pub volume_group_snapshot_class: String,
    pub volume_snapshot_class: String,
    pub verify: bool,
    pub verify_image: String,
    pub timeout: Duration,
}

/// Restore/restore-drill options for volumes.
#[derive(Debug, Clone)]
pub struct VolumeRestoreOptions {
    pub target_namespace: String,
    pub prefix: String,
    pub drill: bool,
    pub keep: bool,
    pub verify_image: String,
    pub timeout: Duration,
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Write a complete bundle atomically. Returns the manifest with file checksums filled in.
pub fn write_bundle(
    out: &Path,
    catalog: &Catalog,
    mut manifest: BackupManifest,
    etcd_snapshot: Option<&Path>,
    overwrite: bool,
) -> Result<BackupManifest> {
    if out.exists() && !overwrite {
        bail!("{} already exists (pass --overwrite)", out.display());
    }
    let parent = out.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let name = out
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("backup");
    let temp = parent.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        now_nanos()
    ));
    if temp.exists() {
        fs::remove_dir_all(&temp).ok();
    }
    fs::create_dir_all(&temp).with_context(|| format!("create {}", temp.display()))?;

    let result = (|| -> Result<BackupManifest> {
        let catalog_text = format!("{}\n", serde_json::to_string_pretty(catalog)?);
        let catalog_path = temp.join(CATALOG_FILE);
        fs::write(&catalog_path, catalog_text)
            .with_context(|| format!("write {}", catalog_path.display()))?;
        manifest.catalog = BackupFile::from_path(&catalog_path, CATALOG_FILE)?;

        if let Some(source) = etcd_snapshot {
            let target = temp.join(ETCD_FILE);
            fs::copy(source, &target)
                .with_context(|| format!("copy {} to {}", source.display(), target.display()))?;
            manifest.etcd = Some(BackupFile::from_path(&target, ETCD_FILE)?);
        }

        let manifest_path = temp.join(MANIFEST_FILE);
        fs::write(
            &manifest_path,
            format!("{}\n", serde_json::to_string_pretty(&manifest)?),
        )
        .with_context(|| format!("write {}", manifest_path.display()))?;

        let mut files = vec![
            (CATALOG_FILE.to_string(), sha256_file(&catalog_path)?),
            (MANIFEST_FILE.to_string(), sha256_file(&manifest_path)?),
        ];
        if manifest.etcd.is_some() {
            files.push((ETCD_FILE.to_string(), sha256_file(&temp.join(ETCD_FILE))?));
        }
        files.sort();
        let sums = files
            .into_iter()
            .map(|(path, sha)| format!("{sha}  {path}\n"))
            .collect::<String>();
        fs::write(temp.join(CHECKSUMS_FILE), sums)
            .with_context(|| format!("write {}", temp.join(CHECKSUMS_FILE).display()))?;

        if out.exists() {
            fs::remove_dir_all(out).with_context(|| format!("replace {}", out.display()))?;
        }
        fs::rename(&temp, out)
            .with_context(|| format!("publish {} to {}", temp.display(), out.display()))?;
        Ok(manifest.clone())
    })();

    if result.is_err() {
        fs::remove_dir_all(&temp).ok();
    }
    result
}

/// Read and verify a bundle, including every checksum and the catalog hash.
pub fn read_bundle(path: &Path) -> Result<LoadedBundle> {
    let root = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let sums_path = root.join(CHECKSUMS_FILE);
    let sums_text =
        fs::read_to_string(&sums_path).with_context(|| format!("read {}", sums_path.display()))?;
    let entries = parse_checksums(&sums_text)?;
    if entries.is_empty() {
        bail!("{} contains no checksums", sums_path.display());
    }
    let paths: BTreeSet<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    if !paths.contains(CATALOG_FILE) || !paths.contains(MANIFEST_FILE) {
        bail!(
            "{} must list both {} and {}",
            sums_path.display(),
            CATALOG_FILE,
            MANIFEST_FILE
        );
    }
    for entry in &entries {
        let file = safe_join(&root, &entry.path)?;
        if !file.is_file() {
            bail!("bundle file {} is missing", entry.path);
        }
        let actual = sha256_file(&file)?;
        if actual != entry.sha256 {
            bail!(
                "checksum mismatch for {}: expected {}, got {}",
                entry.path,
                entry.sha256,
                actual
            );
        }
    }
    let manifest_path = root.join(MANIFEST_FILE);
    let manifest: BackupManifest = serde_json::from_slice(
        &fs::read(&manifest_path).with_context(|| format!("read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("parse {}", manifest_path.display()))?;
    if manifest.format != BACKUP_FORMAT {
        bail!(
            "unsupported backup format {} (expected {})",
            manifest.format,
            BACKUP_FORMAT
        );
    }
    let mut seen_volumes = BTreeSet::new();
    for volume in &manifest.volumes {
        if volume.pvc.is_empty() || volume.snapshot.is_empty() {
            bail!("backup volume entries need a PVC and snapshot name");
        }
        if !seen_volumes.insert(volume.pvc.clone()) {
            bail!("duplicate backup volume for PVC {}", volume.pvc);
        }
        for (path, sha) in &volume.file_sha256 {
            if path.is_empty()
                || sha.len() != 64
                || !sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                bail!("invalid checksum entry for {}", volume.pvc);
            }
        }
    }
    let catalog_path = root.join(&manifest.catalog.path);
    let catalog_bytes =
        fs::read(&catalog_path).with_context(|| format!("read {}", catalog_path.display()))?;
    let catalog_sha = sha256_bytes(&catalog_bytes);
    if catalog_sha != manifest.catalog.sha256 {
        bail!(
            "catalog checksum mismatch: manifest says {}, file is {}",
            manifest.catalog.sha256,
            catalog_sha
        );
    }
    if catalog_bytes.len() as u64 != manifest.catalog.bytes {
        bail!(
            "catalog size mismatch: manifest says {}, file is {}",
            manifest.catalog.bytes,
            catalog_bytes.len()
        );
    }
    let catalog: Catalog = serde_json::from_slice(&catalog_bytes).context("parse catalog.json")?;
    if BackupCounts::of(&catalog) != manifest.counts {
        bail!("catalog object counts do not match the backup manifest");
    }
    if let Some(etcd) = &manifest.etcd {
        if !paths.contains(etcd.path.as_str()) {
            bail!("{} is not listed in SHA256SUMS", etcd.path);
        }
        let file = safe_join(&root, &etcd.path)?;
        let bytes = fs::metadata(&file)
            .with_context(|| format!("stat {}", file.display()))?
            .len();
        if bytes != etcd.bytes {
            bail!(
                "etcd snapshot size mismatch: manifest says {}, file is {}",
                etcd.bytes,
                bytes
            );
        }
        if sha256_file(&file)? != etcd.sha256 {
            bail!("etcd snapshot checksum mismatch");
        }
    }
    Ok(LoadedBundle {
        root,
        manifest,
        catalog,
    })
}

/// Verify a bundle without loading it for restore.
pub fn verify_bundle(path: &Path) -> Result<LoadedBundle> {
    let bundle = read_bundle(path)?;
    validate_catalog(&bundle.catalog)
        .is_ok()
        .then_some(())
        .context("restored catalog is not valid")?;
    Ok(bundle)
}

fn parse_checksums(text: &str) -> Result<Vec<ChecksumEntry>> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let sha = parts.next().unwrap_or_default();
        let path = parts.next().unwrap_or_default();
        if sha.len() != 64 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("invalid checksum on line {}", index + 1);
        }
        if path.is_empty() || path.contains('/') || path.contains('\\') || path == ".." {
            bail!("unsafe bundle path {path:?} on line {}", index + 1);
        }
        if !seen.insert(path.to_string()) {
            bail!("duplicate checksum entry for {path}");
        }
        entries.push(ChecksumEntry {
            sha256: sha.to_ascii_lowercase(),
            path: path.to_string(),
        });
    }
    Ok(entries)
}

fn safe_join(root: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == ".." {
        bail!("unsafe bundle path {name:?}");
    }
    Ok(root.join(name))
}

/// Select the requested catalog objects plus their direct dependencies.
///
/// An empty selection copies the whole catalog. A shard selection includes its cell and every
/// symbol/deployment/route hosted by that shard. Symbol, cell, market-data and order-system
/// selections expand their direct references so a merge validates without restoring unrelated
/// shards.
pub fn select_catalog(catalog: &Catalog, selection: &RestoreSelection) -> Result<Catalog> {
    if selection.is_empty() {
        return Ok(catalog.clone());
    }
    for name in &selection.cells {
        if !catalog.cells.contains_key(name) {
            bail!("backup has no cell {name}");
        }
    }
    for name in &selection.shards {
        if !catalog.shards.contains_key(name) {
            bail!("backup has no shard {name}");
        }
    }
    for name in &selection.symbols {
        if !catalog.symbols.contains_key(name) {
            bail!("backup has no symbol {name}");
        }
    }
    for name in &selection.market_data_services {
        if !catalog.market_data_services.contains_key(name) {
            bail!("backup has no market-data service {name}");
        }
    }
    for name in &selection.order_systems {
        if !catalog.order_systems.contains_key(name) {
            bail!("backup has no order system {name}");
        }
    }

    let mut cells = selection.cells.clone();
    let mut shards = selection.shards.clone();
    let mut symbols = selection.symbols.clone();
    let mut market_data_services = selection.market_data_services.clone();
    let mut order_systems = selection.order_systems.clone();

    for name in &selection.symbols {
        if let Some(symbol) = catalog.symbols.get(name) {
            for deployment in catalog
                .deployments
                .values()
                .filter(|deployment| deployment.spec.symbol_ref == symbol.name)
            {
                shards.insert(deployment.spec.shard_ref.clone());
            }
        }
    }

    for service in catalog.market_data_services.values() {
        if !market_data_services.contains(&service.name) {
            continue;
        }
        cells.insert(service.spec.cell.clone());
        for group in &service.spec.feed_groups {
            for symbol in &group.symbols {
                symbols.insert(symbol.clone());
            }
        }
    }

    for system in catalog.order_systems.values() {
        if !order_systems.contains(&system.name) {
            continue;
        }
        cells.insert(system.spec.cell.clone());
        for target in &system.spec.targets {
            symbols.insert(target.symbol.clone());
        }
    }

    let explicit_cells = selection.cells.clone();
    loop {
        let before = (
            cells.len(),
            shards.len(),
            symbols.len(),
            market_data_services.len(),
            order_systems.len(),
        );
        for shard_name in shards.clone() {
            if let Some(shard) = catalog.shards.get(&shard_name) {
                cells.insert(shard.spec.cell.clone());
            }
            for deployment in catalog
                .deployments
                .values()
                .filter(|deployment| deployment.spec.shard_ref == shard_name)
            {
                symbols.insert(deployment.spec.symbol_ref.clone());
            }
        }
        for cell_name in explicit_cells.clone() {
            for shard in catalog
                .shards
                .values()
                .filter(|shard| shard.spec.cell == cell_name)
            {
                shards.insert(shard.name.clone());
            }
            for route in catalog
                .routes
                .values()
                .filter(|route| route.spec.cell_ref == cell_name)
            {
                symbols.insert(route.spec.symbol_ref.clone());
            }
        }
        for symbol_name in symbols.clone() {
            for deployment in catalog
                .deployments
                .values()
                .filter(|deployment| deployment.spec.symbol_ref == symbol_name)
            {
                shards.insert(deployment.spec.shard_ref.clone());
            }
        }
        for service in catalog.market_data_services.values() {
            if market_data_services.contains(&service.name)
                || explicit_cells.contains(&service.spec.cell)
            {
                market_data_services.insert(service.name.clone());
                cells.insert(service.spec.cell.clone());
                for group in &service.spec.feed_groups {
                    symbols.extend(group.symbols.iter().cloned());
                }
            }
        }
        for system in catalog.order_systems.values() {
            if order_systems.contains(&system.name) || explicit_cells.contains(&system.spec.cell) {
                order_systems.insert(system.name.clone());
                cells.insert(system.spec.cell.clone());
                symbols.extend(
                    system
                        .spec
                        .targets
                        .iter()
                        .map(|target| target.symbol.clone()),
                );
            }
        }
        if (
            cells.len(),
            shards.len(),
            symbols.len(),
            market_data_services.len(),
            order_systems.len(),
        ) == before
        {
            break;
        }
    }

    let mut selected = Catalog::default();
    for (name, cell) in &catalog.cells {
        if cells.contains(name) {
            selected.cells.insert(name.clone(), cell.clone());
        }
    }
    for (name, shard) in &catalog.shards {
        if shards.contains(name) {
            selected.shards.insert(name.clone(), shard.clone());
        }
    }
    for (name, symbol) in &catalog.symbols {
        if symbols.contains(name) {
            selected.symbols.insert(name.clone(), symbol.clone());
        }
    }
    for (name, deployment) in &catalog.deployments {
        if shards.contains(&deployment.spec.shard_ref) {
            selected
                .deployments
                .insert(name.clone(), deployment.clone());
        }
    }
    for (name, route) in &catalog.routes {
        if symbols.contains(&route.spec.symbol_ref)
            || cells.contains(&route.spec.cell_ref)
            || selected
                .deployments
                .contains_key(&route.spec.active_deployment_ref)
        {
            selected.routes.insert(name.clone(), route.clone());
        }
    }
    for (name, service) in &catalog.market_data_services {
        if market_data_services.contains(name) {
            selected
                .market_data_services
                .insert(name.clone(), service.clone());
        }
    }
    for (name, system) in &catalog.order_systems {
        if order_systems.contains(name) {
            selected.order_systems.insert(name.clone(), system.clone());
        }
    }
    Ok(selected)
}

/// Merge selected backup objects over the current catalog without deleting anything.
pub fn merge_catalog(current: &Catalog, selected: &Catalog) -> Catalog {
    let selected = selected.without_status();
    let mut merged = current.clone();
    for (name, cell) in &selected.cells {
        merged.cells.insert(name.clone(), cell.clone());
    }
    for (name, shard) in &selected.shards {
        let mut object = shard.clone();
        if let Some(existing) = current.shards.get(name) {
            object.status = existing.status.clone();
        }
        merged.shards.insert(name.clone(), object);
    }
    for (name, symbol) in &selected.symbols {
        let mut object = symbol.clone();
        if let Some(existing) = current.symbols.get(name) {
            object.status = existing.status.clone();
        }
        merged.symbols.insert(name.clone(), object);
    }
    for (name, deployment) in &selected.deployments {
        let mut object = deployment.clone();
        if let Some(existing) = current.deployments.get(name) {
            object.status = existing.status.clone();
        }
        merged.deployments.insert(name.clone(), object);
    }
    for (name, route) in &selected.routes {
        let mut object = route.clone();
        if let Some(existing) = current.routes.get(name) {
            object.status = existing.status.clone();
        }
        merged.routes.insert(name.clone(), object);
    }
    for (name, service) in &selected.market_data_services {
        let mut object = service.clone();
        if let Some(existing) = current.market_data_services.get(name) {
            object.status = existing.status.clone();
        }
        merged.market_data_services.insert(name.clone(), object);
    }
    for (name, system) in &selected.order_systems {
        let mut object = system.clone();
        if let Some(existing) = current.order_systems.get(name) {
            object.status = existing.status.clone();
        }
        merged.order_systems.insert(name.clone(), object);
    }
    merged
}

/// Create snapshots for every PVC carrying the shard label.
pub async fn create_volume_snapshots(
    client: &Client,
    catalog: &Catalog,
    options: &SnapshotOptions,
) -> Result<SnapshotSet> {
    if !catalog.shards.contains_key(&options.shard) {
        bail!("unknown shard {}", options.shard);
    }
    if options.mode == BackupMode::VolumeGroup
        && options.volume_group_snapshot_class.trim().is_empty()
    {
        bail!("volume-group mode requires --volume-group-snapshot-class");
    }
    if options.mode == BackupMode::Logical {
        bail!("logical backups are not implemented; use volume snapshots or an engine replay");
    }

    let pvcs = list_shard_pvcs(client, &options.namespace, &options.shard).await?;
    if pvcs.is_empty() {
        bail!(
            "no PVCs with {}={} in namespace {}",
            SHARD_LABEL,
            options.shard,
            options.namespace
        );
    }
    let (volume_group_snapshot, mut volumes) = match options.mode {
        BackupMode::VolumeGroup => {
            let (name, volumes) = create_group_snapshot(client, options, &pvcs).await?;
            (Some(name), volumes)
        }
        BackupMode::Volume => (
            None,
            create_per_volume_snapshots(client, options, &pvcs).await?,
        ),
        BackupMode::Logical => unreachable!("logical mode is rejected above"),
    };
    for volume in &mut volumes {
        let info = wait_snapshot_ready(
            client,
            &options.namespace,
            &volume.snapshot,
            options.timeout,
        )
        .await?;
        volume.ready = info.ready;
        volume.restore_size = info.restore_size;
    }
    if options.verify {
        for (index, volume) in volumes.iter_mut().enumerate() {
            let pvc = pvcs
                .iter()
                .find(|pvc| pvc_name(pvc) == volume.pvc)
                .with_context(|| format!("source PVC {} disappeared", volume.pvc))?;
            let files = verify_snapshot_restore(client, options, volume, pvc, index).await?;
            volume.file_sha256 = files;
            volume.verified_at = Some(now_epoch());
        }
    }
    Ok(SnapshotSet {
        volume_group_snapshot,
        volumes,
    })
}

async fn list_shard_pvcs(
    client: &Client,
    namespace: &str,
    shard: &str,
) -> Result<Vec<PersistentVolumeClaim>> {
    let api = Api::<PersistentVolumeClaim>::namespaced(client.clone(), namespace);
    let selector = format!("{SHARD_LABEL}={shard}");
    let pvcs = api
        .list(&ListParams::default().labels(&selector))
        .await
        .with_context(|| format!("list PVCs with {selector}"))?;
    Ok(pvcs.items)
}

async fn create_group_snapshot(
    client: &Client,
    options: &SnapshotOptions,
    pvcs: &[PersistentVolumeClaim],
) -> Result<(String, Vec<VolumeBackup>)> {
    let name = dns_name(&format!("vg-{}", options.backup_id));
    let api = group_snapshot_api(client, &options.namespace);
    let mut spec = json!({
        "source": { "selector": { "matchLabels": { SHARD_LABEL: options.shard } } }
    });
    if !options.volume_group_snapshot_class.trim().is_empty() {
        spec["volumeGroupSnapshotClassName"] = json!(options.volume_group_snapshot_class);
    }
    let object: DynamicObject = serde_json::from_value(json!({
        "apiVersion": "groupsnapshot.storage.k8s.io/v1alpha1",
        "kind": "VolumeGroupSnapshot",
        "metadata": {
            "name": name,
            "namespace": options.namespace,
            "labels": {
                SHARD_LABEL: options.shard,
                BACKUP_LABEL: label_value(&options.backup_id),
            },
        },
        "spec": spec,
    }))?;
    // Kubernetes creation timestamps are commonly second-granular; allow a one-second margin
    // so a snapshot created in the same second as the group request is not filtered out.
    let started = now_nanos().saturating_sub(1_000_000_000) as i128;
    api.create(&PostParams::default(), &object)
        .await
        .with_context(|| format!("create VolumeGroupSnapshot {name}"))?;
    let refs =
        wait_group_snapshot_ready(client, &options.namespace, &name, options.timeout).await?;

    let mut by_pvc = BTreeMap::new();
    if refs.is_empty() {
        let wanted: BTreeSet<String> = pvcs.iter().map(pvc_name).collect();
        by_pvc = newest_snapshots_for_pvcs(client, &options.namespace, &wanted, started).await?;
    } else {
        for snapshot in refs {
            if let Some(object) = snapshot_api(client, &options.namespace)
                .get_opt(&snapshot)
                .await?
                && let Some(pvc) = snapshot_pvc(&object)
            {
                by_pvc.insert(pvc, snapshot);
            }
        }
    }
    let mut volumes = Vec::new();
    for pvc in pvcs {
        let pvc_name = pvc_name(pvc);
        let snapshot = by_pvc.get(&pvc_name).cloned().unwrap_or_default();
        if snapshot.is_empty() {
            bail!("VolumeGroupSnapshot {name} has no snapshot for PVC {pvc_name}");
        }
        volumes.push(volume_from_pvc(pvc, snapshot));
    }
    Ok((name, volumes))
}

async fn create_per_volume_snapshots(
    client: &Client,
    options: &SnapshotOptions,
    pvcs: &[PersistentVolumeClaim],
) -> Result<Vec<VolumeBackup>> {
    let api = snapshot_api(client, &options.namespace);
    let mut volumes = Vec::new();
    for (index, pvc) in pvcs.iter().enumerate() {
        let pvc_name = pvc_name(pvc);
        let name = dns_name(&format!("snap-{}-{index}", options.backup_id));
        let mut spec = json!({
            "source": { "persistentVolumeClaimName": pvc_name },
        });
        if !options.volume_snapshot_class.trim().is_empty() {
            spec["volumeSnapshotClassName"] = json!(options.volume_snapshot_class);
        }
        let object: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "snapshot.storage.k8s.io/v1",
            "kind": "VolumeSnapshot",
            "metadata": {
                "name": name,
                "namespace": options.namespace,
                "labels": {
                    SHARD_LABEL: options.shard,
                    BACKUP_LABEL: label_value(&options.backup_id),
                },
            },
            "spec": spec,
        }))?;
        api.create(&PostParams::default(), &object)
            .await
            .with_context(|| format!("create VolumeSnapshot {name} for {pvc_name}"))?;
        volumes.push(volume_from_pvc(pvc, name));
    }
    Ok(volumes)
}

fn volume_from_pvc(pvc: &PersistentVolumeClaim, snapshot: String) -> VolumeBackup {
    VolumeBackup {
        pvc: pvc_name(pvc),
        snapshot,
        ready: false,
        restore_size: String::new(),
        requested_size: pvc
            .spec
            .as_ref()
            .and_then(|spec| spec.resources.as_ref())
            .and_then(|resources| resources.requests.as_ref())
            .and_then(|requests| requests.get("storage"))
            .map(|quantity| quantity.0.clone())
            .unwrap_or_default(),
        storage_class: pvc
            .spec
            .as_ref()
            .and_then(|spec| spec.storage_class_name.clone())
            .unwrap_or_default(),
        access_modes: pvc
            .spec
            .as_ref()
            .and_then(|spec| spec.access_modes.clone())
            .unwrap_or_default(),
        file_sha256: BTreeMap::new(),
        verified_at: None,
    }
}

async fn newest_snapshots_for_pvcs(
    client: &Client,
    namespace: &str,
    pvcs: &BTreeSet<String>,
    since_nanos: i128,
) -> Result<BTreeMap<String, String>> {
    let list = snapshot_api(client, namespace)
        .list(&ListParams::default())
        .await
        .with_context(|| "list VolumeSnapshots for the group backup")?;
    let mut newest: BTreeMap<String, (i128, String)> = BTreeMap::new();
    for snapshot in list.items {
        let Some(pvc) = snapshot_pvc(&snapshot) else {
            continue;
        };
        if !pvcs.contains(&pvc) {
            continue;
        }
        let name = snapshot_name(&snapshot).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let created = snapshot
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|time| time.0.as_nanosecond())
            .unwrap_or(0);
        if created < since_nanos {
            continue;
        }
        let replace = newest
            .get(&pvc)
            .is_none_or(|(current, _)| created >= *current);
        if replace {
            newest.insert(pvc, (created, name));
        }
    }
    Ok(newest
        .into_iter()
        .map(|(pvc, (_, snapshot))| (pvc, snapshot))
        .collect())
}

fn snapshot_pvc(snapshot: &DynamicObject) -> Option<String> {
    snapshot
        .data
        .get("spec")
        .and_then(|spec| spec.get("source"))
        .and_then(|source| source.get("persistentVolumeClaimName"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn snapshot_name(snapshot: &DynamicObject) -> Option<String> {
    snapshot.metadata.name.clone()
}

struct SnapshotInfo {
    ready: bool,
    restore_size: String,
}

fn group_snapshot_refs(status: Option<&Value>) -> Vec<String> {
    let mut refs = Vec::new();
    let Some(list) = status
        .and_then(|status| status.get("volumeSnapshotRefList"))
        .and_then(Value::as_array)
    else {
        return refs;
    };
    for entry in list {
        let name = entry.as_str().map(ToString::to_string).or_else(|| {
            entry
                .get("name")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        });
        if let Some(name) = name
            && !name.is_empty()
        {
            refs.push(name);
        }
    }
    refs
}

async fn wait_snapshot_ready(
    client: &Client,
    namespace: &str,
    name: &str,
    timeout: Duration,
) -> Result<SnapshotInfo> {
    let api = snapshot_api(client, namespace);
    let started = Instant::now();
    loop {
        if let Some(snapshot) = api.get_opt(name).await? {
            let status = snapshot.data.get("status");
            if let Some(error) = status
                .and_then(|status| status.get("error"))
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                && !error.is_empty()
            {
                bail!("VolumeSnapshot {name} failed: {error}");
            }
            let ready = status
                .and_then(|status| status.get("readyToUse"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if ready {
                return Ok(SnapshotInfo {
                    ready,
                    restore_size: status
                        .and_then(|status| status.get("restoreSize"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                });
            }
        } else {
            bail!("VolumeSnapshot {name} disappeared while waiting for readiness");
        }
        if started.elapsed() > timeout {
            bail!("timed out waiting for VolumeSnapshot {name} to become ready");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn wait_group_snapshot_ready(
    client: &Client,
    namespace: &str,
    name: &str,
    timeout: Duration,
) -> Result<Vec<String>> {
    let api = group_snapshot_api(client, namespace);
    let started = Instant::now();
    loop {
        if let Some(snapshot) = api.get_opt(name).await? {
            let status = snapshot.data.get("status");
            if let Some(error) = status
                .and_then(|status| status.get("error"))
                .and_then(|error| {
                    error
                        .as_str()
                        .or_else(|| error.get("message").and_then(Value::as_str))
                })
                && !error.is_empty()
            {
                bail!("VolumeGroupSnapshot {name} failed: {error}");
            }
            if status
                .and_then(|status| status.get("readyToUse"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Ok(group_snapshot_refs(status));
            }
        } else {
            bail!("VolumeGroupSnapshot {name} disappeared while waiting for readiness");
        }
        if started.elapsed() > timeout {
            bail!("timed out waiting for VolumeGroupSnapshot {name}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn verify_snapshot_restore(
    client: &Client,
    options: &SnapshotOptions,
    volume: &VolumeBackup,
    source_pvc: &PersistentVolumeClaim,
    index: usize,
) -> Result<BTreeMap<String, String>> {
    let restore = VolumeRestoreOptions {
        target_namespace: options.namespace.clone(),
        prefix: format!("verify-{}-{index}-", options.backup_id),
        drill: true,
        keep: false,
        verify_image: options.verify_image.clone(),
        timeout: options.timeout,
    };
    let _ = source_pvc;
    let pvc = create_restored_pvc_by_spec(client, &restore, volume, index).await?;
    let pvc_name = pvc_name(&pvc);
    let result = hash_restored_pvc(
        client,
        &restore.target_namespace,
        &pvc_name,
        &options.verify_image,
        options.timeout,
    )
    .await;
    delete_pvc(client, &restore.target_namespace, &pvc_name).await;
    result
}

/// Restore snapshots from a manifest into new PVCs and optionally hash their contents.
pub async fn restore_volumes(
    client: &Client,
    manifest: &BackupManifest,
    options: &VolumeRestoreOptions,
) -> Result<Vec<String>> {
    let mut created = Vec::new();
    for (index, volume) in manifest.volumes.iter().enumerate() {
        let pvc = match create_restored_pvc_by_spec(client, options, volume, index).await {
            Ok(pvc) => pvc,
            Err(error) => {
                cleanup_created(client, options, &created).await;
                return Err(error);
            }
        };
        let name = pvc_name(&pvc);
        created.push(name.clone());
        if !volume.file_sha256.is_empty() {
            let actual = hash_restored_pvc(
                client,
                &options.target_namespace,
                &name,
                &options.verify_image,
                options.timeout,
            )
            .await;
            match actual {
                Ok(actual) => {
                    if actual != volume.file_sha256 {
                        cleanup_created(client, options, &created).await;
                        bail!("restored checksum mismatch for {}", volume.pvc);
                    }
                }
                Err(error) => {
                    cleanup_created(client, options, &created).await;
                    return Err(error);
                }
            }
        }
    }
    if options.drill && !options.keep {
        cleanup_created(client, options, &created).await;
    }
    Ok(created)
}

async fn create_restored_pvc_by_spec(
    client: &Client,
    options: &VolumeRestoreOptions,
    volume: &VolumeBackup,
    index: usize,
) -> Result<PersistentVolumeClaim> {
    let name = dns_name(&format!("{}{}-{}", options.prefix, volume.pvc, index));
    let storage = if !volume.restore_size.is_empty() {
        volume.restore_size.clone()
    } else if !volume.requested_size.is_empty() {
        volume.requested_size.clone()
    } else {
        "1Gi".to_string()
    };
    let mut spec = json!({
        "accessModes": if volume.access_modes.is_empty() { vec!["ReadWriteOnce".to_string()] } else { volume.access_modes.clone() },
        "resources": { "requests": { "storage": storage } },
        "dataSource": {
            "name": volume.snapshot,
            "kind": "VolumeSnapshot",
            "apiGroup": "snapshot.storage.k8s.io",
        }
    });
    if !volume.storage_class.is_empty() {
        spec["storageClassName"] = json!(volume.storage_class);
    }
    let pvc: PersistentVolumeClaim = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": name,
            "namespace": options.target_namespace,
            "labels": { DRILL_LABEL: "true", BACKUP_LABEL: label_value(&volume.snapshot) },
        },
        "spec": spec,
    }))?;
    let api = Api::<PersistentVolumeClaim>::namespaced(client.clone(), &options.target_namespace);
    let created = api
        .create(&PostParams::default(), &pvc)
        .await
        .with_context(|| format!("create restored PVC {name} in {}", options.target_namespace))?;
    wait_pvc_bound(client, &options.target_namespace, &name, options.timeout).await?;
    Ok(created)
}

async fn wait_pvc_bound(
    client: &Client,
    namespace: &str,
    name: &str,
    timeout: Duration,
) -> Result<()> {
    let api = Api::<PersistentVolumeClaim>::namespaced(client.clone(), namespace);
    let started = Instant::now();
    loop {
        if let Some(pvc) = api.get_opt(name).await?
            && pvc
                .status
                .as_ref()
                .and_then(|status| status.phase.as_deref())
                == Some("Bound")
        {
            return Ok(());
        }
        if started.elapsed() > timeout {
            bail!("timed out waiting for restored PVC {name} to bind");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn hash_restored_pvc(
    client: &Client,
    namespace: &str,
    pvc: &str,
    image: &str,
    timeout: Duration,
) -> Result<BTreeMap<String, String>> {
    let name = dns_name(&format!("hash-{pvc}-{}", now_nanos()));
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": { DRILL_LABEL: "true" },
        },
        "spec": {
            "restartPolicy": "Never",
            "automountServiceAccountToken": false,
            "securityContext": {
                "runAsNonRoot": true,
                "runAsUser": 10001,
                "runAsGroup": 10001,
                "fsGroup": 10001,
                "seccompProfile": { "type": "RuntimeDefault" },
            },
            "containers": [{
                "name": "checksum",
                "image": image,
                "command": ["sh", "-c", "find /data -type f -exec sha256sum {} \\; | sort"],
                "volumeMounts": [
                    { "name": "data", "mountPath": "/data", "readOnly": true },
                    { "name": "tmp", "mountPath": "/tmp" },
                ],
                "securityContext": {
                    "allowPrivilegeEscalation": false,
                    "readOnlyRootFilesystem": true,
                    "capabilities": { "drop": ["ALL"] },
                },
                "resources": {
                    "requests": { "cpu": "10m", "memory": "32Mi" },
                    "limits": { "cpu": "250m", "memory": "128Mi" },
                },
            }],
            "volumes": [
                {
                    "name": "data",
                    "persistentVolumeClaim": { "claimName": pvc },
                },
                { "name": "tmp", "emptyDir": {} },
            ],
        },
    }))?;
    let api = Api::<Pod>::namespaced(client.clone(), namespace);
    api.create(&PostParams::default(), &pod)
        .await
        .with_context(|| format!("create checksum pod {name}"))?;
    let outcome = wait_pod_finished(client, namespace, &name, timeout).await;
    let logs = api
        .logs(&name, &LogParams::default())
        .await
        .unwrap_or_default();
    let _ = api.delete(&name, &DeleteParams::default()).await;
    match outcome {
        Ok(true) => parse_hash_log(&logs),
        Ok(false) => bail!("checksum pod {name} failed:\n{logs}"),
        Err(error) => {
            let _ = api.delete(&name, &DeleteParams::default()).await;
            Err(error)
        }
    }
}

async fn wait_pod_finished(
    client: &Client,
    namespace: &str,
    name: &str,
    timeout: Duration,
) -> Result<bool> {
    let api = Api::<Pod>::namespaced(client.clone(), namespace);
    let started = Instant::now();
    loop {
        if let Some(pod) = api.get_opt(name).await? {
            match pod
                .status
                .as_ref()
                .and_then(|status| status.phase.as_deref())
            {
                Some("Succeeded") => return Ok(true),
                Some("Failed") => return Ok(false),
                _ => {}
            }
        }
        if started.elapsed() > timeout {
            bail!("timed out waiting for checksum pod {name}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn parse_hash_log(logs: &str) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    for line in logs.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((sha, path)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if sha.len() != 64 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let path = path.trim();
        let path = path.strip_prefix("/data/").unwrap_or(path);
        files.insert(path.to_string(), sha.to_ascii_lowercase());
    }
    Ok(files)
}

async fn cleanup_created(client: &Client, options: &VolumeRestoreOptions, pvcs: &[String]) {
    for pvc in pvcs {
        delete_pvc(client, &options.target_namespace, pvc).await;
    }
}

async fn delete_pvc(client: &Client, namespace: &str, name: &str) {
    let api = Api::<PersistentVolumeClaim>::namespaced(client.clone(), namespace);
    let _ = api.delete(name, &DeleteParams::default()).await;
}

fn pvc_name(pvc: &PersistentVolumeClaim) -> String {
    pvc.metadata.name.clone().unwrap_or_default()
}

fn snapshot_api(client: &Client, namespace: &str) -> Api<DynamicObject> {
    dynamic_api(
        client,
        namespace,
        "snapshot.storage.k8s.io",
        "v1",
        "VolumeSnapshot",
        "volumesnapshots",
    )
}

fn group_snapshot_api(client: &Client, namespace: &str) -> Api<DynamicObject> {
    dynamic_api(
        client,
        namespace,
        "groupsnapshot.storage.k8s.io",
        "v1alpha1",
        "VolumeGroupSnapshot",
        "volumegroupsnapshots",
    )
}

fn dynamic_api(
    client: &Client,
    namespace: &str,
    group: &str,
    version: &str,
    kind: &str,
    plural: &str,
) -> Api<DynamicObject> {
    let resource = ApiResource {
        group: group.to_string(),
        version: version.to_string(),
        api_version: format!("{group}/{version}"),
        kind: kind.to_string(),
        plural: plural.to_string(),
    };
    Api::namespaced_with(client.clone(), namespace, &resource)
}

fn label_value(value: &str) -> String {
    let mut value = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    if value.len() > 63 {
        value.truncate(63);
    }
    value.trim_matches('-').to_string()
}

fn dns_name(value: &str) -> String {
    let mut name = value
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if name.len() > 63 {
        let mut end = 63;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
        name = name.trim_end_matches('-').to_string();
    }
    if name.is_empty() {
        "backup".to_string()
    } else {
        name
    }
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

/// Build a restore plan without touching the cluster.
pub fn plan_restore(bundle: &LoadedBundle, selection: &RestoreSelection) -> Result<RestorePlan> {
    let selected = select_catalog(&bundle.catalog, selection)?;
    let counts = BackupCounts::of(&selected);
    if counts.shards == 0
        && counts.symbols == 0
        && counts.market_data_services == 0
        && counts.order_systems == 0
    {
        bail!("selection does not include a shard, symbol, market-data service or order system");
    }
    let report = validate_catalog(&selected);
    if !report.is_ok() {
        bail!(
            "selected backup catalog is not valid:\n{}",
            report.to_text()
        );
    }
    Ok(RestorePlan {
        counts,
        volumes: bundle.manifest.volumes.len(),
        crd_sha256: bundle.manifest.catalog.sha256.clone(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    pub counts: BackupCounts,
    pub volumes: usize,
    pub crd_sha256: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::demo_catalog;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "matchengine-{name}-{}-{}",
            std::process::id(),
            now_nanos()
        ))
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn bundle_round_trips_and_detects_tampering() {
        let dir = temp_dir("backup-roundtrip");
        let catalog = demo_catalog();
        let shard = catalog.shards.values().next().unwrap().clone();
        let manifest = BackupManifest::new(
            "test-1".to_string(),
            "match-eu-1",
            &shard,
            BackupMode::VolumeGroup,
            &catalog,
            "vg-class",
            "snap-class",
        );
        let etcd = dir.with_extension("etcd.db");
        fs::write(&etcd, b"etcd snapshot bytes").unwrap();
        let written = write_bundle(&dir, &catalog, manifest, Some(&etcd), false).unwrap();
        assert_eq!(written.counts, BackupCounts::of(&catalog));
        let loaded = read_bundle(&dir).unwrap();
        assert_eq!(loaded.manifest.id, "test-1");
        assert_eq!(loaded.catalog.shards.len(), catalog.shards.len());
        assert!(loaded.manifest.etcd.is_some());
        fs::remove_file(&etcd).ok();

        let catalog_path = dir.join(CATALOG_FILE);
        let mut bytes = fs::read(&catalog_path).unwrap();
        bytes.push(b' ');
        fs::write(&catalog_path, bytes).unwrap();
        assert!(read_bundle(&dir).is_err());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn selective_restore_expands_dependencies_and_keeps_unrelated_objects() {
        let catalog = demo_catalog();
        let mut selection = RestoreSelection::default();
        selection.shards.insert("fx-eu-01".to_string());
        let selected = select_catalog(&catalog, &selection).unwrap();
        assert!(selected.shards.contains_key("fx-eu-01"));
        assert!(!selected.shards.contains_key("me-btcusdt"));
        assert!(selected.cells.contains_key("eu-1"));
        assert!(
            selected
                .deployments
                .values()
                .any(|deployment| deployment.spec.shard_ref == "fx-eu-01")
        );

        let mut current = catalog.clone();
        current.shards.get_mut("fx-eu-01").unwrap().spec.image = "changed".to_string();
        let merged = merge_catalog(&current, &selected);
        assert_eq!(
            merged.shards.get("fx-eu-01").unwrap().spec.image,
            catalog.shards.get("fx-eu-01").unwrap().spec.image
        );
        assert!(merged.shards.contains_key("me-btcusdt"));
        assert_eq!(merged.shards.len(), current.shards.len());
    }

    #[test]
    fn restore_plan_rejects_an_empty_selection() {
        let dir = temp_dir("backup-plan");
        let catalog = demo_catalog();
        let shard = catalog.shards.values().next().unwrap().clone();
        let manifest = BackupManifest::new(
            "test-plan".to_string(),
            "match-eu-1",
            &shard,
            BackupMode::VolumeGroup,
            &catalog,
            "vg-class",
            "snap-class",
        );
        write_bundle(&dir, &catalog, manifest, None, false).unwrap();
        let bundle = read_bundle(&dir).unwrap();
        let mut selection = RestoreSelection::default();
        selection
            .market_data_services
            .insert("missing-service".to_string());
        assert!(plan_restore(&bundle, &selection).is_err());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn parse_hash_log_strips_the_mount_prefix() {
        let log =
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789  /data/log/state\n";
        let files = parse_hash_log(log).unwrap();
        assert_eq!(
            files.get("log/state").map(String::as_str),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );
    }
}

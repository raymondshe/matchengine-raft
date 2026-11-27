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
use hmac::{Hmac, Mac};
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Pod};
use kube::Client;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, ListParams, LogParams, Patch, PatchParams,
    PostParams,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub use crate::model::BackupMode;
use crate::model::SymbolShard;
use crate::store::{Catalog, now_epoch};
use crate::validate::validate_catalog;

pub const BACKUP_FORMAT: &str = "matchengine-backup/v1";
pub const DRILL_FORMAT: &str = "matchengine-backup-drill/v1";
pub const MANIFEST_FILE: &str = "backup.json";
pub const CATALOG_FILE: &str = "catalog.json";
pub const CHECKSUMS_FILE: &str = "SHA256SUMS";
pub const ETCD_FILE: &str = "etcd.snapshot.db";
pub const DRILL_SUFFIX: &str = ".drill.json";
pub const DRILL_SIGNATURE_SUFFIX: &str = ".drill.sig";
pub const DEFAULT_MAX_BACKUP_AGE_SECS: u64 = 93_600;
pub const DEFAULT_MAX_DRILL_AGE_SECS: u64 = 691_200;
pub const DEFAULT_MAX_STATUS_AGE_SECS: u64 = 7_200;
pub const MIN_DRILL_KEY_BYTES: usize = 32;
pub const BACKUP_STATUS_FORMAT: &str = "matchengine-backup-status/v1";
pub const BACKUP_STATUS_CONFIGMAP: &str = "match-backup-status";
pub const BACKUP_STATUS_NAMESPACE: &str = "match-control";

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
    #[serde(default)]
    pub trading_terminals: usize,
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
            trading_terminals: catalog.trading_terminals.len(),
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
    pub trading_terminals: BTreeSet<String>,
}

impl RestoreSelection {
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
            && self.shards.is_empty()
            && self.symbols.is_empty()
            && self.market_data_services.is_empty()
            && self.order_systems.is_empty()
            && self.trading_terminals.is_empty()
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

/// A signed record that a bundle passed a restore drill.
///
/// The signature is HMAC-SHA256 over the exact `drill.json` bytes, written next to it as
/// `<bundle-id>.drill.sig`. The signing key is mounted from a Secret and never enters the bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DrillAttestation {
    pub format: String,
    pub bundle_id: String,
    pub namespace: String,
    pub shard: String,
    pub bundle_mode: BackupMode,
    pub bundle_created_at: u64,
    pub verified_at: u64,
    pub checksums_sha256: String,
    pub catalog_sha256: String,
    pub volumes_requested: usize,
    pub volumes_verified: usize,
    pub drill_kind: String,
    pub result: String,
    pub signer: String,
}

impl DrillAttestation {
    pub fn is_catalog_drill(&self) -> bool {
        self.drill_kind == "catalog"
    }
}

/// Summary of the newest uploaded bundle and signed drill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FreshnessReport {
    pub ok: bool,
    pub bundle_id: String,
    pub bundle_shard: String,
    pub bundle_namespace: String,
    pub bundle_created_at: u64,
    pub bundle_age_secs: u64,
    pub bundle_volumes: usize,
    pub catalog_sha256: String,
    pub drill_path: String,
    pub drill_bundle_id: String,
    pub drill_kind: String,
    pub drill_verified_at: u64,
    pub drill_age_secs: u64,
    pub drill_volumes_verified: usize,
    pub drill_signer: String,
}

/// A successful status check published by the backup/status Jobs for the manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedBackupStatus {
    pub format: String,
    pub published_at: u64,
    pub max_backup_age_secs: u64,
    pub max_drill_age_secs: u64,
    pub report: FreshnessReport,
}

impl PublishedBackupStatus {
    pub fn new(report: FreshnessReport, max_backup_age_secs: u64, max_drill_age_secs: u64) -> Self {
        Self {
            format: BACKUP_STATUS_FORMAT.to_string(),
            published_at: now_epoch(),
            max_backup_age_secs,
            max_drill_age_secs,
            report,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupBundleView {
    pub id: String,
    pub shard: String,
    pub namespace: String,
    pub created_at: u64,
    pub age_secs: u64,
    pub volumes: usize,
    pub catalog_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupDrillView {
    pub bundle_id: String,
    pub kind: String,
    pub verified_at: u64,
    pub age_secs: u64,
    pub volumes_verified: usize,
    pub signer: String,
    pub path: String,
}

/// Manager-facing view with ages recomputed against the current clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupStatusView {
    pub shard: String,
    pub configured: bool,
    pub status: String,
    pub ok: bool,
    pub message: String,
    pub published_at: Option<u64>,
    pub checked_age_secs: Option<u64>,
    pub max_backup_age_secs: Option<u64>,
    pub max_drill_age_secs: Option<u64>,
    pub reasons: Vec<String>,
    pub bundle: Option<BackupBundleView>,
    pub drill: Option<BackupDrillView>,
    pub source: String,
}

/// Build the manager-facing backup view from the last published record.
pub fn view_backup_status(
    shard: &str,
    record: Option<&PublishedBackupStatus>,
    now: u64,
) -> BackupStatusView {
    let source = format!("{BACKUP_STATUS_NAMESPACE}/{BACKUP_STATUS_CONFIGMAP}#{shard}");
    let Some(record) = record else {
        return BackupStatusView {
            shard: shard.to_string(),
            configured: false,
            status: "not-configured".to_string(),
            ok: false,
            message: "No backup status has been published. Apply deploy/backup.yaml and run the match-backup Job."
                .to_string(),
            published_at: None,
            checked_age_secs: None,
            max_backup_age_secs: None,
            max_drill_age_secs: None,
            reasons: Vec::new(),
            bundle: None,
            drill: None,
            source,
        };
    };
    let report = &record.report;
    let bundle_age_secs = now.saturating_sub(report.bundle_created_at);
    let drill_age_secs = now.saturating_sub(report.drill_verified_at);
    let checked_age_secs = now.saturating_sub(record.published_at);
    let mut reasons = Vec::new();
    if bundle_age_secs > record.max_backup_age_secs {
        reasons.push(format!(
            "newest bundle is {} old; limit is {}",
            human_age(bundle_age_secs),
            human_age(record.max_backup_age_secs)
        ));
    }
    if drill_age_secs > record.max_drill_age_secs {
        reasons.push(format!(
            "newest signed drill is {} old; limit is {}",
            human_age(drill_age_secs),
            human_age(record.max_drill_age_secs)
        ));
    }
    if checked_age_secs > DEFAULT_MAX_STATUS_AGE_SECS {
        reasons.push(format!(
            "status has not been refreshed for {}; expected at least hourly",
            human_age(checked_age_secs)
        ));
    }
    if report.bundle_shard != shard {
        reasons.push(format!(
            "published bundle is for shard {}, not {shard}",
            report.bundle_shard
        ));
    }
    let ok = reasons.is_empty();
    BackupStatusView {
        shard: shard.to_string(),
        configured: true,
        status: if ok { "ok" } else { "stale" }.to_string(),
        ok,
        message: if ok {
            "The newest verified bundle and signed restore drill are within their limits."
                .to_string()
        } else {
            "The backup or restore drill needs attention.".to_string()
        },
        published_at: Some(record.published_at),
        checked_age_secs: Some(checked_age_secs),
        max_backup_age_secs: Some(record.max_backup_age_secs),
        max_drill_age_secs: Some(record.max_drill_age_secs),
        reasons,
        bundle: Some(BackupBundleView {
            id: report.bundle_id.clone(),
            shard: report.bundle_shard.clone(),
            namespace: report.bundle_namespace.clone(),
            created_at: report.bundle_created_at,
            age_secs: bundle_age_secs,
            volumes: report.bundle_volumes,
            catalog_sha256: report.catalog_sha256.clone(),
        }),
        drill: Some(BackupDrillView {
            bundle_id: report.drill_bundle_id.clone(),
            kind: report.drill_kind.clone(),
            verified_at: report.drill_verified_at,
            age_secs: drill_age_secs,
            volumes_verified: report.drill_volumes_verified,
            signer: report.drill_signer.clone(),
            path: report.drill_path.clone(),
        }),
        source,
    }
}

fn human_age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

pub fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(data);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn verify_hmac_sha256_hex(key: &[u8], data: &[u8], signature: &str) -> Result<()> {
    let signature = signature.trim();
    let bytes = decode_hex(signature).context("drill signature is not valid hex")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(data);
    mac.verify_slice(&bytes)
        .context("drill signature does not match")
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        bytes.push((high * 16 + low) as u8);
    }
    Some(bytes)
}

fn safe_file_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
        && value != "."
        && value != ".."
}

fn drill_paths(out_dir: &Path, bundle_id: &str) -> Result<(PathBuf, PathBuf)> {
    if !safe_file_component(bundle_id) {
        bail!("backup id {bundle_id:?} is not safe for a drill file name");
    }
    Ok((
        out_dir.join(format!("{bundle_id}{DRILL_SUFFIX}")),
        out_dir.join(format!("{bundle_id}{DRILL_SIGNATURE_SUFFIX}")),
    ))
}

/// Write a signed drill record for a verified bundle.
///
/// `volumes_verified` must equal the bundle's volume count for a volume drill; catalog-only
/// bundles use zero.
pub fn write_drill_attestation(
    bundle: &LoadedBundle,
    key: &[u8],
    signer: &str,
    volumes_verified: usize,
    out_dir: &Path,
) -> Result<PathBuf> {
    if key.len() < MIN_DRILL_KEY_BYTES {
        bail!("drill signing key must be at least {MIN_DRILL_KEY_BYTES} bytes");
    }
    if signer.trim().is_empty() {
        bail!("drill signer is empty");
    }
    let requested = bundle.manifest.volumes.len();
    if requested > 0 && volumes_verified != requested {
        bail!("volume drill verified {volumes_verified} of {requested} volumes; refusing to sign");
    }
    if requested > 0
        && bundle
            .manifest
            .volumes
            .iter()
            .any(|volume| volume.verified_at.is_none() && volume.file_sha256.is_empty())
    {
        bail!("refusing to sign a volume drill without recorded checksums or verification");
    }
    if requested == 0 && volumes_verified != 0 {
        bail!("catalog-only drill cannot verify volumes");
    }
    let checksums_path = bundle.root.join(CHECKSUMS_FILE);
    let checksums_sha256 = sha256_file(&checksums_path)?;
    let attestation = DrillAttestation {
        format: DRILL_FORMAT.to_string(),
        bundle_id: bundle.manifest.id.clone(),
        namespace: bundle.manifest.namespace.clone(),
        shard: bundle.manifest.shard.clone(),
        bundle_mode: bundle.manifest.mode,
        bundle_created_at: bundle.manifest.created_at,
        verified_at: now_epoch(),
        checksums_sha256,
        catalog_sha256: bundle.manifest.catalog.sha256.clone(),
        volumes_requested: requested,
        volumes_verified,
        drill_kind: if requested == 0 {
            "catalog".to_string()
        } else {
            "volume".to_string()
        },
        result: "passed".to_string(),
        signer: signer.to_string(),
    };
    let bytes = format!("{}\n", serde_json::to_string_pretty(&attestation)?).into_bytes();
    let signature = hmac_sha256_hex(key, &bytes);
    fs::create_dir_all(out_dir)
        .with_context(|| format!("create drill directory {}", out_dir.display()))?;
    let (json_path, signature_path) = drill_paths(out_dir, &bundle.manifest.id)?;
    fs::write(&json_path, &bytes).with_context(|| format!("write {}", json_path.display()))?;
    fs::write(&signature_path, format!("{signature}\n"))
        .with_context(|| format!("write {}", signature_path.display()))?;
    Ok(json_path)
}

/// Verify a signed drill record and return it.
pub fn verify_drill_attestation(path: &Path, key: &[u8]) -> Result<DrillAttestation> {
    if key.len() < MIN_DRILL_KEY_BYTES {
        bail!("drill signing key must be at least {MIN_DRILL_KEY_BYTES} bytes");
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("drill file has no UTF-8 name")?;
    let stem = file_name
        .strip_suffix(DRILL_SUFFIX)
        .context("drill file must end in .drill.json")?;
    let signature_path = path.with_file_name(format!("{stem}{DRILL_SIGNATURE_SUFFIX}"));
    let signature = fs::read_to_string(&signature_path)
        .with_context(|| format!("read {}", signature_path.display()))?;
    verify_hmac_sha256_hex(key, &bytes, &signature)?;
    let attestation: DrillAttestation =
        serde_json::from_slice(&bytes).context("parse signed drill record")?;
    if attestation.format != DRILL_FORMAT {
        bail!(
            "unsupported drill format {} (expected {DRILL_FORMAT})",
            attestation.format
        );
    }
    if attestation.result != "passed" {
        bail!("drill result is {:?}, not passed", attestation.result);
    }
    if !safe_file_component(&attestation.bundle_id) {
        bail!("drill has an unsafe bundle id");
    }
    if attestation.checksums_sha256.len() != 64
        || !attestation
            .checksums_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || attestation.catalog_sha256.len() != 64
        || !attestation
            .catalog_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("drill checksums are malformed");
    }
    match attestation.drill_kind.as_str() {
        "catalog" if attestation.volumes_requested == 0 && attestation.volumes_verified == 0 => {}
        "volume"
            if attestation.volumes_requested > 0
                && attestation.volumes_verified == attestation.volumes_requested => {}
        "catalog" => bail!("catalog drill references a volume bundle"),
        "volume" => bail!("volume drill did not verify every volume"),
        other => bail!("unknown drill kind {other:?}"),
    }
    Ok(attestation)
}

fn find_files(
    root: &Path,
    name: Option<&str>,
    suffix: Option<&str>,
    depth: usize,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    if depth == 0 {
        return Ok(());
    }
    let entries =
        fs::read_dir(root).with_context(|| format!("read directory {}", root.display()))?;
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with('.') {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            find_files(&entry.path(), name, suffix, depth - 1, out)?;
        } else if file_type.is_file()
            && (name.is_some_and(|want| file_name == want)
                || suffix.is_some_and(|want| file_name.ends_with(want)))
        {
            out.push(entry.path());
        }
    }
    Ok(())
}

/// Find the newest bundle by its manifest timestamp and verify it.
pub fn newest_bundle(root: &Path) -> Result<LoadedBundle> {
    let mut files = Vec::new();
    find_files(root, Some(MANIFEST_FILE), None, 4, &mut files)?;
    if files.is_empty() {
        bail!("no backup bundles under {}", root.display());
    }
    let mut newest: Option<(u64, PathBuf)> = None;
    for path in files {
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let manifest: BackupManifest =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        if newest
            .as_ref()
            .is_none_or(|(created, _)| manifest.created_at > *created)
        {
            newest = Some((manifest.created_at, path));
        }
    }
    let (_, path) = newest.context("no readable backup manifest")?;
    verify_bundle(&path)
}

/// Find the newest signed drill and verify its signature.
pub fn newest_drill(root: &Path, key: &[u8]) -> Result<(PathBuf, DrillAttestation)> {
    let mut files = Vec::new();
    find_files(root, None, Some(DRILL_SUFFIX), 4, &mut files)?;
    if files.is_empty() {
        bail!("no signed drills under {}", root.display());
    }
    let mut newest: Option<(u64, PathBuf)> = None;
    for path in files {
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let attestation: DrillAttestation =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        if newest
            .as_ref()
            .is_none_or(|(verified, _)| attestation.verified_at > *verified)
        {
            newest = Some((attestation.verified_at, path));
        }
    }
    let (_, path) = newest.context("no readable signed drill")?;
    let attestation = verify_drill_attestation(&path, key)?;
    Ok((path, attestation))
}

/// Check the newest bundle and newest signed drill against the configured age limits.
pub fn check_backup_freshness(
    bundle_dir: &Path,
    drill_dir: &Path,
    key: &[u8],
    max_backup_age_secs: u64,
    max_drill_age_secs: u64,
) -> Result<FreshnessReport> {
    let bundle = newest_bundle(bundle_dir)?;
    let now = now_epoch();
    let bundle_age_secs = now.saturating_sub(bundle.manifest.created_at);
    if bundle_age_secs > max_backup_age_secs {
        bail!(
            "newest backup {} is stale: age {}s exceeds {}s",
            bundle.manifest.id,
            bundle_age_secs,
            max_backup_age_secs
        );
    }
    let (drill_path, drill) = newest_drill(drill_dir, key)?;
    if drill.shard != bundle.manifest.shard {
        bail!(
            "newest signed drill is for shard {:?}, not {:?}",
            drill.shard,
            bundle.manifest.shard
        );
    }
    if drill.namespace != bundle.manifest.namespace {
        bail!(
            "newest signed drill is for namespace {:?}, not {:?}",
            drill.namespace,
            bundle.manifest.namespace
        );
    }
    let drill_age_secs = now.saturating_sub(drill.verified_at);
    if drill_age_secs > max_drill_age_secs {
        bail!(
            "newest signed drill is stale: age {}s exceeds {}s",
            drill_age_secs,
            max_drill_age_secs
        );
    }
    if drill.bundle_id == bundle.manifest.id {
        let checksums = sha256_file(&bundle.root.join(CHECKSUMS_FILE))?;
        if drill.checksums_sha256 != checksums {
            bail!(
                "signed drill for {} does not match the bundle checksums",
                bundle.manifest.id
            );
        }
    }
    Ok(FreshnessReport {
        ok: true,
        bundle_id: bundle.manifest.id.clone(),
        bundle_shard: bundle.manifest.shard.clone(),
        bundle_namespace: bundle.manifest.namespace.clone(),
        bundle_created_at: bundle.manifest.created_at,
        bundle_age_secs,
        bundle_volumes: bundle.manifest.volumes.len(),
        catalog_sha256: bundle.manifest.catalog.sha256.clone(),
        drill_path: drill_path.display().to_string(),
        drill_bundle_id: drill.bundle_id,
        drill_kind: drill.drill_kind,
        drill_verified_at: drill.verified_at,
        drill_age_secs,
        drill_volumes_verified: drill.volumes_verified,
        drill_signer: drill.signer,
    })
}

/// Publish the manager-visible status into a ConfigMap.
///
/// The ConfigMap is shared by shards: `data[<shard>]` is the JSON status. A merge patch preserves
/// keys written by other shards.
pub async fn publish_backup_status(
    client: &Client,
    configmap: &str,
    namespace: &str,
    status: &PublishedBackupStatus,
) -> Result<()> {
    let shard = status.report.bundle_shard.clone();
    if !safe_file_component(&shard) {
        bail!("published backup status has an unsafe shard name");
    }
    let text = serde_json::to_string_pretty(status)?;
    let mut data = BTreeMap::new();
    data.insert(shard, text);
    let labels = json!({
        "app.kubernetes.io/name": "match-backup",
        "matching.io/role": "backup-status",
    });
    let annotations = json!({
        "matching.io/backup-status-format": BACKUP_STATUS_FORMAT,
    });
    let patch = json!({
        "metadata": {
            "name": configmap,
            "namespace": namespace,
            "labels": labels,
            "annotations": annotations,
        },
        "data": data,
    });
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    if api
        .get_opt(configmap)
        .await
        .with_context(|| format!("read ConfigMap {namespace}/{configmap}"))?
        .is_some()
    {
        api.patch(configmap, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .with_context(|| format!("patch ConfigMap {namespace}/{configmap}"))?;
        return Ok(());
    }
    let object: ConfigMap = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": configmap,
            "namespace": namespace,
            "labels": labels,
            "annotations": annotations,
        },
        "data": data,
    }))?;
    match api.create(&PostParams::default(), &object).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(error)) if error.code == 409 => {
            api.patch(configmap, &PatchParams::default(), &Patch::Merge(&patch))
                .await
                .with_context(|| format!("patch ConfigMap {namespace}/{configmap}"))?;
            Ok(())
        }
        Err(error) => {
            Err(error).with_context(|| format!("create ConfigMap {namespace}/{configmap}"))
        }
    }
}

/// Read the status published for one shard from the manager-readable ConfigMap.
pub async fn read_backup_status(
    client: &Client,
    namespace: &str,
    shard: &str,
) -> Result<Option<PublishedBackupStatus>> {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    let Some(configmap) = api
        .get_opt(BACKUP_STATUS_CONFIGMAP)
        .await
        .with_context(|| format!("read ConfigMap {namespace}/{BACKUP_STATUS_CONFIGMAP}"))?
    else {
        return Ok(None);
    };
    let Some(text) = configmap.data.and_then(|data| data.get(shard).cloned()) else {
        return Ok(None);
    };
    let status: PublishedBackupStatus =
        serde_json::from_str(&text).context("parse published backup status")?;
    if status.format != BACKUP_STATUS_FORMAT {
        bail!(
            "unsupported backup status format {} (expected {BACKUP_STATUS_FORMAT})",
            status.format
        );
    }
    Ok(Some(status))
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
/// symbol/deployment/route hosted by that shard. Symbol, cell, market-data, order-system and
/// trading-terminal selections expand their direct references so a merge validates without
/// restoring unrelated shards.
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
    for name in &selection.trading_terminals {
        if !catalog.trading_terminals.contains_key(name) {
            bail!("backup has no trading terminal {name}");
        }
    }

    let mut cells = selection.cells.clone();
    let mut shards = selection.shards.clone();
    let mut symbols = selection.symbols.clone();
    let mut market_data_services = selection.market_data_services.clone();
    let mut order_systems = selection.order_systems.clone();
    let mut trading_terminals = selection.trading_terminals.clone();

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
            trading_terminals.len(),
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
        for terminal in catalog.trading_terminals.values() {
            if trading_terminals.contains(&terminal.name)
                || explicit_cells.contains(&terminal.spec.cell)
            {
                trading_terminals.insert(terminal.name.clone());
                cells.insert(terminal.spec.cell.clone());
            }
        }
        if (
            cells.len(),
            shards.len(),
            symbols.len(),
            market_data_services.len(),
            order_systems.len(),
            trading_terminals.len(),
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
    for (name, terminal) in &catalog.trading_terminals {
        if trading_terminals.contains(name) {
            selected
                .trading_terminals
                .insert(name.clone(), terminal.clone());
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
    for (name, terminal) in &selected.trading_terminals {
        let mut object = terminal.clone();
        if let Some(existing) = current.trading_terminals.get(name) {
            object.status = existing.status.clone();
        }
        merged.trading_terminals.insert(name.clone(), object);
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
        && counts.trading_terminals == 0
    {
        bail!(
            "selection does not include a shard, symbol, market-data service, order system or trading terminal"
        );
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
    fn trading_terminal_survives_bundle_export_and_selective_restore() {
        use crate::model::{Phase, TradingTerminal, TradingTerminalSpec};
        let dir = temp_dir("backup-terminal");
        let mut catalog = demo_catalog();
        let mut terminal = TradingTerminal {
            name: "terminal-eu".to_string(),
            namespace: "match-eu-1".to_string(),
            spec: TradingTerminalSpec {
                cell: "eu-1".to_string(),
                image: "matchengine-trading-terminal:arm64-v5".to_string(),
                replicas: 1,
                autoscaling: Default::default(),
                port: 8080,
                ingress: String::new(),
                tls_secret: String::new(),
                md_upstream: crate::model::default_trading_terminal_md_upstream(),
                desk_upstream: crate::model::default_trading_terminal_desk_upstream(),
                dns_upstream: String::new(),
                resources: None,
                paused: false,
                previous_image: None,
            },
            status: Default::default(),
        };
        catalog
            .trading_terminals
            .insert(terminal.name.clone(), terminal.clone());

        let shard = catalog.shards.values().next().unwrap().clone();
        let manifest = BackupManifest::new(
            "terminal-backup".to_string(),
            "match-eu-1",
            &shard,
            BackupMode::VolumeGroup,
            &catalog,
            "vg-class",
            "snap-class",
        );
        write_bundle(&dir, &catalog, manifest, None, false).unwrap();
        let bundle = read_bundle(&dir).unwrap();
        assert_eq!(bundle.manifest.counts.trading_terminals, 1);
        assert_eq!(bundle.catalog.trading_terminals.len(), 1);

        let mut selection = RestoreSelection::default();
        selection
            .trading_terminals
            .insert("terminal-eu".to_string());
        let selected = select_catalog(&bundle.catalog, &selection).unwrap();
        assert!(selected.trading_terminals.contains_key("terminal-eu"));
        assert!(selected.cells.contains_key("eu-1"));
        let plan = plan_restore(&bundle, &selection).unwrap();
        assert_eq!(plan.counts.trading_terminals, 1);

        terminal.status.phase = Phase::Ready;
        let mut current = catalog.clone();
        current
            .trading_terminals
            .insert(terminal.name.clone(), terminal);
        let merged = merge_catalog(&current, &selected);
        assert_eq!(
            merged.trading_terminals["terminal-eu"].status.phase,
            Phase::Ready
        );
        fs::remove_dir_all(dir).ok();
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

    fn fixture(dir: &Path, created_at: u64) -> LoadedBundle {
        let catalog = demo_catalog();
        let shard = catalog.shards.values().next().unwrap().clone();
        let mut manifest = BackupManifest::new(
            "drill-test".to_string(),
            "match-eu-1",
            &shard,
            BackupMode::VolumeGroup,
            &catalog,
            "vg-class",
            "snap-class",
        );
        manifest.created_at = created_at;
        write_bundle(dir, &catalog, manifest, None, false).unwrap();
        read_bundle(dir).unwrap()
    }

    #[test]
    fn hmac_sha256_known_vector() {
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    const DRILL_KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

    #[test]
    fn refuses_to_sign_an_unverified_volume_drill() {
        let dir = temp_dir("backup-drill-unverified");
        let catalog = demo_catalog();
        let shard = catalog.shards.values().next().unwrap().clone();
        let mut manifest = BackupManifest::new(
            "drill-unverified".to_string(),
            "match-eu-1",
            &shard,
            BackupMode::Volume,
            &catalog,
            "",
            "snap-class",
        );
        manifest.volumes.push(VolumeBackup {
            pvc: "data-fx-eu-01-0".to_string(),
            snapshot: "snap-0".to_string(),
            ready: true,
            restore_size: "1Gi".to_string(),
            requested_size: "1Gi".to_string(),
            storage_class: "fast".to_string(),
            access_modes: vec!["ReadWriteOnce".to_string()],
            file_sha256: BTreeMap::new(),
            verified_at: None,
        });
        write_bundle(&dir, &catalog, manifest, None, false).unwrap();
        let bundle = read_bundle(&dir).unwrap();
        let error = write_drill_attestation(&bundle, DRILL_KEY, "test", 1, &dir.join("drills"))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("without recorded checksums or verification"),
            "{error}"
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn published_status_view_flags_stale_and_missing_records() {
        let missing = view_backup_status("fx-eu-01", None, 1_000);
        assert!(!missing.configured);
        assert_eq!(missing.status, "not-configured");
        assert!(missing.bundle.is_none());

        let report = FreshnessReport {
            ok: true,
            bundle_id: "fx-eu-01-1".to_string(),
            bundle_shard: "fx-eu-01".to_string(),
            bundle_namespace: "match-eu-1".to_string(),
            bundle_created_at: 900,
            bundle_age_secs: 0,
            bundle_volumes: 0,
            catalog_sha256: "a".repeat(64),
            drill_path: "/drills/fx-eu-01-1.drill.json".to_string(),
            drill_bundle_id: "fx-eu-01-1".to_string(),
            drill_kind: "catalog".to_string(),
            drill_verified_at: 950,
            drill_age_secs: 0,
            drill_volumes_verified: 0,
            drill_signer: "cronjob".to_string(),
        };
        let fresh = view_backup_status(
            "fx-eu-01",
            Some(&PublishedBackupStatus::new(report.clone(), 3_600, 3_600)),
            1_000,
        );
        assert!(fresh.ok);
        assert_eq!(fresh.status, "ok");
        assert_eq!(
            fresh.bundle.as_ref().map(|b| b.id.as_str()),
            Some("fx-eu-01-1")
        );

        // Both ages exceed the limits and the status itself is old.
        let stale = view_backup_status(
            "fx-eu-01",
            Some(&PublishedBackupStatus {
                format: BACKUP_STATUS_FORMAT.to_string(),
                published_at: 0,
                max_backup_age_secs: 10,
                max_drill_age_secs: 10,
                report: report.clone(),
            }),
            100_000,
        );
        assert!(!stale.ok);
        assert_eq!(stale.status, "stale");
        assert!(stale.reasons.iter().any(|r| r.contains("bundle is")));
        assert!(stale.reasons.iter().any(|r| r.contains("drill is")));
        assert!(
            stale
                .reasons
                .iter()
                .any(|r| r.contains("has not been refreshed"))
        );

        // A record published for a different shard is reported, not silently accepted.
        let other = view_backup_status(
            "fx-eu-02",
            Some(&PublishedBackupStatus::new(report, 3_600, 3_600)),
            1_000,
        );
        assert!(!other.ok);
        assert!(other.reasons.iter().any(|r| r.contains("fx-eu-01")));
    }

    #[test]
    fn rejects_a_short_drill_key() {
        let dir = temp_dir("backup-drill-short-key");
        let bundle = fixture(&dir, now_epoch());
        let drills = dir.join("drills");
        assert!(
            write_drill_attestation(&bundle, b"short", "test", 0, &drills).is_err(),
            "a short HMAC key must be rejected"
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn signed_drill_round_trips_and_detects_tampering() {
        let dir = temp_dir("backup-drill-sign");
        let bundle = fixture(&dir, now_epoch());
        let drills = dir.join("drills");
        let path = write_drill_attestation(&bundle, DRILL_KEY, "test", 0, &drills).unwrap();
        let attestation = verify_drill_attestation(&path, DRILL_KEY).unwrap();
        assert_eq!(attestation.bundle_id, bundle.manifest.id);
        assert_eq!(attestation.drill_kind, "catalog");
        assert_eq!(attestation.result, "passed");
        assert!(verify_drill_attestation(&path, b"fedcba9876543210fedcba9876543210").is_err());

        let mut bytes = fs::read(&path).unwrap();
        bytes.push(b' ');
        fs::write(&path, bytes).unwrap();
        assert!(verify_drill_attestation(&path, DRILL_KEY).is_err());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn freshness_rejects_a_stale_bundle_or_drill() {
        let dir = temp_dir("backup-freshness");
        let fresh = fixture(&dir.join("fresh"), now_epoch());
        let drills = dir.join("drills");
        write_drill_attestation(&fresh, DRILL_KEY, "test", 0, &drills).unwrap();
        let report =
            check_backup_freshness(&dir.join("fresh"), &drills, DRILL_KEY, 3_600, 3_600).unwrap();
        assert!(report.ok);
        assert_eq!(report.bundle_id, fresh.manifest.id);

        let stale_dir = dir.join("stale-bundle");
        let stale = fixture(&stale_dir, 0);
        let stale_drills = dir.join("stale-drills");
        write_drill_attestation(&stale, DRILL_KEY, "test", 0, &stale_drills).unwrap();
        assert!(
            check_backup_freshness(&stale_dir, &stale_drills, DRILL_KEY, 1, 3_600).is_err(),
            "a zero-timestamp bundle must be stale"
        );

        // Re-sign a drill with an old timestamp so only the drill age check can fail.
        let (json_path, signature_path) = drill_paths(&drills, &fresh.manifest.id).unwrap();
        let mut attestation: DrillAttestation =
            serde_json::from_slice(&fs::read(&json_path).unwrap()).unwrap();
        attestation.verified_at = 0;
        let bytes =
            format!("{}\n", serde_json::to_string_pretty(&attestation).unwrap()).into_bytes();
        fs::write(&json_path, &bytes).unwrap();
        fs::write(
            &signature_path,
            format!("{}\n", hmac_sha256_hex(DRILL_KEY, &bytes)),
        )
        .unwrap();
        assert!(
            check_backup_freshness(&dir.join("fresh"), &drills, DRILL_KEY, 3_600, 1).is_err(),
            "an old signed drill must be stale"
        );
        fs::remove_dir_all(dir).ok();
    }
}

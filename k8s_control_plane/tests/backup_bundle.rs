//! Read-only CLI checks for a backup bundle. No cluster or CSI driver is required.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use matchengine_k8s_control_plane::backup::{
    BackupManifest, BackupMode, CATALOG_FILE, write_bundle,
};
use matchengine_k8s_control_plane::store::demo_catalog;

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("matchengine-{name}-{}-{nanos}", std::process::id()))
}

fn write_fixture_at(dir: &Path, created_at: u64) -> String {
    let catalog = demo_catalog();
    let shard = catalog.shards.values().next().unwrap().clone();
    let mut manifest = BackupManifest::new(
        "cli-test".to_string(),
        "match-eu-1",
        &shard,
        BackupMode::VolumeGroup,
        &catalog,
        "vg-class",
        "snap-class",
    );
    manifest.created_at = created_at;
    write_bundle(dir, &catalog, manifest, None, false).unwrap();
    shard.name
}

fn write_fixture(dir: &Path) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    write_fixture_at(dir, now)
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_k8s-control-plane")
}

#[test]
fn verifies_a_bundle_and_plans_a_selective_restore() {
    let dir = temp_dir("backup-cli-verify");
    let _shard = write_fixture(&dir);

    let verify = Command::new(binary())
        .args(["backup-verify", "--input"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );
    assert!(String::from_utf8_lossy(&verify.stdout).contains("verified backup cli-test"));

    let plan = Command::new(binary())
        .args(["restore", "--input"])
        .arg(&dir)
        .args(["--shard", &_shard])
        .output()
        .unwrap();
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let stdout = String::from_utf8_lossy(&plan.stdout);
    assert!(stdout.contains("verified only"));
    assert!(stdout.contains("1 shards"));

    fs::remove_dir_all(dir).ok();
}

#[test]
fn rejects_a_tampered_catalog() {
    let dir = temp_dir("backup-cli-tamper");
    write_fixture(&dir);
    let catalog = dir.join(CATALOG_FILE);
    let mut bytes = fs::read(&catalog).unwrap();
    bytes.extend_from_slice(b"\n");
    fs::write(&catalog, bytes).unwrap();

    let verify = Command::new(binary())
        .args(["backup-verify", "--input"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(!verify.status.success());
    assert!(String::from_utf8_lossy(&verify.stderr).contains("checksum mismatch"));

    fs::remove_dir_all(dir).ok();
}

#[test]
fn backup_status_rejects_a_stale_bundle() {
    let dir = temp_dir("backup-cli-stale");
    write_fixture_at(&dir, 0);
    let drills = dir.join("drills");
    let key = dir.join("drill.key");
    fs::write(&key, b"0123456789abcdef0123456789abcdef").unwrap();
    let drill = Command::new(binary())
        .args(["restore", "--input"])
        .arg(&dir)
        .args(["--drill", "--attest-key"])
        .arg(&key)
        .args(["--attest-out"])
        .arg(&drills)
        .output()
        .unwrap();
    assert!(
        drill.status.success(),
        "{}",
        String::from_utf8_lossy(&drill.stderr)
    );

    let status = Command::new(binary())
        .args(["backup-status", "--bundle-dir"])
        .arg(&dir)
        .args(["--drill-dir"])
        .arg(&drills)
        .args(["--key"])
        .arg(&key)
        .args(["--max-backup-age-secs", "1"])
        .output()
        .unwrap();
    assert!(!status.status.success());
    assert!(String::from_utf8_lossy(&status.stderr).contains("is stale"));

    fs::remove_dir_all(dir).ok();
}

#[test]
fn signs_a_catalog_drill_and_checks_freshness() {
    let dir = temp_dir("backup-cli-drill");
    write_fixture(&dir);
    let drills = dir.join("drills");
    let key = dir.join("drill.key");
    fs::write(&key, b"0123456789abcdef0123456789abcdef").unwrap();

    let drill = Command::new(binary())
        .args(["restore", "--input"])
        .arg(&dir)
        .args(["--drill", "--attest-key"])
        .arg(&key)
        .args(["--attest-out"])
        .arg(&drills)
        .output()
        .unwrap();
    assert!(
        drill.status.success(),
        "{}",
        String::from_utf8_lossy(&drill.stderr)
    );
    let stdout = String::from_utf8_lossy(&drill.stdout);
    assert!(stdout.contains("catalog verified"));
    assert!(stdout.contains("signed drill"));

    let status = Command::new(binary())
        .args(["backup-status", "--bundle-dir"])
        .arg(&dir)
        .args(["--drill-dir"])
        .arg(&drills)
        .args(["--key"])
        .arg(&key)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.contains("backup status: ok"));
    assert!(stdout.contains("kind catalog"));

    let json = Command::new(binary())
        .args(["backup-status", "--bundle-dir"])
        .arg(&dir)
        .args(["--drill-dir"])
        .arg(&drills)
        .args(["--key"])
        .arg(&key)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(report["ok"], true);
    assert_eq!(report["drillKind"], "catalog");

    fs::remove_dir_all(dir).ok();
}

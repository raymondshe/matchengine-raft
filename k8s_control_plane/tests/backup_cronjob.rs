//! Manifest checks for the scheduled backup CronJobs. No cluster is required.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_yaml::Value;

fn deploy(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("deploy")
        .join(name)
}

fn documents(name: &str) -> Vec<Value> {
    let text = fs::read_to_string(deploy(name)).unwrap();
    serde_yaml::Deserializer::from_str(&text)
        .map(|document| Value::deserialize(document).unwrap())
        .collect()
}

fn named<'a>(docs: &'a [Value], kind: &str, name: &str) -> &'a Value {
    docs.iter()
        .find(|doc| {
            doc["kind"].as_str() == Some(kind) && doc["metadata"]["name"].as_str() == Some(name)
        })
        .unwrap_or_else(|| panic!("missing {kind} {name}"))
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("matchengine-{name}-{}-{nanos}", std::process::id()))
}

#[test]
fn backup_manifest_has_the_expected_cronjobs_and_scripts() {
    let docs = documents("backup.yaml");
    let config = named(&docs, "ConfigMap", "match-backup-config");
    assert_eq!(config["data"]["BACKUP_MODE"].as_str(), Some("crd-only"));
    assert_eq!(config["data"]["BACKUP_RETENTION"].as_str(), Some("720h"));
    assert!(
        config["data"]["RCLONE_REMOTE"]
            .as_str()
            .unwrap()
            .starts_with("s3:")
    );

    let scripts = named(&docs, "ConfigMap", "match-backup-scripts");
    let backup = scripts["data"]["backup.sh"].as_str().unwrap();
    let status = scripts["data"]["status.sh"].as_str().unwrap();
    assert!(backup.contains("kc \"$@\""));
    assert!(backup.contains("kc restore --input \"$OUT\" --drill"));
    assert!(backup.contains("--attest-key"));
    assert!(backup.contains("--attest-out"));
    assert!(backup.contains("rc check"));
    assert!(backup.contains("rc delete --min-age"));
    assert!(backup.contains("kc backup-status"));
    assert!(backup.contains("--publish-configmap match-backup-status"));
    assert!(backup.contains("--configmap-namespace match-control"));
    assert!(status.contains("backup-status"));
    // The drill must be paired with the newest bundle, not selected independently.
    assert!(status.contains("LATEST_BUNDLE.drill.json"));
    assert!(status.contains("no signed drill"));
    assert!(backup.contains("K8S_CONTROL_PLANE_BIN"));
    assert!(status.contains("RCLONE_BIN"));

    let dir = temp_dir("backup-cronjob");
    fs::create_dir_all(&dir).unwrap();
    for (name, script) in [("backup.sh", backup), ("status.sh", status)] {
        let path = dir.join(name);
        fs::write(&path, script).unwrap();
        let check = Command::new("sh").arg("-n").arg(&path).output().unwrap();
        assert!(
            check.status.success(),
            "{name} has a shell syntax error:\n{}",
            String::from_utf8_lossy(&check.stderr)
        );
    }
    fs::remove_dir_all(dir).ok();

    for name in ["match-backup", "match-backup-status"] {
        let job = named(&docs, "CronJob", name);
        let pod = &job["spec"]["jobTemplate"]["spec"]["template"]["spec"];
        assert_eq!(pod["serviceAccountName"].as_str(), Some("match-backup"));
        let container = &pod["containers"][0];
        assert_eq!(
            container["image"].as_str(),
            Some("matchengine-k8s-control-plane:local")
        );
        assert_eq!(container["command"][0].as_str(), Some("/bin/sh"));
        assert!(
            container["command"][1]
                .as_str()
                .unwrap()
                .starts_with("/scripts/")
        );
        assert_eq!(
            container["securityContext"]["readOnlyRootFilesystem"].as_bool(),
            Some(true)
        );
    }
}

#[cfg(unix)]
#[test]
fn backup_script_drills_uploads_prunes_and_checks_freshness() {
    use std::os::unix::fs::PermissionsExt;

    let docs = documents("backup.yaml");
    let scripts = named(&docs, "ConfigMap", "match-backup-scripts");
    let backup = scripts["data"]["backup.sh"].as_str().unwrap();

    let dir = temp_dir("backup-script");
    let bin = dir.join("bin");
    let work = dir.join("work");
    fs::create_dir_all(&bin).unwrap();
    let log = dir.join("calls.log");
    let signing = dir.join("signing.key");
    let rclone_config = dir.join("rclone.conf");
    fs::write(&signing, b"key").unwrap();
    fs::write(&rclone_config, b"[s3]\ntype = s3\n").unwrap();

    let rclone = bin.join("rclone");
    fs::write(
        &rclone,
        format!(
            "#!/bin/sh\nprintf 'RCLONE %s\\n' \"$*\" >> \"{}\"\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    let control_plane = bin.join("control-plane");
    fs::write(
        &control_plane,
        format!(
            "#!/bin/sh\nprintf 'K8S %s\\n' \"$*\" >> \"{}\"\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    for path in [&rclone, &control_plane] {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    let script = dir.join("backup.sh");
    fs::write(&script, backup).unwrap();
    let output = Command::new("sh")
        .arg(&script)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("K8S_CONTROL_PLANE_BIN", &control_plane)
        .env("RCLONE_BIN", &rclone)
        .env("TEST_LOG", &log)
        .env("WORK_DIR", &work)
        .env("BACKUP_SHARD", "fx-eu-01")
        .env("BACKUP_NAMESPACE", "match-eu-1")
        .env("BACKUP_MODE", "crd-only")
        .env("BACKUP_RETENTION", "1h")
        .env("BACKUP_TIMEOUT_SECS", "30")
        .env("RCLONE_REMOTE", "s3:bucket/prefix")
        .env("RCLONE_CONFIG", &rclone_config)
        .env("BACKUP_SIGNING_KEY", &signing)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fs::read_to_string(&log).unwrap();
    for expected in [
        "K8S backup fx-eu-01",
        "--id fx-eu-01-",
        "--crd-only",
        "K8S backup-verify --input",
        "K8S restore --input",
        "--drill",
        "--attest-key",
        "--attest-out",
        "RCLONE copy",
        "RCLONE check",
        "--one-way",
        "RCLONE copyto",
        ".drill.json",
        ".drill.sig",
        "RCLONE delete --min-age 1h",
        "RCLONE rmdirs",
        "K8S backup-status --bundle-dir",
    ] {
        assert!(calls.contains(expected), "missing {expected:?} in\n{calls}");
    }
    let backup_at = calls.find("K8S backup fx-eu-01").unwrap();
    let verify_at = calls.find("K8S backup-verify").unwrap();
    let drill_at = calls.find("K8S restore --input").unwrap();
    let upload_at = calls.find("RCLONE copy").unwrap();
    let prune_at = calls.find("RCLONE delete --min-age").unwrap();
    let status_at = calls.find("K8S backup-status").unwrap();
    assert!(backup_at < verify_at);
    assert!(verify_at < drill_at);
    assert!(drill_at < upload_at);
    assert!(upload_at < prune_at);
    assert!(prune_at < status_at);
    fs::remove_dir_all(dir).ok();
}

#[cfg(unix)]
#[test]
fn status_script_downloads_the_latest_bundle_and_runs_backup_status() {
    use std::os::unix::fs::PermissionsExt;

    let docs = documents("backup.yaml");
    let scripts = named(&docs, "ConfigMap", "match-backup-scripts");
    let status = scripts["data"]["status.sh"].as_str().unwrap();

    let dir = temp_dir("backup-status-script");
    let bin = dir.join("bin");
    let work = dir.join("work");
    fs::create_dir_all(&bin).unwrap();
    let log = dir.join("calls.log");
    let signing = dir.join("signing.key");
    let rclone_config = dir.join("rclone.conf");
    fs::write(&signing, b"key").unwrap();
    fs::write(&rclone_config, b"[s3]\ntype = s3\n").unwrap();

    let rclone = bin.join("rclone");
    fs::write(
        &rclone,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "lsf" ]; then
  case "$*" in
    *"/bundles"*) printf '%s\n' "fx-eu-01-100/" ;;
    *"/drills"*) printf '%s\n' "fx-eu-01-100.drill.json" "fx-eu-01-100.drill.sig" ;;
  esac
fi
exit 0
"#,
            log = log.display()
        ),
    )
    .unwrap();
    let control_plane = bin.join("control-plane");
    fs::write(
        &control_plane,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{}"
exit 0
"#,
            log.display()
        ),
    )
    .unwrap();
    for path in [&rclone, &control_plane] {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    let script = dir.join("status.sh");
    fs::write(&script, status).unwrap();
    let output = Command::new("sh")
        .arg(&script)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("K8S_CONTROL_PLANE_BIN", &control_plane)
        .env("RCLONE_BIN", &rclone)
        .env("TEST_LOG", &log)
        .env("WORK_DIR", &work)
        .env("BACKUP_SHARD", "fx-eu-01")
        .env("RCLONE_REMOTE", "s3:bucket/prefix")
        .env("RCLONE_CONFIG", &rclone_config)
        .env("BACKUP_SIGNING_KEY", &signing)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fs::read_to_string(&log).unwrap();
    assert!(calls.contains("lsf --dirs-only s3:bucket/prefix/fx-eu-01/bundles"));
    assert!(calls.contains("copy s3:bucket/prefix/fx-eu-01/bundles/fx-eu-01-100"));
    assert!(calls.contains("copyto s3:bucket/prefix/fx-eu-01/drills/fx-eu-01-100.drill.json"));
    assert!(calls.contains("backup-status --bundle-dir"));
    assert!(calls.contains("--drill-dir"));
    assert!(calls.contains("--key"));
    assert!(calls.contains("--publish-configmap match-backup-status"));
    assert!(calls.contains("--configmap-namespace match-control"));
    fs::remove_dir_all(dir).ok();
}

#[test]
fn backup_alerts_cover_failed_and_stale_jobs() {
    let docs = documents("backup-alerts.yaml");
    let rule = named(&docs, "PrometheusRule", "match-backup");
    let rules = rule["spec"]["groups"][0]["rules"].as_sequence().unwrap();
    let names: Vec<&str> = rules
        .iter()
        .filter_map(|rule| rule["alert"].as_str())
        .collect();
    for expected in [
        "MatchBackupJobFailed",
        "MatchBackupStale",
        "MatchBackupStatusStale",
        "MatchBackupNeverSucceeded",
    ] {
        assert!(names.contains(&expected), "missing alert {expected}");
    }
}

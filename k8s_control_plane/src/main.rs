//! `k8s-control-plane` CLI: serve the manager GUI/API, render manifests, validate a catalog and
//! print the operator's reconcile plan.
//!
//! ```text
//! cargo run -- serve
//! cargo run -- render --shard fx-eu-01 --profile local
//! cargo run -- validate
//! cargo run -- plan --shard fx-eu-01 --scenario fresh
//! ```
//!
//! (From the repository root, add `--manifest-path k8s_control_plane/Cargo.toml`.)

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;

use matchengine_k8s_control_plane::api;
use matchengine_k8s_control_plane::backup;
use matchengine_k8s_control_plane::bundle::{ShardBundle, demo_bundles};
use matchengine_k8s_control_plane::model::{BackupMode, Release};
use matchengine_k8s_control_plane::reconcile::{Scenario, plan_shard, simulate, summarize};
use matchengine_k8s_control_plane::render::{
    Profile, RenderOptions, as_list, render_shard, render_trading_terminal,
};
use matchengine_k8s_control_plane::store::{Catalog, Store, demo_catalog};
use matchengine_k8s_control_plane::validate::{validate_catalog, validate_symbol};

#[derive(Debug, Parser)]
#[command(
    name = "k8s-control-plane",
    version,
    about = "Reference manager for matchengine-raft Kubernetes symbol shards"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the manager API and embedded GUI.
    Serve {
        #[arg(long, default_value = "127.0.0.1:8099", env = "CONTROL_PLANE_ADDR")]
        addr: String,
        /// JSON catalog file. Created (with the demo seed) when missing.
        #[arg(long, env = "CONTROL_PLANE_STATE")]
        state: Option<PathBuf>,
        /// Start empty instead of loading the built-in demo shards.
        #[arg(long)]
        no_seed: bool,
        /// Persist desired state as matching.io CRDs through the Kubernetes API server.
        #[arg(long, env = "CONTROL_PLANE_CRD")]
        crd: bool,
        /// Namespace used for CRs whose model namespace is empty.
        #[arg(
            long,
            default_value = "match-eu-1",
            env = "CONTROL_PLANE_CRD_NAMESPACE"
        )]
        crd_namespace: String,
        /// Host dialled by the engine-console proxy instead of the pod FQDN. Set it to
        /// `127.0.0.1` when the demo manager and a local engine share a machine.
        #[arg(long, env = "ENGINE_CONSOLE_HOST")]
        engine_console_host: Option<String>,
    },
    /// Print the eight catalog CustomResourceDefinitions as YAML.
    Crds {
        /// Write here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Export the live CRD catalog as a JSON backup.
    CrdBackup {
        /// Write here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Namespace used for CRs whose model namespace is empty.
        #[arg(
            long,
            default_value = "match-eu-1",
            env = "CONTROL_PLANE_CRD_NAMESPACE"
        )]
        namespace: String,
    },
    /// Validate a JSON catalog and restore it into CRDs through the Kubernetes API server.
    CrdRestore {
        #[arg(long)]
        input: PathBuf,
        /// Namespace used for CRs whose model namespace is empty.
        #[arg(
            long,
            default_value = "match-eu-1",
            env = "CONTROL_PLANE_CRD_NAMESPACE"
        )]
        namespace: String,
    },
    /// Create a volume-snapshot + CRD/etcd backup bundle for one shard.
    Backup {
        shard: String,
        /// Bundle id and manifest id. Defaults to <shard>-<unix-seconds>.
        #[arg(long)]
        id: Option<String>,
        /// Bundle directory. Defaults to backups/<id>.
        #[arg(long)]
        out: Option<PathBuf>,
        /// `volume-group` uses the Kubernetes VolumeGroupSnapshot API; `volume` creates one
        /// VolumeSnapshot per PVC.
        #[arg(long, value_parser = ["volume-group", "volume"], default_value = "volume-group")]
        mode: String,
        /// Namespace containing the shard's PVCs.
        #[arg(
            long,
            default_value = "match-eu-1",
            env = "CONTROL_PLANE_CRD_NAMESPACE"
        )]
        namespace: String,
        #[arg(long, default_value = "")]
        volume_group_snapshot_class: String,
        #[arg(long, default_value = "")]
        volume_snapshot_class: String,
        /// Write only the CRD/etcd bundle; skip volume snapshots.
        #[arg(long)]
        crd_only: bool,
        /// Externally produced `etcdctl snapshot save` file to include in the bundle.
        #[arg(long)]
        etcd_snapshot: Option<PathBuf>,
        /// Skip the snapshot restore/hash verification.
        #[arg(long)]
        no_verify: bool,
        #[arg(long, default_value = "busybox:1.36")]
        verify_image: String,
        #[arg(long, default_value_t = 300)]
        timeout_secs: u64,
        #[arg(long)]
        overwrite: bool,
    },
    /// Verify every checksum and the catalog in a backup bundle.
    BackupVerify {
        #[arg(long)]
        input: PathBuf,
    },
    /// Check the newest uploaded bundle and signed restore drill for freshness.
    BackupStatus {
        /// Directory containing one or more backup bundles (searched recursively).
        #[arg(long, default_value = "backups")]
        bundle_dir: PathBuf,
        /// Directory containing `<bundle-id>.drill.json` and `.drill.sig` files.
        #[arg(long, default_value = "drills")]
        drill_dir: PathBuf,
        /// HMAC-SHA256 key used to verify signed drill records.
        #[arg(long, env = "BACKUP_SIGNING_KEY_FILE")]
        key: PathBuf,
        #[arg(long, default_value_t = backup::DEFAULT_MAX_BACKUP_AGE_SECS)]
        max_backup_age_secs: u64,
        #[arg(long, default_value_t = backup::DEFAULT_MAX_DRILL_AGE_SECS)]
        max_drill_age_secs: u64,
        /// Print a machine-readable report.
        #[arg(long)]
        json: bool,
        /// Publish the verified status into this ConfigMap for the manager.
        #[arg(long)]
        publish_configmap: Option<String>,
        /// Namespace of `--publish-configmap`.
        #[arg(long, default_value = "match-control", env = "BACKUP_STATUS_NAMESPACE")]
        configmap_namespace: String,
    },
    /// Verify and selectively restore a backup bundle. Without an action it only verifies.
    Restore {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        cell: Vec<String>,
        #[arg(long)]
        shard: Vec<String>,
        #[arg(long)]
        symbol: Vec<String>,
        #[arg(long)]
        market_data_service: Vec<String>,
        #[arg(long)]
        order_system: Vec<String>,
        #[arg(long)]
        trading_terminal: Vec<String>,
        /// Write the selected CRD objects through the manager's CRD store.
        #[arg(long, conflicts_with = "drill")]
        apply_crds: bool,
        /// Restore snapshots into temporary PVCs, verify checksums and clean up. Never writes CRDs.
        #[arg(long)]
        drill: bool,
        /// Restore snapshots into new PVCs and keep them.
        #[arg(long)]
        restore_volumes: bool,
        /// Namespace for restored PVCs. Defaults to the bundle's source namespace.
        #[arg(long)]
        target_namespace: Option<String>,
        /// Prefix for restored PVC names; keeps live PVCs untouched.
        #[arg(long, default_value = "restore-")]
        volume_prefix: String,
        /// Keep drill PVCs for inspection.
        #[arg(long)]
        keep: bool,
        /// HMAC-SHA256 key used to sign the drill result. Requires `--drill` and `--attest-out`.
        #[arg(long, requires_all = ["drill", "attest_out"], env = "BACKUP_ATTEST_KEY")]
        attest_key: Option<PathBuf>,
        /// Directory for `<bundle-id>.drill.json` and `.drill.sig`. Requires `--drill`.
        #[arg(long, requires_all = ["drill", "attest_key"])]
        attest_out: Option<PathBuf>,
        /// Label recorded in the signed drill record.
        #[arg(
            long,
            default_value = "k8s-control-plane",
            env = "BACKUP_ATTEST_SIGNER"
        )]
        attest_signer: String,
        #[arg(long, default_value = "busybox:1.36")]
        verify_image: String,
        #[arg(long, default_value_t = 300)]
        timeout_secs: u64,
        #[arg(
            long,
            default_value = "match-eu-1",
            env = "CONTROL_PLANE_CRD_NAMESPACE"
        )]
        namespace: String,
    },
    /// Render one shard, one trading terminal (or every shard) as a `v1/List` of Kubernetes objects.
    Render {
        #[arg(long)]
        shard: Option<String>,
        /// Render every shard into one `List` (separated in the output).
        #[arg(long)]
        all: bool,
        /// Render one trading terminal instead of a shard.
        #[arg(long, conflicts_with_all = ["shard", "all"])]
        terminal: Option<String>,
        #[arg(long, value_parser = ["production", "local"], default_value = "production")]
        profile: String,
        #[arg(long)]
        namespace: Option<String>,
        #[arg(long)]
        storage_class: Option<String>,
        #[arg(long)]
        replicas: Option<u8>,
        #[arg(long)]
        no_non_root: bool,
        /// Catalog JSON; defaults to the built-in demo catalog.
        #[arg(long)]
        data: Option<PathBuf>,
        /// ShardBundle JSON to add (repeatable). Overrides `--data` when any is present.
        #[arg(long = "bundle")]
        bundles: Vec<PathBuf>,
        /// Write here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Print the built-in demo bundles, or write one JSON file per shard.
    Seed {
        /// Directory to write `<shard>.json` into. Omit to print a JSON array.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Validate the catalog (or a single bundle file).
    Validate {
        #[arg(long)]
        data: Option<PathBuf>,
        #[arg(long = "bundle")]
        bundles: Vec<PathBuf>,
        /// Validate symbol fields only for this symbol.
        #[arg(long)]
        symbol: Option<String>,
    },
    /// Print the operator's reconcile plan for one shard.
    Plan {
        #[arg(long)]
        shard: String,
        #[arg(long, value_parser = ["fresh", "ready", "degraded", "leader-loss", "config-drift", "upgrade"], default_value = "ready")]
        scenario: String,
        #[arg(long)]
        target_image: Option<String>,
        /// Target wire version. Supplying this (or `--snapshot-version`) enables the
        /// rolling-vs-stop-the-world decision.
        #[arg(long)]
        wire_version: Option<u16>,
        #[arg(long)]
        snapshot_version: Option<u16>,
        #[arg(long)]
        mixed_version_safe: bool,
        #[arg(long)]
        data: Option<PathBuf>,
        #[arg(long = "bundle")]
        bundles: Vec<PathBuf>,
    },
}

fn sanitize_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "k8s_control_plane=info,tower_http=warn".into()),
        )
        .with_target(false)
        .init();

    match Cli::parse().command {
        Command::Serve {
            addr,
            state,
            no_seed,
            crd,
            crd_namespace,
            engine_console_host,
        } => {
            serve(
                addr,
                state,
                no_seed,
                crd,
                crd_namespace,
                engine_console_host,
            )
            .await
        }
        Command::Crds { out } => {
            let yaml = matchengine_k8s_control_plane::crd::crd_manifest_yaml()
                .map_err(|error| anyhow::anyhow!("render CRDs: {error}"))?;
            match out {
                Some(path) => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, yaml)
                        .with_context(|| format!("write {}", path.display()))?;
                    eprintln!("wrote CRDs to {}", path.display());
                }
                None => print!("{yaml}"),
            }
            Ok(())
        }
        Command::CrdBackup { out, namespace } => {
            let store = Store::crd(namespace, false)
                .await
                .map_err(|error| anyhow::anyhow!("open CRD catalog: {error}"))?;
            let catalog = store.snapshot();
            let text = serde_json::to_string_pretty(&catalog)?;
            match out {
                Some(path) => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, format!("{text}\n"))
                        .with_context(|| format!("write {}", path.display()))?;
                    eprintln!(
                        "wrote {} cells, {} shards, {} symbols to {}",
                        catalog.cells.len(),
                        catalog.shards.len(),
                        catalog.symbols.len(),
                        path.display()
                    );
                }
                None => println!("{text}"),
            }
            Ok(())
        }
        Command::CrdRestore { input, namespace } => {
            let bytes =
                std::fs::read(&input).with_context(|| format!("read {}", input.display()))?;
            let catalog: Catalog = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", input.display()))?;
            let report = validate_catalog(&catalog);
            if !report.is_ok() {
                bail!("catalog validation failed:\n{}", report.to_text());
            }
            let store = Store::crd(namespace, false)
                .await
                .map_err(|error| anyhow::anyhow!("open CRD catalog: {error}"))?;
            store
                .commit(catalog.clone())
                .await
                .map_err(|error| anyhow::anyhow!("restore CRDs: {error}"))?;
            eprintln!(
                "restored {} cells, {} shards, {} symbols, {} deployments, {} routes",
                catalog.cells.len(),
                catalog.shards.len(),
                catalog.symbols.len(),
                catalog.deployments.len(),
                catalog.routes.len()
            );
            Ok(())
        }
        Command::Backup {
            shard,
            id,
            out,
            mode,
            namespace,
            volume_group_snapshot_class,
            volume_snapshot_class,
            crd_only,
            etcd_snapshot,
            no_verify,
            verify_image,
            timeout_secs,
            overwrite,
        } => {
            let mode = match mode.as_str() {
                "volume" => BackupMode::Volume,
                _ => BackupMode::VolumeGroup,
            };
            let client = kube::Client::try_default()
                .await
                .map_err(|error| anyhow::anyhow!("load Kubernetes client: {error}"))?;
            let store = Store::crd(namespace.clone(), false)
                .await
                .map_err(|error| anyhow::anyhow!("open CRD catalog: {error}"))?;
            let catalog = store.snapshot();
            let shard_spec = catalog
                .shards
                .get(&shard)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown shard {shard}"))?;
            let id = id.unwrap_or_else(|| {
                format!(
                    "{shard}-{}",
                    matchengine_k8s_control_plane::store::now_epoch()
                )
            });
            if id.is_empty()
                || id == "."
                || id == ".."
                || !id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
            {
                bail!("--id must be a non-empty name using only letters, digits, '.', '-' or '_'");
            }
            let out = out.unwrap_or_else(|| PathBuf::from("backups").join(&id));
            let snapshot_options = backup::SnapshotOptions {
                namespace: namespace.clone(),
                shard: shard.clone(),
                backup_id: id.clone(),
                mode,
                volume_group_snapshot_class: volume_group_snapshot_class.clone(),
                volume_snapshot_class: volume_snapshot_class.clone(),
                verify: !no_verify,
                verify_image,
                timeout: Duration::from_secs(timeout_secs),
            };
            let mut manifest = backup::BackupManifest::new(
                id.clone(),
                &namespace,
                &shard_spec,
                mode,
                &catalog,
                &volume_group_snapshot_class,
                &volume_snapshot_class,
            );
            if !crd_only {
                let snapshots =
                    backup::create_volume_snapshots(&client, &catalog, &snapshot_options).await?;
                manifest.volume_group_snapshot =
                    snapshots.volume_group_snapshot.unwrap_or_default();
                manifest.volumes = snapshots.volumes;
            }
            let manifest = backup::write_bundle(
                &out,
                &catalog,
                manifest,
                etcd_snapshot.as_deref(),
                overwrite,
            )?;
            let objects = manifest.counts.cells
                + manifest.counts.shards
                + manifest.counts.symbols
                + manifest.counts.deployments
                + manifest.counts.routes
                + manifest.counts.market_data_services
                + manifest.counts.order_systems;
            println!(
                "backup {id}: {} volume(s), {objects} objects, checksums {}",
                manifest.volumes.len(),
                out.join(backup::CHECKSUMS_FILE).display()
            );
            if let Some(etcd) = &manifest.etcd {
                println!(
                    "etcd snapshot: {} ({} bytes, sha256 {})",
                    etcd.path, etcd.bytes, etcd.sha256
                );
            }
            println!("bundle: {}", out.display());
            Ok(())
        }
        Command::BackupVerify { input } => {
            let bundle = backup::verify_bundle(&input)?;
            println!(
                "verified backup {}: shard {}, {} volume(s), catalog {}",
                bundle.manifest.id,
                bundle.manifest.shard,
                bundle.manifest.volumes.len(),
                bundle.manifest.catalog.sha256
            );
            Ok(())
        }
        Command::BackupStatus {
            bundle_dir,
            drill_dir,
            key,
            max_backup_age_secs,
            max_drill_age_secs,
            json,
            publish_configmap,
            configmap_namespace,
        } => {
            let key = fs::read(&key)
                .map_err(|error| anyhow::anyhow!("read signing key {}: {error}", key.display()))?;
            let report = backup::check_backup_freshness(
                &bundle_dir,
                &drill_dir,
                &key,
                max_backup_age_secs,
                max_drill_age_secs,
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("backup status: ok");
                println!(
                    "  newest bundle: {} (age {}s, {} volume(s), catalog {})",
                    report.bundle_id,
                    report.bundle_age_secs,
                    report.bundle_volumes,
                    report.catalog_sha256
                );
                println!(
                    "  newest signed drill: {} (age {}s, kind {}, bundle {}, {} volume(s), signer {})",
                    report.drill_path,
                    report.drill_age_secs,
                    report.drill_kind,
                    report.drill_bundle_id,
                    report.drill_volumes_verified,
                    report.drill_signer
                );
            }
            if let Some(configmap) = publish_configmap {
                let client = kube::Client::try_default()
                    .await
                    .map_err(|error| anyhow::anyhow!("load Kubernetes client: {error}"))?;
                let published = backup::PublishedBackupStatus::new(
                    report.clone(),
                    max_backup_age_secs,
                    max_drill_age_secs,
                );
                backup::publish_backup_status(
                    &client,
                    &configmap,
                    &configmap_namespace,
                    &published,
                )
                .await?;
                eprintln!(
                    "published backup status: {configmap_namespace}/{configmap}#{}",
                    report.bundle_shard
                );
            }
            Ok(())
        }
        Command::Restore {
            input,
            cell,
            shard,
            symbol,
            market_data_service,
            order_system,
            trading_terminal,
            apply_crds,
            drill,
            restore_volumes,
            target_namespace,
            volume_prefix,
            keep,
            attest_key,
            attest_out,
            attest_signer,
            verify_image,
            timeout_secs,
            namespace,
        } => {
            let bundle = backup::verify_bundle(&input)?;
            let selection = backup::RestoreSelection {
                cells: cell.into_iter().collect(),
                shards: shard.into_iter().collect(),
                symbols: symbol.into_iter().collect(),
                market_data_services: market_data_service.into_iter().collect(),
                order_systems: order_system.into_iter().collect(),
                trading_terminals: trading_terminal.into_iter().collect(),
            };
            let plan = backup::plan_restore(&bundle, &selection)?;
            println!(
                "backup {}: catalog {}, {} cells, {} shards, {} symbols, {} order systems, {} trading terminals, {} volumes",
                bundle.manifest.id,
                plan.crd_sha256,
                plan.counts.cells,
                plan.counts.shards,
                plan.counts.symbols,
                plan.counts.order_systems,
                plan.counts.trading_terminals,
                plan.volumes
            );
            if drill {
                let attest = match (attest_key, attest_out) {
                    (Some(key_path), Some(out)) => {
                        let key = fs::read(&key_path).map_err(|error| {
                            anyhow::anyhow!("read signing key {}: {error}", key_path.display())
                        })?;
                        if key.len() < backup::MIN_DRILL_KEY_BYTES {
                            bail!(
                                "drill signing key must be at least {} bytes",
                                backup::MIN_DRILL_KEY_BYTES
                            );
                        }
                        Some((key, out))
                    }
                    (None, None) => None,
                    _ => bail!("--attest-key and --attest-out must be used together"),
                };
                let volumes = if bundle.manifest.volumes.is_empty() {
                    Vec::new()
                } else {
                    let target =
                        target_namespace.unwrap_or_else(|| bundle.manifest.namespace.clone());
                    let options = backup::VolumeRestoreOptions {
                        target_namespace: target,
                        prefix: format!("drill-{}-", sanitize_id(&bundle.manifest.id)),
                        drill: true,
                        keep,
                        verify_image,
                        timeout: Duration::from_secs(timeout_secs),
                    };
                    let client = kube::Client::try_default()
                        .await
                        .map_err(|error| anyhow::anyhow!("load Kubernetes client: {error}"))?;
                    backup::restore_volumes(&client, &bundle.manifest, &options).await?
                };
                if volumes.is_empty() {
                    println!(
                        "restore drill passed: catalog verified; the bundle has no volume snapshots"
                    );
                } else {
                    println!(
                        "restore drill passed: {} PVC(s) mounted and hashed; desired state not changed",
                        volumes.len()
                    );
                }
                if let Some((key, out)) = attest {
                    let path = backup::write_drill_attestation(
                        &bundle,
                        &key,
                        &attest_signer,
                        volumes.len(),
                        &out,
                    )?;
                    println!("signed drill: {}", path.display());
                }
                return Ok(());
            }
            if apply_crds {
                let store = Store::crd(namespace.clone(), false)
                    .await
                    .map_err(|error| anyhow::anyhow!("open CRD catalog: {error}"))?;
                let current = store.snapshot();
                let selected = backup::select_catalog(&bundle.catalog, &selection)?;
                let merged = backup::merge_catalog(&current, &selected);
                let report = validate_catalog(&merged);
                if !report.is_ok() {
                    bail!(
                        "merged catalog validation failed:
{}",
                        report.to_text()
                    );
                }
                store
                    .commit(merged)
                    .await
                    .map_err(|error| anyhow::anyhow!("restore CRDs: {error}"))?;
                println!("restored CRDs from backup {}", bundle.manifest.id);
            }
            if restore_volumes {
                let target = target_namespace.unwrap_or_else(|| bundle.manifest.namespace.clone());
                let options = backup::VolumeRestoreOptions {
                    target_namespace: target,
                    prefix: volume_prefix,
                    drill: false,
                    keep: true,
                    verify_image,
                    timeout: Duration::from_secs(timeout_secs),
                };
                let client = kube::Client::try_default()
                    .await
                    .map_err(|error| anyhow::anyhow!("load Kubernetes client: {error}"))?;
                let volumes = backup::restore_volumes(&client, &bundle.manifest, &options).await?;
                println!("restored {} PVC(s): {}", volumes.len(), volumes.join(", "));
            }
            if !apply_crds && !restore_volumes {
                println!("verified only; pass --apply-crds, --restore-volumes or --drill");
            }
            Ok(())
        }
        Command::Render {
            shard,
            all,
            terminal,
            profile,
            namespace,
            storage_class,
            replicas,
            no_non_root,
            data,
            bundles,
            out,
        } => {
            let catalog = load_catalog(data, &bundles)?;
            let options = RenderOptions {
                profile: if profile == "local" {
                    Profile::Local
                } else {
                    Profile::Production
                },
                namespace,
                image_pull_policy: None,
                storage_class,
                replicas,
                non_root: Some(!no_non_root),
            };
            let mut items = Vec::new();
            if let Some(name) = terminal {
                let objects = render_trading_terminal(&catalog, &name, "").map_err(|error| {
                    anyhow::anyhow!(
                        "cannot render trading terminal {name}: {error}{}",
                        issues(&error)
                    )
                })?;
                items.extend(objects);
            } else {
                let names: Vec<String> = if all {
                    catalog.shards.keys().cloned().collect()
                } else {
                    vec![shard.context("--shard is required unless --all or --terminal is set")?]
                };
                for name in &names {
                    let objects = render_shard(&catalog, name, &options).map_err(|error| {
                        anyhow::anyhow!("cannot render {name}: {error}{}", issues(&error))
                    })?;
                    items.extend(objects);
                }
            }
            let list = as_list(items);
            let text = serde_json::to_string_pretty(&list)?;
            match out {
                Some(path) => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, format!("{text}\n"))
                        .with_context(|| format!("write {}", path.display()))?;
                    eprintln!(
                        "wrote {} objects to {}",
                        list["items"].as_array().map(Vec::len).unwrap_or(0),
                        path.display()
                    );
                }
                None => println!("{text}"),
            }
            Ok(())
        }
        Command::Seed { out } => {
            let bundles = demo_bundles();
            match out {
                Some(dir) => {
                    std::fs::create_dir_all(&dir)
                        .with_context(|| format!("create {}", dir.display()))?;
                    for bundle in &bundles {
                        let path = dir.join(format!("{}.json", bundle.shard.name));
                        std::fs::write(
                            &path,
                            format!("{}\n", serde_json::to_string_pretty(bundle)?),
                        )
                        .with_context(|| format!("write {}", path.display()))?;
                        eprintln!("wrote {}", path.display());
                    }
                }
                None => println!("{}", serde_json::to_string_pretty(&bundles)?),
            }
            Ok(())
        }
        Command::Validate {
            data,
            bundles,
            symbol,
        } => {
            let catalog = load_catalog(data, &bundles)?;
            let report = match symbol {
                Some(name) => match catalog.symbols.get(&name) {
                    Some(symbol) => validate_symbol(symbol),
                    None => bail!("unknown symbol {name}"),
                },
                None => validate_catalog(&catalog),
            };
            print!("{}", report.to_text());
            if report.is_ok() {
                eprintln!(
                    "catalog valid: {} shards, {} symbols, {} warnings",
                    catalog.shards.len(),
                    catalog.symbols.len(),
                    report.warnings.len()
                );
                Ok(())
            } else {
                bail!("{} validation errors", report.errors.len())
            }
        }
        Command::Plan {
            shard,
            scenario,
            target_image,
            wire_version,
            snapshot_version,
            mixed_version_safe,
            data,
            bundles,
        } => {
            let catalog = load_catalog(data, &bundles)?;
            let scenario = Scenario::parse(&scenario)?;
            let observation = simulate(&catalog, &shard, scenario)?;
            let target = release(wire_version, snapshot_version, mixed_version_safe);
            let plan = plan_shard(
                &catalog,
                &shard,
                &observation,
                target_image.as_deref(),
                target.as_ref(),
            )
            .map_err(|error| anyhow::anyhow!("{error}{}", issues(&error)))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "summary": summarize(&plan),
                    "plan": plan,
                }))?
            );
            Ok(())
        }
    }
}

fn issues(error: &matchengine_k8s_control_plane::error::ApiError) -> String {
    if error.issues.is_empty() {
        String::new()
    } else {
        let detail = error
            .issues
            .iter()
            .map(|issue| format!("\n  [{}] {}: {}", issue.code, issue.object, issue.message))
            .collect::<String>();
        format!("\n{detail}")
    }
}

fn release(
    wire_version: Option<u16>,
    snapshot_version: Option<u16>,
    mixed_version_safe: bool,
) -> Option<Release> {
    if wire_version.is_none() && snapshot_version.is_none() {
        return None;
    }
    let default = Release::default();
    Some(Release {
        wire_version: wire_version.unwrap_or(default.wire_version),
        snapshot_version: snapshot_version.unwrap_or(default.snapshot_version),
        mixed_version_safe,
    })
}

fn load_catalog(data: Option<PathBuf>, bundles: &[PathBuf]) -> Result<Catalog> {
    if !bundles.is_empty() {
        let mut catalog = Catalog::default();
        for path in bundles {
            let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
            let bundle: ShardBundle = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse bundle {}", path.display()))?;
            catalog.insert_bundle(bundle);
        }
        return Ok(catalog);
    }
    match data {
        Some(path) => {
            let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            serde_json::from_slice(&bytes)
                .with_context(|| format!("parse catalog {}", path.display()))
        }
        None => Ok(demo_catalog()),
    }
}

async fn serve(
    addr: String,
    state: Option<PathBuf>,
    no_seed: bool,
    crd: bool,
    crd_namespace: String,
    engine_console_host: Option<String>,
) -> Result<()> {
    let store = if crd {
        Store::crd(crd_namespace.clone(), !no_seed)
            .await
            .map_err(|error| anyhow::anyhow!("open CRD catalog: {error}"))?
    } else {
        Store::open(state.clone(), !no_seed)
            .map_err(|error| anyhow::anyhow!("open catalog: {error}"))?
    };
    let store = Arc::new(store);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    let local = listener.local_addr()?;
    tracing::info!(%local, state = ?state, crd, crd_namespace, mode = store.mode(), "control plane listening");
    println!("control plane GUI: http://{local}/");
    let console = api::ConsoleOptions {
        host: engine_console_host,
    };
    axum::serve(listener, api::router_with_options(store, console))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve")?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_cli_parses_volume_mode_and_optional_classes() {
        let cli = Cli::try_parse_from([
            "k8s-control-plane",
            "backup",
            "fx-eu-01",
            "--id",
            "fx-eu-01-nightly",
            "--mode",
            "volume",
            "--volume-snapshot-class",
            "csi-snap",
            "--no-verify",
        ])
        .unwrap();
        match cli.command {
            Command::Backup {
                shard,
                id,
                mode,
                volume_snapshot_class,
                no_verify,
                ..
            } => {
                assert_eq!(shard, "fx-eu-01");
                assert_eq!(id.as_deref(), Some("fx-eu-01-nightly"));
                assert_eq!(mode, "volume");
                assert_eq!(volume_snapshot_class, "csi-snap");
                assert!(no_verify);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn restore_cli_rejects_apply_and_drill_together() {
        let error = Cli::try_parse_from([
            "k8s-control-plane",
            "restore",
            "--input",
            "backups/fx-eu-01",
            "--apply-crds",
            "--drill",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn restore_cli_requires_drill_for_attestation() {
        let error = Cli::try_parse_from([
            "k8s-control-plane",
            "restore",
            "--input",
            "backups/fx-eu-01",
            "--attest-key",
            "drill.key",
            "--attest-out",
            "drills",
        ])
        .unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        let cli = Cli::try_parse_from([
            "k8s-control-plane",
            "restore",
            "--input",
            "backups/fx-eu-01",
            "--drill",
            "--attest-key",
            "drill.key",
            "--attest-out",
            "drills",
            "--attest-signer",
            "nightly",
        ])
        .unwrap();
        match cli.command {
            Command::Restore {
                attest_signer,
                attest_key,
                attest_out,
                ..
            } => {
                assert_eq!(attest_signer, "nightly");
                assert!(attest_key.is_some());
                assert!(attest_out.is_some());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn backup_status_cli_parses_defaults() {
        let cli = Cli::try_parse_from(["k8s-control-plane", "backup-status", "--key", "drill.key"])
            .unwrap();
        match cli.command {
            Command::BackupStatus {
                bundle_dir,
                drill_dir,
                max_backup_age_secs,
                max_drill_age_secs,
                json,
                ..
            } => {
                assert_eq!(bundle_dir, PathBuf::from("backups"));
                assert_eq!(drill_dir, PathBuf::from("drills"));
                assert_eq!(max_backup_age_secs, backup::DEFAULT_MAX_BACKUP_AGE_SECS);
                assert_eq!(max_drill_age_secs, backup::DEFAULT_MAX_DRILL_AGE_SECS);
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn target_release_is_optional() {
        assert!(release(None, None, false).is_none());
        let some = release(Some(8), None, true).unwrap();
        assert_eq!(some.wire_version, 8);
        assert_eq!(some.snapshot_version, Release::default().snapshot_version);
        assert!(some.mixed_version_safe);
    }

    #[test]
    fn render_cli_accepts_a_trading_terminal_name() {
        let cli = Cli::try_parse_from(["k8s-control-plane", "render", "--terminal", "terminal-eu"])
            .unwrap();
        match cli.command {
            Command::Render {
                shard,
                all,
                terminal,
                ..
            } => {
                assert_eq!(terminal.as_deref(), Some("terminal-eu"));
                assert!(shard.is_none());
                assert!(!all);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        // Existing shard rendering is unchanged, and a terminal is not a shard.
        let cli =
            Cli::try_parse_from(["k8s-control-plane", "render", "--shard", "fx-eu-01"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Render { ref terminal, .. } if terminal.is_none()
        ));
        assert_eq!(
            Cli::try_parse_from([
                "k8s-control-plane",
                "render",
                "--shard",
                "fx-eu-01",
                "--terminal",
                "terminal-eu",
            ])
            .unwrap_err()
            .kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
    }

    #[test]
    fn restore_cli_accepts_a_trading_terminal_selection() {
        let cli = Cli::try_parse_from([
            "k8s-control-plane",
            "restore",
            "--input",
            "backups/fx-eu-01",
            "--trading-terminal",
            "terminal-eu",
            "--trading-terminal",
            "terminal-us",
        ])
        .unwrap();
        match cli.command {
            Command::Restore {
                trading_terminal, ..
            } => assert_eq!(trading_terminal, ["terminal-eu", "terminal-us"]),
            other => panic!("unexpected command: {other:?}"),
        }
    }
}

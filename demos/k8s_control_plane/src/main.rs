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
//! (From the repository root, add `--manifest-path demos/k8s_control_plane/Cargo.toml`.)

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;

use matchengine_k8s_control_plane::api;
use matchengine_k8s_control_plane::bundle::{ShardBundle, demo_bundles};
use matchengine_k8s_control_plane::model::Release;
use matchengine_k8s_control_plane::reconcile::{Scenario, plan_shard, simulate, summarize};
use matchengine_k8s_control_plane::render::{Profile, RenderOptions, as_list, render_shard};
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
        /// Host dialled by the engine-console proxy instead of the pod FQDN. Set it to
        /// `127.0.0.1` when the demo manager and a local engine share a machine.
        #[arg(long, env = "ENGINE_CONSOLE_HOST")]
        engine_console_host: Option<String>,
    },
    /// Render one shard (or every shard) as a `v1/List` of Kubernetes objects.
    Render {
        #[arg(long)]
        shard: Option<String>,
        /// Render every shard into one `List` (separated in the output).
        #[arg(long)]
        all: bool,
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
            engine_console_host,
        } => serve(addr, state, no_seed, engine_console_host).await,
        Command::Render {
            shard,
            all,
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
            let names: Vec<String> = if all {
                catalog.shards.keys().cloned().collect()
            } else {
                vec![shard.context("--shard is required unless --all is set")?]
            };
            for name in &names {
                let objects = render_shard(&catalog, name, &options).map_err(|error| {
                    anyhow::anyhow!("cannot render {name}: {error}{}", issues(&error))
                })?;
                items.extend(objects);
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
    engine_console_host: Option<String>,
) -> Result<()> {
    let store = Arc::new(
        Store::open(state.clone(), !no_seed)
            .map_err(|error| anyhow::anyhow!("open catalog: {error}"))?,
    );
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    let local = listener.local_addr()?;
    tracing::info!(%local, state = ?state, "control plane listening");
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
    fn target_release_is_optional() {
        assert!(release(None, None, false).is_none());
        let some = release(Some(8), None, true).unwrap();
        assert_eq!(some.wire_version, 8);
        assert_eq!(some.snapshot_version, Release::default().snapshot_version);
        assert!(some.mixed_version_safe);
    }
}

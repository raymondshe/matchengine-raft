//! Lease-elected cell operator for matchengine-raft symbol shards.
//!
//! ```text
//! cargo run --bin match-operator -- \
//!   --lease-namespace match-control --lease-name match-engine-operator \
//!   --profile local
//! ```

use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use matchengine_k8s_control_plane::operator::{OperatorOptions, run};
use matchengine_k8s_control_plane::render::Profile;

#[derive(Debug, Parser)]
#[command(
    name = "match-operator",
    version,
    about = "Lease-elected Kubernetes operator for matchengine-raft CRDs"
)]
struct Opt {
    /// Namespace containing the leader-election Lease.
    #[arg(
        long,
        default_value = "match-control",
        env = "OPERATOR_LEASE_NAMESPACE"
    )]
    lease_namespace: String,
    /// Leader-election Lease name.
    #[arg(
        long,
        default_value = "match-engine-operator",
        env = "OPERATOR_LEASE_NAME"
    )]
    lease_name: String,
    /// Unique holder identity. Defaults to POD_NAME, HOSTNAME or the process id.
    #[arg(long, env = "OPERATOR_IDENTITY")]
    identity: Option<String>,
    /// Lease duration in seconds.
    #[arg(long, default_value_t = 15, env = "OPERATOR_LEASE_DURATION_SECONDS")]
    lease_duration_seconds: u64,
    /// Renewal interval in seconds; must be shorter than the lease duration.
    #[arg(long, default_value_t = 5, env = "OPERATOR_LEASE_RENEW_SECONDS")]
    lease_renew_seconds: u64,
    /// Full reconcile interval. Watches still trigger an immediate reconcile.
    #[arg(long, default_value_t = 30, env = "OPERATOR_RESYNC_SECONDS")]
    resync_seconds: u64,
    /// Restrict CRDs, workloads and pruning to one namespace. Omit for cell-wide.
    #[arg(long, env = "OPERATOR_NAMESPACE")]
    namespace: Option<String>,
    /// Retention window for PVCs whose shard CR no longer exists. `0` marks them
    /// `matching.io/retain-until=manual` instead of starting a deletion timer.
    #[arg(long, default_value_t = 0, env = "OPERATOR_RETENTION_SECONDS")]
    retention_seconds: u64,
    /// Per-pod `/health` timeout.
    #[arg(long, default_value_t = 750, env = "OPERATOR_OBSERVE_TIMEOUT_MS")]
    observe_timeout_ms: u64,
    /// Manifest profile used by the renderer.
    #[arg(
        long,
        value_parser = ["local", "production"],
        default_value = "production",
        env = "OPERATOR_PROFILE"
    )]
    profile: String,
    /// Log desired changes and status, but do not apply/prune or write status.
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "match_operator=info,matchengine_k8s_control_plane=info".into()
            }),
        )
        .with_target(false)
        .init();

    let opt = Opt::parse();
    let identity = opt
        .identity
        .or_else(|| std::env::var("POD_NAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| format!("operator-{}", std::process::id()));
    let profile = match opt.profile.as_str() {
        "local" => Profile::Local,
        _ => Profile::Production,
    };
    let options = OperatorOptions {
        profile,
        resync: Duration::from_secs(opt.resync_seconds.max(1)),
        observe_timeout: Duration::from_millis(opt.observe_timeout_ms.max(100)),
        lease_namespace: opt.lease_namespace,
        lease_name: opt.lease_name,
        identity,
        lease_duration: Duration::from_secs(opt.lease_duration_seconds),
        lease_renew: Duration::from_secs(opt.lease_renew_seconds),
        namespace: opt.namespace,
        retention_seconds: opt.retention_seconds,
        dry_run: opt.dry_run,
    };
    let client = kube::Client::try_default()
        .await
        .context("load Kubernetes client")?;
    run(client, options).await
}

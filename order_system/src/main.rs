mod api;
mod freeze;
mod state;

use std::fs;
use std::path::PathBuf;

use actix_web::App;
use actix_web::HttpServer;
use actix_web::middleware::Logger;
use actix_web::web;
use clap::Parser;

use crate::freeze::Product;
use crate::state::TargetSpec;
use crate::state::TargetsFile;

/// Standalone demo order desk for a running matchengine-raft node.
#[derive(Parser, Debug)]
#[command(about = "Demo order desk WebUI for a running matchengine-raft node")]
struct Opt {
    /// Address for the demo WebUI and its BFF API.
    #[arg(long, default_value = "127.0.0.1:24300")]
    addr: String,

    /// JSON file with one target per engine/symbol. When omitted, the legacy
    /// --engine/--perp-engine flags build `spot` and `perpetual` targets.
    #[arg(long, env = "ORDER_TARGETS")]
    targets: Option<PathBuf>,

    /// HTTP address of the running engine node that serves the Spot orderbook.
    #[arg(long, default_value = "127.0.0.1:24301")]
    engine: String,

    /// Raft node id of the engine at --engine.
    #[arg(long, default_value_t = 1)]
    engine_id: u64,

    /// HTTP address of a separate engine node that serves the Perpetual
    /// orderbook. Omit it to run a Spot-only desk with Perpetual disabled.
    #[arg(long)]
    perp_engine: Option<String>,

    /// Raft node id of the engine at --perp-engine.
    #[arg(long, default_value_t = 1)]
    perp_engine_id: u64,

    /// First user ID handled by this order-system shard (inclusive).
    #[arg(long, default_value_t = 0)]
    user_id_min: u64,

    /// Last user ID handled by this order-system shard (inclusive).
    #[arg(long, default_value_t = u64::MAX)]
    user_id_max: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info,actix_web=warn"));
    let opt = Opt::parse();

    if let Some(perp_engine) = &opt.perp_engine
        && *perp_engine == opt.engine
    {
        anyhow::bail!(
            "--perp-engine must differ from --engine: Spot and Perpetual each need their own engine and orderbook"
        );
    }

    if opt.user_id_min > opt.user_id_max {
        anyhow::bail!("--user-id-min must be <= --user-id-max");
    }
    let targets = if let Some(path) = &opt.targets {
        let raw = fs::read_to_string(path)
            .map_err(|error| anyhow::anyhow!("read --targets {}: {error}", path.display()))?;
        serde_json::from_str::<TargetsFile>(&raw)
            .map_err(|error| anyhow::anyhow!("parse --targets {}: {error}", path.display()))?
            .targets
    } else {
        let mut targets = vec![TargetSpec {
            id: "spot".to_string(),
            label: "Spot".to_string(),
            engine: opt.engine.clone(),
            engine_id: opt.engine_id,
            product: Product::Spot,
            engine_members: Vec::new(),
        }];
        if let Some(engine) = opt.perp_engine.clone() {
            targets.push(TargetSpec {
                id: "perpetual".to_string(),
                label: "Perpetual".to_string(),
                engine,
                engine_id: opt.perp_engine_id,
                product: Product::Perpetual,
                engine_members: Vec::new(),
            });
        }
        targets
    };
    if targets.is_empty() {
        anyhow::bail!("at least one --targets entry is required");
    }
    let mut seen = std::collections::BTreeSet::new();
    for target in &targets {
        if target.id.trim().is_empty() || !seen.insert(target.id.clone()) {
            anyhow::bail!("each target needs a non-empty unique id");
        }
        if target.engine.trim().is_empty() {
            anyhow::bail!("target {} needs a non-empty engine address", target.id);
        }
    }
    let app = state::DemoApp::new(targets).with_user_range(opt.user_id_min, opt.user_id_max);
    let target_summary = app
        .backends()
        .targets
        .iter()
        .map(|target| format!("{}@{}", target.id, target.http_addr))
        .collect::<Vec<_>>()
        .join(", ");
    // The engine may still be starting or uninitialized; the page polls until it
    // is ready and shows the connection error until then.
    let app = web::Data::new(app);

    let server = HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(app.clone())
            .app_data(web::JsonConfig::default().limit(1024 * 1024))
            .service(api::redirect)
            .service(api::redirect_demo)
            .service(api::index)
            .service(api::js)
            .service(api::css)
            .service(api::health)
            .service(api::ready)
            .service(api::backends)
            .service(api::state)
            .service(api::book)
            .service(api::events)
            .service(api::place)
            .service(api::freeze_preview)
            .service(api::cancel)
            .service(api::amend)
            .service(api::configure_symbol)
    })
    .bind(&opt.addr)?;

    println!(
        "demo order desk: http://{}/demo/ (users {}..={}; targets {}; legacy spot engine {})",
        opt.addr, opt.user_id_min, opt.user_id_max, target_summary, opt.engine
    );
    server.run().await?;
    Ok(())
}

mod api;
mod freeze;
mod state;

use actix_web::App;
use actix_web::HttpServer;
use actix_web::middleware::Logger;
use actix_web::web;
use clap::Parser;

/// Standalone demo order desk for a running matchengine-raft node.
#[derive(Parser, Debug)]
#[command(about = "Demo order desk WebUI for a running matchengine-raft node")]
struct Opt {
    /// Address for the demo WebUI and its BFF API.
    #[arg(long, default_value = "127.0.0.1:24300")]
    addr: String,

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

    let perpetual = opt
        .perp_engine
        .clone()
        .map(|addr| (opt.perp_engine_id, addr));
    let app = state::DemoApp::new(opt.engine_id, opt.engine.clone(), perpetual);
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
        "demo order desk: http://{}/demo/ (spot engine {}; perpetual engine {})",
        opt.addr,
        opt.engine,
        opt.perp_engine.as_deref().unwrap_or("not configured")
    );
    server.run().await?;
    Ok(())
}

mod api;
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

    /// HTTP address of the running engine node.
    #[arg(long, default_value = "127.0.0.1:24301")]
    engine: String,

    /// Raft node id of the engine at --engine.
    #[arg(long, default_value_t = 1)]
    engine_id: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info,actix_web=warn"));
    let opt = Opt::parse();

    let app = state::DemoApp::new(opt.engine_id, opt.engine.clone());
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
            .service(api::state)
            .service(api::book)
            .service(api::events)
            .service(api::place)
            .service(api::cancel)
            .service(api::amend)
            .service(api::configure_symbol)
    })
    .bind(&opt.addr)?;

    println!(
        "demo order desk: http://{}/demo/ (engine {})",
        opt.addr, opt.engine
    );
    server.run().await?;
    Ok(())
}

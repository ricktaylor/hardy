use std::path::PathBuf;

use clap::Parser;
use hardy_async::TaskPool;
use hardy_otel::init;
use tracing::info;

use self::server::BpaServer;

mod bpsec;
mod config;
mod error;
mod server;
mod static_routes;

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Use a custom configuration file
    #[arg(short = 'c', long = "config", value_name = "FILE")]
    config_file: Option<PathBuf>,

    /// Upgrade the bundle store to the current format
    #[arg(short = 'u', long = "upgrade-store")]
    upgrade_storage: bool,

    /// Attempt to recover any damaged records in the store
    #[arg(short = 'r', long = "recover-store")]
    recover_storage: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let config = config::Config::load(args.config_file)?;
    let _guard = init(PKG_NAME, PKG_VERSION, config.log_level);

    info!("{} version {} starting...", PKG_NAME, PKG_VERSION);

    // Process policy lives here: the top-level task pool and the wiring of
    // SIGINT/SIGTERM to its cancellation token.
    let tasks = TaskPool::new();
    hardy_async::signal::listen_for_cancel(&tasks);

    let server = BpaServer::new(config, tasks, args.upgrade_storage, args.recover_storage).await?;
    server.run().await
}

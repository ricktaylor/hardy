use std::path::PathBuf;

use clap::Parser;
use hardy_async::{TaskPool, signal::listen_for_cancel};
use hardy_otel::init;
use tracing::{error, info};

use crate::server::Tcpclv4Server;

mod config;
mod server;

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    // Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = config::Config::load(args.config)?;

    let _guard = init(PKG_NAME, PKG_VERSION, config.log_level);

    info!("{} version {} starting...", PKG_NAME, PKG_VERSION);

    // Process policy lives here: the top-level task pool and the wiring of
    // SIGINT/SIGTERM to its cancellation token.
    let tasks = TaskPool::new();
    listen_for_cancel(&tasks);

    let server = Tcpclv4Server::new(config, tasks).inspect_err(|e| error!("{e}"))?;
    server.run().await.inspect_err(|e| error!("{e}"))
}

mod cla;
mod codec;
mod config;
mod connect;
mod listen;

use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use hardy_async::{TaskPool, sync::spin::Once};
use hardy_bpv7::eid::NodeId;
use tracing::{debug, error, info, warn};

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = config::Config::load(args.config)?;

    {
        use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};
        let filter = EnvFilter::builder()
            .with_default_directive(
                tracing_subscriber::filter::LevelFilter::from_level(config.log_level).into(),
            )
            .from_env_lossy();
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_filter(filter))
            .init();
    }

    info!("{} version {} starting...", PKG_NAME, PKG_VERSION);

    inner_main(config).await.inspect_err(|e| error!("{e}"))
}

async fn inner_main(config: config::Config) -> anyhow::Result<()> {
    let cla = Arc::new(cla::Cla::new(config.cla));

    let tasks = TaskPool::new();
    hardy_async::signal::listen_for_cancel(&tasks);

    info!("Connecting to BPA at {}", config.bpa_address);

    let client = hardy_proto::client::BpaClient::new(config.bpa_address, tasks.clone())
        .map_err(|e| anyhow::anyhow!("Invalid BPA address: {e}"))?;

    let node_ids = client
        .register_cla(config.cla_name.clone(), cla.clone())
        .await
        .map_err(|e| anyhow::anyhow!("CLA registration failed: {e}"))?;

    info!(
        "CLA {} registered, node IDs: {:?}",
        config.cla_name,
        node_ids.iter().map(|n| n.to_string()).collect::<Vec<_>>()
    );

    info!("Started successfully");

    tasks.cancel_token().cancelled().await;

    // Gracefully unregister from the BPA before shutting down
    cla.unregister().await;

    tasks.shutdown().await;

    info!("Stopped");

    Ok(())
}

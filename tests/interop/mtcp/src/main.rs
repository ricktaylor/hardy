mod cla;
mod codec;
mod config;
mod connect;
mod listen;

use hardy_async::TaskPool;
use hardy_async::sync::spin::Once;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpv7::eid::NodeId;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use clap::Parser;


const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = config::Config::load(args.config)?;

    let log_level = std::env::var("MTCP_CLA_LOG_LEVEL")
        .ok()
        .and_then(|s| s.parse::<tracing::Level>().ok())
        .or(config.log_level)
        .unwrap_or(tracing::Level::ERROR);

    {
        use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};
        let filter = EnvFilter::builder()
            .with_default_directive(
                tracing_subscriber::filter::LevelFilter::from_level(log_level).into(),
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

    info!("Connecting to BPA at {}", config.bpa_address);

    let remote_bpa = hardy_proto::client::RemoteBpa::new(config.bpa_address);

    let node_ids = remote_bpa
        .register_cla(
            config.cla_name.clone(),
            Some(hardy_bpa::cla::ClaAddressType::Tcp),
            cla.clone(),
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("CLA registration failed: {e}"))?;

    info!(
        "CLA {} registered, node IDs: {:?}",
        config.cla_name,
        node_ids.iter().map(|n| n.to_string()).collect::<Vec<_>>()
    );

    let tasks = TaskPool::new();
    hardy_async::signal::listen_for_cancel(&tasks);

    info!("Started successfully");

    tasks.cancel_token().cancelled().await;

    // Gracefully unregister from the BPA before shutting down
    cla.unregister().await;

    tasks.shutdown().await;

    info!("Stopped");

    Ok(())
}

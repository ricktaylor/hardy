mod cla;
mod codec;
mod config;
mod connect;
mod listen;

use std::{path::PathBuf, sync::Arc};

use anyhow::Context;
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

    inner_main(config).await.inspect_err(|e| error!("{e:#}"))
}

async fn inner_main(config: config::Config) -> anyhow::Result<()> {
    let cla = Arc::new(cla::Cla::new(config.cla));

    let tasks = TaskPool::new();
    hardy_async::signal::listen_for_cancel(&tasks);

    info!("Connecting to BPA at {}", config.bpa_address);

    let client = hardy_proto::client::BpaClient::new(config.bpa_address, tasks.clone())
        .context("Invalid BPA address")?;

    // Register: the registration handle returns once the handshake completes (a
    // failure returns here), and its session runs on the pool until the
    // pool is cancelled, the BPA closes it, or the connection is lost.
    // There is no automatic re-registration; a supervisor restarts the
    // process. The registration ran its own `on_unregister`, so teardown
    // here is just the pool.
    let handle = client
        .register_cla(config.cla_name.clone(), cla.clone())
        .await
        .context("CLA registration failed")?;
    info!(
        "CLA {} registered, node IDs: {:?}",
        config.cla_name,
        handle
            .id()
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
    );

    let result = handle.await;
    // Read before `shutdown`, which cancels the token itself.
    let locally_stopped = tasks.is_cancelled();
    tasks.shutdown().await;
    info!("Stopped");

    match result {
        Ok(()) if locally_stopped => Ok(()),
        // A clean end without a local shutdown is the BPA closing the
        // session; this daemon never unregisters unprompted, so exit
        // nonzero and let a supervisor restart it once the BPA is back.
        Ok(()) => Err(anyhow::anyhow!("The BPA closed the CLA session")),
        Err(e) => Err(e).context("CLA session ended"),
    }
}

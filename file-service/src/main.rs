use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use config::Config;
use hardy_async::TaskPool;
use hardy_async::signal::listen_for_cancel;
use hardy_bpa::bpa::BpaRegistration;
use hardy_file_service::FileService;
use hardy_proto::client::RemoteBpa;
use tracing::info;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

mod config;

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = Config::load(args.config)?;

    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::from_level(config.log_level).into())
        .from_env_lossy();
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(filter))
        .init();

    info!("{PKG_NAME} version {PKG_VERSION} starting...");

    let service = Arc::new(FileService::new(
        config.destination,
        config.lifetime,
        config.outbox,
        config.inbox,
    )?);

    info!("Connecting to BPA at {}", config.bpa_address);

    let remote_bpa = RemoteBpa::new(config.bpa_address);
    let eid = remote_bpa
        .register_application(config.service_id, service.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Application registration failed: {e}"))?;

    info!("Registered as {eid}");

    let tasks = TaskPool::new();
    listen_for_cancel(&tasks);

    info!("Started successfully");

    tasks.cancel_token().cancelled().await;

    service.unregister().await;
    tasks.shutdown().await;

    info!("Stopped");

    Ok(())
}

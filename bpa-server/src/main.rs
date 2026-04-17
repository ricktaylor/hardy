mod bpa;
mod cli;
mod config;
mod error;

use hardy_async::TaskPool;
use tracing::{error, info};

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(feature = "otel")]
fn configure_tracing(log_level: tracing::Level) -> hardy_otel::OtelGuard {
    hardy_otel::init(PKG_NAME, PKG_VERSION, log_level)
}

#[cfg(not(feature = "otel"))]
fn configure_tracing(log_level: tracing::Level) {
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Some(args) = cli::parse() else {
        return Ok(());
    };

    let mut config = config::Config::load(args.config_file)?;
    let _guard = configure_tracing(config.log_level);

    info!("{} version {} starting...", PKG_NAME, PKG_VERSION);

    #[cfg(feature = "grpc")]
    let grpc_config = config.grpc.take();

    let bpa = bpa::build(config, args.upgrade_storage).await?;

    bpa.start(args.recover_storage);

    let tasks = TaskPool::new();
    hardy_async::signal::listen_for_cancel(&tasks);

    #[cfg(feature = "grpc")]
    if let Some(grpc_config) = grpc_config {
        let server = hardy_proto::server::GrpcServer::new(&grpc_config, bpa.clone())
            .map_err(|e| anyhow::anyhow!("Failed to create gRPC server: {e}"))?;
        let cancel = tasks.cancel_token().clone();
        hardy_async::spawn!(tasks, "grpc_server", async move {
            if let Err(e) = server.serve(cancel).await {
                error!("gRPC server failed: {e}");
            }
        });
    }

    info!("Started successfully");

    tasks.cancel_token().cancelled().await;
    tasks.shutdown().await;
    bpa.shutdown().await;

    info!("Stopped");

    Ok(())
}

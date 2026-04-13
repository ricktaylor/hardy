mod bpa;
mod cli;
mod config;
mod error;

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

    let config = config::Config::load(args.config_file)?;
    let _ = configure_tracing(config.log_level);

    info!("{} version {} starting...", PKG_NAME, PKG_VERSION);

    bpa::run(config, args.upgrade_storage, args.recover_storage)
        .await
        .inspect_err(|e| error!("{e}"))
}

mod clas;
mod cli;
mod config;
mod filters;
mod grpc;
mod policy;
mod services;
mod static_routes;

use hardy_async::TaskPool;
use std::sync::Arc;
use trace_err::*;
use tracing::{debug, error, info, warn};

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

fn listen_for_cancel(tasks: &TaskPool) {
    #[cfg(unix)]
    let mut term_handler =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .trace_expect("Failed to register signal handlers");
    #[cfg(not(unix))]
    let mut term_handler = std::future::pending();

    let cancel_token = tasks.cancel_token().clone();
    hardy_async::spawn!(tasks, "signal_handler", async move {
        tokio::select! {
            _ = term_handler.recv() => {
                info!("Received terminate signal, stopping...");
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received CTRL+C, stopping...");
            }
        }
        cancel_token.cancel();
    });
}

type StorageBackends = (
    Option<Arc<dyn hardy_bpa::storage::MetadataStorage>>,
    Option<Arc<dyn hardy_bpa::storage::BundleStorage>>,
);

#[allow(unused_variables)]
async fn init_storage(
    storage: &config::StorageConfig,
    upgrade_storage: bool,
) -> anyhow::Result<StorageBackends> {
    let metadata_storage = match storage.metadata.as_ref() {
        None => None,
        Some(cfg) => Some(match cfg {
            config::MetadataStorage::Memory(cfg) => hardy_bpa::storage::metadata_mem::new(cfg),

            #[cfg(feature = "sqlite-storage")]
            config::MetadataStorage::Sqlite(cfg) => hardy_sqlite_storage::new(cfg, upgrade_storage),

            #[cfg(feature = "postgres-storage")]
            config::MetadataStorage::Postgres(cfg) => {
                hardy_postgres_storage::new(cfg, upgrade_storage)
                    .await
                    .trace_expect("Failed to connect to Postgres metadata store")
            }
        }),
    };

    let bundle_storage = match storage.bundle.as_ref() {
        None => None,
        Some(cfg) => Some(match cfg {
            config::BundleStorage::Memory(cfg) => hardy_bpa::storage::bundle_mem::new(cfg),

            #[cfg(feature = "localdisk-storage")]
            config::BundleStorage::LocalDisk(cfg) => {
                hardy_localdisk_storage::new(cfg, upgrade_storage)
            }

            #[cfg(feature = "s3-storage")]
            config::BundleStorage::S3(cfg) => hardy_s3_storage::new(cfg).await?,
        }),
    };

    Ok((metadata_storage, bundle_storage))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Some(cli) = cli::parse() else {
        return Ok(());
    };

    let config = config::load(&cli);

    let log_level = std::env::var("HARDY_BPA_SERVER_LOG_LEVEL")
        .ok()
        .and_then(|s| s.parse::<tracing::Level>().ok())
        .or(config.log_level)
        .unwrap_or(tracing::Level::ERROR);

    #[cfg(feature = "otel")]
    let _guard = hardy_otel::init(PKG_NAME, PKG_VERSION, log_level);

    #[cfg(not(feature = "otel"))]
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

    inner_main(config, cli).await.inspect_err(|e| error!("{e}"))
}

async fn inner_main(config: config::Config, cli: cli::Args) -> anyhow::Result<()> {
    let (metadata_storage, bundle_storage) =
        init_storage(&config.storage, cli.upgrade_storage).await?;

    let bpa_config = hardy_bpa::config::Config {
        lru_capacity: config.storage.lru_capacity,
        max_cached_bundle_size: config.storage.max_cached_bundle_size,
        ..config.bpa
    };
    info!("Configured node IDs: {}", bpa_config.node_ids);
    let bpa = Arc::new(hardy_bpa::bpa::Bpa::new(
        bpa_config,
        metadata_storage,
        bundle_storage,
    ));

    // Prepare for graceful shutdown
    let tasks = TaskPool::new();

    // Load static routes
    if let Some(config) = &config.static_routes {
        static_routes::init(config, &bpa, &tasks).await?;
    }

    // Register filters
    filters::register(
        &config.rfc9171_validity,
        #[cfg(feature = "ipn-legacy-filter")]
        &config.ipn_legacy_nodes,
        &bpa,
    )?;

    services::register(&config.built_in_services, bpa.as_ref()).await;

    bpa.start(cli.recover_storage);

    clas::init(&config.clas, bpa.as_ref()).await?;

    if let Some(config) = &config.grpc {
        grpc::init(config, &bpa, &tasks);
    }

    listen_for_cancel(&tasks);

    info!("Started successfully");

    // Block until the cancel token is fired (by the signal handler on SIGTERM/CTRL+C).
    // Only then proceed to graceful shutdown; calling tasks.shutdown() directly would
    // cancel the token immediately, causing all services to stop right after startup.
    tasks.cancel_token().cancelled().await;

    tasks.shutdown().await;
    bpa.shutdown().await;

    info!("Stopped");

    Ok(())
}

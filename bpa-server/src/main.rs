mod clas;
mod config;
mod policy;
mod static_routes;

#[cfg(feature = "grpc")]
mod grpc;

use std::sync::Arc;
use time::macros::format_description;
use trace_err::*;
use tracing::{debug, error, info, warn};
use tracing_subscriber::fmt::time::OffsetTime;

fn listen_for_cancel(
    cancel_token: &tokio_util::sync::CancellationToken,
    task_tracker: &tokio_util::task::TaskTracker,
) {
    #[cfg(unix)]
    let mut term_handler =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .trace_expect("Failed to register signal handlers");
    #[cfg(not(unix))]
    let mut term_handler = std::future::pending();

    let cancel_token = cancel_token.clone();
    let task_tracker_cloned = task_tracker.clone();
    task_tracker.spawn(async move {
        tokio::select! {
            _ = term_handler.recv() => {
                // Signal stop
                info!("Received terminate signal, stopping...");
            }
            _ = tokio::signal::ctrl_c() => {
                // Signal stop
                info!("Received CTRL+C, stopping...");
            }
        }

        // Cancel everything
        cancel_token.cancel();
        task_tracker_cloned.close();
    });
}

fn start_storage(config: &mut config::Config) {
    if let Some(metadata_storage) = &config.metadata_storage {
        config.bpa.metadata_storage = match metadata_storage {
            config::MetadataStorage::Memory(metadata_storage) => metadata_storage
                .as_ref()
                .map(|metadata_storage| hardy_bpa::storage::metadata_mem::new(metadata_storage)),

            #[cfg(feature = "sqlite-storage")]
            config::MetadataStorage::Sqlite(metadata_storage) => Some(hardy_sqlite_storage::new(
                metadata_storage
                    .as_ref()
                    .unwrap_or(&hardy_sqlite_storage::Config::default()),
                config.upgrade_storage,
            )),
            // #[cfg(feature = "postgres-storage")]
            // config::MetadataStorage::Postgres(config) => todo!(),
        };
    }

    if let Some(bundle_storage) = &config.bundle_storage {
        config.bpa.bundle_storage = match bundle_storage {
            config::BundleStorage::Memory(bundle_storage) => bundle_storage
                .as_ref()
                .map(|bundle_storage| hardy_bpa::storage::bundle_mem::new(bundle_storage)),

            #[cfg(feature = "localdisk-storage")]
            config::BundleStorage::LocalDisk(bundle_storage) => Some(hardy_localdisk_storage::new(
                bundle_storage
                    .as_ref()
                    .unwrap_or(&hardy_localdisk_storage::Config::default()),
                config.upgrade_storage,
            )),
            // #[cfg(feature = "s3-storage")]
            // config::BundleStorage::S3(config) => todo!(),
        };
    }
}

fn start_logging(config: &config::Config, config_source: String) {
    let timer = OffsetTime::new(
        time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC),
        format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
    );

    tracing_subscriber::fmt()
        .with_timer(timer)
        .with_max_level(config.log_level)
        .with_target(config.log_level > tracing::Level::INFO)
        .init();

    info!(
        "{} version {} starting...",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    );
    info!("{config_source}");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse command line
    let Some((config, config_source)) = config::init() else {
        return Ok(());
    };

    // Start logging
    start_logging(&config, config_source);

    inner_main(config).await.inspect_err(|e| error!("{e}"))
}

async fn inner_main(mut config: config::Config) -> anyhow::Result<()> {
    // Start storage backends
    start_storage(&mut config);

    // Start the BPA
    let bpa = Arc::new(
        hardy_bpa::bpa::Bpa::start(&config.bpa, config.recover_storage)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to start BPA: {e}"))?,
    );

    // Prepare for graceful shutdown
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let task_tracker = tokio_util::task::TaskTracker::new();

    // Start CLAs
    clas::init(config.clas, &bpa).await?;

    // Start gRPC server
    #[cfg(feature = "grpc")]
    if let Some(config) = &config.grpc {
        grpc::init(config, &bpa, &cancel_token, &task_tracker);
    }

    // Load static routes
    if let Some(config) = config.static_routes {
        static_routes::init(config, &bpa, &cancel_token, &task_tracker).await?;
    }

    // And wait for shutdown signal
    listen_for_cancel(&cancel_token, &task_tracker);

    info!("Started successfully");

    // And wait for cancel token
    cancel_token.cancelled().await;

    // Wait for all tasks to finish
    task_tracker.wait().await;

    // Shut down bpa
    bpa.shutdown().await;

    info!("Stopped");

    Ok(())
}

mod config;
mod static_routes;

mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

// This is the generic Error type used almost everywhere
type Error = Box<dyn std::error::Error + Send + Sync>;

use std::sync::Arc;
use trace_err::*;
use tracing::{error, info, trace};

fn listen_for_cancel(
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: &tokio_util::sync::CancellationToken,
) {
    cfg_if::cfg_if! {
        if #[cfg(unix)] {
            let mut term_handler =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .trace_expect("Failed to register signal handlers");
        } else {
            let mut term_handler = std::future::pending();
        }
    }

    let cancel_token = cancel_token.clone();
    task_set.spawn(async move {
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
    });
}

fn start_storage(config: &mut config::Config) {
    if let Some(metadata_storage) = config.metadata_storage.take() {
        config.bpa.metadata_storage = match metadata_storage {
            #[cfg(feature = "sqlite-storage")]
            config::MetadataStorage::Sqlite(metadata_storage) => Some(
                hardy_sqlite_storage::Storage::init(metadata_storage, config.upgrade_storage),
            ),

            #[cfg(feature = "postgres-storage")]
            config::MetadataStorage::Postgres(config) => todo!(),
        };
    }

    if let Some(bundle_storage) = config.bundle_storage.take() {
        config.bpa.bundle_storage = match bundle_storage {
            #[cfg(feature = "localdisk-storage")]
            config::BundleStorage::LocalDisk(bundle_storage) => Some(
                hardy_localdisk_storage::Storage::init(bundle_storage, config.upgrade_storage),
            ),

            #[cfg(feature = "s3-storage")]
            config::BundleStorage::S3(config) => todo!(),
        };
    }
}

fn start_logging(config: &config::Config, config_source: String) {
    let log_level = config
        .log_level
        .parse::<tracing_subscriber::filter::LevelFilter>()
        .expect("Invalid 'log_level' value in configuration");

    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(
            log_level > tracing_subscriber::filter::LevelFilter::from_level(tracing::Level::INFO),
        )
        .init();

    info!(
        "{} version {} starting...",
        built_info::PKG_NAME,
        built_info::PKG_VERSION
    );
    info!("{config_source}");
}

#[tokio::main]
async fn main() {
    // Parse command line
    let Some((mut config, config_source)) = config::init() else {
        return;
    };

    // Start logging
    start_logging(&config, config_source);

    // Start storage backends
    start_storage(&mut config);

    // Start the BPA
    let bpa = Arc::new(hardy_bpa::bpa::Bpa::start(&config.bpa).await);

    // Prepare for graceful shutdown
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let mut task_set = tokio::task::JoinSet::new();

    // Load static routes
    if let Some(config) = config.static_routes {
        static_routes::init(config, &bpa, &mut task_set, &cancel_token).await;
    }

    // And wait for shutdown signal
    listen_for_cancel(&mut task_set, &cancel_token);

    info!("Started successfully");

    // And wait for cancel token
    cancel_token.cancelled().await;

    // Shut down bpa
    bpa.shutdown().await;

    // Wait for all tasks to finish
    while let Some(r) = task_set.join_next().await {
        r.trace_expect("Task terminated unexpectedly")
    }

    info!("Stopped");
}

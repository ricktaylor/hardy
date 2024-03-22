use hardy_bpa_core::*;
use log_err::*;

mod cache;
mod cla_registry;
mod logger;
mod services;
mod settings;
mod ingress;

// Buildtime info
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

fn init_metadata_storage(
    config: &config::Config,
) -> Result<std::sync::Arc<impl storage::MetadataStorage>, anyhow::Error> {
    #[cfg(feature = "sqlite-storage")]
    hardy_sqlite_storage::Storage::init(&config.get(hardy_sqlite_storage::Config::KEY)?)
}

fn init_bundle_storage(
    config: &config::Config,
) -> Result<std::sync::Arc<impl storage::BundleStorage>, anyhow::Error> {
    #[cfg(feature = "localdisk-storage")]
    hardy_localdisk_storage::Storage::init(&config.get(hardy_localdisk_storage::Config::KEY)?)
}

fn listen_for_cancel(
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    let mut term_handler =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .log_expect("Failed to register signal handlers");

    task_set.spawn(async move {
        tokio::select! {
            Some(_) = term_handler.recv() =>
                {
                    // Signal stop
                    log::info!("{} stopping...", built_info::PKG_NAME);
                    cancel_token.cancel();
                }
            _ = cancel_token.cancelled() => {}
        }
    });
}

#[tokio::main]
async fn main() {
    // load config
    let Some(config) = settings::init() else {
        return;
    };

    // Init logger
    logger::init(&config);
    log::info!("{} starting...", built_info::PKG_NAME);

    // Init pluggable storage engines
    let cache = cache::Cache::new(
        &config,
        init_metadata_storage(&config).log_expect("Failed to initialize metadata store"),
        init_bundle_storage(&config).log_expect("Failed to initialize bundle store"),
    );

    // Prepare for graceful shutdown
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let mut task_set = tokio::task::JoinSet::new();
    listen_for_cancel(&mut task_set, cancel_token.clone());

    // Perform a cache check
    cache
        .check(&cancel_token)
        .await
        .log_expect("Cache check failed");
    if !cancel_token.is_cancelled() {
        // Create queues
        let ingress = ingress::Ingress::new(&config, cache);

        // Init gRPC services
        services::init(&config, ingress, &mut task_set, cancel_token);

        log::info!("{} started", built_info::PKG_NAME);
    }

    // Wait for all tasks to finish
    while let Some(r) = task_set.join_next().await {
        r.log_expect("Task terminated unexpectedly")
    }

    log::info!("{} stopped", built_info::PKG_NAME);
}

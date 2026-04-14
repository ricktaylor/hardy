use hardy_async::TaskPool;
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::config;

mod filters;
mod policy;
mod services;

pub(crate) mod clas;
pub(crate) mod grpc;
pub(crate) mod static_routes;
pub(crate) mod storage;

pub(crate) async fn run(
    config: config::Config,
    upgrade_storage: bool,
    recover_storage: bool,
) -> anyhow::Result<()> {
    let backends = storage::Storage::try_new(&config.storage, upgrade_storage).await?;

    let mut builder = hardy_bpa::bpa::Bpa::builder()
        .status_reports(config.status_reports)
        .poll_channel_depth(config.poll_channel_depth)
        .processing_pool_size(config.processing_pool_size)
        .node_ids(config.node_ids)
        .metadata_storage(backends.metadata)
        .bundle_storage(backends.bundle);

    if config.storage.uses_cache() {
        builder = builder
            .lru_capacity(config.storage.lru_capacity)
            .max_cached_bundle_size(config.storage.max_cached_bundle_size);
    } else {
        builder = builder.no_cache();
    }

    // --- Configure ---
    let bpa = Arc::new(builder.build());

    if let Some(config) = &config.static_routes {
        static_routes::init(config, bpa.as_ref()).await?;
    }
    filters::register(
        &config.rfc9171_validity,
        #[cfg(feature = "ipn-legacy-filter")]
        &config.ipn_legacy_nodes,
        &bpa,
    )?;
    services::register(&config.built_in_services, bpa.as_ref()).await;
    clas::init(&config.clas, bpa.as_ref()).await?;

    // --- Start ---
    bpa.start(recover_storage);

    let tasks = TaskPool::new();
    if let Some(config) = &config.grpc {
        let bpa_reg: Arc<dyn hardy_bpa::bpa::BpaRegistration> = bpa.clone();
        grpc::init(config, &bpa_reg, &tasks);
    }
    hardy_async::signal::listen_for_cancel(&tasks);

    info!("Started successfully");

    // --- Wait for shutdown signal ---
    tasks.cancel_token().cancelled().await;

    // --- Shutdown ---
    tasks.shutdown().await;
    bpa.shutdown().await;

    info!("Stopped");

    Ok(())
}

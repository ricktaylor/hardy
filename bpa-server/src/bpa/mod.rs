use hardy_async::TaskPool;
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::config;

mod filters;
mod policy;

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

    // --- Build ---
    let mut builder = Bpa::builder()
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

    // Filters
    builder = builder.filter(
        Hook::Ingress,
        "rfc9171-validity",
        &[],
        Filter::Read(Arc::new(
            hardy_bpa::filters::rfc9171::Rfc9171ValidityFilter::new(&config.rfc9171_validity),
        )),
    );

    #[cfg(feature = "ipn-legacy-filter")]
    if let Some(filter) = hardy_ipn_legacy_filter::IpnLegacyFilter::new(&config.ipn_legacy_nodes) {
        builder = builder.filter(Hook::Egress, "ipn-legacy", &[], Filter::Write(filter));
    }

    // Static routes
    if let Some(sr_config) = &config.static_routes {
        builder = static_routes::add_to_builder(builder, sr_config)?;
    }

    // Services
    #[cfg(feature = "echo")]
    if let Some(services) = &config.built_in_services.echo {
        if services.is_empty() {
            warn!("built-in-services.echo: no endpoints configured, skipping");
        } else {
            for service_id in services {
                builder = builder.service(
                    Arc::new(hardy_echo_service::EchoService::new()),
                    service_id.clone(),
                );
            }
        }
    }

    #[cfg(not(feature = "echo"))]
    if config.built_in_services.echo.is_some() {
        warn!("Ignoring built-in-services.echo: echo feature is disabled at compile time");
    }

    // CLAs
    builder = clas::add_to_builder(builder, &config.clas).await?;

    let bpa = Arc::new(
        builder
            .build()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    );

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

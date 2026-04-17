use hardy_bpa::bpa::Bpa;
use hardy_bpa::filters::{Filter, Hook};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::warn;

use crate::config;

pub(crate) mod clas;
pub(crate) mod policy;
pub(crate) mod static_routes;
pub(crate) mod storage;

/// Build a BPA from the given configuration.
pub(crate) async fn build(
    config: config::Config,
    upgrade_storage: bool,
) -> anyhow::Result<Arc<Bpa>> {
    let backends = storage::Storage::try_new(&config.storage, upgrade_storage).await?;

    let mut builder = Bpa::builder()
        .status_reports(config.status_reports)
        .poll_channel_depth(config.poll_channel_depth)
        .processing_pool_size(config.processing_pool_size)
        .node_ids(config.node_ids)
        .metadata_storage(backends.metadata)
        .bundle_storage(backends.bundle)
        .filter(
            Hook::Ingress,
            "rfc9171-validity",
            &[],
            Filter::Read(Arc::new(
                hardy_bpa::filters::rfc9171::Rfc9171ValidityFilter::new(&config.rfc9171_validity),
            )),
        );

    if config.storage.uses_cache() {
        builder = builder
            .lru_capacity(config.storage.lru_capacity)
            .max_cached_bundle_size(config.storage.max_cached_bundle_size);
    } else {
        builder = builder.no_cache();
    }

    #[cfg(feature = "ipn-legacy-filter")]
    if !config.ipn_legacy_nodes.0.is_empty() {
        let filter = hardy_ipn_legacy_filter::IpnLegacyFilter::new(config.ipn_legacy_nodes.0);
        builder = builder.filter(
            Hook::Egress,
            "ipn-legacy",
            &[],
            Filter::Write(Arc::new(filter)),
        );
    }

    if let Some(sr_config) = config.static_routes {
        let agent = static_routes::new(sr_config.routes_file, sr_config.priority, sr_config.watch)?;
        builder = builder.routing_agent(sr_config.protocol_id, agent);
    }

    #[cfg(feature = "echo")]
    if let Some(services) = config.built_in_services.echo {
        if services.is_empty() {
            warn!("built-in-services.echo: no endpoints configured, skipping");
        } else {
            for service_id in services {
                builder =
                    builder.service(Arc::new(hardy_echo_service::EchoService::new()), service_id);
            }
        }
    }

    #[cfg(not(feature = "echo"))]
    if config.built_in_services.echo.is_some() {
        warn!("Ignoring built-in-services.echo: echo feature is disabled at compile time");
    }

    let mut policies = HashMap::new();
    for (name, policy_config) in config.policies {
        policies.insert(name, policy::new(policy_config)?);
    }

    for cla_config in config.clas {
        let Some(cla) = clas::new(&cla_config.name, &cla_config.cla_type)? else {
            continue;
        };

        let egress_policy = cla_config
            .policy
            .as_ref()
            .map(|name| {
                policies.get(name).cloned().ok_or_else(|| {
                    anyhow::anyhow!(
                        "CLA '{}' references unknown policy '{name}'",
                        cla_config.name
                    )
                })
            })
            .transpose()?;

        let name = cla_config.name;
        builder = builder.cla(name, cla, egress_policy);
    }

    let bpa = Arc::new(builder.build().await.map_err(|e| anyhow::anyhow!("{e}"))?);
    Ok(bpa)
}

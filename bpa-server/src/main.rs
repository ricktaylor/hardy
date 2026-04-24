mod cli;
mod config;
mod error;
mod static_routes;

use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::Arc;

use anyhow::Context;
use hardy_async::TaskPool;
use hardy_bpa::bpa::Bpa;
use hardy_bpa::filter::rfc9171::Rfc9171ValidityFilter;
use hardy_bpa::filter::{Filter, Hook};
#[cfg(feature = "ipn-legacy-filter")]
use hardy_ipn_legacy_filter::IpnLegacyFilter;
#[cfg(feature = "grpc")]
use hardy_proto::server::GrpcServer;
use tracing::{error, info, warn};

use crate::static_routes::StaticRoutesAgent;

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

    let bpa = build(config, args.upgrade_storage).await?;

    bpa.start(args.recover_storage);

    let tasks = TaskPool::new();
    hardy_async::signal::listen_for_cancel(&tasks);

    #[cfg(feature = "grpc")]
    if let Some(grpc_config) = grpc_config {
        let server = GrpcServer::new(&grpc_config, bpa.clone())
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

/// Build a BPA from the given configuration.
async fn build(config: config::Config, upgrade_storage: bool) -> anyhow::Result<Arc<Bpa>> {
    let (metadata_storage, bundle_storage) = config.storage.build(upgrade_storage).await?;

    let mut builder = Bpa::builder()
        .status_reports(config.status_reports)
        .poll_channel_depth(config.poll_channel_depth)
        .processing_pool_size(config.processing_pool_size)
        .node_ids(config.node_ids)
        .metadata_storage(metadata_storage)
        .bundle_storage(bundle_storage)
        .filter(
            Hook::Ingress,
            "rfc9171-validity",
            &[],
            Filter::Read(Arc::new(Rfc9171ValidityFilter::new(
                &config.rfc9171_validity,
            ))),
        );

    if let Some(service_priority) = config.service_priority {
        builder = builder.service_priority(service_priority);
    }

    if config.storage.uses_cache() {
        builder = builder
            .lru_capacity(config.storage.lru_capacity)
            .max_cached_bundle_size(config.storage.max_cached_bundle_size);
    } else {
        builder = builder.no_cache();
    }

    #[cfg(feature = "ipn-legacy-filter")]
    if !config.ipn_legacy_nodes.0.is_empty() {
        let filter = IpnLegacyFilter::new(config.ipn_legacy_nodes.0);
        builder = builder.filter(
            Hook::Egress,
            "ipn-legacy",
            &[],
            Filter::Write(Arc::new(filter)),
        );
    }

    if let Some(sr_config) = config.static_routes {
        let routes_file = std::env::current_dir()
            .context("Failed to get current directory")?
            .join(&sr_config.routes_file);

        let routes_file = match routes_file.canonicalize() {
            Ok(path) => path,
            Err(e) => {
                if e.kind() != ErrorKind::NotFound {
                    return Err(anyhow::anyhow!(
                        "Failed to canonicalise routes_file '{}': {e}'",
                        routes_file.display()
                    ));
                }
                routes_file
            }
        };

        let agent = Arc::new(StaticRoutesAgent::new(
            routes_file,
            sr_config.priority,
            sr_config.watch,
        ));

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
        policies.insert(name, policy_config.build()?);
    }

    for cla_config in config.clas {
        let Some(cla) = cla_config.build()? else {
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

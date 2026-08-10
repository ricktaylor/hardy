// The BPA server: builds a `hardy_bpa::Bpa` from the loaded configuration
// and runs it, optionally alongside a gRPC front end, until cancelled.

use std::{collections::HashMap, sync::Arc};

use anyhow::Context;
use hardy_async::TaskPool;
use hardy_bpa::{
    bpa::Bpa,
    filter::{Filter, Hook, rfc9171::Rfc9171ValidityFilter},
};
#[cfg(feature = "ipn-legacy-filter")]
use hardy_ipn_legacy_filter::IpnLegacyFilter;
#[cfg(feature = "grpc")]
use hardy_proto::server::GrpcServer;
use tracing::{info, warn};

use crate::bpsec::{self, PatternKeyProvider};
use crate::config::Config;

// The standalone server around a [`hardy_bpa::Bpa`]: the BPA plus what
// running it needs, with everything between "constructed" and "stopped"
// inside [`run`](Self::run).
pub struct BpaServer {
    bpa: Arc<Bpa>,
    recover_storage: bool,
    #[cfg(feature = "grpc")]
    grpc_config: Option<hardy_proto::server::Config>,
    tasks: TaskPool,
}

impl BpaServer {
    // Builds the BPA from the loaded configuration: storage backends,
    // filters, BPSec key provider, routing agent, built-in services, and
    // the configured CLAs are all assembled here, and the BPSec key-file
    // watcher is spawned on `tasks`.
    //
    // `tasks` hosts the server's background tasks; it comes from the
    // composition root, which owns process policy (wiring SIGINT/SIGTERM
    // to the pool's cancellation token via `signal::listen_for_cancel`).
    pub async fn new(
        mut config: Config,
        tasks: TaskPool,
        upgrade_storage: bool,
        recover_storage: bool,
    ) -> anyhow::Result<Self> {
        #[cfg(feature = "grpc")]
        let grpc_config = config.grpc.take();

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

        if let Some(bpsec_config) = config.bpsec.take() {
            let source = bpsec_config
                .build()
                .context("Failed to load BPSec configuration")?;
            let provider = Arc::new(PatternKeyProvider::new(source));
            builder = builder.key_provider(provider.clone());
            bpsec::watch_keys(&tasks, bpsec_config, provider);
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
            let agent = sr_config.build()?;
            builder = builder.routing_agent(sr_config.protocol_id, agent);
        }

        cfg_select! {
            feature = "echo" => {
                if let Some(services) = config.built_in_services.echo {
                    if services.is_empty() {
                        warn!("built-in-services.echo: no endpoints configured, skipping");
                    } else {
                        for service_id in services {
                            builder = builder
                                .service(Arc::new(hardy_echo_service::EchoService::new()), service_id);
                        }
                    }
                }
            }
            _ => {
                if config.built_in_services.echo.is_some() {
                    warn!("Ignoring built-in-services.echo: echo feature is disabled at compile time");
                }
            }
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

        Ok(Self {
            bpa,
            recover_storage,
            #[cfg(feature = "grpc")]
            grpc_config,
            tasks,
        })
    }

    // Runs the server to completion: start the BPA (optionally recovering
    // the store), serve the gRPC front end if configured, then wait for
    // the pool's cancellation token (the composition root wires signals to
    // it) and shut down gracefully.
    pub async fn run(self) -> anyhow::Result<()> {
        self.bpa.start(self.recover_storage);

        #[cfg(feature = "grpc")]
        if let Some(grpc_config) = &self.grpc_config {
            let server = GrpcServer::new(grpc_config, self.bpa.clone())
                .map_err(|e| anyhow::anyhow!("Failed to create gRPC server: {e}"))?;
            let cancel = self.tasks.cancel_token().clone();
            hardy_async::spawn!(self.tasks, "grpc_server", async move {
                if let Err(e) = server.serve(cancel).await {
                    tracing::error!("gRPC server failed: {e}");
                }
            });
        }

        info!("Started successfully");

        self.tasks.cancel_token().cancelled().await;

        self.shutdown().await;

        info!("Stopped");

        Ok(())
    }

    // Stops the server, in dependency order: the background tasks (gRPC
    // front end, BPSec watcher) are wound down before the BPA they drive
    // work into.
    async fn shutdown(&self) {
        self.tasks.shutdown().await;
        self.bpa.shutdown().await;
    }
}

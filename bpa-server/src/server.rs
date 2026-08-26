// The BPA server: builds a `hardy_bpa::Bpa` from the loaded configuration
// and runs it, optionally alongside a gRPC front end, until cancelled.
//
// The `config` module is pure data; turning that data into running
// subsystems is this module's job, inline in `BpaServer::new`. Crate-local
// runtime types construct themselves from config (`PatternKeySource::load`,
// `StaticRoutesAgent`); the gRPC front end is assembled by the `grpc`
// module.

use std::{collections::HashMap, io::ErrorKind, sync::Arc};

use anyhow::Context;
use hardy_async::TaskPool;
use hardy_bpa::{
    bpa::Bpa,
    cla::Cla,
    filter::{Filter, Hook, rfc9171::Rfc9171ValidityFilter},
    policy::EgressPolicy,
    routing::RoutingAgent,
    storage::{BundleMemStorage, BundleStorage, MetadataMemStorage, MetadataStorage},
};
#[cfg(feature = "echo")]
use hardy_echo_service::EchoService;
#[cfg(feature = "file-cla")]
use hardy_file_cla::Cla as FileCla;
#[cfg(feature = "ipn-legacy-filter")]
use hardy_ipn_legacy_filter::IpnLegacyFilter;
#[cfg(feature = "localdisk-storage")]
use hardy_localdisk_storage::LocalDiskStorage;
#[cfg(feature = "postgres-storage")]
use hardy_postgres_storage::PostgresStorage;
#[cfg(feature = "s3-storage")]
use hardy_s3_storage::S3Storage;
#[cfg(feature = "sqlite-storage")]
use hardy_sqlite_storage::SqliteStorage;
#[cfg(feature = "tcpclv4")]
use hardy_tcpclv4::{Tcpclv4, tls};
use tracing::{info, warn};

use crate::bpsec::{self, PatternKeyProvider, PatternKeySource};
use crate::config::{Config, EgressPolicyConfig, cla::ClaType, storage};
#[cfg(feature = "grpc")]
use crate::grpc::GrpcServer;
use crate::static_routes::StaticRoutesAgent;

// The standalone server around a [`hardy_bpa::Bpa`]: the BPA plus what
// running it needs, with everything between "constructed" and "stopped"
// inside [`run`](Self::run).
pub struct BpaServer {
    bpa: Arc<Bpa>,
    recover_storage: bool,
    #[cfg(feature = "grpc")]
    // The composed gRPC front end, not yet serving.
    grpc: Option<GrpcServer>,
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
        config: Config,
        tasks: TaskPool,
        #[allow(unused_variables)] upgrade_storage: bool,
        recover_storage: bool,
    ) -> anyhow::Result<Self> {
        // The upgrade flag is consumed only by persistent backends, so a
        // memory-only build leaves it unused.
        let metadata_storage: Arc<dyn MetadataStorage> = match &config.storage.metadata {
            storage::MetadataStorageConfig::Memory(cfg) => {
                Arc::new(MetadataMemStorage::new(cfg.max_bundles))
            }
            #[cfg(feature = "sqlite-storage")]
            storage::MetadataStorageConfig::Sqlite(cfg) => Arc::new(SqliteStorage::new(
                cfg.db_dir.clone(),
                cfg.db_name.clone(),
                upgrade_storage,
            )),
            #[cfg(feature = "postgres-storage")]
            storage::MetadataStorageConfig::Postgres(cfg) => {
                let mut storage = PostgresStorage::builder();
                if let Some(url) = &cfg.database_url {
                    storage = storage.database_url(url);
                }
                if let Some(limit) = cfg.max_connections {
                    storage = storage.max_connections(limit);
                }
                if let Some(limit) = cfg.min_connections {
                    storage = storage.min_connections(limit);
                }
                if let Some(timeout) = cfg.connect_timeout {
                    storage = storage.connect_timeout(timeout.get());
                }
                if let Some(timeout) = cfg.idle_timeout {
                    storage = storage.idle_timeout(timeout.get());
                }
                if let Some(lifetime) = cfg.max_lifetime {
                    storage = storage.max_lifetime(lifetime.get());
                }
                if let Some(size) = cfg.poll_page_size {
                    storage = storage.poll_page_size(size);
                }
                Arc::new(storage.build(upgrade_storage).await?)
            }
        };

        let bundle_storage: Arc<dyn BundleStorage> = match &config.storage.bundle {
            storage::BundleStorageConfig::Memory(cfg) => {
                Arc::new(BundleMemStorage::new(cfg.capacity, cfg.min_bundles))
            }
            #[cfg(feature = "localdisk-storage")]
            storage::BundleStorageConfig::LocalDisk(cfg) => Arc::new(LocalDiskStorage::new(
                cfg.store_dir.clone(),
                cfg.fsync,
                upgrade_storage,
            )),
            #[cfg(feature = "s3-storage")]
            storage::BundleStorageConfig::S3(cfg) => {
                let mut storage = S3Storage::builder(cfg.bucket.clone());
                if let Some(prefix) = &cfg.prefix {
                    storage = storage.prefix(prefix);
                }
                if let Some(region) = &cfg.region {
                    storage = storage.region(region);
                }
                if let Some(endpoint) = &cfg.endpoint_url {
                    storage = storage.endpoint_url(endpoint);
                }
                if cfg.force_path_style {
                    storage = storage.force_path_style();
                }
                if let Some(threshold) = cfg.multipart_threshold {
                    storage = storage.multipart_threshold(threshold);
                }
                if let Some(size) = cfg.multipart_part_size {
                    storage = storage.multipart_part_size(size);
                }
                Arc::new(storage.build().await?)
            }
        };

        let mut builder = Bpa::builder()
            .node_ids(config.node_ids)
            .metadata_storage(metadata_storage)
            .bundle_storage(bundle_storage)
            .filter(
                Hook::Ingress,
                "rfc9171-validity",
                &[],
                Filter::Read(Arc::new({
                    let mut filter = Rfc9171ValidityFilter::new();
                    if let Some(enabled) = config.rfc9171_validity.primary_block_integrity {
                        filter = filter.primary_block_integrity(enabled);
                    }
                    if let Some(enabled) = config.rfc9171_validity.bundle_age_required {
                        filter = filter.bundle_age_required(enabled);
                    }
                    filter
                })),
            );

        if let Some(status_reports) = config.status_reports {
            builder = builder.status_reports(status_reports);
        }
        if let Some(depth) = config.poll_channel_depth {
            builder = builder.poll_channel_depth(depth);
        }
        if let Some(size) = config.max_bundle_size {
            builder = builder.max_bundle_size(size);
        }
        if let Some(pool_size) = config.processing_pool_size {
            builder = builder.processing_pool_size(pool_size);
        }
        if let Some(service_priority) = config.service_priority {
            builder = builder.service_priority(service_priority);
        }

        if let Some(bpsec_config) = config.bpsec {
            let source = PatternKeySource::load(&bpsec_config)
                .context("Failed to load BPSec configuration")?;
            let provider = Arc::new(PatternKeyProvider::new(source));
            builder = builder.key_provider(provider.clone());
            bpsec::watch_keys(&tasks, bpsec_config, provider);
        }

        if config.storage.uses_cache() {
            if let Some(capacity) = config.storage.lru_capacity {
                builder = builder.lru_capacity(capacity);
            }
            if let Some(size) = config.storage.max_cached_bundle_size {
                builder = builder.max_cached_bundle_size(size);
            }
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
            let routes_file = sr_config
                .routes_file
                .map(|file| {
                    let file = std::env::current_dir()
                        .context("Failed to get current directory")?
                        .join(file);
                    match file.canonicalize() {
                        Ok(path) => Ok(path),
                        Err(e) if e.kind() == ErrorKind::NotFound => Ok(file),
                        Err(e) => Err(anyhow::anyhow!(
                            "Failed to canonicalise routes_file '{}': {e}",
                            file.display()
                        )),
                    }
                })
                .transpose()?;
            let agent: Arc<dyn RoutingAgent> = Arc::new(StaticRoutesAgent::new(
                routes_file,
                sr_config.priority,
                sr_config.watch.into(),
            ));
            builder = builder.routing_agent(
                sr_config
                    .protocol_id
                    .unwrap_or_else(|| StaticRoutesAgent::DEFAULT_PROTOCOL_ID.to_string()),
                agent,
            );
        }

        cfg_select! {
            feature = "echo" => {
                if let Some(services) = config.built_in_services.echo {
                    if services.is_empty() {
                        warn!("built-in-services.echo: no endpoints configured, skipping");
                    } else {
                        for service_id in services {
                            builder = builder
                                .service(Arc::new(EchoService::new()), service_id);
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

        // No egress policy types exist yet: the enum's only variant is the
        // serde catch-all, so every arm fails and a real variant slots in
        // as an `Ok` arm here.
        // Unknown policy types are an extension point, like unknown CLA
        // types: tolerated with a warning so a config can name policies
        // this build was not compiled with. A CLA that references one
        // fails below with the "references unknown policy" error.
        let policies: HashMap<String, Arc<dyn EgressPolicy>> = config
            .policies
            .into_iter()
            .filter_map(
                |(name, policy_config)| -> Option<(String, Arc<dyn EgressPolicy>)> {
                    match policy_config {
                        EgressPolicyConfig::Unknown => {
                            warn!("Ignoring policy '{name}' with unknown type");
                            None
                        }
                    }
                },
            )
            .collect();

        for cla_config in config.clas {
            let cla: Option<Arc<dyn Cla>> = match &cla_config.cla_type {
                #[cfg(feature = "tcpclv4")]
                ClaType::TcpClv4(tcpcl) => {
                    let name = &cla_config.name;
                    let mut cla_builder = Tcpclv4::builder();

                    cla_builder = match &tcpcl.listeners {
                        Some(listeners) => {
                            listeners.iter().fold(cla_builder, |cla_builder, address| {
                                cla_builder.listen(*address)
                            })
                        }
                        None => cla_builder.listen_default(),
                    };
                    if let Some(mru) = tcpcl.segment_mru {
                        cla_builder = cla_builder.segment_mru(mru);
                    }
                    if let Some(mru) = tcpcl.transfer_mru {
                        cla_builder = cla_builder.transfer_mru(mru);
                    }
                    if let Some(limit) = tcpcl.max_idle_connections {
                        cla_builder = cla_builder.max_idle_connections(limit);
                    }
                    if let Some(limit) = tcpcl.max_outstanding_transfers {
                        cla_builder = cla_builder.max_outstanding_transfers(limit);
                    }
                    if let Some(rate) = tcpcl.connection_rate_limit {
                        cla_builder = cla_builder.connection_rate_limit(rate);
                    }
                    if let Some(timeout) = tcpcl.contact_timeout {
                        cla_builder = cla_builder.contact_timeout(timeout);
                    }
                    if let Some(interval) = tcpcl.keepalive_interval {
                        cla_builder = cla_builder.keepalive_interval(interval);
                    }

                    if let Some(tls_config) = &tcpcl.tls {
                        let mut tls_builder = tls::Tls::builder().required(tls_config.required);

                        if let Some(dir) = &tls_config.ca_certs {
                            tls_builder = tls_builder.ca_certs(dir.clone());
                        }
                        if tls_config.insecure_skip_verify {
                            tls_builder = tls_builder.dangerous().insecure_skip_verify();
                        }
                        if let Some(identity) = &tls_config.identity {
                            tls_builder = tls_builder
                                .identity(identity.cert_file.clone(), identity.key_file.clone());
                        }
                        tls_builder = tls_builder.client_auth(tls_config.client_auth.into());
                        if let Some(server_name) = &tls_config.server_name {
                            tls_builder = tls_builder.server_name(server_name.clone());
                        }

                        cla_builder = cla_builder.tls(
                            tls_builder
                                .build()
                                .with_context(|| format!("Failed to create CLA '{name}'"))?,
                        );
                    }

                    Some(Arc::new(
                        cla_builder
                            .build()
                            .with_context(|| format!("Failed to create CLA '{name}'"))?,
                    ))
                }
                #[cfg(feature = "file-cla")]
                ClaType::File(file) => Some(Arc::new(FileCla::new(file).map_err(|e| {
                    anyhow::anyhow!("Failed to create CLA '{}': {e}", cla_config.name)
                })?)),
                ClaType::Other { cla_type, .. } => {
                    warn!(
                        "Ignoring CLA '{}' with unknown type '{cla_type}'",
                        cla_config.name
                    );
                    None
                }
            };
            let Some(cla) = cla else {
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

            builder = builder.cla(cla_config.name, cla, egress_policy);
        }

        let bpa = Arc::new(builder.build().await.map_err(anyhow::Error::from_boxed)?);

        #[cfg(feature = "grpc")]
        let grpc = config
            .grpc
            .map(|grpc| {
                GrpcServer::new(
                    grpc.address,
                    grpc.services,
                    grpc.drain_timeout,
                    &bpa,
                    &tasks,
                )
            })
            .transpose()?;

        Ok(Self {
            bpa,
            recover_storage,
            #[cfg(feature = "grpc")]
            grpc,
            tasks,
        })
    }

    // Runs the server to completion: start the BPA (optionally recovering
    // the store), serve the gRPC front end if configured, then wait for
    // the pool's cancellation token (the composition root wires signals to
    // it) and shut down gracefully.
    pub async fn run(self) -> anyhow::Result<()> {
        let Self {
            bpa,
            recover_storage,
            #[cfg(feature = "grpc")]
            grpc,
            tasks,
        } = self;

        bpa.start(recover_storage);

        #[cfg(feature = "grpc")]
        if let Some(grpc) = grpc {
            let cancel = tasks.cancel_token().clone();
            hardy_async::spawn!(tasks, "grpc_server", async move {
                grpc.serve(cancel).await;
            });
        }

        info!("Started successfully");

        tasks.cancel_token().cancelled().await;

        tasks.shutdown().await;
        bpa.shutdown().await;

        info!("Stopped");

        Ok(())
    }
}

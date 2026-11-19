// The BPA server: builds a `hardy_bpa::Bpa` from the loaded configuration
// and runs it, optionally alongside a gRPC front end, until cancelled.
//
// The `config` module is pure data; turning that data into running
// subsystems is this module's job, inline in `BpaServer::new`. Crate-local
// runtime types construct themselves from config (`PatternKeySource::load`,
// `StaticRoutesAgent`).

use std::{collections::HashMap, io::ErrorKind, sync::Arc};

use anyhow::Context;
use hardy_async::TaskPool;
use hardy_bpa::{
    bpa::Bpa,
    cla::BoxFactory,
    policy::FlowControllerFactory,
    storage::{BundleMemStorage, BundleStorage, MetadataMemStorage, MetadataStorage},
};
#[cfg(feature = "echo")]
use hardy_echo_service::EchoService;
#[cfg(feature = "file-cla")]
use hardy_file_cla::Cla as FileCla;
#[cfg(feature = "localdisk-storage")]
use hardy_localdisk_storage::LocalDiskStorage;
#[cfg(feature = "postgres-storage")]
use hardy_postgres_storage::PostgresStorage;
#[cfg(feature = "grpc")]
use hardy_proto::server::GrpcServer;
#[cfg(feature = "s3-storage")]
use hardy_s3_storage::S3Storage;
#[cfg(feature = "sqlite-storage")]
use hardy_sqlite_storage::SqliteStorage;
#[cfg(feature = "tcpclv4")]
use hardy_tcpclv4::Tcpclv4;
use tracing::{info, warn};

use crate::bpsec::{self, PatternKeyProvider, PatternKeySource};
use crate::config::{
    Config, FlowControllerFactoryConfig,
    cla::{ClaConfig, ClaType},
    storage,
};
use crate::static_routes::StaticRoutesAgent;

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
        config: Config,
        tasks: TaskPool,
        #[allow(unused_variables)] upgrade_storage: bool,
        recover_storage: bool,
    ) -> anyhow::Result<Self> {
        #[cfg(feature = "grpc")]
        let grpc_config = config.grpc;

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
            .node_ids(config.admin_endpoints)
            .metadata_storage(metadata_storage)
            .bundle_storage(bundle_storage);

        if let Some(enabled) = config.rfc9171_validity.primary_block_integrity {
            builder = builder.primary_block_integrity(enabled);
        }
        if let Some(enabled) = config.rfc9171_validity.bundle_age_required {
            builder = builder.bundle_age_required(enabled);
        }
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

        if !config.ipn_legacy_nodes.is_empty() {
            builder = builder.ipn_legacy_peers(config.ipn_legacy_nodes);
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
            builder = builder.routing_agent(
                sr_config
                    .protocol_id
                    .unwrap_or_else(|| StaticRoutesAgent::DEFAULT_PROTOCOL_ID.to_string()),
                StaticRoutesAgent::factory(routes_file, sr_config.priority, sr_config.watch.into()),
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
                                .raw_service(Some(service_id), EchoService::factory());
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
        let policies: HashMap<String, Arc<dyn FlowControllerFactory>> = config
            .policies
            .into_iter()
            .filter_map(
                |(name, policy_config)| -> Option<(String, Arc<dyn FlowControllerFactory>)> {
                    match policy_config {
                        FlowControllerFactoryConfig::Unknown => {
                            warn!("Ignoring policy '{name}' with unknown type");
                            None
                        }
                    }
                },
            )
            .collect();

        for ClaConfig {
            name,
            cla_type,
            policy: policy_name,
        } in config.clas
        {
            // The factories have nothing in common but the trait, so the
            // match yields an erased one and the declaration is written
            // once. An unknown type is ignored whole, policy and all.
            let factory = match cla_type {
                #[cfg(feature = "tcpclv4")]
                ClaType::TcpClv4(tcpcl) => BoxFactory::new(
                    Tcpclv4::factory(tcpcl)
                        .with_context(|| format!("Failed to create CLA '{name}'"))?,
                ),
                #[cfg(feature = "file-cla")]
                ClaType::File(file) => BoxFactory::new(
                    FileCla::factory(&file)
                        .with_context(|| format!("Failed to create CLA '{name}'"))?,
                ),
                ClaType::Other { cla_type, .. } => {
                    warn!("Ignoring CLA '{name}' with unknown type '{cla_type}'");
                    continue;
                }
            };

            // The CLA names the egress policy shaping its queues; the
            // policies themselves are configured against the BPA.
            if let Some(policy_name) = policy_name {
                let policy = policies.get(&policy_name).cloned().ok_or_else(|| {
                    anyhow::anyhow!("CLA '{name}' references unknown policy '{policy_name}'")
                })?;
                builder = builder.egress_policy(&name, policy);
            }

            builder = builder.cla_boxed(name, factory);
        }

        let bpa = Arc::new(builder.build());

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
    //
    // Every CLA, service and routing agent the configuration declares
    // registers in that start, and one that cannot run takes the server
    // down rather than leaving a transport silently missing.
    pub async fn run(self) -> anyhow::Result<()> {
        self.bpa
            .start(self.recover_storage)
            .await
            .map_err(anyhow::Error::from_boxed)?;

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

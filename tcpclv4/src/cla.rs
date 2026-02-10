use super::*;
use hardy_bpa::async_trait;

impl ClaInner {
    fn start_listeners(
        &self,
        config: &config::Config,
        tasks: &Arc<hardy_async::TaskPool>,
        tls_config: &Option<Arc<tls::TlsConfig>>,
    ) {
        // Start the listeners
        if let Some(address) = config.address {
            tasks.spawn(
                Arc::new(listen::Listener {
                    tasks: tasks.clone(),
                    contact_timeout: config.session_defaults.contact_timeout,
                    must_use_tls: config.session_defaults.must_use_tls,
                    keepalive_interval: config.session_defaults.keepalive_interval,
                    segment_mru: config.segment_mru,
                    transfer_mru: config.transfer_mru,
                    node_ids: self.node_ids.clone(),
                    sink: self.sink.clone(),
                    registry: self.registry.clone(),
                    tls_config: tls_config.clone(),
                })
                .listen(address),
            );
        }
    }
}

#[async_trait]
impl hardy_bpa::cla::Cla for Cla {
    #[cfg_attr(feature = "tracing", instrument(skip(self, sink)))]
    async fn on_register(&self, sink: Box<dyn hardy_bpa::cla::Sink>, node_ids: &[NodeId]) {
        // Initialize TLS config once and reuse it for all connections
        let tls_config = if let Some(tls_config) = &self.config.tls {
            match tls::TlsConfig::new(tls_config) {
                Ok(cfg) => {
                    info!("TLS configuration loaded successfully");
                    Some(Arc::new(cfg))
                }
                Err(e) => {
                    warn!("Failed to load TLS configuration: {e}");
                    None
                }
            }
        } else {
            None
        };

        let inner = ClaInner {
            registry: Arc::new(connection::ConnectionRegistry::new(
                self.config.max_idle_connections,
            )),
            sink: sink.into(),
            node_ids: node_ids.into(),
            tls_config: tls_config.clone(),
        };

        if !self.config.session_defaults.must_use_tls
            || tls_config
                .as_ref()
                .and_then(|c| c.server_config.as_ref())
                .is_some()
        {
            inner.start_listeners(&self.config, &self.tasks, &tls_config);
        }

        // Ensure single initialization
        self.inner.call_once(|| inner);
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn on_unregister(&self) {
        if let Some(inner) = self.inner.get() {
            // Shutdown all pooled connections
            inner.registry.shutdown().await;
        }

        self.tasks.shutdown().await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle)))]
    async fn forward(
        &self,
        _queue: Option<u32>,
        cla_addr: &hardy_bpa::cla::ClaAddress,
        mut bundle: hardy_bpa::Bytes,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        let Some(inner) = self.inner.get() else {
            error!("forward called before on_register!");
            return Err(hardy_bpa::cla::Error::Disconnected);
        };

        if let hardy_bpa::cla::ClaAddress::Tcp(remote_addr) = cla_addr {
            info!("Forwarding bundle to TCPCLv4 peer at {remote_addr}");
            // We try this 5 times, because peers can close at random times
            for _ in 0..5 {
                // See if we have an active connection already
                bundle = match inner.registry.forward(remote_addr, bundle).await {
                    Ok(r) => {
                        info!("Bundle forwarded successfully using existing connection");
                        return Ok(r);
                    }
                    Err(bundle) => {
                        debug!("No live connections, will attempt to create new one");
                        bundle
                    }
                };

                // Reuse the TLS config that was loaded during registration
                // Do a new active connect
                let conn = connect::Connector {
                    tasks: self.tasks.clone(),
                    contact_timeout: self.config.session_defaults.contact_timeout,
                    must_use_tls: self.config.session_defaults.must_use_tls,
                    keepalive_interval: self.config.session_defaults.keepalive_interval,
                    segment_mru: self.config.segment_mru,
                    transfer_mru: self.config.transfer_mru,
                    node_ids: inner.node_ids.clone(),
                    sink: inner.sink.clone(),
                    registry: inner.registry.clone(),
                    tls_config: inner.tls_config.clone(),
                };
                match conn.connect(remote_addr).await {
                    Ok(()) | Err(transport::Error::Timeout) => {}
                    Err(_) => {
                        // No point retrying
                        break;
                    }
                }
            }
        }

        Ok(hardy_bpa::cla::ForwardBundleResult::NoNeighbour)
    }
}

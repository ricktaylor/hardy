use super::*;
use hardy_bpa::async_trait;

impl ClaInner {
    fn start_listeners(
        &self,
        config: &config::Config,
        cancel_token: &tokio_util::sync::CancellationToken,
        task_tracker: &tokio_util::task::TaskTracker,
        tls_config: &Option<Arc<tls::TlsConfig>>,
    ) {
        // Start the listeners
        if let Some(address) = config.address {
            task_tracker.spawn(
                Arc::new(listen::Listener {
                    cancel_token: cancel_token.clone(),
                    task_tracker: task_tracker.clone(),
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
    async fn on_register(
        &self,
        sink: Box<dyn hardy_bpa::cla::Sink>,
        node_ids: &[Eid],
    ) -> hardy_bpa::cla::Result<()> {
        if self.config.session_defaults.must_use_tls && self.config.tls.is_none() {
            return Err(hardy_bpa::cla::Error::Internal(
                transport::Error::InvalidProtocol.into(),
            ));
        }

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

        if !self.config.session_defaults.must_use_tls || self.config.tls.is_some() {
            inner.start_listeners(
                &self.config,
                &self.cancel_token,
                &self.task_tracker,
                &tls_config,
            );
        }

        self.inner.set(inner).map_err(|_| {
            error!("CLA on_register called twice!");
            hardy_bpa::cla::Error::AlreadyConnected
        })
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn on_unregister(&self) {
        if let Some(inner) = self.inner.get() {
            self.cancel_token.cancel();
            self.task_tracker.close();

            // Shutdown all pooled connections
            inner.registry.shutdown().await;

            self.task_tracker.wait().await;
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle)))]
    async fn forward(
        &self,
        _queue: Option<u32>,
        cla_addr: &hardy_bpa::cla::ClaAddress,
        mut bundle: hardy_bpa::Bytes,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        let inner = self.inner.get().ok_or_else(|| {
            error!("forward called before on_register!");
            hardy_bpa::cla::Error::Disconnected
        })?;

        if let hardy_bpa::cla::ClaAddress::Tcp(remote_addr) = cla_addr {
            info!("Forwarding bundle to TCPCLv4 peer at {}", remote_addr);
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
                    cancel_token: self.cancel_token.clone(),
                    task_tracker: self.task_tracker.clone(),
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

use super::*;
use hardy_bpa::async_trait;

impl ClaInner {
    fn start_listeners(
        &self,
        config: &config::Config,
        cancel_token: &tokio_util::sync::CancellationToken,
        task_tracker: &tokio_util::task::TaskTracker,
    ) {
        // Start the listeners
            task_tracker.spawn(
                Arc::new(listen::Listener {
                    cancel_token: cancel_token.clone(),
                    task_tracker: task_tracker.clone(),
                    contact_timeout: config.session_defaults.contact_timeout,
                    use_tls: config.session_defaults.use_tls,
                    keepalive_interval: config.session_defaults.keepalive_interval,
                    segment_mru: config.segment_mru,
                    transfer_mru: config.transfer_mru,
                    node_ids: self.node_ids.clone(),
                    sink: self.sink.clone(),
                    registry: self.registry.clone(),
                })
            .listen(config.address),
            );
    }
}

#[async_trait]
impl hardy_bpa::cla::Cla for Cla {
    async fn on_register(
        &self,
        sink: Box<dyn hardy_bpa::cla::Sink>,
        node_ids: &[Eid],
    ) -> hardy_bpa::cla::Result<()> {
        let sink: Arc<dyn hardy_bpa::cla::Sink> = sink.into();
        let inner = ClaInner {
            registry: Arc::new(connection::ConnectionRegistry::new(
                sink.clone(),
                self.config.max_idle_connections,
            )),
            sink,
            node_ids: node_ids.into(),
        };

        inner.start_listeners(&self.config, &self.cancel_token, &self.task_tracker);

        self.inner.set(inner).map_err(|_| {
            error!("CLA on_register called twice!");
            hardy_bpa::cla::Error::AlreadyConnected
        })
    }

    async fn on_unregister(&self) {
        if let Some(inner) = self.inner.get() {
            self.cancel_token.cancel();
            self.task_tracker.close();

            // Shutdown all pooled connections
            inner.registry.shutdown().await;

            self.task_tracker.wait().await;
        }
    }

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
            // We try this 5 times, because peers can close at random times
            for _ in 0..5 {
                // See if we have an active connection already
                bundle = match inner.registry.forward(remote_addr, bundle).await {
                    Ok(r) => return r,
                    Err(bundle) => bundle,
                };

                // Do a new active connect
                let conn = connect::Connector {
                    cancel_token: self.cancel_token.clone(),
                    task_tracker: self.task_tracker.clone(),
                    contact_timeout: self.config.session_defaults.contact_timeout,
                    use_tls: self.config.session_defaults.use_tls,
                    keepalive_interval: self.config.session_defaults.keepalive_interval,
                    segment_mru: self.config.segment_mru,
                    transfer_mru: self.config.transfer_mru,
                    node_ids: inner.node_ids.clone(),
                    sink: inner.sink.clone(),
                    registry: inner.registry.clone(),
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

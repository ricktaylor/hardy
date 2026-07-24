use super::*;
use hardy_bpa::async_trait;

impl Cla {
    fn start_listeners(&self) {
        if let Some(address) = self.address {
            // Only start listener if TLS is not required, or we have server TLS config
            if !self.session_config.require_tls
                || self
                    .tls_config
                    .as_ref()
                    .and_then(|c| c.server_config.as_ref())
                    .is_some()
            {
                let ctx = self
                    .connection_context()
                    .trace_expect("start_listeners called before registration");

                let listener = listen::Listener {
                    connection_rate_limit: self.connection_rate_limit,
                    ctx,
                };
                self.tasks
                    .spawn(listener.listen(self.tasks.clone(), address));
            }
        }
    }
}

#[async_trait]
impl hardy_bpa::cla::Cla for Cla {
    fn address_type(&self) -> Option<hardy_bpa::cla::ClaAddressType> {
        Some(hardy_bpa::cla::ClaAddressType::Tcp)
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, sink)))]
    async fn on_register(&self, sink: Box<dyn hardy_bpa::cla::Sink>, node_ids: &[NodeId]) {
        // Store sink and node_ids in single atomic operation
        self.inner.call_once(|| Inner {
            sink: sink.into(),
            node_ids: node_ids.into(),
        });

        // Start listeners now that we have a sink
        self.start_listeners();
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn on_unregister(&self) {
        // Cancel sessions first so they exit promptly when channels close
        self.session_cancel_token.cancel();

        // Shutdown all pooled connections (drops tx senders)
        self.registry.shutdown();

        // Wait for all session tasks to complete
        self.tasks.shutdown().await;
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    async fn forward(
        &self,
        _queue: Option<u32>,
        cla_addr: &hardy_bpa::cla::ClaAddress,
        bundle_id: &hardy_bpv7::bundle::Id,
        bundle: hardy_bpa::Bytes,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        let ctx = self.connection_context().ok_or_else(|| {
            error!("forward called before on_register!");
            hardy_bpa::cla::Error::Disconnected
        })?;

        let hardy_bpa::cla::ClaAddress::Tcp(remote_addr) = cla_addr else {
            return Ok(hardy_bpa::cla::ForwardBundleResult::NoNeighbour);
        };

        debug!("Forwarding bundle to TCPCLv4 peer at {remote_addr}");

        // Take ownership of the transfer and resolve it out-of-band: the
        // session-side transmit-and-acknowledge cycle costs at least one
        // round trip, and answering the offer first keeps the BPA's egress
        // flowing while transfers overlap across pooled connections. The
        // permit bounds accepted-but-unresolved transfers per peer; awaiting
        // it withholds the verdict, which is the flow control back to the
        // BPA.
        let permits = self
            .transfer_permits
            .lock()
            .entry(*remote_addr)
            .or_insert_with(|| {
                Arc::new(tokio::sync::Semaphore::new(self.max_outstanding_transfers))
            })
            .clone();
        let permit = permits
            .acquire_owned()
            .await
            .trace_expect("Transfer permit semaphore closed");

        let tasks = self.tasks.clone();
        let bundle_id = bundle_id.clone();
        let remote_addr = *remote_addr;
        self.tasks.spawn(async move {
            let _permit = permit;

            let outcome = match transfer(tasks, &ctx, remote_addr, bundle).await {
                hardy_bpa::cla::ForwardBundleResult::Sent => {
                    hardy_bpa::cla::TransferOutcome::Delivered
                }
                hardy_bpa::cla::ForwardBundleResult::NoNeighbour => {
                    hardy_bpa::cla::TransferOutcome::Failed
                }
                hardy_bpa::cla::ForwardBundleResult::Accepted => {
                    // Sessions resolve transfers terminally
                    warn!("Pooled session deferred a transfer; treating as failed");
                    hardy_bpa::cla::TransferOutcome::Failed
                }
            };

            if let Err(e) = ctx.sink.transfer_outcome(&bundle_id, outcome).await {
                debug!("Failed to report transfer outcome: {e}");
            }
        });

        Ok(hardy_bpa::cla::ForwardBundleResult::Accepted)
    }
}

// Transmit a bundle to `remote_addr` over a pooled session, dialing new
// connections as the pool allows, and return the terminal result of the
// transfer: `Sent` only once the peer has fully acknowledged it.
async fn transfer(
    tasks: Arc<hardy_async::TaskPool>,
    ctx: &context::ConnectionContext,
    remote_addr: std::net::SocketAddr,
    mut bundle: hardy_bpa::Bytes,
) -> hardy_bpa::cla::ForwardBundleResult {
    // We try this 5 times, because peers can close at random times
    for _ in 0..5 {
        // Use a pooled session, dialing a new connection when the
        // pool has capacity and no session is free
        bundle = match ctx
            .registry
            .forward(&remote_addr, bundle, connection::OnBusy::Dial)
            .await
        {
            Ok(r) => {
                debug!("Bundle forwarded successfully using existing connection");
                return r;
            }
            Err(bundle) => {
                debug!("No free connections, will attempt to create new one");
                bundle
            }
        };

        // One dial at a time per peer: concurrent forwards coalesce here
        // rather than racing parallel dials, and the pool is re-checked
        // under the lock — the previous holder's session may already be
        // registered
        let dial_lock = ctx.registry.dial_lock(remote_addr);
        let _dialing = dial_lock.lock().await;
        bundle = match ctx
            .registry
            .forward(&remote_addr, bundle, connection::OnBusy::Dial)
            .await
        {
            Ok(r) => return r,
            Err(bundle) => bundle,
        };

        // Do a new active connect
        let conn = connect::Connector {
            tasks: tasks.clone(),
            ctx: ctx.clone(),
        };
        match conn.connect(&remote_addr).await {
            Ok(()) => {}
            Err(transport::Error::Timeout) if !ctx.registry.has_sessions(&remote_addr) => {
                // Nothing to fall back to: keep dialing
            }
            Err(e) => {
                // The dial failed but a session may be up: fall back to
                // queueing on it rather than stalling the forward behind
                // further dial attempts. Silently dropped SYNs (dial
                // timeout) are the norm for firewalled peers with
                // asymmetric reachability that hold a session open without
                // being able to accept another.
                debug!("Dial to {remote_addr} failed: {e:?}; falling back to a busy session");
                return ctx
                    .registry
                    .forward(&remote_addr, bundle, connection::OnBusy::Queue)
                    .await
                    .unwrap_or(hardy_bpa::cla::ForwardBundleResult::NoNeighbour);
            }
        }
    }

    // Repeated dial timeouts: last try on a busy session before
    // reporting the neighbour gone
    ctx.registry
        .forward(&remote_addr, bundle, connection::OnBusy::Queue)
        .await
        .unwrap_or(hardy_bpa::cla::ForwardBundleResult::NoNeighbour)
}

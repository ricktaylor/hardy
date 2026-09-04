// The TCPCLv4 CLA server: builds the `hardy_tcpclv4` entity from the
// loaded `Config`, registers it with the BPA, dials the static peers, and
// runs until cancelled. The config-to-builder mapping lives here too (kept
// in sync with the `clas:` entry mirror in bpa-server/src/config/tcpclv4.rs).

use core::time::Duration;
use std::sync::Arc;

use anyhow::Context;
use hardy_async::{CancellationToken, TaskPool};
use hardy_proto::client::BpaClient;
use hardy_tcpclv4::{Tcpclv4, tls};
use tokio::net::lookup_host;
use tracing::{info, warn};

use crate::config::Config;

// How long to wait before re-dialing a static peer that could not be
// resolved or connected.
const PEER_REDIAL_INTERVAL: Duration = Duration::from_secs(5);

// The standalone server around a [`hardy_tcpclv4::Tcpclv4`] entity: the
// entity plus what running it needs, with everything between "constructed"
// and "stopped" inside [`run`](Self::run).
pub struct Tcpclv4Server {
    cla: Arc<Tcpclv4>,
    bpa_address: String,
    cla_name: String,
    peers: Vec<String>,
    tasks: TaskPool,
}

impl Tcpclv4Server {
    // Builds the TCPCLv4 entity from the loaded configuration, reconciling
    // the config-file surface into builder calls: absent keys leave the
    // builder defaults in force, and contradictions are reported with
    // config-key names before any file is touched. The TLS material is
    // loaded by `TlsBuilder::build`, and the listeners are bound inside
    // `Tcpclv4Builder::build`.
    //
    // `tasks` hosts the server's background tasks; it comes from the
    // composition root, which owns process policy (wiring SIGINT/SIGTERM
    // to the pool's cancellation token via `signal::listen_for_cancel`).
    pub fn new(config: Config, tasks: TaskPool) -> anyhow::Result<Self> {
        let mut builder = Tcpclv4::builder();

        builder = match config.listeners {
            Some(listeners) => listeners
                .into_iter()
                .fold(builder, |builder, address| builder.listen(address)),
            None => builder.listen_default(),
        };
        if let Some(mru) = config.segment_mru {
            builder = builder.segment_mru(mru);
        }
        if let Some(mru) = config.transfer_mru {
            builder = builder.transfer_mru(mru);
        }
        if let Some(limit) = config.max_idle_connections {
            builder = builder.max_idle_connections(limit);
        }
        if let Some(limit) = config.max_outstanding_transfers {
            builder = builder.max_outstanding_transfers(limit);
        }
        if let Some(rate) = config.connection_rate_limit {
            builder = builder.connection_rate_limit(rate);
        }
        if let Some(timeout) = config.contact_timeout {
            builder = builder.contact_timeout(timeout);
        }
        if let Some(interval) = config.keepalive_interval {
            builder = builder.keepalive_interval(interval);
        }

        if let Some(tls_config) = &config.tls {
            let mut tls_builder = tls::Tls::builder().required(tls_config.required);

            if let Some(dir) = &tls_config.ca_certs {
                tls_builder = tls_builder.ca_certs(dir.clone());
            }
            if tls_config.insecure_skip_verify {
                tls_builder = tls_builder.dangerous().insecure_skip_verify();
            }
            if let Some(identity) = &tls_config.identity {
                tls_builder =
                    tls_builder.identity(identity.cert_file.clone(), identity.key_file.clone());
            }
            tls_builder = tls_builder.client_auth(tls_config.client_auth.into());
            if let Some(name) = &tls_config.server_name {
                tls_builder = tls_builder.server_name(name.clone());
            }

            builder = builder.tls(tls_builder.build()?);
        }

        Ok(Self {
            cla: Arc::new(builder.build()?),
            bpa_address: config.bpa_address,
            cla_name: config.cla_name,
            peers: config.peers,
            tasks,
        })
    }

    // Runs the server to completion: register with the BPA, dial the
    // static peers, then hold the handle until its session ends —
    // the pool's cancellation token (the composition root wires signals
    // to it), a clean close, or a lost connection.
    pub async fn run(self) -> anyhow::Result<()> {
        info!("Connecting to BPA at {}", self.bpa_address);

        let client = BpaClient::new(self.bpa_address.clone(), self.tasks.clone())
            .context("Invalid BPA address")?;

        // Register: the registration handle returns once the handshake completes (a
        // failure returns here), and its session runs on the pool. There
        // is no automatic re-registration; a supervisor restarts the
        // process.
        let handle = client
            .register_cla(self.cla_name.clone(), self.cla.clone())
            .await
            .context("CLA registration failed")?;
        info!(
            "CLA {} registered, node IDs: {:?}",
            self.cla_name,
            handle
                .id()
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
        );

        // Dial the static peers in the background; peering is a transport
        // concern of the CLA entity, independent of the BPA registration.
        for peer in &self.peers {
            let cla = self.cla.clone();
            let peer = peer.clone();
            let cancel = self.tasks.cancel_token().clone();
            hardy_async::spawn!(self.tasks, "peer_connect", async move {
                Self::connect_peer(&cla, &peer, cancel).await;
            });
        }

        info!("Started successfully");

        // Await the session's end: cancellation, a clean close, or a lost
        // connection. The registration ran its own `on_unregister`, so
        // teardown here is just the remaining tasks.
        let result = handle.await;
        // Read before `shutdown`, which cancels the token itself.
        let locally_stopped = self.tasks.is_cancelled();
        self.tasks.shutdown().await;
        info!("Stopped");

        match result {
            Ok(()) if locally_stopped => Ok(()),
            // A clean end without a local shutdown is the BPA closing the
            // session; this daemon never unregisters unprompted, so exit
            // nonzero and let a supervisor restart it once the BPA is
            // back.
            Ok(()) => Err(anyhow::anyhow!("The BPA closed the CLA session")),
            Err(e) => Err(e).context("CLA session ended"),
        }
    }

    // Dials `peer` until a session is established or the server is
    // cancelled. Resolution and connection failures are not fatal: a static
    // peer may simply not be up yet, so retry at a gentle pace.
    async fn connect_peer(cla: &Tcpclv4, peer: &str, cancel: CancellationToken) {
        loop {
            match lookup_host(peer).await {
                Ok(mut addrs) => {
                    if let Some(addr) = addrs.next() {
                        info!("Connecting to peer {peer} ({addr})");
                        match cla.connect(&addr).await {
                            Ok(()) => {
                                info!("Connected to peer {peer}");
                                return;
                            }
                            Err(e) => warn!("Failed to connect to peer {peer}: {e}"),
                        }
                    } else {
                        warn!("No addresses resolved for peer {peer}");
                    }
                }
                Err(e) => warn!("Failed to resolve peer {peer}: {e}"),
            }

            tokio::select! {
                _ = tokio::time::sleep(PEER_REDIAL_INTERVAL) => {}
                _ = cancel.cancelled() => return,
            }
        }
    }
}

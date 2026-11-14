// The TCPCLv4 CLA server: builds the `hardy_tcpclv4` entity from the
// loaded `Config`, registers it with the BPA, dials the static peers, and
// runs until cancelled.

use core::time::Duration;
use std::sync::Arc;

use hardy_async::{CancellationToken, TaskPool};
use hardy_bpa::bpa::BpaRegistration;
use hardy_proto::client::RemoteBpa;
use hardy_tcpclv4::Tcpclv4;
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
    // Builds the TCPCLv4 entity from the loaded configuration: the TLS
    // material is loaded and the listeners are bound here, so their errors
    // surface before the server runs.
    //
    // `tasks` hosts the server's background tasks; it comes from the
    // composition root, which owns process policy (wiring SIGINT/SIGTERM
    // to the pool's cancellation token via `signal::listen_for_cancel`).
    pub fn new(config: Config, tasks: TaskPool) -> anyhow::Result<Self> {
        Ok(Self {
            cla: Arc::new(config.cla.try_into()?),
            bpa_address: config.bpa_address,
            cla_name: config.cla_name,
            peers: config.peers,
            tasks,
        })
    }

    // Runs the server to completion: register with the BPA, keep the
    // static peers dialed, then wait for the pool's cancellation token
    // (the composition root wires signals to it) and unregister
    // gracefully.
    pub async fn run(self) -> anyhow::Result<()> {
        info!("Connecting to BPA at {}", self.bpa_address);

        let remote_bpa = RemoteBpa::new(self.bpa_address.clone());

        let node_ids = remote_bpa
            .register_cla(
                self.cla_name.clone(),
                self.cla.clone(),
                None,
                self.cla.cla_init(),
            )
            .await
            .map_err(|e| anyhow::anyhow!("CLA registration failed: {e}"))?;

        info!(
            "CLA {} registered, node IDs: {:?}",
            self.cla_name,
            node_ids.iter().map(|n| n.to_string()).collect::<Vec<_>>()
        );

        for peer in &self.peers {
            let cla = self.cla.clone();
            let peer = peer.clone();
            let cancel = self.tasks.cancel_token().clone();
            hardy_async::spawn!(self.tasks, "peer_connect", async move {
                Self::connect_peer(&cla, &peer, cancel).await;
            });
        }

        info!("Started successfully");

        self.tasks.cancel_token().cancelled().await;

        self.shutdown().await;

        info!("Stopped");

        Ok(())
    }

    // Leaves the network gracefully, in dependency order: unregister from
    // the BPA first, so it stops offering bundles and sweeps this CLA's
    // queues, then wait for the server's remaining tasks to finish.
    async fn shutdown(&self) {
        self.cla.unregister().await;
        self.tasks.shutdown().await;
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

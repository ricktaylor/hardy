// The TCPCLv4 CLA server: builds the `hardy_tcpclv4` entity from the
// loaded `Config`, registers it with the BPA, dials the static peers, and
// runs until cancelled.

use core::time::Duration;

use anyhow::Context as _;
use hardy_async::{CancellationToken, TaskPool};
use hardy_bpa::cla::RegistrarExt;
use hardy_proto::client::RemoteBpa;
use hardy_tcpclv4::{Tcpclv4, config::Config as ClaConfig};
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
    cla_config: ClaConfig,
    bpa_address: String,
    cla_name: String,
    peers: Vec<String>,
    tasks: TaskPool,
}

impl Tcpclv4Server {
    // Takes the loaded configuration apart; nothing is built until
    // [`run`](Self::run), where the BPA builds the entity.
    //
    // `tasks` hosts the server's background tasks; it comes from the
    // composition root, which owns process policy (wiring SIGINT/SIGTERM
    // to the pool's cancellation token via `signal::listen_for_cancel`).
    pub fn new(config: Config, tasks: TaskPool) -> Self {
        Self {
            cla_config: config.cla,
            bpa_address: config.bpa_address,
            cla_name: config.cla_name,
            peers: config.peers,
            tasks,
        }
    }

    // Runs the server to completion: register with the BPA, which builds
    // the entity, keep the static peers dialed, then wait for the pool's
    // cancellation token (the composition root wires signals to it) and
    // unregister gracefully.
    pub async fn run(self) -> anyhow::Result<()> {
        // The TLS material is loaded and the listeners are bound as the
        // factory is assembled, so their errors surface before anything
        // reaches the BPA.
        let factory =
            Tcpclv4::factory(self.cla_config).context("Failed to create the TCPCLv4 CLA")?;

        info!("Connecting to BPA at {}", self.bpa_address);

        let cla = RemoteBpa::new(self.bpa_address.clone())
            .register_cla(&self.cla_name, factory)
            .await
            .map_err(|e| anyhow::anyhow!("CLA registration failed: {e}"))?;

        info!("CLA {} registered", self.cla_name);

        for peer in &self.peers {
            let cla = cla.clone();
            let peer = peer.clone();
            let cancel = self.tasks.cancel_token().clone();
            hardy_async::spawn!(self.tasks, "peer_connect", async move {
                Self::connect_peer(&cla, &peer, cancel).await;
            });
        }

        info!("Started successfully");

        self.tasks.cancel_token().cancelled().await;

        // Leave the network gracefully, in dependency order: unregister
        // from the BPA first, so it stops offering bundles and sweeps this
        // CLA's queues, then wait for the server's remaining tasks to
        // finish.
        cla.unregister().await;
        self.tasks.shutdown().await;

        info!("Stopped");

        Ok(())
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

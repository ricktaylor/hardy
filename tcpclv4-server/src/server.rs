// The TCPCLv4 CLA server: builds the `hardy_tcpclv4` entity from the
// loaded `Config`, registers it with the BPA, dials the static peers, and
// runs until cancelled. The config-to-builder mapping lives here too (kept
// in sync with the `clas:` entry mirror in bpa-server/src/config/tcpclv4.rs).

use core::time::Duration;
use std::sync::Arc;

use hardy_async::{CancellationToken, TaskPool};
use hardy_bpa::{bpa::BpaRegistration, cla};
use hardy_proto::client::RemoteBpa;
use hardy_tcpclv4::{Tcpclv4, tls};
use tokio::{net::lookup_host, time::Instant};
use tracing::{info, warn};

use crate::config::Config;

// The least time between two dials of one static peer: a peer that cannot
// be resolved or connected, or whose sessions keep ending, is re-dialled
// at this pace.
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

    // Runs the server to completion: register with the BPA, keep the
    // static peers connected, then wait for the pool's cancellation token
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
                Self::connect_peer(&cla, &peer, PEER_REDIAL_INTERVAL, cancel).await;
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

    // Keeps `peer` connected until the server is cancelled or the entity
    // unregisters: dials it, waits until the entity holds no session to the
    // dialled address (the peer restarted, or the link went away), and dials
    // again. Resolution and
    // connection failures are not fatal: a static peer may simply not be up
    // yet. Dials of one peer start at least `redial_interval` apart.
    //
    // This is the minimal keep-up target; the BPA's dial subsystem
    // (bpa/docs/dial_on_demand_plan.md) is planned to take it over.
    async fn connect_peer(
        cla: &Tcpclv4,
        peer: &str,
        redial_interval: Duration,
        cancel: CancellationToken,
    ) {
        loop {
            let dialled_at = Instant::now();
            match lookup_host(peer).await {
                Ok(mut addrs) => {
                    if let Some(addr) = addrs.next() {
                        info!("Connecting to peer {peer} ({addr})");
                        // A dial to an unreachable peer can retry for several
                        // contact timeouts; shutdown does not wait for it
                        let connected = tokio::select! {
                            r = cla.connect(&addr) => r,
                            _ = cancel.cancelled() => return,
                        };
                        match connected {
                            Ok(()) => {
                                info!("Connected to peer {peer}");
                                tokio::select! {
                                    _ = cla.disconnected(&addr) => {
                                        info!("No session left to peer {peer} ({addr})");
                                    }
                                    _ = cancel.cancelled() => return,
                                }
                            }
                            Err(cla::Error::Disconnected) => {
                                info!("Unregistered from the BPA: no longer dialling peer {peer}");
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
                _ = tokio::time::sleep_until(dialled_at + redial_interval) => {}
                _ = cancel.cancelled() => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, TcpListener as StdTcpListener};

    use hardy_bpa::{
        async_trait,
        cla::{self, Cla, ClaAddress, Segment, Sink, TransferOutcome},
        stream::Receiver,
    };
    use hardy_bpv7::{
        bundle::Id,
        eid::{IpnNodeId, NodeId},
    };
    use tokio::{
        net::{TcpListener, TcpStream},
        sync::mpsc,
        task::JoinHandle,
    };

    use super::*;

    // Reports each adjacency the entity registers on a channel
    struct AddPeerSink(mpsc::UnboundedSender<ClaAddress>);

    #[async_trait]
    impl Sink for AddPeerSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            _peer_node: Option<&NodeId>,
            _peer_addr: Option<&ClaAddress>,
            _stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<cla::Acceptance> {
            Ok(cla::Acceptance::Accepted)
        }

        async fn add_peer(&self, cla_addr: ClaAddress, _node_ids: &[NodeId]) -> cla::Result<bool> {
            _ = self.0.send(cla_addr);
            Ok(true)
        }

        async fn remove_peer(&self, _cla_addr: &ClaAddress) -> cla::Result<bool> {
            Ok(true)
        }

        async fn transfer_outcome(
            &self,
            _bundle_id: &Id,
            _outcome: TransferOutcome,
        ) -> cla::Result<()> {
            Ok(())
        }
    }

    fn ipn(node_number: u32) -> NodeId {
        NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number,
        })
    }

    // A registered entity listening on an ephemeral loopback port, its
    // listen address, and the adjacencies it registers
    async fn listening_entity() -> (Tcpclv4, SocketAddr, mpsc::UnboundedReceiver<ClaAddress>) {
        // The entity binds its own listener and does not expose the bound
        // address, so an ephemeral port is probed and handed to it
        let probe = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let address = probe.local_addr().unwrap();
        drop(probe);
        let cla = Tcpclv4::builder().listen(address).build().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        cla.on_register(Box::new(AddPeerSink(tx)), &[ipn(2)], None)
            .await;
        (cla, address, rx)
    }

    // A TCP relay in front of `upstream`: each accepted connection is
    // relayed until the test cuts it by dropping the relay's end, which
    // the dialler sees as the peer going away
    async fn relay(upstream: SocketAddr) -> (SocketAddr, mpsc::UnboundedReceiver<JoinHandle<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                let mut outbound = TcpStream::connect(upstream).await.unwrap();
                _ = tx.send(tokio::spawn(async move {
                    _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }));
            }
        });
        (address, rx)
    }

    async fn next<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
        // the timeout only bounds a regression; the event follows at once
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("event never arrived")
            .unwrap()
    }

    // A static peer is kept connected: when the session to it ends (the
    // peer restarted, or the link went away), the server dials it again
    // without waiting for anything else, and stops when cancelled.
    #[tokio::test]
    async fn static_peer_is_redialled_when_its_session_ends() {
        let (peer, peer_listen, mut peer_adjacencies) = listening_entity().await;
        let (relay_address, mut relayed) = relay(peer_listen).await;

        let cla = Arc::new(Tcpclv4::builder().build().unwrap());
        let (tx, mut adjacencies) = mpsc::unbounded_channel();
        cla.on_register(Box::new(AddPeerSink(tx)), &[ipn(1)], None)
            .await;

        let cancel = CancellationToken::new();
        let keep_up = tokio::spawn({
            let (cla, cancel) = (cla.clone(), cancel.clone());
            let peer = relay_address.to_string();
            async move { Tcpclv4Server::connect_peer(&cla, &peer, Duration::ZERO, cancel).await }
        });

        // The dial completes: the session registers at both ends, the
        // dialler's side last, just before `connect` returns
        let first = next(&mut relayed).await;
        let first_session = next(&mut peer_adjacencies).await;
        assert_eq!(next(&mut adjacencies).await, ClaAddress::Tcp(relay_address));

        // The link goes away under the established session
        first.abort();

        let _second = next(&mut relayed).await;
        let second_session = next(&mut peer_adjacencies).await;
        assert_ne!(second_session, first_session);
        assert_eq!(next(&mut adjacencies).await, ClaAddress::Tcp(relay_address));

        cancel.cancel();
        // the timeout only bounds a regression; cancellation is immediate
        tokio::time::timeout(Duration::from_secs(10), keep_up)
            .await
            .expect("connect_peer ignored cancellation")
            .unwrap();

        cla.on_unregister().await;
        peer.on_unregister().await;
    }

    // Cancellation stops a dial in progress: a peer that accepts the TCP
    // connection but never answers holds `connect` for up to five contact
    // timeouts, and shutdown must not wait for that.
    #[tokio::test]
    async fn static_peer_dialling_stops_when_cancelled_mid_dial() {
        let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = silent.local_addr().unwrap();
        let cla = Arc::new(
            Tcpclv4::builder()
                .contact_timeout(hardy_tcpclv4::ContactTimeout::MAX)
                .build()
                .unwrap(),
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        cla.on_register(Box::new(AddPeerSink(tx)), &[ipn(1)], None)
            .await;

        let cancel = CancellationToken::new();
        let keep_up = tokio::spawn({
            let (cla, cancel) = (cla.clone(), cancel.clone());
            async move {
                Tcpclv4Server::connect_peer(&cla, &address.to_string(), Duration::ZERO, cancel)
                    .await
            }
        });

        // The dial is in progress: its contact header has arrived, and the
        // peer will never answer it
        let (mut dialled, _) = silent.accept().await.unwrap();
        let mut header = [0u8; 6];
        tokio::io::AsyncReadExt::read_exact(&mut dialled, &mut header)
            .await
            .unwrap();
        assert_eq!(&header[..4], b"dtn!");

        cancel.cancel();
        // the timeout only bounds a regression: correct code returns at
        // once, the regression waits out five 60-second contact timeouts
        tokio::time::timeout(Duration::from_secs(10), keep_up)
            .await
            .expect("connect_peer waited out a dial after cancellation")
            .unwrap();

        cla.on_unregister().await;
    }

    // An entity that has unregistered from the BPA cannot carry sessions,
    // so the server stops dialling its static peers instead of retrying
    // forever.
    #[tokio::test]
    async fn static_peer_dialling_stops_once_unregistered() {
        let (peer, peer_listen, _peer_adjacencies) = listening_entity().await;
        let cla = Tcpclv4::builder().build().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        cla.on_register(Box::new(AddPeerSink(tx)), &[ipn(1)], None)
            .await;
        cla.on_unregister().await;

        // the timeout only bounds a regression; the first dial is refused
        tokio::time::timeout(
            Duration::from_secs(10),
            Tcpclv4Server::connect_peer(
                &cla,
                &peer_listen.to_string(),
                Duration::ZERO,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("connect_peer kept dialling for an unregistered entity");

        peer.on_unregister().await;
    }
}

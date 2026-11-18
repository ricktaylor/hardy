// Shared infrastructure for TCPCLv4 fuzz targets.
//
// Provides a mock sink and CLA setup that can be used by both the passive
// (listener) and active (connector) fuzz targets.

use std::{net::SocketAddr, sync::Arc};

use hardy_bpa::{
    async_trait,
    cla::{self, Cla as _, Context, Factory, Terms},
    node_ids::NodeIds,
};
use hardy_bpv7::eid::{IpnNodeId, NodeId};
use hardy_tcpclv4::{ContactTimeout, Tcpclv4, builder::Tcpclv4Builder};

// A mock CLA Sink that accepts everything and discards it.
//
// Dispatched bundles are silently dropped. Peer add/remove always succeeds.
pub struct MockSink;

#[async_trait]
impl cla::Sink for MockSink {
    async fn unregister(&self) {}

    async fn dispatch(
        &self,
        _peer_node: Option<&NodeId>,
        _peer_addr: Option<&cla::Address>,
        _stream: &mut dyn hardy_bpa::stream::Receiver<cla::Segment>,
    ) -> cla::Result<cla::Acceptance> {
        Ok(cla::Acceptance::Accepted)
    }

    async fn add_peer(&self, _cla_addr: cla::Address, _node_ids: &[NodeId]) -> cla::Result<bool> {
        Ok(true)
    }

    async fn remove_peer(&self, _cla_addr: &cla::Address) -> cla::Result<bool> {
        Ok(true)
    }

    async fn report_transfer(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _outcome: cla::TransferOutcome,
    ) -> cla::Result<()> {
        Ok(())
    }
}

// The listen address for the passive (listener) fuzz target. The active target
// binds its own ephemeral port, so this is the only fixed port in the harness.
//
// Defaults to `[::1]:4556`. Override with `FUZZ_LISTEN_ADDR` env var
// to avoid port conflicts in CI or parallel fuzzing (e.g., `FUZZ_LISTEN_ADDR=[::1]:0`).
pub fn fuzz_addr() -> SocketAddr {
    std::env::var("FUZZ_LISTEN_ADDR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(SocketAddr::V6(std::net::SocketAddrV6::new(
            std::net::Ipv6Addr::LOCALHOST,
            4556,
            0,
            0,
        )))
}

// Builder tuned for fuzzing — the shortest timeouts the builder accepts,
// to keep corpus replay (coverage) fast. `contact_timeout` is in whole
// seconds, so 1 is the floor.
fn fuzz_builder() -> Tcpclv4Builder {
    Tcpclv4::builder()
        .contact_timeout(ContactTimeout::new(1).expect("1 second is a valid timeout"))
        .no_keepalive()
}

// Build and start the CLA on a mock sink, as the BPA would.
async fn start(factory: impl Factory<Component = Tcpclv4>) -> Arc<Tcpclv4> {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .expect("ipn:0.1 is a valid node identifier");

    let cla = factory
        .build(Context::new(
            Arc::new(MockSink),
            Arc::new(node_ids),
            Terms::default(),
        ))
        .expect("a TCPCLv4 entity builds under the default terms");
    cla.start().await.expect("the fuzz CLA starts");
    cla
}

// Create a TCPCLv4 CLA with default settings, running on a mock sink and
// listening on `fuzz_addr()`.
pub async fn setup_listener() -> Arc<Tcpclv4> {
    start(
        fuzz_builder()
            .listen(fuzz_addr())
            .build()
            .expect("failed to bind the fuzz listener"),
    )
    .await
}

// Create a TCPCLv4 CLA with no listener (for active/connect fuzzing),
// running on a mock sink and ready to `connect()`.
pub async fn setup_connector() -> Arc<Tcpclv4> {
    start(
        fuzz_builder()
            .build()
            .expect("a plaintext build cannot fail"),
    )
    .await
}

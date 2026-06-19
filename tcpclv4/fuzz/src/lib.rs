// Shared infrastructure for TCPCLv4 fuzz targets.
//
// Provides a mock BPA and CLA setup that can be used by both the passive
// (listener) and active (connector) fuzz targets.

use std::{net::SocketAddr, sync::Arc};

use bytes::Bytes;
use hardy_bpa::{async_trait, bpa::BpaRegistration, cla};
use hardy_bpv7::eid::NodeId;

// A mock CLA Sink that accepts everything and discards it.
//
// Dispatched bundles are silently dropped. Peer add/remove always succeeds.
pub struct MockSink;

#[async_trait]
impl cla::Sink for MockSink {
    async fn unregister(&self) {}

    async fn dispatch(
        &self,
        _bundle: Bytes,
        _peer_node: Option<&NodeId>,
        _peer_addr: Option<&cla::ClaAddress>,
    ) -> cla::Result<()> {
        Ok(())
    }

    async fn dispatch_streamed(
        &self,
        _stream: &dyn hardy_bpa::stream::Receiver<hardy_bpa::cla::Segment>,
        _peer_node: Option<&NodeId>,
        _peer_addr: Option<&cla::ClaAddress>,
    ) -> cla::Result<()> {
        Ok(())
    }

    async fn add_peer(
        &self,
        _cla_addr: cla::ClaAddress,
        _node_ids: &[NodeId],
    ) -> cla::Result<bool> {
        Ok(true)
    }

    async fn remove_peer(&self, _cla_addr: &cla::ClaAddress) -> cla::Result<bool> {
        Ok(true)
    }
}

// A mock BPA that registers CLAs by immediately calling `on_register` with a
// `MockSink` and a default node ID.
pub struct MockBpa;

#[async_trait]
impl hardy_bpa::bpa::BpaRegistration for MockBpa {
    async fn register_cla(
        &self,
        _name: String,
        cla: Arc<dyn cla::Cla>,
        _policy: Option<Arc<dyn hardy_bpa::policy::EgressPolicy>>,
    ) -> cla::Result<Vec<NodeId>> {
        let node_ids = vec![NodeId::Ipn(hardy_bpv7::eid::IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })];
        cla.on_register(Box::new(MockSink), &node_ids).await;
        Ok(node_ids)
    }

    async fn register_routing_agent(
        &self,
        _name: String,
        _agent: Arc<dyn hardy_bpa::routing::RoutingAgent>,
    ) -> hardy_bpa::routing::Result<Vec<NodeId>> {
        unimplemented!("not needed for CLA fuzzing")
    }

    async fn register_service(
        &self,
        _service_id: hardy_bpv7::eid::Service,
        _service: Arc<dyn hardy_bpa::services::Service>,
    ) -> hardy_bpa::services::Result<hardy_bpv7::eid::Eid> {
        unimplemented!("not needed for CLA fuzzing")
    }

    async fn register_application(
        &self,
        _service_id: hardy_bpv7::eid::Service,
        _application: Arc<dyn hardy_bpa::services::Application>,
    ) -> hardy_bpa::services::Result<hardy_bpv7::eid::Eid> {
        unimplemented!("not needed for CLA fuzzing")
    }

    async fn register_dynamic_service(
        &self,
        _service: Arc<dyn hardy_bpa::services::Service>,
    ) -> hardy_bpa::services::Result<hardy_bpv7::eid::Eid> {
        unimplemented!("not needed for CLA fuzzing")
    }

    async fn register_dynamic_application(
        &self,
        _application: Arc<dyn hardy_bpa::services::Application>,
    ) -> hardy_bpa::services::Result<hardy_bpv7::eid::Eid> {
        unimplemented!("not needed for CLA fuzzing")
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

// Session config tuned for fuzzing — the shortest timeouts the config allows,
// to keep corpus replay (coverage) fast. `contact_timeout` is in whole seconds,
// so 1 is the floor.
fn fuzz_session_config() -> hardy_tcpclv4::config::SessionConfig {
    hardy_tcpclv4::config::SessionConfig {
        contact_timeout: 1,
        keepalive_interval: None,
        require_tls: false,
    }
}

// Create a TCPCLv4 CLA with default config, registered against a mock BPA,
// listening on `fuzz_addr()`.
pub async fn setup_listener() -> Arc<hardy_tcpclv4::Cla> {
    let config = hardy_tcpclv4::config::Config {
        address: Some(fuzz_addr()),
        session_defaults: fuzz_session_config(),
        ..Default::default()
    };

    let cla = Arc::new(hardy_tcpclv4::Cla::new(&config).expect("CLA construction should not fail"));

    MockBpa
        .register_cla("fuzz-tcpclv4".to_string(), cla.clone(), None)
        .await
        .expect("CLA registration should not fail");

    cla
}

// Create a TCPCLv4 CLA with no listener (for active/connect fuzzing).
// The CLA is registered against a mock BPA and ready to `connect()`.
pub async fn setup_connector() -> Arc<hardy_tcpclv4::Cla> {
    let config = hardy_tcpclv4::config::Config {
        address: None,
        session_defaults: fuzz_session_config(),
        ..Default::default()
    };

    let cla = Arc::new(hardy_tcpclv4::Cla::new(&config).expect("CLA construction should not fail"));

    MockBpa
        .register_cla("fuzz-tcpclv4-active".to_string(), cla.clone(), None)
        .await
        .expect("CLA registration should not fail");

    cla
}

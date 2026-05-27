use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::cla;
use hardy_bpv7::eid::NodeId;
use std::net::SocketAddr;
use std::sync::Arc;

fn mock_cla_context() -> cla::ClaContext {
    let (ingress_tx, _) = flume::unbounded();
    let (peer_tx, _) = flume::unbounded();
    let token = hardy_async::CancellationToken::new();
    cla::ClaContext::new(ingress_tx, peer_tx, token)
}

pub struct MockBpa;

#[hardy_bpa::async_trait]
impl BpaRegistration for MockBpa {
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
        cla.on_register(mock_cla_context(), &node_ids).await;
        Ok(node_ids)
    }

    async fn register_routing_agent(
        &self,
        _name: String,
        _agent: Arc<dyn hardy_bpa::routes::RoutingAgent>,
    ) -> hardy_bpa::routes::Result<Vec<NodeId>> {
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

fn fuzz_session_config() -> hardy_tcpclv4::config::SessionConfig {
    hardy_tcpclv4::config::SessionConfig {
        contact_timeout: 2,
        keepalive_interval: None,
        require_tls: false,
    }
}

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

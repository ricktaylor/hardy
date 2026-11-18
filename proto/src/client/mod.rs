use super::*;
use proxy::*;

mod application;
mod cla;
mod routing;
mod service;

fn from_timestamp(t: prost_types::Timestamp) -> Result<time::OffsetDateTime, tonic::Status> {
    Ok(time::OffsetDateTime::from_unix_timestamp(t.seconds)
        .map_err(|e| tonic::Status::from_error(e.into()))?
        + time::Duration::nanoseconds(t.nanos.into()))
}

/// A remote BPA, reached over gRPC.
///
/// Implements the same registration doors as a local `Bpa`, so a CLA,
/// service, application or routing agent attaches to a BPA in another
/// process through the interface it already uses.
///
/// # Example
///
/// ```ignore
/// let remote_bpa = RemoteBpa::new("http://[::1]:50051".to_string());
/// let cla = remote_bpa.register_cla("tcp0", Tcpclv4::builder().build()?).await?;
/// ```
pub struct RemoteBpa {
    grpc_addr: String,
}

impl RemoteBpa {
    /// Create a new RemoteBpa client.
    ///
    /// # Arguments
    ///
    /// * `grpc_addr` - The gRPC server address (e.g., "http://\[::1\]:50051")
    pub fn new(grpc_addr: String) -> Self {
        Self { grpc_addr }
    }

    /// Get the gRPC address this client connects to.
    pub fn grpc_addr(&self) -> &str {
        &self.grpc_addr
    }
}

#[async_trait]
impl hardy_bpa::cla::Registrar for RemoteBpa {
    async fn register_cla_boxed(
        &self,
        name: &str,
        factory: hardy_bpa::cla::BoxFactory<'_>,
    ) -> hardy_bpa::cla::Result<()> {
        cla::register_cla(self.grpc_addr.clone(), name, factory).await
    }
}

#[async_trait]
impl hardy_bpa::bpa::BpaRegistration for RemoteBpa {
    async fn register_raw_service(
        &self,
        service_id: hardy_bpv7::eid::Service,
        service: Arc<dyn hardy_bpa::service::raw::Service>,
    ) -> hardy_bpa::service::Result<hardy_bpv7::eid::Eid> {
        service::register_raw_service(self.grpc_addr.clone(), Some(service_id), service).await
    }

    async fn register_service(
        &self,
        service_id: hardy_bpv7::eid::Service,
        application: Arc<dyn hardy_bpa::service::Service>,
    ) -> hardy_bpa::service::Result<hardy_bpv7::eid::Eid> {
        application::register_service(self.grpc_addr.clone(), Some(service_id), application).await
    }

    async fn register_dynamic_raw_service(
        &self,
        service: Arc<dyn hardy_bpa::service::raw::Service>,
    ) -> hardy_bpa::service::Result<hardy_bpv7::eid::Eid> {
        service::register_raw_service(self.grpc_addr.clone(), None, service).await
    }

    async fn register_dynamic_service(
        &self,
        application: Arc<dyn hardy_bpa::service::Service>,
    ) -> hardy_bpa::service::Result<hardy_bpv7::eid::Eid> {
        application::register_service(self.grpc_addr.clone(), None, application).await
    }

    async fn register_routing_agent(
        &self,
        name: String,
        agent: Arc<dyn hardy_bpa::routing::Agent>,
    ) -> hardy_bpa::routing::Result<Vec<hardy_bpv7::eid::NodeId>> {
        routing::register_routing_agent(self.grpc_addr.clone(), name, agent).await
    }
}

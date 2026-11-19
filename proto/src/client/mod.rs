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
impl hardy_bpa::service::Registrar for RemoteBpa {
    async fn register_service_boxed(
        &self,
        id: Option<hardy_bpv7::eid::Service>,
        factory: hardy_bpa::service::BoxFactory<'_>,
    ) -> hardy_bpa::service::Result<()> {
        application::register_service(self.grpc_addr.clone(), id, factory).await
    }

    async fn register_raw_service_boxed(
        &self,
        id: Option<hardy_bpv7::eid::Service>,
        factory: hardy_bpa::service::raw::BoxFactory<'_>,
    ) -> hardy_bpa::service::Result<()> {
        service::register_raw_service(self.grpc_addr.clone(), id, factory).await
    }
}

#[async_trait]
impl hardy_bpa::routing::Registrar for RemoteBpa {
    async fn register_routing_agent_boxed(
        &self,
        name: &str,
        factory: hardy_bpa::routing::BoxFactory<'_>,
    ) -> hardy_bpa::routing::Result<()> {
        routing::register_routing_agent(self.grpc_addr.clone(), name, factory).await
    }
}

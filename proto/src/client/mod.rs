use core::num::NonZeroUsize;

use hardy_bpa::cla::Lanes;

use super::*;
use proxy::*;

mod application;
mod cla;
mod routing;
mod service;

// The proxy handlers a service or application needs for its declared
// delivery lanes: room for every lane of a limited declaration at once, so
// the host's core count never caps them; a serial or unlimited one keeps
// the default bound.
fn delivery_handlers(lanes: Lanes) -> NonZeroUsize {
    match lanes {
        Lanes::Limited(n) => handlers_for_lanes(n),
        Lanes::Serial | Lanes::Unlimited => default_handlers(),
    }
}

fn from_timestamp(t: prost_types::Timestamp) -> Result<time::OffsetDateTime, tonic::Status> {
    Ok(time::OffsetDateTime::from_unix_timestamp(t.seconds)
        .map_err(|e| tonic::Status::from_error(e.into()))?
        + time::Duration::nanoseconds(t.nanos.into()))
}

/// A remote BPA client that implements `BpaRegistration` via gRPC.
///
/// This allows CLAs, services, and applications to connect to a remote BPA
/// server using the same interface as a local `Bpa` instance.
///
/// # Example
///
/// ```ignore
/// let remote_bpa = RemoteBpa::new("http://[::1]:50051".to_string());
/// cla.register(&remote_bpa, "tcp0".to_string(), None).await?;
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
impl hardy_bpa::bpa::BpaRegistration for RemoteBpa {
    async fn register_cla(
        &self,
        name: String,
        cla: Arc<dyn hardy_bpa::cla::Cla>,
        _policy: Option<Arc<dyn hardy_bpa::policy::FlowControllerFactory>>,
        init: hardy_bpa::cla::ClaInit,
    ) -> hardy_bpa::cla::Result<Vec<hardy_bpv7::eid::NodeId>> {
        // Note: policy is not supported over gRPC currently
        cla::register_cla(self.grpc_addr.clone(), name, cla, init).await
    }

    async fn register_service(
        &self,
        service_id: hardy_bpv7::eid::Service,
        service: Arc<dyn hardy_bpa::services::Service>,
        lanes: Lanes,
    ) -> hardy_bpa::services::Result<hardy_bpv7::eid::Eid> {
        service::register_endpoint_service(self.grpc_addr.clone(), Some(service_id), service, lanes)
            .await
    }

    async fn register_application(
        &self,
        service_id: hardy_bpv7::eid::Service,
        application: Arc<dyn hardy_bpa::services::Application>,
        lanes: Lanes,
    ) -> hardy_bpa::services::Result<hardy_bpv7::eid::Eid> {
        application::register_application_service(
            self.grpc_addr.clone(),
            Some(service_id),
            application,
            lanes,
        )
        .await
    }

    async fn register_dynamic_service(
        &self,
        service: Arc<dyn hardy_bpa::services::Service>,
        lanes: Lanes,
    ) -> hardy_bpa::services::Result<hardy_bpv7::eid::Eid> {
        service::register_endpoint_service(self.grpc_addr.clone(), None, service, lanes).await
    }

    async fn register_dynamic_application(
        &self,
        application: Arc<dyn hardy_bpa::services::Application>,
        lanes: Lanes,
    ) -> hardy_bpa::services::Result<hardy_bpv7::eid::Eid> {
        application::register_application_service(self.grpc_addr.clone(), None, application, lanes)
            .await
    }

    async fn register_routing_agent(
        &self,
        name: String,
        agent: Arc<dyn hardy_bpa::routing::RoutingAgent>,
    ) -> hardy_bpa::routing::Result<Vec<hardy_bpv7::eid::NodeId>> {
        routing::register_routing_agent(self.grpc_addr.clone(), name, agent).await
    }
}

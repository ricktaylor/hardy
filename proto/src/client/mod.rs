use super::*;
use proxy::*;

mod application;
mod cla;
mod service;

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
    /// * `grpc_addr` - The gRPC server address (e.g., "http://[::1]:50051")
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
        address_type: Option<hardy_bpa::cla::ClaAddressType>,
        cla: Arc<dyn hardy_bpa::cla::Cla>,
        _policy: Option<Arc<dyn hardy_bpa::policy::EgressPolicy>>,
    ) -> hardy_bpa::cla::Result<Vec<hardy_bpv7::eid::NodeId>> {
        // Note: policy is not supported over gRPC currently
        cla::register_cla(self.grpc_addr.clone(), name, address_type, cla).await
    }

    async fn register_service(
        &self,
        service_id: Option<hardy_bpv7::eid::Service>,
        service: Arc<dyn hardy_bpa::services::Service>,
    ) -> hardy_bpa::services::Result<hardy_bpv7::eid::Eid> {
        service::register_endpoint_service(self.grpc_addr.clone(), service_id, service).await
    }

    async fn register_application(
        &self,
        service_id: Option<hardy_bpv7::eid::Service>,
        application: Arc<dyn hardy_bpa::services::Application>,
    ) -> hardy_bpa::services::Result<hardy_bpv7::eid::Eid> {
        application::register_application_service(self.grpc_addr.clone(), service_id, application)
            .await
    }
}

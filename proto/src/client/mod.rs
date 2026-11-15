use super::*;
use proxy::*;

mod application;
mod cla;
mod routing;
mod service;

// One endpoint's gRPC registration stream, which a failed multi-endpoint
// registration closes again.
struct Registration(Box<dyn Close>);

impl Registration {
    fn new<S, R>(proxy: Arc<RpcProxy<S, R>>) -> Self
    where
        R: RecvMsg + Send + Sync + 'static,
        R::Msg: Send,
        S: SendMsg + Send + Sync + 'static,
        S::Msg: Send,
    {
        Self(Box::new(proxy))
    }

    // Closes the stream: the BPA unregisters the endpoint, and the
    // service sees `on_unregister` for it.
    async fn close(self) {
        self.0.close().await;
    }
}

#[async_trait]
trait Close: Send + Sync {
    async fn close(&self);
}

#[async_trait]
impl<S, R> Close for Arc<RpcProxy<S, R>>
where
    R: RecvMsg + Send + Sync + 'static,
    R::Msg: Send,
    S: SendMsg + Send + Sync + 'static,
    S::Msg: Send,
{
    async fn close(&self) {
        self.shutdown().await;
    }
}

// Registers one endpoint per id, one stream each, in order. A gRPC stream
// carries one endpoint, so the set is not atomic at the BPA: if an id
// fails, the endpoints already registered are closed again before the
// error is returned.
async fn register_each<F, Fut>(
    service_ids: &[hardy_bpv7::eid::Service],
    mut register: F,
) -> hardy_bpa::services::Result<Vec<hardy_bpv7::eid::Eid>>
where
    F: FnMut(hardy_bpv7::eid::Service) -> Fut,
    Fut: core::future::Future<
            Output = hardy_bpa::services::Result<(hardy_bpv7::eid::Eid, Registration)>,
        >,
{
    if service_ids.is_empty() {
        return Err(hardy_bpa::services::Error::NoServiceIds);
    }
    let mut registered = Vec::with_capacity(service_ids.len());
    for service_id in service_ids {
        match register(service_id.clone()).await {
            Ok(registration) => registered.push(registration),
            Err(e) => {
                for (_, registration) in registered {
                    registration.close().await;
                }
                return Err(e);
            }
        }
    }
    Ok(registered.into_iter().map(|(eid, _)| eid).collect())
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
    ) -> hardy_bpa::services::Result<hardy_bpv7::eid::Eid> {
        service::register_endpoint_service(self.grpc_addr.clone(), Some(service_id), service)
            .await
            .map(|(eid, _)| eid)
    }

    async fn register_application(
        &self,
        service_id: hardy_bpv7::eid::Service,
        application: Arc<dyn hardy_bpa::services::Application>,
    ) -> hardy_bpa::services::Result<hardy_bpv7::eid::Eid> {
        application::register_application_service(
            self.grpc_addr.clone(),
            Some(service_id),
            application,
        )
        .await
        .map(|(eid, _)| eid)
    }

    async fn register_service_endpoints(
        &self,
        service_ids: &[hardy_bpv7::eid::Service],
        service: Arc<dyn hardy_bpa::services::Service>,
    ) -> hardy_bpa::services::Result<Vec<hardy_bpv7::eid::Eid>> {
        register_each(service_ids, |service_id| {
            service::register_endpoint_service(
                self.grpc_addr.clone(),
                Some(service_id),
                service.clone(),
            )
        })
        .await
    }

    async fn register_application_endpoints(
        &self,
        service_ids: &[hardy_bpv7::eid::Service],
        application: Arc<dyn hardy_bpa::services::Application>,
    ) -> hardy_bpa::services::Result<Vec<hardy_bpv7::eid::Eid>> {
        register_each(service_ids, |service_id| {
            application::register_application_service(
                self.grpc_addr.clone(),
                Some(service_id),
                application.clone(),
            )
        })
        .await
    }

    async fn register_dynamic_service(
        &self,
        service: Arc<dyn hardy_bpa::services::Service>,
    ) -> hardy_bpa::services::Result<hardy_bpv7::eid::Eid> {
        service::register_endpoint_service(self.grpc_addr.clone(), None, service)
            .await
            .map(|(eid, _)| eid)
    }

    async fn register_dynamic_application(
        &self,
        application: Arc<dyn hardy_bpa::services::Application>,
    ) -> hardy_bpa::services::Result<hardy_bpv7::eid::Eid> {
        application::register_application_service(self.grpc_addr.clone(), None, application)
            .await
            .map(|(eid, _)| eid)
    }

    async fn register_routing_agent(
        &self,
        name: String,
        agent: Arc<dyn hardy_bpa::routing::RoutingAgent>,
    ) -> hardy_bpa::routing::Result<Vec<hardy_bpv7::eid::NodeId>> {
        routing::register_routing_agent(self.grpc_addr.clone(), name, agent).await
    }
}

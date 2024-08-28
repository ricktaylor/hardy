use super::*;
use cla_sink_server::{ClaSink, ClaSinkServer};
use hardy_proto::cla::*;
use tonic::{Request, Response, Status};

pub struct Service {
    cla_registry: cla_registry::ClaRegistry,
    dispatcher: Arc<dispatcher::Dispatcher>,
}

impl Service {
    fn new(
        _config: &config::Config,
        cla_registry: cla_registry::ClaRegistry,
        dispatcher: Arc<dispatcher::Dispatcher>,
    ) -> Self {
        Service {
            cla_registry,
            dispatcher,
        }
    }
}

#[tonic::async_trait]
impl ClaSink for Service {
    #[instrument(skip(self))]
    async fn register_cla(
        &self,
        request: Request<RegisterClaRequest>,
    ) -> Result<Response<RegisterClaResponse>, Status> {
        self.cla_registry
            .register(request.into_inner())
            .await
            .map(Response::new)
    }

    #[instrument(skip(self))]
    async fn unregister_cla(
        &self,
        request: Request<UnregisterClaRequest>,
    ) -> Result<Response<UnregisterClaResponse>, Status> {
        self.cla_registry
            .unregister(request.into_inner())
            .await
            .map(Response::new)
    }

    #[instrument(skip(self))]
    async fn receive_bundle(
        &self,
        request: Request<ReceiveBundleRequest>,
    ) -> Result<Response<ReceiveBundleResponse>, Status> {
        let request = request.into_inner();
        self.cla_registry.exists(request.handle).await?;
        self.dispatcher
            .receive_bundle(Box::from(request.bundle))
            .await
            .map(|_| Response::new(ReceiveBundleResponse {}))
            .map_err(Status::from_error)
    }

    #[instrument(skip(self))]
    async fn confirm_forwarding(
        &self,
        request: Request<ConfirmForwardingRequest>,
    ) -> Result<Response<ConfirmForwardingResponse>, Status> {
        let request = request.into_inner();
        self.cla_registry.exists(request.handle).await?;
        self.dispatcher
            .confirm_forwarding(request.handle, &request.bundle_id)
            .await
            .map(|_| Response::new(ConfirmForwardingResponse {}))
    }

    #[instrument(skip(self))]
    async fn add_neighbour(
        &self,
        request: Request<AddNeighbourRequest>,
    ) -> Result<Response<AddNeighbourResponse>, Status> {
        self.cla_registry
            .add_neighbour(request.into_inner())
            .await
            .map(|_| Response::new(AddNeighbourResponse {}))
    }

    #[instrument(skip(self))]
    async fn remove_neighbour(
        &self,
        request: Request<RemoveNeighbourRequest>,
    ) -> Result<Response<RemoveNeighbourResponse>, Status> {
        self.cla_registry
            .remove_neighbour(request.into_inner())
            .await
            .map(|_| Response::new(RemoveNeighbourResponse {}))
    }
}

pub fn new_service(
    config: &config::Config,
    cla_registry: cla_registry::ClaRegistry,
    dispatcher: Arc<dispatcher::Dispatcher>,
) -> ClaSinkServer<Service> {
    ClaSinkServer::new(Service::new(config, cla_registry, dispatcher))
}

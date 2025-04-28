use super::*;
use cla_sink_server::{ClaSink, ClaSinkServer};
use hardy_proto::cla::*;
use tokio::sync::{Mutex, RwLock};
use tonic::{Request, Response, Status};

pub struct Service {
    bpa: Arc<hardy_bpa::bpa::Bpa>,
}

impl Service {
    fn new(_config: &config::Config, bpa: Arc<hardy_bpa::bpa::Bpa>) -> Self {
        Service { bpa }
    }
}

#[tonic::async_trait]
impl ClaSink for Service {
    #[instrument(skip(self))]
    async fn register_cla(
        &self,
        request: Request<RegisterClaRequest>,
    ) -> Result<Response<RegisterClaResponse>, Status> {
        // Connect to client gRPC address
        let request = request.into_inner();
        let endpoint = Arc::new(Mutex::new(
            cla_client::ClaClient::connect(request.grpc_address.clone())
                .await
                .map_err(|e| {
                    warn!(
                        "Failed to connect to CLA client at {}",
                        request.grpc_address
                    );
                    tonic::Status::invalid_argument(e.to_string())
                })?,
        ));

        self.bpa
            .register_cla(request.into_inner())
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
            .receive_bundle(request.bundle)
            .await
            .map(|_| Response::new(ReceiveBundleResponse {}))
            .map_err(Status::from_error)
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
    bpa: Arc<hardy_bpa::bpa::Bpa>,
) -> ClaSinkServer<Service> {
    ClaSinkServer::new(Service::new(config, bpa))
}

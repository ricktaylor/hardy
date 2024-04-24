use super::*;
use cla_sink_server::{ClaSink, ClaSinkServer};
use hardy_proto::cla::*;
use tonic::{Request, Response, Status};

pub struct Service {
    cla_registry: cla_registry::ClaRegistry,
    ingress: ingress::Ingress,
}

impl Service {
    fn new(
        _config: &config::Config,
        cla_registry: cla_registry::ClaRegistry,
        ingress: ingress::Ingress,
    ) -> Self {
        Service {
            cla_registry,
            ingress,
        }
    }
}

#[tonic::async_trait]
impl ClaSink for Service {
    async fn register_cla(
        &self,
        request: Request<RegisterClaRequest>,
    ) -> Result<Response<RegisterClaResponse>, Status> {
        self.cla_registry
            .register(request.into_inner())
            .await
            .map(|_| Response::new(RegisterClaResponse {}))
    }

    async fn unregister_cla(
        &self,
        request: Request<UnregisterClaRequest>,
    ) -> Result<Response<UnregisterClaResponse>, Status> {
        self.cla_registry
            .unregister(request.into_inner())
            .map(|_| Response::new(UnregisterClaResponse {}))
    }

    async fn forward_bundle(
        &self,
        request: Request<ForwardBundleRequest>,
    ) -> Result<Response<ForwardBundleResponse>, Status> {
        let request = request.into_inner();
        self.ingress
            .receive(
                Some(ingress::ClaSource {
                    protocol: request.protocol,
                    address: request.address,
                }),
                request.bundle,
            )
            .await
            .map(|_| Response::new(ForwardBundleResponse {}))
            .map_err(|e| Status::from_error(e.into()))
    }
}

pub fn new_service(
    config: &config::Config,
    cla_registry: cla_registry::ClaRegistry,
    ingress: ingress::Ingress,
) -> ClaSinkServer<Service> {
    ClaSinkServer::new(Service::new(config, cla_registry, ingress))
}

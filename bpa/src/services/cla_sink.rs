use super::*;
use cla_sink_server::{ClaSink, ClaSinkServer};
use hardy_proto::bpa::*;
use std::sync::Arc;

use tonic::{Request, Response, Status};

pub struct Service {
    cla_registry: Arc<cla_registry::ClaRegistry>,
    ingress: ingress::Ingress,
}

impl Service {
    fn new(
        _config: &config::Config,
        cla_registry: Arc<cla_registry::ClaRegistry>,
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
        self.cla_registry.register(request.into_inner()).await?;
        Ok(Response::new(RegisterClaResponse {}))
    }

    async fn unregister_cla(
        &self,
        request: Request<UnregisterClaRequest>,
    ) -> Result<Response<UnregisterClaResponse>, Status> {
        self.cla_registry.unregister(request.into_inner())?;
        Ok(Response::new(UnregisterClaResponse {}))
    }

    async fn forward_bundle(
        &self,
        request: Request<ForwardBundleRequest>,
    ) -> Result<Response<ForwardBundleResponse>, Status> {
        let request = request.into_inner();
        if !self
            .ingress
            .receive(Some((request.protocol, request.address)), request.bundle)
            .await
            .map_err(|e| Status::from_error(e.into()))?
        {
            Err(Status::invalid_argument("Data is not a bundle"))
        } else {
            Ok(Response::new(ForwardBundleResponse {}))
        }
    }
}

pub fn new_service(config: &config::Config, ingress: ingress::Ingress) -> ClaSinkServer<Service> {
    ClaSinkServer::new(Service::new(
        config,
        cla_registry::ClaRegistry::new(config),
        ingress,
    ))
}

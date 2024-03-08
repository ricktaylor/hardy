use super::*;
use cla_sink_server::{ClaSink, ClaSinkServer};
use hardy_proto::bpa::*;
use std::sync::Arc;

use tonic::{Request, Response, Status};

#[derive(Debug)]
pub struct Service {
    cla_registry: cla::ClaRegistry,
}

impl Service {
    pub fn new(_config: Arc<settings::Config>, cla_registry: cla::ClaRegistry) -> Self {
        Service { cla_registry }
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
        _request: Request<ForwardBundleRequest>,
    ) -> Result<Response<ForwardBundleResponse>, Status> {
        Ok(Response::new(ForwardBundleResponse {}))
    }
}

pub fn new_service(
    config: Arc<settings::Config>,
    cla_registry: cla::ClaRegistry,
) -> ClaSinkServer<Service> {
    let service = Service::new(config, cla_registry);

    ClaSinkServer::new(service)
}

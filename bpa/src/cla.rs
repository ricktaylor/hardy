use hardy_proto::bpa::*;
use cla_sink_server::{ClaSink, ClaSinkServer};

use tonic::{Request, Response, Status};

#[derive(Debug, Default)]
pub struct Service {}

#[tonic::async_trait]
impl ClaSink for Service {
    async fn register_cla(
        &self,
        _request: Request<RegisterClaRequest>,
    ) -> Result<Response<RegisterClaResponse>, Status> {
        Ok(Response::new(RegisterClaResponse {}))
    }

    async fn unregister_cla(
        &self,
        _request: Request<UnregisterClaRequest>,
    ) -> Result<Response<UnregisterClaResponse>, Status> {
        Ok(Response::new(UnregisterClaResponse {}))
    }

    async fn forward_bundle(
        &self,
        _request: Request<ForwardBundleRequest>,
    ) -> Result<Response<ForwardBundleResponse>, Status> {
        Ok(Response::new(ForwardBundleResponse {}))
    }
}

pub fn new_service() -> ClaSinkServer<Service> {
    let service = Service::default();

    ClaSinkServer::new(service)
}

use super::*;
use cla_sink_server::{ClaSink, ClaSinkServer};
use hardy_proto::bpa::*;

use tonic::{Request, Response, Status};

#[derive(Debug)]
pub struct Service {
    cla_registry: cla::ClaRegistry,
    cache: cache::Cache,
}

impl Service {
    pub fn new(
        _config: &settings::Config,
        cla_registry: cla::ClaRegistry,
        cache: cache::Cache,
    ) -> Self {
        Service {
            cla_registry,
            cache,
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
        let failure = self
            .cache
            .store(&std::sync::Arc::new(request.into_inner().bundle))
            .await
            .map_err(|e| Status::from_error(e.into()))?;

        Ok(Response::new(ForwardBundleResponse {
            failure: failure.map(|reason| BundleProcessingFailure { reason }),
        }))
    }
}

pub fn new_service(config: &settings::Config, cache: cache::Cache) -> ClaSinkServer<Service> {
    ClaSinkServer::new(Service::new(config, cla::ClaRegistry::new(config), cache))
}

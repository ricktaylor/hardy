use super::*;
use cla_server::{Cla, ClaServer};
use hardy_proto::cla::*;
use tonic::{Request, Response, Status};

pub struct Service {}

impl Service {
    fn new(_config: &config::Config) -> Self {
        Service {}
    }
}

#[tonic::async_trait]
impl Cla for Service {
    #[instrument(skip(self))]
    async fn forward_bundle(
        &self,
        request: Request<ForwardBundleRequest>,
    ) -> Result<Response<ForwardBundleResponse>, Status> {
        // Implement the logic for the forward_bundle function here
        let _request = request.into_inner();
        todo!()
    }
}

pub fn new_service(config: &config::Config) -> ClaServer<Service> {
    ClaServer::new(Service::new(config))
}

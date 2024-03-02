use hardy_proto::bpa::cla_sink_server::{ClaSink, ClaSinkServer};
use hardy_proto::bpa::{ForwardBundle, PollRequest, ReceiveBundle, ReceiveResponse};

use tonic::{Request, Response, Status};

#[derive(Debug, Default)]
pub struct Service {}

#[tonic::async_trait]
impl ClaSink for Service {
    async fn receive(
        &self,
        _request: Request<ReceiveBundle>,
    ) -> Result<Response<ReceiveResponse>, Status> {
        Ok(Response::new(ReceiveResponse {}))
    }

    async fn poll(
        &self,
        _request: Request<PollRequest>,
    ) -> Result<Response<ForwardBundle>, Status> {
        Ok(Response::new(ForwardBundle {}))
    }
}

pub fn new_service() -> ClaSinkServer<Service> {
    let service = Service::default();

    ClaSinkServer::new(service)
}

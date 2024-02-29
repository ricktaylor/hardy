use hardy_proto::bpa::cla_sink_server::{ClaSink, ClaSinkServer};
use hardy_proto::bpa::{ReceiveBundle, ReceiveResponse, ForwardRequest, ForwardBundle};

use tokio::sync::mpsc;
use tonic::{Request, Streaming, Response, Status};
use tokio_stream::{wrappers::ReceiverStream,StreamExt};

#[derive(Debug, Default)]
pub struct Service {}

#[tonic::async_trait]
impl ClaSink for Service {

    type ReceiveStream = ReceiverStream<Result<ReceiveResponse, Status>>;

    async fn receive(
        &self,
        request: Request<Streaming<ReceiveBundle>>,
    ) -> Result<Response<Self::ReceiveStream>, Status> {
        let (tx, rx) = mpsc::channel(4);
        let mut stream = request.into_inner();
        while let Some(r) = stream.next().await {
            let bundle = r?;
            let tx = tx.clone();
            tokio::spawn(async move {
                tx.send(Ok(ReceiveResponse{
                    id: bundle.id.clone(),
                    status: Some(
                        process_request(bundle).map_or_else(|s| s, |_| tonic::Status::ok("processed") ).into()
                    )
                })).await
            });
        }
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type ForwardStream = ReceiverStream<Result<ForwardBundle, Status>>;

    async fn forward(
        &self,
        _request: Request<ForwardRequest>,
    ) -> Result<Response<Self::ForwardStream>, Status> {
        unimplemented!()
    }
}

pub fn new_service() -> ClaSinkServer<Service> {
    let service = Service::default();

    ClaSinkServer::new(service)
}

fn process_request(bundle: ReceiveBundle) -> Result<(),Status> {
    todo!()
}

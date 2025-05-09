use super::*;
use application_sink_server::{ApplicationSink, ApplicationSinkServer};
use hardy_proto::application::*;
use tokio::sync::mpsc::*;
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
impl ApplicationSink for Service {
    #[instrument(skip(self))]
    async fn register_application(
        &self,
        request: Request<RegisterApplicationRequest>,
    ) -> Result<Response<RegisterApplicationResponse>, Status> {
        self.app_registry
            .register(request.into_inner())
            .await
            .map(Response::new)
    }

    #[instrument(skip(self))]
    async fn unregister_application(
        &self,
        request: Request<UnregisterApplicationRequest>,
    ) -> Result<Response<UnregisterApplicationResponse>, Status> {
        self.app_registry
            .unregister(request.into_inner())
            .await
            .map(Response::new)
    }

    #[instrument(skip(self))]
    async fn send(&self, request: Request<SendRequest>) -> Result<Response<SendResponse>, Status> {
        let request = request.into_inner();
        let mut send_request = dispatcher::SendRequest {
            source: self.app_registry.find_by_token(&request.token).await?,
            destination: match request
                .destination
                .parse::<bpv7::Eid>()
                .map_err(|e| Status::from_error(e.into()))?
            {
                bpv7::Eid::Null => {
                    return Err(Status::invalid_argument("Cannot send to Null endpoint"))
                }
                eid => eid,
            },
            data: request.data,
            lifetime: request.lifetime,
            ..Default::default()
        };

        if let Some(flags) = request.flags {
            let mut bundle_flags = bpv7::BundleFlags::default();
            if flags & (send_request::SendFlags::DoNotFragment as u32) != 0 {
                bundle_flags.do_not_fragment = true;
            }
            if flags & (send_request::SendFlags::RequestAck as u32) != 0 {
                bundle_flags.app_ack_requested = true;
            }
            if flags & (send_request::SendFlags::ReportStatusTime as u32) != 0 {
                bundle_flags.report_status_time = true;
            }
            if flags & (send_request::SendFlags::NotifyReception as u32) != 0 {
                bundle_flags.receipt_report_requested = true;
            }
            if flags & (send_request::SendFlags::NotifyForwarding as u32) != 0 {
                bundle_flags.forward_report_requested = true;
            }
            if flags & (send_request::SendFlags::NotifyDelivery as u32) != 0 {
                bundle_flags.delivery_report_requested = true;
            }
            if flags & (send_request::SendFlags::NotifyDeletion as u32) != 0 {
                bundle_flags.delete_report_requested = true;
            }
            send_request.flags = Some(bundle_flags);
        }

        self.dispatcher
            .local_dispatch(send_request)
            .await
            .map(|_| Response::new(SendResponse {}))
            .map_err(Status::from_error)
    }

    #[instrument(skip(self))]
    async fn collect(
        &self,
        request: Request<CollectRequest>,
    ) -> Result<Response<CollectResponse>, Status> {
        let request = request.into_inner();
        let Some(response) = self
            .dispatcher
            .collect(
                self.app_registry.find_by_token(&request.token).await?,
                request.bundle_id,
            )
            .await
            .map_err(Status::from_error)?
        else {
            return Err(Status::not_found("No such bundle"));
        };

        Ok(Response::new(CollectResponse {
            bundle_id: response.bundle_id,
            data: response.data,
            expiry: Some(to_timestamp(response.expiry)),
            ack_requested: response.app_ack_requested,
        }))
    }

    type PollStream = tokio_stream::wrappers::ReceiverStream<Result<PollResponse, Status>>;

    #[instrument(skip(self))]
    async fn poll(
        &self,
        request: Request<PollRequest>,
    ) -> Result<Response<Self::PollStream>, Status> {
        let request = request.into_inner();
        let (tx_inner, mut rx_inner) = channel::<Bundle>(16);
        let (tx_outer, rx_outer) = channel(16);

        // Stream the response
        tokio::spawn(async move {
            while let Some(bundle) = rx_inner.recv().await {
                // Double check that we are returning something valid
                if let BundleStatus::CollectionPending = &bundle.metadata.status {
                    if bundle.has_expired()
                        && tx_outer
                            .send(Ok(PollResponse {
                                bundle_id: bundle.bundle.id.to_key(),
                                expiry: Some(to_timestamp(bundle.expiry())),
                            }))
                            .await
                            .is_err()
                    {
                        break;
                    }
                }
            }
        });

        self.dispatcher
            .poll_for_collection(
                self.app_registry.find_by_token(&request.token).await?,
                tx_inner,
            )
            .await
            .map_err(Status::from_error)
            .map(|_| Response::new(tokio_stream::wrappers::ReceiverStream::new(rx_outer)))
    }
}

pub fn new_service(
    config: &config::Config,
    bpa: Arc<hardy_bpa::bpa::Bpa>,
) -> ApplicationSinkServer<Service> {
    ApplicationSinkServer::new(Service::new(config, bpa))
}

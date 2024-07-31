use super::*;
use application_sink_server::{ApplicationSink, ApplicationSinkServer};
use hardy_proto::application::*;
use tokio::sync::mpsc::*;
use tonic::{Request, Response, Status};

pub struct Service {
    app_registry: app_registry::AppRegistry,
    dispatcher: dispatcher::Dispatcher,
}

impl Service {
    fn new(
        _config: &config::Config,
        app_registry: app_registry::AppRegistry,
        dispatcher: dispatcher::Dispatcher,
    ) -> Self {
        Service {
            app_registry,
            dispatcher,
        }
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
        let (tx, rx) = channel(4);
        let dispatcher = self.dispatcher.clone();

        // Get the items
        let items = dispatcher
            .poll_for_collection(self.app_registry.find_by_token(&request.token).await?)
            .await
            .map_err(Status::from_error)?;

        // Stream the response
        tokio::spawn(async move {
            for (bundle_id, expiry) in items {
                tx.send(Ok(PollResponse {
                    bundle_id,
                    expiry: Some(to_timestamp(expiry)),
                }))
                .await
                .trace_expect("Failed to send collect_all response to stream channel");
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }
}

pub fn new_service(
    config: &config::Config,
    app_registry: app_registry::AppRegistry,
    dispatcher: dispatcher::Dispatcher,
) -> ApplicationSinkServer<Service> {
    ApplicationSinkServer::new(Service::new(config, app_registry, dispatcher))
}

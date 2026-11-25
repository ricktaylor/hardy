use hardy_bpa::service::{
    RegistrarExt,
    raw::{Context, Sink, factory_fn},
};
use hardy_bpv7::eid::Eid;

use super::*;
use proto::service::*;

// The BPA's end of a remote whole-bundle service: it forwards deliveries
// through the proxy standing in for the far service, and the service
// reaches the BPA back through the sink.
struct LowLevelService {
    sink: Arc<dyn Sink>,
    endpoint: Eid,
    proxy: Arc<RpcProxy<Result<BpaToService, tonic::Status>, ServiceToBpa>>,
}

impl LowLevelService {
    async fn call(
        &self,
        msg: bpa_to_service::Msg,
    ) -> hardy_bpa::service::Result<service_to_bpa::Msg> {
        match self.proxy.call(msg).await {
            Ok(None) => Err(hardy_bpa::service::Error::Disconnected),
            Ok(Some(msg)) => Ok(msg),
            Err(e) => Err(hardy_bpa::service::Error::Internal(e.into())),
        }
    }
}

// The far service's end of the session: every request it makes lands on
// the sink its registration handed back.
struct Handler {
    sink: Arc<dyn Sink>,
}

impl Handler {
    async fn send(
        &self,
        mut request: ServiceSendRequest,
    ) -> Result<bpa_to_service::Msg, tonic::Status> {
        // The unary wire message already delivered the whole bundle, so it
        // enters the BPA as a one-segment stream.
        self.sink
            .send(&mut request.data)
            .await
            .map(|bundle_id| {
                bpa_to_service::Msg::Send(SendResponse {
                    bundle_id: bundle_id.to_key(),
                })
            })
            .map_err(|e| tonic::Status::from_error(e.into()))
    }
}

#[async_trait]
impl hardy_bpa::service::raw::Service for LowLevelService {
    async fn unregister(&self) -> hardy_bpa::service::Result<()> {
        self.proxy.shutdown().await;
        Ok(())
    }

    // The wire carries no segmented bundle messages, so a delivery travels
    // as one unary ServiceReceiveRequest and the stream is assembled in
    // memory with `stream::buffer_stream` before marshalling. See
    // bpa/docs/streaming_pipeline_design.md for the chunked form this is a
    // step towards.
    async fn deliver(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
        expiry: time::OffsetDateTime,
        total_len: u64,
        stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::stream::Segment>,
    ) -> hardy_bpa::service::Result<()> {
        let data = hardy_bpa::stream::buffer_stream(stream, total_len).await?;
        match self
            .call(bpa_to_service::Msg::Receive(ServiceReceiveRequest {
                data,
                expiry: Some(to_timestamp(expiry)),
                bundle_id: bundle_id.to_key(),
            }))
            .await?
        {
            service_to_bpa::Msg::Receive(_) => Ok(()),
            msg => {
                warn!("Unexpected response: {msg:?}");
                Err(hardy_bpa::service::Error::Internal(
                    tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
                ))
            }
        }
    }

    async fn report_status(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
        from: &hardy_bpv7::eid::Eid,
        kind: hardy_bpa::service::StatusNotify,
        reason: hardy_bpv7::status_report::ReasonCode,
        timestamp: Option<time::OffsetDateTime>,
    ) {
        match self
            .call(bpa_to_service::Msg::StatusNotify(StatusNotifyRequest {
                bundle_id: bundle_id.to_key(),
                from: from.to_string(),
                kind: match kind {
                    hardy_bpa::service::StatusNotify::Received => {
                        status_notify_request::StatusKind::Received
                    }
                    hardy_bpa::service::StatusNotify::Forwarded => {
                        status_notify_request::StatusKind::Forwarded
                    }
                    hardy_bpa::service::StatusNotify::Delivered => {
                        status_notify_request::StatusKind::Delivered
                    }
                    hardy_bpa::service::StatusNotify::Deleted => {
                        status_notify_request::StatusKind::Deleted
                    }
                }
                .into(),
                reason: reason.into(),
                timestamp: timestamp.map(to_timestamp),
            }))
            .await
        {
            Ok(service_to_bpa::Msg::StatusNotify(_)) => {}
            Ok(msg) => {
                warn!("Unexpected response: {msg:?}");
            }
            Err(e) => {
                warn!("Service refused notification: {e}");
            }
        }
    }
}

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = bpa_to_service::Msg;
    type RMsg = service_to_bpa::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        let msg = match msg {
            service_to_bpa::Msg::Send(msg) => self.send(msg).await,
            _ => {
                warn!("Ignoring unsolicited response: {msg:?}");
                return None;
            }
        };

        match msg {
            Ok(msg) => Some(msg),
            Err(e) => Some(bpa_to_service::Msg::Status(e.into())),
        }
    }

    async fn on_close(&self) {
        // The far service is gone: unregister, which takes the service
        // down with the session.
        self.sink.unregister().await;
    }
}

pub struct GrpcService {
    bpa: Arc<dyn hardy_bpa::service::Registrar>,
    session_tasks: hardy_async::TaskPool,
    channel_size: usize,
}

#[async_trait]
impl service_server::Service for GrpcService {
    type RegisterStream =
        tokio_stream::wrappers::ReceiverStream<Result<BpaToService, tonic::Status>>;

    async fn register(
        &self,
        request: tonic::Request<tonic::Streaming<ServiceToBpa>>,
    ) -> Result<tonic::Response<Self::RegisterStream>, tonic::Status> {
        let (channel_sender, rx) = tokio::sync::mpsc::channel(self.channel_size);
        let channel_receiver = request.into_inner();

        // The handshake and the proxy run on their own task: the response
        // stream has to be returned at once, so the client can start
        // sending.
        let bpa = self.bpa.clone();
        hardy_async::spawn!(self.session_tasks, "service_session", async move {
            run_service_session(channel_sender, channel_receiver, bpa).await;
        });

        Ok(tonic::Response::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
    }
}

async fn run_service_session(
    channel_sender: tokio::sync::mpsc::Sender<Result<BpaToService, tonic::Status>>,
    channel_receiver: tonic::Streaming<ServiceToBpa>,
    bpa: Arc<dyn hardy_bpa::service::Registrar>,
) {
    let (proxy, msg_id, opening) = match RpcProxy::accept(channel_sender, channel_receiver).await {
        Ok(session) => session,
        Err(e) => {
            warn!("Service session failed to open: {e}");
            return;
        }
    };

    match register(&*bpa, opening, &proxy).await {
        Ok(svc) => {
            let response = RegisterResponse {
                endpoint_id: svc.endpoint.to_string(),
            };
            let handler = Handler {
                sink: svc.sink.clone(),
            };
            proxy.open(
                msg_id,
                bpa_to_service::Msg::Register(response),
                Box::new(handler),
            );
        }
        Err(status) => {
            warn!("Service registration failed: {status}");
            proxy.fail(status).await;
        }
    }
}

// Registers the service the opening message asks for, built around the
// proxy the BPA delivers through.
async fn register(
    bpa: &dyn hardy_bpa::service::Registrar,
    opening: service_to_bpa::Msg,
    proxy: &Arc<RpcProxy<Result<BpaToService, tonic::Status>, ServiceToBpa>>,
) -> Result<Arc<LowLevelService>, tonic::Status> {
    let service_to_bpa::Msg::Register(request) = opening else {
        warn!("Service sent incorrect message: {opening:?}");
        return Err(tonic::Status::internal(format!(
            "Unexpected response: {opening:?}"
        )));
    };

    let service_id = request
        .service_id
        .as_ref()
        .map(|service_id| match service_id {
            register_request::ServiceId::Dtn(s) => hardy_bpv7::eid::Service::Dtn(s.clone().into()),
            register_request::ServiceId::Ipn(s) => hardy_bpv7::eid::Service::Ipn(*s),
        })
        .ok_or_else(|| tonic::Status::invalid_argument("service_id is required"))?;

    bpa.register_raw_service(
        Some(service_id),
        factory_fn(|cx: Context| {
            Ok(Arc::new(LowLevelService {
                sink: cx.sink,
                endpoint: cx.endpoint,
                proxy: proxy.clone(),
            }))
        }),
    )
    .await
    .map_err(|e| tonic::Status::from_error(e.into()))
}

pub fn new_endpoint_service(
    bpa: Arc<dyn hardy_bpa::service::Registrar>,
    tasks: &hardy_async::TaskPool,
) -> service_server::ServiceServer<GrpcService> {
    service_server::ServiceServer::new(GrpcService {
        bpa,
        session_tasks: tasks.clone(),
        channel_size: 16,
    })
    .max_encoding_message_size(crate::MAX_MESSAGE_SIZE)
    .max_decoding_message_size(crate::MAX_MESSAGE_SIZE)
}

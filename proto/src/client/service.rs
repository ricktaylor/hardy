// `Sink` is this module's own proxy sink, so the BPA's trait of that
// name is imported anonymously, for its methods alone.
use hardy_bpa::service::raw::{BoxFactory, Context, Service, Sink as _};

use super::*;
use proto::service::*;

async fn receive(
    service: &dyn hardy_bpa::service::raw::Service,
    mut request: ServiceReceiveRequest,
) -> Result<ReceiveResponse, tonic::Status> {
    let expiry = request
        .expiry
        .map(from_timestamp)
        .ok_or(tonic::Status::invalid_argument("Missing expiry"))??;

    let bundle_id = hardy_bpv7::bundle::Id::from_key(&request.bundle_id)
        .map_err(|e| tonic::Status::invalid_argument(format!("Invalid bundle_id: {e}")))?;

    // The unary wire message already delivered the whole bundle, so it
    // reaches the service as a one-segment stream.
    let total_len = request.data.len() as u64;
    service
        .deliver(&bundle_id, expiry, total_len, &mut request.data)
        .await
        .map_err(|e| tonic::Status::from_error(e.into()))?;

    Ok(ReceiveResponse {})
}

async fn status_notify(
    service: &dyn hardy_bpa::service::raw::Service,
    request: StatusNotifyRequest,
) -> Result<(), tonic::Status> {
    let timestamp = request
        .timestamp
        .map(|t| from_timestamp(t).map_err(|e| tonic::Status::from_error(Box::new(e))))
        .transpose()?;

    let kind = match status_notify_request::StatusKind::try_from(request.kind)
        .map_err(|e| tonic::Status::from_error(e.into()))?
    {
        status_notify_request::StatusKind::Unused => {
            warn!("Unused status kind");
            return Err(tonic::Status::invalid_argument("Unused status"));
        }
        status_notify_request::StatusKind::Deleted => hardy_bpa::service::StatusNotify::Deleted,
        status_notify_request::StatusKind::Delivered => hardy_bpa::service::StatusNotify::Delivered,
        status_notify_request::StatusKind::Forwarded => hardy_bpa::service::StatusNotify::Forwarded,
        status_notify_request::StatusKind::Received => hardy_bpa::service::StatusNotify::Received,
    };

    let reason = hardy_bpv7::status_report::ReasonCode::try_from(request.reason)
        .map_err(|e| tonic::Status::from_error(e.into()))?;

    let bundle_id = hardy_bpv7::bundle::Id::from_key(&request.bundle_id)
        .map_err(|e| tonic::Status::invalid_argument(format!("Invalid bundle_id: {e}")))?;

    let from = request
        .from
        .parse::<hardy_bpv7::eid::Eid>()
        .map_err(|e| tonic::Status::invalid_argument(format!("Invalid from EID: {e}")))?;

    service
        .report_status(&bundle_id, &from, kind, reason, timestamp)
        .await;

    Ok(())
}

struct Sink {
    proxy: RpcProxy<ServiceToBpa, BpaToService>,
}

impl Sink {
    async fn call(
        &self,
        msg: service_to_bpa::Msg,
    ) -> hardy_bpa::service::Result<bpa_to_service::Msg> {
        match self.proxy.call(msg).await {
            Ok(None) => Err(hardy_bpa::service::Error::Disconnected),
            Ok(Some(msg)) => Ok(msg),
            Err(e) => Err(hardy_bpa::service::Error::Internal(e.into())),
        }
    }
}

#[async_trait]
impl hardy_bpa::service::raw::Sink for Sink {
    // INTERIM BUFFERING: the wire has no streamed Send message yet, so the
    // segment stream is accumulated (bounded by the transport cap) and sent
    // as one unary ServiceSendRequest. This is a deliberate stepping stone
    // toward the full streaming pipeline (chunked wire messages are the
    // blocker); see bpa/docs/streaming_pipeline_design.md.
    async fn send(
        &self,
        stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::stream::Segment>,
    ) -> hardy_bpa::service::Result<hardy_bpv7::bundle::Id> {
        // The transport cap doubles as the pre-check from
        // `client::application::Sink::send`: an oversized bundle returns a
        // typed error here instead of letting tonic break the gRPC stream,
        // which would cascade into `on_close` and unregister this service.
        let data = hardy_bpa::stream::concat_stream(stream, crate::MAX_PAYLOAD_SIZE)
            .await
            .map_err(|e| match e {
                hardy_bpa::stream::ConcatError::Cancelled => {
                    hardy_bpa::service::Error::StreamCancelled
                }
                hardy_bpa::stream::ConcatError::TooLarge { size, max } => {
                    hardy_bpa::service::Error::PayloadTooLarge {
                        size: size as u64,
                        max: max as u64,
                    }
                }
            })?;
        match self
            .call(service_to_bpa::Msg::Send(ServiceSendRequest { data }))
            .await?
        {
            bpa_to_service::Msg::Send(response) => {
                hardy_bpv7::bundle::Id::from_key(&response.bundle_id)
                    .map_err(|e| hardy_bpa::service::Error::Internal(e.into()))
            }
            msg => {
                warn!("Unexpected response: {msg:?}");
                Err(hardy_bpa::service::Error::Internal(
                    tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
                ))
            }
        }
    }

    async fn unregister(&self) {
        self.proxy.shutdown().await;
    }
}

// The service the session owns, shared with the proxy's handler.
//
// The service is built from a context carrying the sink the proxy stands
// behind, so the proxy exists first and learns the service it serves
// here. The handler holds the only strong reference: the session owns
// the service it registered, and drops it on close.
type SessionService = Arc<Mutex<Option<Arc<dyn Service>>>>;

struct Handler {
    service: SessionService,
}

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = service_to_bpa::Msg;
    type RMsg = bpa_to_service::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        match msg {
            bpa_to_service::Msg::Receive(request) => {
                let service = self.service.lock().clone();
                if let Some(service) = service {
                    match receive(service.as_ref(), request).await {
                        Ok(msg) => Some(service_to_bpa::Msg::Receive(msg)),
                        Err(e) => Some(service_to_bpa::Msg::Status(e.into())),
                    }
                } else {
                    Some(service_to_bpa::Msg::Status(
                        tonic::Status::unavailable("Service has disconnected").into(),
                    ))
                }
            }
            bpa_to_service::Msg::StatusNotify(request) => {
                let service = self.service.lock().clone();
                if let Some(service) = service {
                    match status_notify(service.as_ref(), request).await {
                        Ok(_) => Some(service_to_bpa::Msg::StatusNotify(StatusNotifyResponse {})),
                        Err(e) => Some(service_to_bpa::Msg::Status(e.into())),
                    }
                } else {
                    Some(service_to_bpa::Msg::Status(
                        tonic::Status::unavailable("Service has disconnected").into(),
                    ))
                }
            }
            _ => {
                warn!("Ignoring unsolicited response: {msg:?}");
                None
            }
        }
    }

    async fn on_close(&self) {
        let service = self.service.lock().take();
        if let Some(service) = service
            && let Err(e) = service.unregister().await
        {
            warn!("Service unregister failed: {e}");
        }
    }
}

/// Register a low-level Service with the BPA via gRPC.
///
/// This connects to the BPA's Service gRPC endpoint and establishes a
/// bidirectional streaming connection for the service lifecycle.
pub async fn register_raw_service(
    grpc_addr: String,
    service_id: Option<eid::Service>,
    factory: BoxFactory<'_>,
) -> hardy_bpa::service::Result<()> {
    let mut svc_client = service_client::ServiceClient::connect(grpc_addr.clone())
        .await
        .map_err(|e| {
            error!("Failed to connect to gRPC server '{grpc_addr}': {e}");
            hardy_bpa::service::Error::Internal(e.into())
        })?
        .max_encoding_message_size(crate::MAX_MESSAGE_SIZE)
        .max_decoding_message_size(crate::MAX_MESSAGE_SIZE);

    // Create a channel for sending messages to the service.
    let (mut channel_sender, rx) = tokio::sync::mpsc::channel(16);

    // Call the service's streaming method
    let mut channel_receiver = svc_client
        .register(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .map_err(|e| {
            error!("Service Registration failed: {e}");
            hardy_bpa::service::Error::Internal(e.into())
        })?
        .into_inner();

    // Send the initial registration message.
    let response = match RpcProxy::send(
        &mut channel_sender,
        &mut channel_receiver,
        service_to_bpa::Msg::Register(RegisterRequest {
            service_id: service_id.map(|service_id| match service_id {
                eid::Service::Ipn(service_number) => {
                    register_request::ServiceId::Ipn(service_number)
                }
                eid::Service::Dtn(service_name) => {
                    register_request::ServiceId::Dtn(service_name.into())
                }
            }),
        }),
    )
    .await
    .map_err(|e| {
        error!("Failed to send registration: {e}");
        hardy_bpa::service::Error::Internal(e.into())
    })? {
        None => return Err(hardy_bpa::service::Error::Disconnected),
        Some(bpa_to_service::Msg::Register(response)) => response,
        Some(msg) => {
            error!("Service Registration failed: Unexpected response: {msg:?}");
            return Err(hardy_bpa::service::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            ));
        }
    };

    let eid: eid::Eid = response
        .endpoint_id
        .parse()
        .map_err(|e: hardy_bpv7::eid::Error| {
            warn!("Failed to parse EID in response: {e}");
            hardy_bpa::service::Error::Internal(e.into())
        })?;

    let session: SessionService = Arc::new(Mutex::new(None));
    let proxy = RpcProxy::run(
        channel_sender,
        channel_receiver,
        Box::new(Handler {
            service: session.clone(),
        }),
        Side::Client,
    );
    let sink = Arc::new(Sink { proxy });

    // The service runs against the proxy from here, so a build or
    // registration failure takes the session down with it.
    let registered = async {
        let service = factory.build(Context::new(sink.clone(), eid.clone()))?;
        service.register().await?;
        Ok(service)
    }
    .await;

    match registered {
        Ok(service) => {
            *session.lock() = Some(service);
            info!("Proxy Service {eid} registered");
            Ok(())
        }
        Err(e) => {
            sink.unregister().await;
            Err(e)
        }
    }
}

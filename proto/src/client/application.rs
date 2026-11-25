// `Sink` is this module's own proxy sink, so the BPA's trait of that
// name is imported anonymously, for its methods alone.
use hardy_bpa::service::{BoxFactory, Context, Service, Sink as _};

use super::*;
use proto::service::*;

async fn receive(
    service: &dyn hardy_bpa::service::Service,
    mut request: AppReceiveRequest,
) -> Result<ReceiveResponse, tonic::Status> {
    let expiry = request
        .expiry
        .map(from_timestamp)
        .ok_or(tonic::Status::invalid_argument("Missing expiry"))?
        .map_err(|e| tonic::Status::from_error(e.into()))?;

    let bundle_id = hardy_bpv7::bundle::Id::from_key(&request.bundle_id)
        .map_err(|e| tonic::Status::invalid_argument(format!("Invalid bundle_id: {e}")))?;

    // The unary wire message already delivered the whole payload, so it
    // reaches the application as a one-segment stream.
    let total_len = request.payload.len() as u64;
    service
        .deliver(
            &bundle_id,
            expiry,
            request.ack_requested,
            total_len,
            &mut request.payload,
        )
        .await
        .map_err(|e| tonic::Status::from_error(e.into()))?;

    Ok(ReceiveResponse {})
}

async fn status_notify(
    service: &dyn hardy_bpa::service::Service,
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
    proxy: RpcProxy<AppToBpa, BpaToApp>,
}

impl Sink {
    async fn call(&self, msg: app_to_bpa::Msg) -> hardy_bpa::service::Result<bpa_to_app::Msg> {
        match self.proxy.call(msg).await {
            Ok(None) => Err(hardy_bpa::service::Error::Disconnected),
            Ok(Some(msg)) => Ok(msg),
            Err(e) => Err(hardy_bpa::service::Error::Internal(e.into())),
        }
    }
}

#[async_trait]
impl hardy_bpa::service::Sink for Sink {
    async fn send(
        &self,
        destination: eid::Eid,
        data: hardy_bpa::Bytes,
        lifetime: std::time::Duration,
        options: Option<hardy_bpa::service::SendOptions>,
    ) -> hardy_bpa::service::Result<hardy_bpv7::bundle::Id> {
        // Pre-check size so an oversized payload returns a typed error
        // instead of letting tonic break the stream during encoding,
        // which would cascade into `on_close` and unregister this
        // application.
        if data.len() > crate::MAX_PAYLOAD_SIZE {
            return Err(hardy_bpa::service::Error::PayloadTooLarge {
                size: data.len() as u64,
                max: crate::MAX_PAYLOAD_SIZE as u64,
            });
        }
        match self
            .call(app_to_bpa::Msg::Send(AppSendRequest {
                destination: destination.to_string(),
                payload: data,
                lifetime: lifetime.as_millis() as u64,
                options: options.map(|o| {
                    let mut options = 0;
                    if o.do_not_fragment {
                        options |= app_send_request::SendOptions::DoNotFragment as u32;
                    }
                    if o.request_ack {
                        options |= app_send_request::SendOptions::RequestAck as u32;
                    }
                    if o.notify_reception {
                        options |= app_send_request::SendOptions::NotifyReception as u32;
                        if o.report_status_time {
                            options |= app_send_request::SendOptions::ReportStatusTime as u32;
                        }
                    }
                    if o.notify_forwarding {
                        options |= app_send_request::SendOptions::NotifyForwarding as u32;
                        if o.report_status_time {
                            options |= app_send_request::SendOptions::ReportStatusTime as u32;
                        }
                    }
                    if o.notify_delivery {
                        options |= app_send_request::SendOptions::NotifyDelivery as u32;
                        if o.report_status_time {
                            options |= app_send_request::SendOptions::ReportStatusTime as u32;
                        }
                    }
                    if o.notify_deletion {
                        options |= app_send_request::SendOptions::NotifyDeletion as u32;
                        if o.report_status_time {
                            options |= app_send_request::SendOptions::ReportStatusTime as u32;
                        }
                    }
                    options
                }),
            }))
            .await?
        {
            bpa_to_app::Msg::Send(response) => {
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

// The application the session owns, shared with the proxy's handler.
//
// The application is built from a context carrying the sink the proxy
// stands behind, so the proxy exists first and learns the application it
// serves here. The handler holds the only strong reference: the session
// owns the application it registered, and drops it on close.
type SessionService = Arc<Mutex<Option<Arc<dyn Service>>>>;

struct Handler {
    service: SessionService,
}

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = app_to_bpa::Msg;
    type RMsg = bpa_to_app::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        match msg {
            bpa_to_app::Msg::Receive(request) => {
                let service = self.service.lock().clone();
                if let Some(service) = service {
                    match receive(service.as_ref(), request).await {
                        Ok(msg) => Some(app_to_bpa::Msg::Receive(msg)),
                        Err(e) => Some(app_to_bpa::Msg::Status(e.into())),
                    }
                } else {
                    Some(app_to_bpa::Msg::Status(
                        tonic::Status::unavailable("Service has disconnected").into(),
                    ))
                }
            }
            bpa_to_app::Msg::StatusNotify(request) => {
                let service = self.service.lock().clone();
                if let Some(service) = service {
                    match status_notify(service.as_ref(), request).await {
                        Ok(_) => Some(app_to_bpa::Msg::StatusNotify(StatusNotifyResponse {})),
                        Err(e) => Some(app_to_bpa::Msg::Status(e.into())),
                    }
                } else {
                    Some(app_to_bpa::Msg::Status(
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
            warn!("Application unregister failed: {e}");
        }
    }
}

pub async fn register_service(
    grpc_addr: String,
    service_id: Option<eid::Service>,
    factory: BoxFactory<'_>,
) -> hardy_bpa::service::Result<()> {
    let mut app_client = application_client::ApplicationClient::connect(grpc_addr.clone())
        .await
        .map_err(|e| {
            error!("Failed to connect to gRPC server '{grpc_addr}': {e}");
            hardy_bpa::service::Error::Internal(e.into())
        })?
        .max_encoding_message_size(crate::MAX_MESSAGE_SIZE)
        .max_decoding_message_size(crate::MAX_MESSAGE_SIZE);

    let (mut channel_sender, rx) = tokio::sync::mpsc::channel(16);

    let mut channel_receiver = app_client
        .register(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .map_err(|e| {
            error!("Service Registration failed: {e}");
            hardy_bpa::service::Error::Internal(e.into())
        })?
        .into_inner();

    let response = match RpcProxy::send(
        &mut channel_sender,
        &mut channel_receiver,
        app_to_bpa::Msg::Register(RegisterRequest {
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
        Some(bpa_to_app::Msg::Register(response)) => response,
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

    // The application runs against the proxy from here, so a build or
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
            info!("Proxy Application service {eid} registered");
            Ok(())
        }
        Err(e) => {
            sink.unregister().await;
            Err(e)
        }
    }
}

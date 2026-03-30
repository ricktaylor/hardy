use super::*;
use proto::service::*;

async fn receive(
    service: &dyn hardy_bpa::services::Application,
    request: AppReceiveRequest,
) -> Result<ReceiveResponse, tonic::Status> {
    let source = request
        .source
        .parse::<hardy_bpv7::eid::Eid>()
        .map_err(|e| tonic::Status::from_error(e.into()))?;

    let expiry = request
        .expiry
        .map(from_timestamp)
        .ok_or(tonic::Status::invalid_argument("Missing expiry"))?
        .map_err(|e| tonic::Status::from_error(e.into()))?;

    service
        .on_receive(source, expiry, request.ack_requested, request.payload)
        .await;

    Ok(ReceiveResponse {})
}

async fn status_notify(
    service: &dyn hardy_bpa::services::Application,
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
        status_notify_request::StatusKind::Deleted => hardy_bpa::services::StatusNotify::Deleted,
        status_notify_request::StatusKind::Delivered => {
            hardy_bpa::services::StatusNotify::Delivered
        }
        status_notify_request::StatusKind::Forwarded => {
            hardy_bpa::services::StatusNotify::Forwarded
        }
        status_notify_request::StatusKind::Received => hardy_bpa::services::StatusNotify::Received,
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
        .on_status_notify(&bundle_id, &from, kind, reason, timestamp)
        .await;

    Ok(())
}

struct Sink {
    proxy: RpcProxy<AppToBpa, BpaToApp>,
}

impl Sink {
    async fn call(&self, msg: app_to_bpa::Msg) -> hardy_bpa::services::Result<bpa_to_app::Msg> {
        match self.proxy.call(msg).await {
            Ok(None) => Err(hardy_bpa::services::Error::Disconnected),
            Ok(Some(msg)) => Ok(msg),
            Err(e) => Err(hardy_bpa::services::Error::Internal(e.into())),
        }
    }
}

#[async_trait]
impl hardy_bpa::services::ApplicationSink for Sink {
    async fn send(
        &self,
        destination: eid::Eid,
        data: hardy_bpa::Bytes,
        lifetime: std::time::Duration,
        options: Option<hardy_bpa::services::SendOptions>,
    ) -> hardy_bpa::services::Result<hardy_bpv7::bundle::Id> {
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
                    .map_err(|e| hardy_bpa::services::Error::Internal(e.into()))
            }
            msg => {
                warn!("Unexpected response: {msg:?}");
                Err(hardy_bpa::services::Error::Internal(
                    tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
                ))
            }
        }
    }

    async fn cancel(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> hardy_bpa::services::Result<bool> {
        match self
            .call(app_to_bpa::Msg::Cancel(CancelRequest {
                bundle_id: bundle_id.to_key(),
            }))
            .await?
        {
            bpa_to_app::Msg::Cancel(response) => Ok(response.cancelled),
            msg => {
                warn!("Unexpected response: {msg:?}");
                Err(hardy_bpa::services::Error::Internal(
                    tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
                ))
            }
        }
    }

    async fn unregister(&self) {
        match self
            .call(app_to_bpa::Msg::Unregister(UnregisterRequest {}))
            .await
        {
            Ok(bpa_to_app::Msg::Unregister(_)) => {}
            Ok(msg) => {
                warn!("Unexpected response: {msg:?}");
            }
            Err(e) => {
                warn!("Failed to request unregistration: {e}");
            }
        }

        self.proxy.close().await;
    }
}

struct Handler {
    service: Weak<dyn hardy_bpa::services::Application>,
}

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = app_to_bpa::Msg;
    type RMsg = bpa_to_app::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        match msg {
            bpa_to_app::Msg::Receive(request) => {
                if let Some(service) = self.service.upgrade() {
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
                if let Some(service) = self.service.upgrade() {
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
            bpa_to_app::Msg::OnUnregister(_) => {
                if let Some(service) = self.service.upgrade() {
                    service.on_unregister().await;
                }
                Some(app_to_bpa::Msg::OnUnregister(OnUnregisterResponse {}))
            }
            _ => {
                warn!("Ignoring unsolicited response: {msg:?}");
                None
            }
        }
    }

    async fn on_close(&self) {
        if let Some(service) = self.service.upgrade() {
            service.on_unregister().await;
        }
    }
}

pub async fn register_application_service(
    grpc_addr: String,
    service_id: Option<eid::Service>,
    service: Arc<dyn hardy_bpa::services::Application>,
) -> hardy_bpa::services::Result<eid::Eid> {
    let mut app_client = application_client::ApplicationClient::connect(grpc_addr.clone())
        .await
        .map_err(|e| {
            error!("Failed to connect to gRPC server '{grpc_addr}': {e}");
            hardy_bpa::services::Error::Internal(e.into())
        })?;

    // Create a channel for sending messages to the service.
    let (mut channel_sender, rx) = tokio::sync::mpsc::channel(16);

    // Call the service's streaming method
    let mut channel_receiver = app_client
        .register(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .map_err(|e| {
            error!("Service Registration failed: {e}");
            hardy_bpa::services::Error::Internal(e.into())
        })?
        .into_inner();

    // Send the initial registration message.
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
        hardy_bpa::services::Error::Internal(e.into())
    })? {
        None => return Err(hardy_bpa::services::Error::Disconnected),
        Some(bpa_to_app::Msg::Register(response)) => response,
        Some(msg) => {
            error!("Service Registration failed: Unexpected response: {msg:?}");
            return Err(hardy_bpa::services::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            ));
        }
    };

    let eid = response
        .endpoint_id
        .parse()
        .map_err(|e: hardy_bpv7::eid::Error| {
            warn!("Failed to parse EID in response: {e}");
            hardy_bpa::services::Error::Internal(e.into())
        })?;

    // Now we have got here, we can create a Sink proxy and call on_register()
    let handler = Box::new(Handler {
        service: Arc::downgrade(&service),
    });

    // Start the proxy
    let proxy = RpcProxy::run(channel_sender, channel_receiver, handler);

    // Call on_register()
    service.on_register(&eid, Box::new(Sink { proxy })).await;

    info!("Proxy Application service {eid} started");
    Ok(eid)
}

use super::*;
use crate::service::*;
use hardy_bpv7::eid;

async fn receive(
    service: &dyn hardy_bpa::services::Service,
    request: ServiceReceiveRequest,
) -> Result<ReceiveResponse, tonic::Status> {
    let expiry = request
        .expiry
        .map(from_timestamp)
        .ok_or(tonic::Status::invalid_argument("Missing expiry"))??;

    service.on_receive(request.data, expiry).await;

    Ok(ReceiveResponse {})
}

async fn status_notify(
    service: &dyn hardy_bpa::services::Service,
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
    proxy: RpcProxy<ServiceToBpa, BpaToService>,
}

impl Sink {
    async fn call(
        &self,
        msg: service_to_bpa::Msg,
    ) -> hardy_bpa::services::Result<bpa_to_service::Msg> {
        match self.proxy.call(msg).await {
            Ok(None) => Err(hardy_bpa::services::Error::Disconnected),
            Ok(Some(msg)) => Ok(msg),
            Err(e) => Err(hardy_bpa::services::Error::Internal(e.into())),
        }
    }
}

#[async_trait]
impl hardy_bpa::services::ServiceSink for Sink {
    async fn send(
        &self,
        data: hardy_bpa::Bytes,
    ) -> hardy_bpa::services::Result<hardy_bpv7::bundle::Id> {
        match self
            .call(service_to_bpa::Msg::Send(ServiceSendRequest { data }))
            .await?
        {
            bpa_to_service::Msg::Send(response) => {
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
            .call(service_to_bpa::Msg::Cancel(CancelRequest {
                bundle_id: bundle_id.to_key(),
            }))
            .await?
        {
            bpa_to_service::Msg::Cancel(response) => Ok(response.cancelled),
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
            .call(service_to_bpa::Msg::Unregister(UnregisterRequest {}))
            .await
        {
            Ok(bpa_to_service::Msg::Unregister(_)) => {}
            Ok(msg) => {
                warn!("Unexpected response: {msg:?}");
            }
            Err(e) => {
                error!("Failed to request unregistration: {e}");
            }
        }

        self.proxy.close().await;
    }
}

struct Handler {
    service: Weak<dyn hardy_bpa::services::Service>,
}

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = service_to_bpa::Msg;
    type RMsg = bpa_to_service::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        match msg {
            bpa_to_service::Msg::Receive(request) => {
                if let Some(service) = self.service.upgrade() {
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
                if let Some(service) = self.service.upgrade() {
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
            bpa_to_service::Msg::OnUnregister(_) => {
                if let Some(service) = self.service.upgrade() {
                    service.on_unregister().await;
                }
                Some(service_to_bpa::Msg::OnUnregister(OnUnregisterResponse {}))
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

/// Register a low-level Service with the BPA via gRPC.
///
/// This connects to the BPA's Service gRPC endpoint and establishes a
/// bidirectional streaming connection for the service lifecycle.
pub async fn register_endpoint_service(
    grpc_addr: String,
    service_id: Option<eid::Service>,
    service: Arc<dyn hardy_bpa::services::Service>,
) -> hardy_bpa::services::Result<eid::Eid> {
    let mut svc_client = service_client::ServiceClient::connect(grpc_addr.clone())
        .await
        .map_err(|e| {
            error!("Failed to connect to gRPC server '{grpc_addr}': {e}");
            hardy_bpa::services::Error::Internal(e.into())
        })?;

    // Create a channel for sending messages to the service.
    let (mut channel_sender, rx) = tokio::sync::mpsc::channel(16);

    // Call the service's streaming method
    let mut channel_receiver = svc_client
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
        hardy_bpa::services::Error::Internal(e.into())
    })? {
        None => return Err(hardy_bpa::services::Error::Disconnected),
        Some(bpa_to_service::Msg::Register(response)) => response,
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

    info!("Proxy Service {eid} started");
    Ok(eid)
}

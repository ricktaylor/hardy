use std::sync::{Arc, Weak};

use hardy_async::{CancellationToken, async_trait};
use hardy_bpa::services::{self, Service, ServiceContext, StatusNotify, context::ServiceOp};
use hardy_bpv7::bundle::Id as BundleId;
use hardy_bpv7::eid::{self, Eid};
use hardy_bpv7::status_report::ReasonCode;
use tracing::{error, info, warn};

use crate::proto::service::{
    CancelRequest, ReceiveResponse, RegisterRequest, ServiceReceiveRequest, ServiceSendRequest,
    StatusNotifyRequest, StatusNotifyResponse, bpa_to_service, register_request, service_client,
    service_to_bpa, status_notify_request,
};
use crate::proxy::{ProxyHandler, RpcProxy};

async fn receive(
    service: &dyn Service,
    request: ServiceReceiveRequest,
) -> Result<ReceiveResponse, tonic::Status> {
    let expiry = request
        .expiry
        .map(super::from_timestamp)
        .ok_or(tonic::Status::invalid_argument("Missing expiry"))??;

    service.on_receive(request.data, expiry).await;

    Ok(ReceiveResponse {})
}

async fn status_notify(
    service: &dyn Service,
    request: StatusNotifyRequest,
) -> Result<(), tonic::Status> {
    let timestamp = request
        .timestamp
        .map(|t| super::from_timestamp(t).map_err(|e| tonic::Status::from_error(Box::new(e))))
        .transpose()?;

    let kind = match status_notify_request::StatusKind::try_from(request.kind)
        .map_err(|e| tonic::Status::from_error(e.into()))?
    {
        status_notify_request::StatusKind::Unused => {
            warn!("Unused status kind");
            return Err(tonic::Status::invalid_argument("Unused status"));
        }
        status_notify_request::StatusKind::Deleted => StatusNotify::Deleted,
        status_notify_request::StatusKind::Delivered => StatusNotify::Delivered,
        status_notify_request::StatusKind::Forwarded => StatusNotify::Forwarded,
        status_notify_request::StatusKind::Received => StatusNotify::Received,
    };

    let reason =
        ReasonCode::try_from(request.reason).map_err(|e| tonic::Status::from_error(e.into()))?;

    let bundle_id = BundleId::from_key(&request.bundle_id)
        .map_err(|e| tonic::Status::invalid_argument(format!("Invalid bundle_id: {e}")))?;

    let from = request
        .from
        .parse::<Eid>()
        .map_err(|e| tonic::Status::invalid_argument(format!("Invalid from EID: {e}")))?;

    service
        .on_status_notify(&bundle_id, &from, kind, reason, timestamp)
        .await;

    Ok(())
}

struct Handler {
    service: Weak<dyn Service>,
    shutdown: CancellationToken,
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
            _ => {
                warn!("Ignoring unsolicited response: {msg:?}");
                None
            }
        }
    }

    async fn on_close(&self) {
        self.shutdown.cancel();
        if let Some(service) = self.service.upgrade() {
            service.on_unregister().await;
        }
    }
}

pub async fn register_endpoint_service(
    grpc_addr: String,
    service_id: Option<eid::Service>,
    service: Arc<dyn Service>,
) -> services::Result<Eid> {
    let mut svc_client = service_client::ServiceClient::connect(grpc_addr.clone())
        .await
        .map_err(|e| {
            error!("Failed to connect to gRPC server '{grpc_addr}': {e}");
            services::Error::Internal(e.into())
        })?;

    let (mut channel_sender, rx) = tokio::sync::mpsc::channel(16);

    let mut channel_receiver = svc_client
        .register(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .map_err(|e| {
            error!("Service Registration failed: {e}");
            services::Error::Internal(e.into())
        })?
        .into_inner();

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
        services::Error::Internal(e.into())
    })? {
        None => return Err(services::Error::Disconnected),
        Some(bpa_to_service::Msg::Register(response)) => response,
        Some(msg) => {
            error!("Service Registration failed: Unexpected response: {msg:?}");
            return Err(services::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            ));
        }
    };

    let eid: Eid = response.endpoint_id.parse().map_err(|e: eid::Error| {
        warn!("Failed to parse EID in response: {e}");
        services::Error::Internal(e.into())
    })?;

    let (ops_tx, ops_rx) = flume::unbounded();
    let shutdown = CancellationToken::new();
    let ctx = ServiceContext::new(ops_tx, eid.clone(), shutdown.clone());

    let handler = Box::new(Handler {
        service: Arc::downgrade(&service),
        shutdown,
    });

    let proxy = Arc::new(RpcProxy::run(channel_sender, channel_receiver, handler));

    // Bridge ServiceOps to gRPC calls
    let proxy_clone = proxy.clone();
    tokio::spawn(async move {
        while let Ok(op) = ops_rx.recv_async().await {
            match op {
                ServiceOp::SendRaw { data, reply } => {
                    let result = match proxy_clone
                        .call(service_to_bpa::Msg::Send(ServiceSendRequest { data }))
                        .await
                    {
                        Ok(Some(bpa_to_service::Msg::Send(response))) => {
                            BundleId::from_key(&response.bundle_id)
                                .map_err(|e| services::Error::Internal(e.into()))
                        }
                        Ok(None) => Err(services::Error::Disconnected),
                        Ok(Some(_)) => Err(services::Error::Internal("Unexpected response".into())),
                        Err(e) => Err(services::Error::Internal(e.into())),
                    };
                    let _ = reply.send(result);
                }
                ServiceOp::Send { reply, .. } => {
                    let _ = reply.send(Err(services::Error::Internal(
                        "low-level service proxy does not support high-level Send".into(),
                    )));
                }
                ServiceOp::Cancel { bundle_id, reply } => {
                    let result = match proxy_clone
                        .call(service_to_bpa::Msg::Cancel(CancelRequest {
                            bundle_id: bundle_id.to_key(),
                        }))
                        .await
                    {
                        Ok(Some(bpa_to_service::Msg::Cancel(response))) => Ok(response.cancelled),
                        Ok(None) => Err(services::Error::Disconnected),
                        Ok(Some(_)) => Err(services::Error::Internal("Unexpected response".into())),
                        Err(e) => Err(services::Error::Internal(e.into())),
                    };
                    let _ = reply.send(result);
                }
            }
        }
    });

    service.on_register(&eid, ctx).await;

    info!("Proxy Service {eid} started");
    Ok(eid)
}

use std::sync::{Arc, Weak};

use hardy_async::{CancellationToken, async_trait};
use hardy_bpa::services::{self, Application, ServiceContext, StatusNotify, context::ServiceOp};
use hardy_bpv7::bundle::Id as BundleId;
use hardy_bpv7::eid::{self, Eid};
use hardy_bpv7::status_report::ReasonCode;
use tracing::{error, info, warn};

use crate::proto::service::{
    AppReceiveRequest, AppSendRequest, CancelRequest, ReceiveResponse, RegisterRequest,
    StatusNotifyRequest, StatusNotifyResponse, app_send_request, app_to_bpa, application_client,
    bpa_to_app, register_request, status_notify_request,
};
use crate::proxy::{ProxyHandler, RpcProxy};

async fn receive(
    service: &dyn Application,
    request: AppReceiveRequest,
) -> Result<ReceiveResponse, tonic::Status> {
    let source = request
        .source
        .parse::<Eid>()
        .map_err(|e| tonic::Status::from_error(e.into()))?;

    let expiry = request
        .expiry
        .map(super::from_timestamp)
        .ok_or(tonic::Status::invalid_argument("Missing expiry"))?
        .map_err(|e| tonic::Status::from_error(e.into()))?;

    service
        .on_receive(source, expiry, request.ack_requested, request.payload)
        .await;

    Ok(ReceiveResponse {})
}

async fn status_notify(
    service: &dyn Application,
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
    service: Weak<dyn Application>,
    shutdown: CancellationToken,
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

fn encode_send_options(o: services::SendOptions) -> u32 {
    let mut options = 0u32;
    if o.do_not_fragment {
        options |= app_send_request::SendOptions::DoNotFragment as u32;
    }
    if o.request_ack {
        options |= app_send_request::SendOptions::RequestAck as u32;
    }
    if o.report_status_time {
        options |= app_send_request::SendOptions::ReportStatusTime as u32;
    }
    if o.notify_reception {
        options |= app_send_request::SendOptions::NotifyReception as u32;
    }
    if o.notify_forwarding {
        options |= app_send_request::SendOptions::NotifyForwarding as u32;
    }
    if o.notify_delivery {
        options |= app_send_request::SendOptions::NotifyDelivery as u32;
    }
    if o.notify_deletion {
        options |= app_send_request::SendOptions::NotifyDeletion as u32;
    }
    options
}

pub async fn register_application_service(
    grpc_addr: String,
    service_id: Option<eid::Service>,
    service: Arc<dyn Application>,
) -> services::Result<Eid> {
    let mut app_client = application_client::ApplicationClient::connect(grpc_addr.clone())
        .await
        .map_err(|e| {
            error!("Failed to connect to gRPC server '{grpc_addr}': {e}");
            services::Error::Internal(e.into())
        })?;

    let (mut channel_sender, rx) = tokio::sync::mpsc::channel(16);

    let mut channel_receiver = app_client
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
        services::Error::Internal(e.into())
    })? {
        None => return Err(services::Error::Disconnected),
        Some(bpa_to_app::Msg::Register(response)) => response,
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
                ServiceOp::SendRaw { reply, .. } => {
                    let _ = reply.send(Err(services::Error::Internal(
                        "application proxy does not support raw Send".into(),
                    )));
                }
                ServiceOp::Send {
                    destination,
                    data,
                    lifetime,
                    options,
                    reply,
                } => {
                    let result = match proxy_clone
                        .call(app_to_bpa::Msg::Send(AppSendRequest {
                            destination: destination.to_string(),
                            payload: data,
                            lifetime: lifetime.as_millis() as u64,
                            options: options.map(encode_send_options),
                        }))
                        .await
                    {
                        Ok(Some(bpa_to_app::Msg::Send(response))) => {
                            BundleId::from_key(&response.bundle_id)
                                .map_err(|e| services::Error::Internal(e.into()))
                        }
                        Ok(None) => Err(services::Error::Disconnected),
                        Ok(Some(_)) => Err(services::Error::Internal("Unexpected response".into())),
                        Err(e) => Err(services::Error::Internal(e.into())),
                    };
                    let _ = reply.send(result);
                }
                ServiceOp::Cancel { bundle_id, reply } => {
                    let result = match proxy_clone
                        .call(app_to_bpa::Msg::Cancel(CancelRequest {
                            bundle_id: bundle_id.to_key(),
                        }))
                        .await
                    {
                        Ok(Some(bpa_to_app::Msg::Cancel(response))) => Ok(response.cancelled),
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

    info!("Proxy Application service {eid} started");
    Ok(eid)
}

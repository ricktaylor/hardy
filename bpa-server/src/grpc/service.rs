use super::*;
use hardy_bpa::async_trait;
use hardy_proto::{proxy::*, service::*, to_timestamp};

struct LowLevelServiceInner {
    sink: Box<dyn hardy_bpa::services::ServiceSink>,
}

struct LowLevelService {
    inner: spin::once::Once<LowLevelServiceInner>,
    proxy: spin::once::Once<RpcProxy<Result<BpaToService, tonic::Status>, ServiceToBpa>>,
}

impl LowLevelService {
    async fn call(
        &self,
        msg: bpa_to_service::Msg,
    ) -> hardy_bpa::services::Result<service_to_bpa::Msg> {
        let proxy = self.proxy.get().ok_or_else(|| {
            error!("call made before on_register!");
            hardy_bpa::services::Error::Disconnected
        })?;

        match proxy.call(msg).await {
            Ok(None) => Err(hardy_bpa::services::Error::Disconnected),
            Ok(Some(msg)) => Ok(msg),
            Err(e) => Err(hardy_bpa::services::Error::Internal(e.into())),
        }
    }

    async fn send(
        &self,
        request: ServiceSendRequest,
    ) -> Result<bpa_to_service::Msg, tonic::Status> {
        self.inner
            .get()
            .ok_or(tonic::Status::internal("on_register not called"))?
            .sink
            .send(request.data)
            .await
            .map(|bundle_id| {
                bpa_to_service::Msg::Send(SendResponse {
                    bundle_id: bundle_id.to_key(),
                })
            })
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    async fn cancel(&self, request: CancelRequest) -> Result<bpa_to_service::Msg, tonic::Status> {
        let bundle_id = hardy_bpv7::bundle::Id::from_key(&request.bundle_id)
            .map_err(|e| tonic::Status::invalid_argument(format!("Invalid bundle_id: {e}")))?;
        self.inner
            .get()
            .ok_or(tonic::Status::internal("on_register not called"))?
            .sink
            .cancel(&bundle_id)
            .await
            .map(|cancelled| bpa_to_service::Msg::Cancel(CancelResponse { cancelled }))
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    async fn unregister(&self) {
        if let Some(inner) = self.inner.get() {
            inner.sink.unregister().await
        }
    }
}

#[async_trait]
impl hardy_bpa::services::Service for LowLevelService {
    async fn on_register(
        &self,
        _endpoint: &hardy_bpv7::eid::Eid,
        sink: Box<dyn hardy_bpa::services::ServiceSink>,
    ) {
        // Ensure single initialization
        self.inner.call_once(|| LowLevelServiceInner { sink });
    }

    async fn on_unregister(&self) {
        match self
            .call(bpa_to_service::Msg::Unregister(UnregisterResponse {}))
            .await
        {
            Ok(service_to_bpa::Msg::Unregister(_)) => {}
            Ok(msg) => {
                warn!("Unexpected response: {msg:?}");
            }
            Err(e) => {
                error!("Failed to notify service of unregistration: {e}");
            }
        }

        // Close the proxy, nothing else is going to be processed
        if let Some(proxy) = self.proxy.get() {
            proxy.close().await;
        }
    }

    async fn on_receive(&self, data: hardy_bpa::Bytes, expiry: time::OffsetDateTime) {
        match self
            .call(bpa_to_service::Msg::Receive(ServiceReceiveRequest {
                data,
                expiry: Some(to_timestamp(expiry)),
            }))
            .await
        {
            Ok(service_to_bpa::Msg::Receive(_)) => {}
            Ok(msg) => {
                warn!("Unexpected response: {msg:?}");
            }
            Err(e) => {
                warn!("Service refused notification: {e}");
            }
        }
    }

    async fn on_status_notify(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
        from: &hardy_bpv7::eid::Eid,
        kind: hardy_bpa::services::StatusNotify,
        reason: hardy_bpv7::status_report::ReasonCode,
        timestamp: Option<time::OffsetDateTime>,
    ) {
        match self
            .call(bpa_to_service::Msg::StatusNotify(StatusNotifyRequest {
                bundle_id: bundle_id.to_key(),
                from: from.to_string(),
                kind: match kind {
                    hardy_bpa::services::StatusNotify::Received => {
                        status_notify_request::StatusKind::Received
                    }
                    hardy_bpa::services::StatusNotify::Forwarded => {
                        status_notify_request::StatusKind::Forwarded
                    }
                    hardy_bpa::services::StatusNotify::Delivered => {
                        status_notify_request::StatusKind::Delivered
                    }
                    hardy_bpa::services::StatusNotify::Deleted => {
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

struct Handler {
    svc: Arc<LowLevelService>,
}

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = bpa_to_service::Msg;
    type RMsg = service_to_bpa::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        let msg = match msg {
            service_to_bpa::Msg::Send(msg) => self.svc.send(msg).await,
            service_to_bpa::Msg::Cancel(msg) => self.svc.cancel(msg).await,
            service_to_bpa::Msg::Unregister(_) => {
                self.svc.unregister().await;
                Ok(bpa_to_service::Msg::Unregister(UnregisterResponse {}))
            }
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
        // Do nothing
    }
}

pub struct GrpcService {
    bpa: Arc<hardy_bpa::bpa::Bpa>,
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
        let (mut channel_sender, rx) = tokio::sync::mpsc::channel(self.channel_size);
        let mut channel_receiver = request.into_inner();

        let svc = Arc::new(LowLevelService {
            inner: spin::once::Once::new(),
            proxy: spin::once::Once::new(),
        });
        RpcProxy::recv(&mut channel_sender, &mut channel_receiver, |msg| async {
            match msg {
                service_to_bpa::Msg::Register(request) => {
                    // Register the Service and respond
                    let endpoint_id = self
                        .bpa
                        .register_service(
                            request
                                .service_id
                                .as_ref()
                                .map(|service_id| match service_id {
                                    register_request::ServiceId::Dtn(s) => {
                                        hardy_bpv7::eid::Service::Dtn(s.clone().into())
                                    }
                                    register_request::ServiceId::Ipn(s) => {
                                        hardy_bpv7::eid::Service::Ipn(*s)
                                    }
                                }),
                            svc.clone(),
                        )
                        .await
                        .map(|endpoint_id| endpoint_id.to_string())
                        .map_err(|e| tonic::Status::from_error(e.into()))?;

                    Ok(bpa_to_service::Msg::Register(RegisterResponse {
                        endpoint_id,
                    }))
                }
                _ => {
                    info!("Service sent incorrect message: {msg:?}");
                    Err(tonic::Status::internal(format!(
                        "Unexpected response: {msg:?}"
                    )))
                }
            }
        })
        .await?;

        // Start the proxy
        let handler = Box::new(Handler { svc: svc.clone() });
        svc.proxy
            .call_once(|| RpcProxy::run(channel_sender, channel_receiver, handler));

        Ok(tonic::Response::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
    }
}

pub fn new_service(bpa: &Arc<hardy_bpa::bpa::Bpa>) -> service_server::ServiceServer<GrpcService> {
    service_server::ServiceServer::new(GrpcService {
        bpa: bpa.clone(),
        channel_size: 16,
    })
}

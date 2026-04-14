use super::*;
use proto::service::*;

type ServiceSink = Arc<dyn hardy_bpa::services::ServiceSink>;

struct LowLevelService {
    sink: Mutex<Option<ServiceSink>>,
    proxy: Once<RpcProxy<Result<BpaToService, tonic::Status>, ServiceToBpa>>,
}

impl LowLevelService {
    fn sink(&self) -> Result<ServiceSink, tonic::Status> {
        self.sink
            .lock()
            .clone()
            .ok_or(tonic::Status::unavailable("Unregistered"))
    }

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
        self.sink()?
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
        self.sink()?
            .cancel(&bundle_id)
            .await
            .map(|cancelled| bpa_to_service::Msg::Cancel(CancelResponse { cancelled }))
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    async fn unregister(&self) {
        let sink = self.sink.lock().take();
        if let Some(sink) = sink {
            sink.unregister().await;
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
        *self.sink.lock() = Some(Arc::from(sink));
    }

    async fn on_unregister(&self) {
        if self.sink.lock().take().is_none() {
            return;
        }

        if let Some(proxy) = self.proxy.get() {
            proxy.shutdown().await;
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
        self.svc.unregister().await;
        if let Some(proxy) = self.svc.proxy.get() {
            proxy.cancel();
        }
    }
}

pub struct GrpcService {
    bpa: Arc<dyn hardy_bpa::bpa::BpaRegistration>,
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

        // Spawn the registration handshake and proxy — we must return the
        // response stream immediately so the client can start sending messages.
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
    mut channel_sender: tokio::sync::mpsc::Sender<Result<BpaToService, tonic::Status>>,
    mut channel_receiver: tonic::Streaming<ServiceToBpa>,
    bpa: Arc<dyn hardy_bpa::bpa::BpaRegistration>,
) {
    let svc = Arc::new(LowLevelService {
        sink: Mutex::new(None),
        proxy: Once::new(),
    });

    let result = RpcProxy::recv(&mut channel_sender, &mut channel_receiver, |msg| async {
        match msg {
            service_to_bpa::Msg::Register(request) => {
                let service_id = request
                    .service_id
                    .as_ref()
                    .map(|service_id| match service_id {
                        register_request::ServiceId::Dtn(s) => {
                            hardy_bpv7::eid::Service::Dtn(s.clone().into())
                        }
                        register_request::ServiceId::Ipn(s) => hardy_bpv7::eid::Service::Ipn(*s),
                    })
                    .ok_or_else(|| tonic::Status::invalid_argument("service_id is required"))?;
                let endpoint_id = bpa
                    .register_service(service_id, svc.clone())
                    .await
                    .map(|endpoint_id| endpoint_id.to_string())
                    .map_err(|e| tonic::Status::from_error(e.into()))?;

                Ok(bpa_to_service::Msg::Register(RegisterResponse {
                    endpoint_id,
                }))
            }
            _ => {
                warn!("Service sent incorrect message: {msg:?}");
                Err(tonic::Status::internal(format!(
                    "Unexpected response: {msg:?}"
                )))
            }
        }
    })
    .await;

    if let Err(e) = result {
        warn!("Service registration failed: {e}");
        return;
    }

    // Start the proxy for ongoing communication
    let handler = Box::new(Handler { svc: svc.clone() });
    svc.proxy
        .call_once(|| RpcProxy::run(channel_sender, channel_receiver, handler));
}

/// Create a new low-level Service gRPC service.
pub fn new_endpoint_service(
    bpa: &Arc<dyn hardy_bpa::bpa::BpaRegistration>,
    tasks: &hardy_async::TaskPool,
) -> service_server::ServiceServer<GrpcService> {
    service_server::ServiceServer::new(GrpcService {
        bpa: bpa.clone(),
        session_tasks: tasks.clone(),
        channel_size: 16,
    })
}

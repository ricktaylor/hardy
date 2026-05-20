use std::sync::Arc;

use hardy_async::async_trait;
use hardy_async::sync::spin::{Mutex, Once};
use hardy_bpa::Bytes;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::services::{self, ServiceContext, StatusNotify};
use hardy_bpv7::bundle::Id as BundleId;
use hardy_bpv7::eid::{self, Eid, Service as EidService};
use hardy_bpv7::status_report::ReasonCode;
use tracing::{error, warn};

use crate::proto::service::{
    AppReceiveRequest, AppSendRequest, AppToBpa, BpaToApp, CancelRequest, CancelResponse,
    RegisterResponse, SendResponse, StatusNotifyRequest, app_send_request, app_to_bpa,
    application_server, bpa_to_app, register_request, status_notify_request,
};
use crate::proxy::{ProxyHandler, RpcProxy};

fn to_timestamp(t: time::OffsetDateTime) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: t.unix_timestamp(),
        nanos: t.nanosecond() as i32,
    }
}

struct Application {
    ctx: Mutex<Option<ServiceContext>>,
    proxy: Once<RpcProxy<Result<BpaToApp, tonic::Status>, AppToBpa>>,
}

impl Application {
    fn ctx(&self) -> Result<ServiceContext, tonic::Status> {
        self.ctx
            .lock()
            .clone()
            .ok_or(tonic::Status::unavailable("Unregistered"))
    }

    async fn call(&self, msg: bpa_to_app::Msg) -> services::Result<app_to_bpa::Msg> {
        let proxy = self.proxy.get().ok_or_else(|| {
            error!("call made before on_register!");
            services::Error::Disconnected
        })?;

        match proxy.call(msg).await {
            Ok(None) => Err(services::Error::Disconnected),
            Ok(Some(msg)) => Ok(msg),
            Err(e) => Err(services::Error::Internal(e.into())),
        }
    }

    async fn send(&self, request: AppSendRequest) -> Result<bpa_to_app::Msg, tonic::Status> {
        let mut options = None;
        if let Some(mut f) = request.options {
            let mut test_bit = |f2| {
                let b = (f & (f2 as u32)) != 0;
                f &= !(f2 as u32);
                b
            };
            options = Some(services::SendOptions {
                do_not_fragment: test_bit(app_send_request::SendOptions::DoNotFragment),
                request_ack: test_bit(app_send_request::SendOptions::RequestAck),
                report_status_time: test_bit(app_send_request::SendOptions::ReportStatusTime),
                notify_reception: test_bit(app_send_request::SendOptions::NotifyReception),
                notify_forwarding: test_bit(app_send_request::SendOptions::NotifyForwarding),
                notify_delivery: test_bit(app_send_request::SendOptions::NotifyDelivery),
                notify_deletion: test_bit(app_send_request::SendOptions::NotifyDeletion),
            });
            if f != 0 {
                return Err(tonic::Status::invalid_argument("Invalid SendOptions"));
            }
        }

        self.ctx()?
            .send(
                request.destination.parse().map_err(|e: eid::Error| {
                    tonic::Status::invalid_argument(format!("Invalid eid: {e}"))
                })?,
                request.payload,
                std::time::Duration::from_millis(request.lifetime),
                options,
            )
            .await
            .map(|bundle_id| {
                bpa_to_app::Msg::Send(SendResponse {
                    bundle_id: bundle_id.to_key(),
                })
            })
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    async fn cancel(&self, request: CancelRequest) -> Result<bpa_to_app::Msg, tonic::Status> {
        let bundle_id = BundleId::from_key(&request.bundle_id)
            .map_err(|e| tonic::Status::invalid_argument(format!("Invalid bundle_id: {e}")))?;
        self.ctx()?
            .cancel(bundle_id)
            .await
            .map(|cancelled| bpa_to_app::Msg::Cancel(CancelResponse { cancelled }))
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    fn disconnect(&self) {
        self.ctx.lock().take();
    }
}

#[async_trait]
impl services::Application for Application {
    async fn on_register(&self, _source: &Eid, ctx: ServiceContext) {
        *self.ctx.lock() = Some(ctx);
    }

    async fn on_unregister(&self) {
        if self.ctx.lock().take().is_none() {
            return;
        }

        if let Some(proxy) = self.proxy.get() {
            proxy.shutdown().await;
        }
    }

    async fn on_receive(
        &self,
        source: Eid,
        expiry: time::OffsetDateTime,
        ack_requested: bool,
        payload: Bytes,
    ) {
        match self
            .call(bpa_to_app::Msg::Receive(AppReceiveRequest {
                source: source.to_string(),
                ack_requested,
                expiry: Some(to_timestamp(expiry)),
                payload,
            }))
            .await
        {
            Ok(app_to_bpa::Msg::Receive(_)) => {}
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
        bundle_id: &BundleId,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
        timestamp: Option<time::OffsetDateTime>,
    ) {
        match self
            .call(bpa_to_app::Msg::StatusNotify(StatusNotifyRequest {
                bundle_id: bundle_id.to_key(),
                from: from.to_string(),
                kind: match kind {
                    StatusNotify::Received => status_notify_request::StatusKind::Received,
                    StatusNotify::Forwarded => status_notify_request::StatusKind::Forwarded,
                    StatusNotify::Delivered => status_notify_request::StatusKind::Delivered,
                    StatusNotify::Deleted => status_notify_request::StatusKind::Deleted,
                }
                .into(),
                reason: reason.into(),
                timestamp: timestamp.map(to_timestamp),
            }))
            .await
        {
            Ok(app_to_bpa::Msg::StatusNotify(_)) => {}
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
    app: Arc<Application>,
}

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = bpa_to_app::Msg;
    type RMsg = app_to_bpa::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        let msg = match msg {
            app_to_bpa::Msg::Send(msg) => self.app.send(msg).await,
            app_to_bpa::Msg::Cancel(msg) => self.app.cancel(msg).await,
            _ => {
                warn!("Ignoring unsolicited response: {msg:?}");
                return None;
            }
        };

        match msg {
            Ok(msg) => Some(msg),
            Err(e) => Some(bpa_to_app::Msg::Status(e.into())),
        }
    }

    async fn on_close(&self) {
        self.app.disconnect();
        if let Some(proxy) = self.app.proxy.get() {
            proxy.cancel();
        }
    }
}

pub struct Service {
    bpa: Arc<dyn BpaRegistration>,
    session_tasks: hardy_async::TaskPool,
    channel_size: usize,
}

#[async_trait]
impl application_server::Application for Service {
    type RegisterStream = tokio_stream::wrappers::ReceiverStream<Result<BpaToApp, tonic::Status>>;

    async fn register(
        &self,
        request: tonic::Request<tonic::Streaming<AppToBpa>>,
    ) -> Result<tonic::Response<Self::RegisterStream>, tonic::Status> {
        let (channel_sender, rx) = tokio::sync::mpsc::channel(self.channel_size);
        let channel_receiver = request.into_inner();

        let bpa = self.bpa.clone();
        hardy_async::spawn!(self.session_tasks, "application_session", async move {
            run_application_session(channel_sender, channel_receiver, bpa).await;
        });

        Ok(tonic::Response::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
    }
}

async fn run_application_session(
    mut channel_sender: tokio::sync::mpsc::Sender<Result<BpaToApp, tonic::Status>>,
    mut channel_receiver: tonic::Streaming<AppToBpa>,
    bpa: Arc<dyn BpaRegistration>,
) {
    let app = Arc::new(Application {
        ctx: Mutex::new(None),
        proxy: Once::new(),
    });

    let result = RpcProxy::recv(&mut channel_sender, &mut channel_receiver, |msg| async {
        match msg {
            app_to_bpa::Msg::Register(request) => {
                let service_id = request
                    .service_id
                    .as_ref()
                    .map(|service_id| match service_id {
                        register_request::ServiceId::Dtn(s) => EidService::Dtn(s.clone().into()),
                        register_request::ServiceId::Ipn(s) => EidService::Ipn(*s),
                    })
                    .ok_or_else(|| tonic::Status::invalid_argument("service_id is required"))?;
                let endpoint_id = bpa
                    .register_application(service_id, app.clone())
                    .await
                    .map(|endpoint_id| endpoint_id.to_string())
                    .map_err(|e| tonic::Status::from_error(e.into()))?;

                Ok(bpa_to_app::Msg::Register(RegisterResponse { endpoint_id }))
            }
            _ => {
                warn!("Application sent incorrect message: {msg:?}");
                Err(tonic::Status::internal(format!(
                    "Unexpected response: {msg:?}"
                )))
            }
        }
    })
    .await;

    if let Err(e) = result {
        warn!("Application registration failed: {e}");
        return;
    }

    let handler = Box::new(Handler { app: app.clone() });
    app.proxy
        .call_once(|| RpcProxy::run(channel_sender, channel_receiver, handler));
}

pub fn new_application_service(
    bpa: &Arc<dyn BpaRegistration>,
    tasks: &hardy_async::TaskPool,
) -> application_server::ApplicationServer<Service> {
    application_server::ApplicationServer::new(Service {
        bpa: bpa.clone(),
        session_tasks: tasks.clone(),
        channel_size: 16,
    })
}

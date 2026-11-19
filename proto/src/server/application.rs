use hardy_bpa::service::{Context, RegistrarExt, Sink, factory_fn};
use hardy_bpv7::eid::Eid;

use super::*;
use proto::service::*;

// The BPA's end of a remote application: it forwards deliveries through
// the proxy standing in for the far application, and the application
// reaches the BPA back through the sink.
struct Application {
    sink: Arc<dyn Sink>,
    endpoint: Eid,
    proxy: Arc<RpcProxy<Result<BpaToApp, tonic::Status>, AppToBpa>>,
}

impl Application {
    async fn call(&self, msg: bpa_to_app::Msg) -> hardy_bpa::service::Result<app_to_bpa::Msg> {
        match self.proxy.call(msg).await {
            Ok(None) => Err(hardy_bpa::service::Error::Disconnected),
            Ok(Some(msg)) => Ok(msg),
            Err(e) => Err(hardy_bpa::service::Error::Internal(e.into())),
        }
    }
}

// The far application's end of the session: every request it makes lands
// on the sink its registration handed back.
struct Handler {
    sink: Arc<dyn Sink>,
}

impl Handler {
    async fn send(&self, request: AppSendRequest) -> Result<bpa_to_app::Msg, tonic::Status> {
        let mut options = None;
        if let Some(mut f) = request.options {
            let mut test_bit = |f2| {
                let b = (f & (f2 as u32)) != 0;
                f &= !(f2 as u32);
                b
            };
            options = Some(hardy_bpa::service::SendOptions {
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

        self.sink
            .send(
                request
                    .destination
                    .parse()
                    .map_err(|e: hardy_bpv7::eid::Error| {
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
}

#[async_trait]
impl hardy_bpa::service::Service for Application {
    async fn unregister(&self) -> hardy_bpa::service::Result<()> {
        self.proxy.shutdown().await;
        Ok(())
    }

    // INTERIM BUFFERING: the wire protocol has no segmented payload
    // messages yet — deliveries travel as one unary AppReceiveRequest — so
    // the stream is assembled in memory via `stream::buffer_stream` before
    // marshalling. This is a deliberate stepping stone toward the full
    // streaming pipeline (chunked wire messages are the blocker); see
    // bpa/docs/streaming_pipeline_design.md.
    async fn deliver(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
        expiry: time::OffsetDateTime,
        ack_requested: bool,
        total_len: u64,
        stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::stream::Segment>,
    ) -> hardy_bpa::service::Result<()> {
        let payload = hardy_bpa::stream::buffer_stream(stream, total_len).await?;
        match self
            .call(bpa_to_app::Msg::Receive(AppReceiveRequest {
                ack_requested,
                expiry: Some(to_timestamp(expiry)),
                payload,
                bundle_id: bundle_id.to_key(),
            }))
            .await?
        {
            app_to_bpa::Msg::Receive(_) => Ok(()),
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
            .call(bpa_to_app::Msg::StatusNotify(StatusNotifyRequest {
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

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = bpa_to_app::Msg;
    type RMsg = app_to_bpa::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        let msg = match msg {
            app_to_bpa::Msg::Send(msg) => self.send(msg).await,
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
        // The far application is gone: unregister, which takes the
        // application down with the session.
        self.sink.unregister().await;
    }
}

pub struct Service {
    bpa: Arc<dyn hardy_bpa::service::Registrar>,
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

        // Spawn the registration handshake and proxy — we must return the
        // response stream immediately so the client can start sending messages.
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
    channel_sender: tokio::sync::mpsc::Sender<Result<BpaToApp, tonic::Status>>,
    channel_receiver: tonic::Streaming<AppToBpa>,
    bpa: Arc<dyn hardy_bpa::service::Registrar>,
) {
    let (proxy, msg_id, opening) = match RpcProxy::accept(channel_sender, channel_receiver).await {
        Ok(session) => session,
        Err(e) => {
            warn!("Application session failed to open: {e}");
            return;
        }
    };

    match register(&*bpa, opening, &proxy).await {
        Ok(app) => {
            let response = RegisterResponse {
                endpoint_id: app.endpoint.to_string(),
            };
            let handler = Handler {
                sink: app.sink.clone(),
            };
            proxy.open(
                msg_id,
                bpa_to_app::Msg::Register(response),
                Box::new(handler),
            );
        }
        Err(status) => {
            warn!("Application registration failed: {status}");
            proxy.fail(status).await;
        }
    }
}

// Registers the application the opening message asks for, built around
// the proxy the BPA delivers through.
async fn register(
    bpa: &dyn hardy_bpa::service::Registrar,
    opening: app_to_bpa::Msg,
    proxy: &Arc<RpcProxy<Result<BpaToApp, tonic::Status>, AppToBpa>>,
) -> Result<Arc<Application>, tonic::Status> {
    let app_to_bpa::Msg::Register(request) = opening else {
        warn!("Application sent incorrect message: {opening:?}");
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

    bpa.register_service(
        Some(service_id),
        factory_fn(|cx: Context| {
            Ok(Arc::new(Application {
                sink: cx.sink,
                endpoint: cx.endpoint,
                proxy: proxy.clone(),
            }))
        }),
    )
    .await
    .map_err(|e| tonic::Status::from_error(e.into()))
}

/// Create a new Application gRPC service.
pub fn new_application_service(
    bpa: Arc<dyn hardy_bpa::service::Registrar>,
    tasks: &hardy_async::TaskPool,
) -> application_server::ApplicationServer<Service> {
    application_server::ApplicationServer::new(Service {
        bpa,
        session_tasks: tasks.clone(),
        channel_size: 16,
    })
    .max_encoding_message_size(crate::MAX_MESSAGE_SIZE)
    .max_decoding_message_size(crate::MAX_MESSAGE_SIZE)
}

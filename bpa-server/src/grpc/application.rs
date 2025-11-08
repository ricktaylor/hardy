use super::*;
use hardy_bpa::async_trait;
use hardy_proto::application::*;
use std::{
    collections::HashMap,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicI32, Ordering},
    },
};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Streaming};

type AckMapEntry<T> = oneshot::Sender<hardy_bpa::service::Result<T>>;

struct Application {
    sink: OnceLock<Box<dyn hardy_bpa::service::Sink>>,
    tx: mpsc::Sender<Result<BpaToApp, tonic::Status>>,
    msg_id: AtomicI32,
    acks: Mutex<HashMap<i32, AckMapEntry<()>>>,
}

impl Application {
    async fn run(
        tx: mpsc::Sender<Result<BpaToApp, tonic::Status>>,
        bpa: Arc<hardy_bpa::bpa::Bpa>,
        mut requests: Streaming<AppToBpa>,
    ) {
        // Expect Register message first
        let app = match requests.message().await {
            Ok(Some(AppToBpa { msg_id, msg })) => match msg {
                Some(app_to_bpa::Msg::Status(status)) => {
                    info!("Service failed before registration started!: {:?}", status);
                    return;
                }
                Some(app_to_bpa::Msg::Register(msg)) => {
                    // Register the Service and respond
                    let app = Arc::new(Self {
                        sink: OnceLock::default(),
                        tx: tx.clone(),
                        msg_id: 0.into(),
                        acks: Mutex::new(HashMap::new()),
                    });
                    let result = bpa
                        .register_service(
                            msg.service_id.as_ref().map(|o| match o {
                                register_application_request::ServiceId::Dtn(s) => {
                                    hardy_bpa::service::ServiceId::DtnService(s)
                                }
                                register_application_request::ServiceId::Ipn(s) => {
                                    hardy_bpa::service::ServiceId::IpnService(*s)
                                }
                            }),
                            app.clone(),
                        )
                        .await;
                    if tx
                        .send(
                            result
                                .map(|endpoint_id| BpaToApp {
                                    msg_id,
                                    msg: Some(bpa_to_app::Msg::Register(
                                        RegisterApplicationResponse {
                                            endpoint_id: endpoint_id.to_string(),
                                        },
                                    )),
                                })
                                .map_err(|e| tonic::Status::from_error(e.into())),
                        )
                        .await
                        .is_err()
                    {
                        return;
                    }
                    app
                }
                Some(msg) => {
                    info!("Service sent incorrect message: {:?}", msg);
                    return;
                }
                None => {
                    info!("Service sent unrecognized message");
                    return;
                }
            },
            Ok(None) => {
                info!("Service disconnected before registration completed");
                return;
            }
            Err(status) => {
                info!("Service failed before registration completed: {status}");
                return;
            }
        };

        // And now just pump messages
        loop {
            let response = match requests.message().await {
                Ok(Some(AppToBpa { msg_id, msg })) => match msg {
                    Some(app_to_bpa::Msg::Register(msg)) => {
                        info!("Service sent duplicate registration message: {:?}", msg);
                        _ = tx
                            .send(Err(tonic::Status::failed_precondition(
                                "Already registered",
                            )))
                            .await;
                        break;
                    }
                    Some(app_to_bpa::Msg::Send(msg)) => Some(app.send(msg).await),
                    Some(app_to_bpa::Msg::Receive(_)) | Some(app_to_bpa::Msg::StatusNotify(_)) => {
                        app.ack_response(msg_id, Ok(())).await
                    }
                    Some(app_to_bpa::Msg::Status(status)) => {
                        app.ack_response(
                            msg_id,
                            Err(tonic::Status::new(status.code.into(), status.message)),
                        )
                        .await
                    }
                    None => {
                        info!("Service sent unrecognized message");
                        Some(Ok(bpa_to_app::Msg::Status(
                            tonic::Status::invalid_argument("Unrecognized message").into(),
                        )))
                    }
                }
                .map(|o| {
                    o.map(|v| BpaToApp {
                        msg_id,
                        msg: Some(v),
                    })
                }),
                Ok(None) => {
                    debug!("Service disconnected");
                    break;
                }
                Err(status) => {
                    info!("Service failed: {status}");
                    break;
                }
            };

            if let Some(response) = response
                && tx.send(response).await.is_err()
            {
                break;
            }
        }

        // Done with app
        if let Some(sink) = app.sink.get() {
            sink.unregister().await;
        }
    }

    async fn send(&self, request: SendRequest) -> Result<bpa_to_app::Msg, tonic::Status> {
        let mut options = None;
        if let Some(mut f) = request.options {
            let mut test_bit = |f2| {
                let b = (f & (f2 as u32)) != 0;
                f &= !(f2 as u32);
                b
            };
            options = Some(hardy_bpa::service::SendOptions {
                do_not_fragment: test_bit(send_request::SendOptions::DoNotFragment),
                request_ack: test_bit(send_request::SendOptions::RequestAck),
                report_status_time: test_bit(send_request::SendOptions::ReportStatusTime),
                notify_reception: test_bit(send_request::SendOptions::NotifyReception),
                notify_forwarding: test_bit(send_request::SendOptions::NotifyForwarding),
                notify_delivery: test_bit(send_request::SendOptions::NotifyDelivery),
                notify_deletion: test_bit(send_request::SendOptions::NotifyDeletion),
            });
            if f != 0 {
                return Err(tonic::Status::invalid_argument("Invalid SendOptions"));
            }
        }

        self.sink
            .get()
            .trace_expect("Service registration not complete!")
            .send(
                request
                    .destination
                    .parse()
                    .map_err(|e: hardy_bpv7::eid::Error| {
                        tonic::Status::invalid_argument(format!("Invalid eid: {e}"))
                    })?,
                &request.payload,
                std::time::Duration::from_millis(request.lifetime),
                options,
            )
            .await
            .map(|bundle_id| {
                bpa_to_app::Msg::Send(SendResponse {
                    bundle_id: bundle_id.into(),
                })
            })
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    async fn ack_response(
        &self,
        msg_id: i32,
        response: Result<(), tonic::Status>,
    ) -> Option<Result<bpa_to_app::Msg, tonic::Status>> {
        if let Some(entry) = self
            .acks
            .lock()
            .trace_expect("Failed to lock mutex")
            .remove(&msg_id)
        {
            _ = entry.send(response.map_err(|s| hardy_bpa::service::Error::Internal(s.into())));
        }
        None
    }

    async fn rpc(&self, msg: bpa_to_app::Msg) -> hardy_bpa::service::Result<()> {
        let (tx, rx) = oneshot::channel();

        // Generate a new msg_id, and add to the forward_ack map
        let msg_id = {
            let mut acks = self.acks.lock().trace_expect("Failed to lock mutex");
            let mut msg_id = self.msg_id.fetch_add(1, Ordering::SeqCst);
            while acks.contains_key(&msg_id) {
                msg_id = self.msg_id.fetch_add(1, Ordering::SeqCst);
            }
            acks.insert(msg_id, tx);
            msg_id
        };

        if self
            .tx
            .send(Ok(BpaToApp {
                msg_id,
                msg: Some(msg),
            }))
            .await
            .is_err()
        {
            // Remove ack waiter
            self.acks
                .lock()
                .trace_expect("Failed to lock mutex")
                .remove(&msg_id);
            return Err(hardy_bpa::service::Error::Disconnected);
        }

        rx.await
            .map_err(|_| hardy_bpa::service::Error::Disconnected)?
            .map_err(|s| hardy_bpa::service::Error::Internal(s.into()))
    }
}

#[async_trait]
impl hardy_bpa::service::Service for Application {
    async fn on_register(
        &self,
        _source: &hardy_bpv7::eid::Eid,
        sink: Box<dyn hardy_bpa::service::Sink>,
    ) {
        if self.sink.set(sink).is_err() {
            error!("Service on_register called twice!");
            panic!("Service on_register called twice!");
        }
    }

    async fn on_unregister(&self) {
        // We do nothing
    }

    async fn on_receive(&self, bundle: hardy_bpa::service::Bundle) {
        self.rpc(bpa_to_app::Msg::Receive(ReceiveBundleRequest {
            bundle_id: bundle.source.to_string(),
            ack_requested: bundle.ack_requested,
            expiry: Some(to_timestamp(bundle.expiry)),
            payload: bundle.payload,
        }))
        .await
        .unwrap_or_else(|e| info!("Service refused notification: {e}"))
    }

    async fn on_status_notify(
        &self,
        bundle_id: &str,
        from: &str,
        kind: hardy_bpa::service::StatusNotify,
        reason: hardy_bpv7::status_report::ReasonCode,
        timestamp: Option<hardy_bpv7::dtn_time::DtnTime>,
    ) {
        self.rpc(bpa_to_app::Msg::StatusNotify(StatusNotifyRequest {
            bundle_id: bundle_id.into(),
            from: from.into(),
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
            } as i32,
            reason: reason.into(),
            timestamp: timestamp.map(|t| prost_types::Timestamp {
                seconds: (t.millisecs() / 1000) as i64,
                nanos: (t.millisecs() % 1000 * 1_000_000) as i32,
            }),
        }))
        .await
        .unwrap_or_else(|e| info!("Service refused notification: {e}"))
    }
}

pub struct Service {
    bpa: Arc<hardy_bpa::bpa::Bpa>,
}

impl Service {
    fn new(bpa: &Arc<hardy_bpa::bpa::Bpa>) -> Self {
        Self { bpa: bpa.clone() }
    }
}

#[tonic::async_trait]
impl application_server::Application for Service {
    type RegisterStream = ReceiverStream<Result<BpaToApp, tonic::Status>>;

    async fn register(
        &self,
        request: Request<tonic::Streaming<AppToBpa>>,
    ) -> Result<Response<Self::RegisterStream>, tonic::Status> {
        let (tx, rx) = mpsc::channel(32);

        // Spawn a task to handle I/O
        tokio::spawn(Application::run(tx, self.bpa.clone(), request.into_inner()));

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

pub fn new_service(
    bpa: &Arc<hardy_bpa::bpa::Bpa>,
) -> application_server::ApplicationServer<Service> {
    application_server::ApplicationServer::new(Service::new(bpa))
}

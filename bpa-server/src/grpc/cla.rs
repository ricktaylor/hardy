use super::*;
use hardy_bpa::async_trait;
use hardy_proto::cla::*;
use std::{
    collections::HashMap,
    sync::{
        OnceLock,
        atomic::{AtomicI32, Ordering},
    },
};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Streaming};

type AckMapEntry<T> = oneshot::Sender<hardy_bpa::cla::Result<T>>;

struct Cla {
    sink: OnceLock<Box<dyn hardy_bpa::cla::Sink>>,
    tx: mpsc::Sender<Result<BpaToCla, tonic::Status>>,
    msg_id: AtomicI32,
    forward_acks: Mutex<HashMap<i32, AckMapEntry<forward_bundle_response::Result>>>,
}

impl Cla {
    async fn run(
        tx: mpsc::Sender<Result<BpaToCla, tonic::Status>>,
        bpa: Arc<hardy_bpa::bpa::Bpa>,
        mut requests: Streaming<ClaToBpa>,
    ) {
        // Expect Register message first
        let cla = match requests.message().await {
            Ok(Some(ClaToBpa { msg_id, msg })) => {
                match msg {
                    Some(cla_to_bpa::Msg::Status(status)) => {
                        info!("CLA failed before registration started!: {:?}", status);
                        return;
                    }
                    Some(cla_to_bpa::Msg::Register(msg)) => {
                        // Register the CLA and respond
                        let cla = Arc::new(Self {
                            sink: OnceLock::default(),
                            tx: tx.clone(),
                            msg_id: 0.into(),
                            forward_acks: Mutex::new(HashMap::new()),
                        });

                        if tx
                        .send(
                            bpa.register_cla(
                                msg.name,
                                msg.address_type.map(|o| match <i32 as std::convert::TryInto<ClaAddressType>>::try_into(o) {
                                    Ok(t) => t.into(),
                                    Err(_) => hardy_bpa::cla::ClaAddressType::Unknown(o as u32),
                                }),
                                cla.clone(),
                            )
                            .await
                            .map(|_| BpaToCla {
                                msg_id,
                                msg: Some(bpa_to_cla::Msg::Register(RegisterClaResponse {})),
                            })
                            .map_err(|e| tonic::Status::from_error(e.into())),
                        )
                        .await
                        .is_err()
                    {
                        return;
                    }
                        cla
                    }
                    Some(msg) => {
                        info!("CLA sent incorrect message: {:?}", msg);
                        return;
                    }
                    None => {
                        info!("CLA sent unrecognized message");
                        return;
                    }
                }
            }
            Ok(None) => {
                info!("CLA disconnected before registration completed");
                return;
            }
            Err(status) => {
                info!("CLA failed before registration completed: {status}");
                return;
            }
        };

        // And now just pump messages
        loop {
            let response = match requests.message().await {
                Ok(Some(ClaToBpa { msg_id, msg })) => match msg {
                    Some(cla_to_bpa::Msg::Register(msg)) => {
                        info!("CLA sent duplicate registration message: {:?}", msg);
                        _ = tx
                            .send(Err(tonic::Status::failed_precondition(
                                "Already registered",
                            )))
                            .await;
                        break;
                    }
                    Some(cla_to_bpa::Msg::Dispatch(msg)) => Some(cla.dispatch(msg.bundle).await),
                    Some(cla_to_bpa::Msg::AddPeer(msg)) => {
                        Some(if let Some(address) = msg.address {
                            cla.add_peer(msg.eid, address).await
                        } else {
                            Err(tonic::Status::invalid_argument("Missing address"))
                        })
                    }
                    Some(cla_to_bpa::Msg::RemovePeer(msg)) => Some(cla.remove_peer(msg.eid).await),
                    Some(cla_to_bpa::Msg::Forward(msg)) => {
                        if let Some(result) = msg.result {
                            cla.forward_ack_response(msg_id, Ok(result))
                        } else {
                            cla.forward_ack_response(
                                msg_id,
                                Err(tonic::Status::invalid_argument("Unrecognized result")),
                            )
                        }
                        .await
                    }
                    Some(cla_to_bpa::Msg::Status(status)) => {
                        cla.forward_ack_response(
                            msg_id,
                            Err(tonic::Status::new(status.code.into(), status.message)),
                        )
                        .await
                    }
                    None => {
                        info!("CLA sent unrecognized message");
                        Some(Ok(bpa_to_cla::Msg::Status(
                            tonic::Status::invalid_argument("Unrecognized message").into(),
                        )))
                    }
                }
                .map(|o| {
                    o.map(|v| BpaToCla {
                        msg_id,
                        msg: Some(v),
                    })
                }),
                Ok(None) => {
                    trace!("CLA disconnected");
                    break;
                }
                Err(status) => {
                    info!("CLA failed: {status}");
                    break;
                }
            };

            if let Some(response) = response {
                if tx.send(response).await.is_err() {
                    break;
                }
            }
        }

        // Done with cla
        if let Some(sink) = cla.sink.get() {
            sink.unregister().await;
        }
    }

    async fn dispatch(&self, bundle: hardy_bpa::Bytes) -> Result<bpa_to_cla::Msg, tonic::Status> {
        self.sink
            .get()
            .expect("CLA registration not complete!")
            .dispatch(bundle)
            .await
            .map(|_| bpa_to_cla::Msg::Dispatch(DispatchBundleResponse {}))
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    async fn add_peer(
        &self,
        eid: String,
        addr: ClaAddress,
    ) -> Result<bpa_to_cla::Msg, tonic::Status> {
        let eid = eid
            .parse()
            .map_err(|e| tonic::Status::invalid_argument(format!("Invalid endpoint id: {e}")))?;
        self.sink
            .get()
            .expect("CLA registration not complete!")
            .add_peer(
                eid,
                addr.try_into().map_err(|e| {
                    tonic::Status::invalid_argument(format!("Invalid address: {e}"))
                })?,
            )
            .await
            .map(|_| bpa_to_cla::Msg::AddPeer(AddPeerResponse {}))
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    async fn remove_peer(&self, eid: String) -> Result<bpa_to_cla::Msg, tonic::Status> {
        let eid = eid
            .parse()
            .map_err(|e| tonic::Status::invalid_argument(format!("Invalid endpoint id: {e}")))?;
        self.sink
            .get()
            .expect("CLA registration not complete!")
            .remove_peer(&eid)
            .await
            .map(|_| bpa_to_cla::Msg::RemovePeer(RemovePeerResponse {}))
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    async fn forward_ack_response(
        &self,
        msg_id: i32,
        response: Result<forward_bundle_response::Result, tonic::Status>,
    ) -> Option<Result<bpa_to_cla::Msg, tonic::Status>> {
        if let Some(entry) = self.forward_acks.lock().await.remove(&msg_id) {
            _ = entry.send(response.map_err(|s| hardy_bpa::cla::Error::Internal(s.into())));
        }
        None
    }
}

#[async_trait]
impl hardy_bpa::cla::Cla for Cla {
    async fn on_register(
        &self,
        sink: Box<dyn hardy_bpa::cla::Sink>,
        _node_ids: &[hardy_bpv7::eid::Eid],
    ) -> hardy_bpa::cla::Result<()> {
        if self.sink.set(sink).is_err() {
            error!("CLA on_register called twice!");
            return Err(hardy_bpa::cla::Error::AlreadyConnected);
        }
        Ok(())
    }

    async fn on_unregister(&self) {
        // We do nothing
    }

    async fn on_forward(
        &self,
        cla_addr: hardy_bpa::cla::ClaAddress,
        bundle: hardy_bpa::Bytes,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        let (tx, rx) = oneshot::channel();

        // Generate a new msg_id, and add to the forward_ack map
        let mut forward_acks = self.forward_acks.lock().await;
        let mut msg_id = self.msg_id.fetch_add(1, Ordering::SeqCst);
        while forward_acks.contains_key(&msg_id) {
            msg_id = self.msg_id.fetch_add(1, Ordering::SeqCst);
        }
        forward_acks.insert(msg_id, tx);
        drop(forward_acks);

        if self
            .tx
            .send(Ok(BpaToCla {
                msg: Some(bpa_to_cla::Msg::Forward(ForwardBundleRequest {
                    bundle: bundle.to_vec().into(),
                    address: Some(
                        cla_addr.try_into().map_err(|e: tonic::Status| {
                            hardy_bpa::cla::Error::Internal(e.into())
                        })?,
                    ),
                })),
                msg_id,
            }))
            .await
            .is_err()
        {
            // Remove ack waiter
            self.forward_acks.lock().await.remove(&msg_id);
            return Err(hardy_bpa::cla::Error::Disconnected);
        }

        rx.await
            .map_err(|_| hardy_bpa::cla::Error::Disconnected)?
            .map(Into::into)
            .map_err(|e| hardy_bpa::cla::Error::Internal(e.into()))
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
impl cla_server::Cla for Service {
    type RegisterStream = ReceiverStream<Result<BpaToCla, tonic::Status>>;

    async fn register(
        &self,
        request: Request<tonic::Streaming<ClaToBpa>>,
    ) -> Result<Response<Self::RegisterStream>, tonic::Status> {
        let (tx, rx) = mpsc::channel(32);

        // Spawn a task to handle I/O
        tokio::spawn(Cla::run(tx, self.bpa.clone(), request.into_inner()));

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

pub fn new_service(bpa: &Arc<hardy_bpa::bpa::Bpa>) -> cla_server::ClaServer<Service> {
    cla_server::ClaServer::new(Service::new(bpa))
}

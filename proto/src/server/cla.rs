use core::num::{NonZeroU32, NonZeroU64};

use hardy_bpa::{
    cla::{Context, RegistrarExt, Request, Sink, Terms, factory_fn},
    node_ids::NodeIds,
};
use hardy_bpv7::eid::NodeId;

use super::*;
use proto::cla::*;

// The BPA's end of a remote CLA: it holds the sink the session dispatches
// into, and forwards through the proxy standing in for the far CLA.
struct Cla {
    sink: Arc<dyn Sink>,
    node_ids: Arc<NodeIds>,
    terms: Terms,
    // The CLA is built before the proxy, which needs a handler holding
    // the CLA, so the proxy arrives once the session is running.
    proxy: Once<RpcProxy<Result<BpaToCla, tonic::Status>, ClaToBpa>>,
}

impl Cla {
    async fn call(&self, msg: bpa_to_cla::Msg) -> hardy_bpa::cla::Result<cla_to_bpa::Msg> {
        let proxy = self.proxy.get().ok_or_else(|| {
            error!("call made before on_register!");
            hardy_bpa::cla::Error::Disconnected
        })?;

        match proxy.call(msg).await {
            Ok(None) => Err(hardy_bpa::cla::Error::Disconnected),
            Ok(Some(msg)) => Ok(msg),
            Err(e) => Err(hardy_bpa::cla::Error::Internal(e.into())),
        }
    }

    async fn dispatch(
        &self,
        mut request: DispatchBundleRequest,
    ) -> Result<bpa_to_cla::Msg, tonic::Status> {
        let peer_node: Option<hardy_bpv7::eid::NodeId> = request
            .peer_node_id
            .map(|s| {
                s.parse().map_err(|e| {
                    tonic::Status::invalid_argument(format!("Invalid peer_node_id: {e}"))
                })
            })
            .transpose()?;

        let peer_addr: Option<hardy_bpa::cla::Address> =
            request.peer_addr.map(|a| a.try_into()).transpose()?;

        // The unary wire message already delivered the whole bundle, so it
        // enters the BPA as a one-segment stream. The verdict rides the
        // response; only sink faults become gRPC errors.
        self.sink
            .dispatch(peer_node.as_ref(), peer_addr.as_ref(), &mut request.bundle)
            .await
            .map(|verdict| {
                bpa_to_cla::Msg::Dispatch(DispatchBundleResponse {
                    verdict: match verdict {
                        hardy_bpa::cla::Acceptance::Accepted => DispatchVerdict::Accepted,
                        hardy_bpa::cla::Acceptance::Refused => DispatchVerdict::Refused,
                    }
                    .into(),
                })
            })
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    async fn add_peer(&self, request: AddPeerRequest) -> Result<bpa_to_cla::Msg, tonic::Status> {
        let node_ids = request
            .node_ids
            .into_iter()
            .map(|s| {
                s.parse()
                    .map_err(|e| tonic::Status::invalid_argument(format!("Invalid node id: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let cla_addr = request
            .address
            .ok_or(tonic::Status::invalid_argument("Missing address"))?
            .try_into()?;

        self.sink
            .add_peer(cla_addr, &node_ids)
            .await
            .map(|added| bpa_to_cla::Msg::AddPeer(AddPeerResponse { added }))
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    async fn remove_peer(
        &self,
        request: RemovePeerRequest,
    ) -> Result<bpa_to_cla::Msg, tonic::Status> {
        let cla_addr = request
            .address
            .ok_or(tonic::Status::invalid_argument("Missing address"))?
            .try_into()?;

        self.sink
            .remove_peer(&cla_addr)
            .await
            .map(|removed| bpa_to_cla::Msg::RemovePeer(RemovePeerResponse { removed }))
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    async fn transfer_outcome(
        &self,
        request: TransferOutcomeRequest,
    ) -> Result<bpa_to_cla::Msg, tonic::Status> {
        let bundle_id = hardy_bpv7::bundle::Id::from_key(&request.bundle_id)
            .map_err(|e| tonic::Status::invalid_argument(format!("Invalid bundle_id: {e}")))?;

        let outcome = match request.outcome {
            Some(transfer_outcome_request::Outcome::Completed(_)) => {
                hardy_bpa::cla::TransferOutcome::Completed
            }
            Some(transfer_outcome_request::Outcome::Failed(status)) => {
                debug!("CLA reports transfer of {bundle_id} failed: {status:?}");
                hardy_bpa::cla::TransferOutcome::Failed
            }
            None => return Err(tonic::Status::invalid_argument("Missing outcome")),
        };

        self.sink
            .report_transfer(&bundle_id, outcome)
            .await
            .map(|_| bpa_to_cla::Msg::TransferOutcome(TransferOutcomeResponse {}))
            .map_err(|e| tonic::Status::from_error(e.into()))
    }
}

#[async_trait]
impl hardy_bpa::cla::Cla for Cla {
    async fn shutdown(&self) -> hardy_bpa::cla::Result<()> {
        if let Some(proxy) = self.proxy.get() {
            proxy.shutdown().await;
        }
        Ok(())
    }

    // INTERIM BUFFERING: the wire protocol has no segmented bundle messages
    // yet — transfers travel as one unary ForwardBundleRequest — so the
    // stream is assembled in memory via `stream::buffer_stream` before
    // marshalling. This is a deliberate stepping stone toward the full
    // streaming pipeline (chunked wire messages are the blocker); see
    // bpa/docs/streaming_pipeline_design.md.
    async fn forward(
        &self,
        lane: Option<u32>,
        cla_addr: &hardy_bpa::cla::Address,
        bundle_id: &hardy_bpv7::bundle::Id,
        total_len: u64,
        stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::cla::Segment>,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardOutcome> {
        let bundle = hardy_bpa::stream::buffer_stream(stream, total_len).await?;
        match self
            .call(bpa_to_cla::Msg::Forward(ForwardBundleRequest {
                bundle,
                address: Some(cla_addr.clone().into()),
                lane,
                bundle_id: bundle_id.to_key(),
            }))
            .await?
        {
            cla_to_bpa::Msg::Forward(response) => match response.result {
                None => Err(hardy_bpa::cla::Error::Internal(
                    tonic::Status::internal("Invalid result code").into(),
                )),
                Some(forward_bundle_response::Result::Sent(_)) => {
                    Ok(hardy_bpa::cla::ForwardOutcome::Sent)
                }
                Some(forward_bundle_response::Result::NoNeighbour(_)) => {
                    Ok(hardy_bpa::cla::ForwardOutcome::NoNeighbour)
                }
                Some(forward_bundle_response::Result::Accepted(_)) => {
                    Ok(hardy_bpa::cla::ForwardOutcome::Accepted)
                }
            },
            msg => {
                warn!("Unexpected response: {msg:?}");
                Err(hardy_bpa::cla::Error::Internal(
                    tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
                ))
            }
        }
    }
}

struct Handler {
    cla: Arc<Cla>,
}

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = bpa_to_cla::Msg;
    type RMsg = cla_to_bpa::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        let msg = match msg {
            cla_to_bpa::Msg::Dispatch(msg) => self.cla.dispatch(msg).await,
            cla_to_bpa::Msg::AddPeer(msg) => self.cla.add_peer(msg).await,
            cla_to_bpa::Msg::RemovePeer(msg) => self.cla.remove_peer(msg).await,
            cla_to_bpa::Msg::TransferOutcome(msg) => self.cla.transfer_outcome(msg).await,
            _ => {
                warn!("Ignoring unsolicited response: {msg:?}");
                return None;
            }
        };

        match msg {
            Ok(msg) => Some(msg),
            Err(e) => Some(bpa_to_cla::Msg::Status(e.into())),
        }
    }

    async fn on_close(&self) {
        // The far CLA is gone: detach, which takes this CLA down with the
        // session. `cancel` rather than `shutdown`, because the proxy task
        // awaiting this handler is one of the tasks being stopped.
        self.cla.sink.unregister().await;
        if let Some(proxy) = self.cla.proxy.get() {
            proxy.cancel();
        }
    }
}

pub struct Service {
    bpa: Arc<dyn hardy_bpa::bpa::BpaRegistration>,
    session_tasks: hardy_async::TaskPool,
    channel_size: usize,
}

#[async_trait]
impl cla_server::Cla for Service {
    type RegisterStream = tokio_stream::wrappers::ReceiverStream<Result<BpaToCla, tonic::Status>>;

    async fn register(
        &self,
        request: tonic::Request<tonic::Streaming<ClaToBpa>>,
    ) -> Result<tonic::Response<Self::RegisterStream>, tonic::Status> {
        let (channel_sender, rx) = tokio::sync::mpsc::channel(self.channel_size);
        let channel_receiver = request.into_inner();

        // Spawn the registration handshake and proxy — we must return the
        // response stream immediately so the client can start sending messages.
        let bpa = self.bpa.clone();
        hardy_async::spawn!(self.session_tasks, "cla_session", async move {
            run_cla_session(channel_sender, channel_receiver, bpa).await;
        });

        Ok(tonic::Response::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
    }
}

async fn run_cla_session(
    mut channel_sender: tokio::sync::mpsc::Sender<Result<BpaToCla, tonic::Status>>,
    mut channel_receiver: tonic::Streaming<ClaToBpa>,
    bpa: Arc<dyn hardy_bpa::bpa::BpaRegistration>,
) {
    // Filled by the registration handshake below, which the BPA drives
    // through the factory.
    let mut registered = None;

    // Wait for the client's registration message and process it
    let result = RpcProxy::recv(&mut channel_sender, &mut channel_receiver, |msg| async {
        match msg {
            cla_to_bpa::Msg::Register(request) => {
                let address_type =
                    request
                        .address_type
                        .map(|address_type| match address_type.try_into() {
                            Ok(ClaAddressType::Tcp) => hardy_bpa::cla::AddressType::Tcp,
                            Err(_) | Ok(ClaAddressType::Private) => {
                                hardy_bpa::cla::AddressType::Private
                            }
                        });
                // Absent = no CLA-side limit declared; an explicit 0 is
                // invalid (omit the field to declare no limit).
                let max_bundle_size = match request.max_bundle_size {
                    Some(0) => {
                        return Err(tonic::Status::invalid_argument(
                            "max_bundle_size 0 is invalid; omit the field to declare no limit",
                        ));
                    }
                    declared => declared.and_then(NonZeroU64::new),
                };
                // Same shape for the lane declaration: absent = no limit.
                let lane_count = match request.lane_count {
                    Some(0) => {
                        return Err(tonic::Status::invalid_argument(
                            "lane_count 0 is invalid; omit the field to declare no limit",
                        ));
                    }
                    declared => declared.and_then(NonZeroU32::new),
                };

                let cla_request = Request {
                    address_type,
                    lane_count,
                    max_reception_size: max_bundle_size,
                };

                let cla = bpa
                    .register_cla(
                        &request.name,
                        factory_fn(cla_request, |cx: Context| {
                            Ok(Arc::new(Cla {
                                sink: cx.sink,
                                node_ids: cx.node_ids,
                                terms: cx.terms,
                                proxy: Once::new(),
                            }))
                        }),
                    )
                    .await
                    .map_err(|e| tonic::Status::from_error(e.into()))?;

                let response = RegisterClaResponse {
                    node_ids: Vec::<NodeId>::from(cla.node_ids.as_ref())
                        .into_iter()
                        .map(|node_id| node_id.to_string())
                        .collect(),
                    // Absent on the wire = no negotiated cap.
                    max_bundle_size: cla.terms.max_reception_size.map(NonZeroU64::get),
                };
                registered = Some(cla);

                Ok(bpa_to_cla::Msg::Register(response))
            }
            _ => {
                warn!("CLA sent incorrect message: {msg:?}");
                Err(tonic::Status::internal(format!(
                    "Unexpected response: {msg:?}"
                )))
            }
        }
    })
    .await;

    if let Err(e) = result {
        warn!("CLA registration failed: {e}");
        return;
    }

    let Some(cla) = registered else {
        warn!("CLA registration completed without registering a CLA");
        return;
    };

    // Start the proxy for ongoing communication
    let handler = Box::new(Handler { cla: cla.clone() });
    cla.proxy
        .call_once(|| RpcProxy::run(channel_sender, channel_receiver, handler, Side::Server));
}

/// Create a new CLA gRPC service.
pub fn new_cla_service(
    bpa: &Arc<dyn hardy_bpa::bpa::BpaRegistration>,
    tasks: &hardy_async::TaskPool,
) -> cla_server::ClaServer<Service> {
    cla_server::ClaServer::new(Service {
        bpa: bpa.clone(),
        session_tasks: tasks.clone(),
        channel_size: 16,
    })
    .max_encoding_message_size(crate::MAX_MESSAGE_SIZE)
    .max_decoding_message_size(crate::MAX_MESSAGE_SIZE)
}

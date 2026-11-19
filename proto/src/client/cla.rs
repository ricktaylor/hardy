use core::num::{NonZeroU32, NonZeroU64};

// `Sink` is this module's own proxy sink, so the BPA's trait of that
// name is imported anonymously, for its methods alone.
use hardy_bpa::{
    cla::{BoxFactory, Cla, Context, Sink as _, Terms},
    node_ids::NodeIds,
};

use super::*;
use proto::cla::*;

async fn forward(
    cla: &dyn hardy_bpa::cla::Cla,
    mut request: ForwardBundleRequest,
) -> Result<ForwardBundleResponse, tonic::Status> {
    let cla_addr = request
        .address
        .ok_or(tonic::Status::invalid_argument("Missing address"))?
        .try_into()?;

    let bundle_id = hardy_bpv7::bundle::Id::from_key(&request.bundle_id)
        .map_err(|e| tonic::Status::invalid_argument(format!("Invalid bundle_id: {e}")))?;

    // The unary wire message already delivered the whole bundle, so it
    // reaches the CLA as a one-segment stream.
    let total_len = request.bundle.len() as u64;
    let result = match cla
        .forward(
            request.lane,
            &cla_addr,
            &bundle_id,
            total_len,
            &mut request.bundle,
        )
        .await
        .map_err(|e| tonic::Status::from_error(e.into()))?
    {
        hardy_bpa::cla::ForwardOutcome::Sent => forward_bundle_response::Result::Sent(()),
        hardy_bpa::cla::ForwardOutcome::Accepted => forward_bundle_response::Result::Accepted(()),
        hardy_bpa::cla::ForwardOutcome::NoNeighbour => {
            forward_bundle_response::Result::NoNeighbour(())
        }
    };

    Ok(ForwardBundleResponse {
        result: Some(result),
    })
}

struct Sink {
    proxy: RpcProxy<ClaToBpa, BpaToCla>,
}

impl Sink {
    async fn call(&self, msg: cla_to_bpa::Msg) -> hardy_bpa::cla::Result<bpa_to_cla::Msg> {
        match self.proxy.call(msg).await {
            Err(e) => Err(hardy_bpa::cla::Error::Internal(e.into())),
            Ok(None) => Err(hardy_bpa::cla::Error::Disconnected),
            Ok(Some(msg)) => Ok(msg),
        }
    }
}

#[async_trait]
impl hardy_bpa::cla::Sink for Sink {
    // The wire carries no streamed Dispatch message, so the segment stream
    // is accumulated, bounded by the transport cap, and sent as one unary
    // DispatchBundleRequest. See bpa/docs/streaming_pipeline_design.md for
    // the chunked form this is a step towards.
    async fn dispatch(
        &self,
        peer_node: Option<&hardy_bpv7::eid::NodeId>,
        peer_addr: Option<&hardy_bpa::cla::Address>,
        stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::stream::Segment>,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::Acceptance> {
        // The transport cap doubles as the pre-check from
        // `client::application::Sink::send`: an oversized bundle is refused
        // here instead of letting tonic break the gRPC stream, which would
        // cascade into `on_close` and unregister this CLA. Both pre-flight
        // failures answer `Refused`, a verdict rather than a sink fault.
        let bundle = match hardy_bpa::stream::concat_stream(stream, crate::MAX_PAYLOAD_SIZE).await {
            Ok(bundle) => bundle,
            Err(hardy_bpa::stream::ConcatError::Cancelled) => {
                debug!("Bundle stream cancelled mid-transfer, refused");
                return Ok(hardy_bpa::cla::Acceptance::Refused);
            }
            Err(hardy_bpa::stream::ConcatError::TooLarge { size, max }) => {
                debug!("Bundle exceeds the transport cap ({size} > {max}), refused");
                return Ok(hardy_bpa::cla::Acceptance::Refused);
            }
        };
        match self
            .call(cla_to_bpa::Msg::Dispatch(DispatchBundleRequest {
                bundle,
                peer_node_id: peer_node.map(|n| n.to_string()),
                peer_addr: peer_addr.map(|a| a.clone().into()),
            }))
            .await?
        {
            bpa_to_cla::Msg::Dispatch(response) => map_dispatch_verdict(response.verdict),
            msg => {
                warn!("Unexpected response: {msg:?}");
                Err(hardy_bpa::cla::Error::Internal(
                    tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
                ))
            }
        }
    }

    async fn add_peer(
        &self,
        cla_addr: hardy_bpa::cla::Address,
        node_ids: &[hardy_bpv7::eid::NodeId],
    ) -> hardy_bpa::cla::Result<bool> {
        match self
            .call(cla_to_bpa::Msg::AddPeer(AddPeerRequest {
                node_ids: node_ids.iter().map(|n| n.to_string()).collect(),
                address: Some(cla_addr.into()),
            }))
            .await?
        {
            bpa_to_cla::Msg::AddPeer(response) => Ok(response.added),
            msg => Err(hardy_bpa::cla::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            )),
        }
    }

    async fn remove_peer(
        &self,
        cla_addr: &hardy_bpa::cla::Address,
    ) -> hardy_bpa::cla::Result<bool> {
        match self
            .call(cla_to_bpa::Msg::RemovePeer(RemovePeerRequest {
                address: Some(cla_addr.clone().into()),
            }))
            .await?
        {
            bpa_to_cla::Msg::RemovePeer(response) => Ok(response.removed),
            msg => Err(hardy_bpa::cla::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            )),
        }
    }

    async fn report_transfer(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
        outcome: hardy_bpa::cla::TransferOutcome,
    ) -> hardy_bpa::cla::Result<()> {
        let outcome = match outcome {
            hardy_bpa::cla::TransferOutcome::Completed => {
                transfer_outcome_request::Outcome::Completed(())
            }
            hardy_bpa::cla::TransferOutcome::Failed => transfer_outcome_request::Outcome::Failed(
                tonic::Status::aborted("Transfer failed").into(),
            ),
        };

        match self
            .call(cla_to_bpa::Msg::TransferOutcome(TransferOutcomeRequest {
                bundle_id: bundle_id.to_key(),
                outcome: Some(outcome),
            }))
            .await?
        {
            bpa_to_cla::Msg::TransferOutcome(_) => Ok(()),
            msg => Err(hardy_bpa::cla::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            )),
        }
    }

    async fn unregister(&self) {
        self.proxy.shutdown().await;
    }
}

// The CLA the session owns, shared with the proxy's handler.
//
// The CLA is built from a context carrying the sink the proxy stands
// behind, so the proxy exists first and learns the CLA it serves here.
// The handler holds the only strong reference: the session owns the CLA
// it registered, and drops it on close.
type SessionCla = Arc<Mutex<Option<Arc<dyn Cla>>>>;

struct Handler {
    cla: SessionCla,
}

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = cla_to_bpa::Msg;
    type RMsg = bpa_to_cla::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        match msg {
            bpa_to_cla::Msg::Forward(request) => {
                let cla = self.cla.lock().clone();
                if let Some(cla) = cla {
                    match forward(cla.as_ref(), request).await {
                        Ok(msg) => Some(cla_to_bpa::Msg::Forward(msg)),
                        Err(e) => Some(cla_to_bpa::Msg::Status(e.into())),
                    }
                } else {
                    Some(cla_to_bpa::Msg::Status(
                        tonic::Status::unavailable("CLA has disconnected").into(),
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
        let cla = self.cla.lock().take();
        if let Some(cla) = cla
            && let Err(e) = cla.unregister().await
        {
            warn!("CLA unregister failed: {e}");
        }
    }
}

pub async fn register_cla(
    grpc_addr: String,
    name: &str,
    factory: BoxFactory<'_>,
) -> hardy_bpa::cla::Result<()> {
    let mut cla_client = cla_client::ClaClient::connect(grpc_addr.clone())
        .await
        .map_err(|e| {
            error!("Failed to connect to gRPC server '{grpc_addr}': {e}");
            hardy_bpa::cla::Error::Internal(e.into())
        })?
        .max_encoding_message_size(crate::MAX_MESSAGE_SIZE)
        .max_decoding_message_size(crate::MAX_MESSAGE_SIZE);

    let (mut channel_sender, rx) = tokio::sync::mpsc::channel(16);

    let mut channel_receiver = cla_client
        .register(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .map_err(|e| {
            error!("CLA Registration failed: {e}");
            hardy_bpa::cla::Error::Internal(e.into())
        })?
        .into_inner();

    // Send the initial registration message, carrying what the CLA asks
    // of the BPA.
    let request = factory.request();
    let response = match RpcProxy::send(
        &mut channel_sender,
        &mut channel_receiver,
        cla_to_bpa::Msg::Register(RegisterClaRequest {
            name: name.to_string(),
            address_type: request.address_type.map(|a| {
                match a {
                    hardy_bpa::cla::AddressType::Tcp => ClaAddressType::Tcp,
                    hardy_bpa::cla::AddressType::Private => ClaAddressType::Private,
                }
                .into()
            }),
            max_bundle_size: request.max_bundle_size.map(NonZeroU64::get),
            lane_count: request.lane_count.map(NonZeroU32::get),
        }),
    )
    .await
    .map_err(|e| {
        error!("Failed to send registration: {e}");
        hardy_bpa::cla::Error::Internal(e.into())
    })? {
        None => return Err(hardy_bpa::cla::Error::Disconnected),
        Some(bpa_to_cla::Msg::Register(response)) => response,
        Some(msg) => {
            error!("CLA Registration failed: Unexpected response: {msg:?}");
            return Err(hardy_bpa::cla::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            ));
        }
    };

    let node_id_count = response.node_ids.len();
    let node_ids = response
        .node_ids
        .into_iter()
        .try_fold(Vec::with_capacity(node_id_count), |mut v, node_id| {
            v.push(node_id.parse::<hardy_bpv7::eid::NodeId>()?);
            Ok::<_, hardy_bpv7::eid::Error>(v)
        })
        .map_err(|e| {
            error!("Failed to parse node IDs in response: {e}");
            hardy_bpa::cla::Error::Internal(e.into())
        })?;
    let node_ids = NodeIds::try_from(node_ids.as_slice()).map_err(|e| {
        error!("Far BPA reported unusable node IDs: {e}");
        hardy_bpa::cla::Error::Internal(e.into())
    })?;

    // The terms the far BPA settled. An absent cap means none was
    // negotiated: a far BPA that predates the field (its cap is
    // unknowable) or one with no size policy, so the CLA keeps its own
    // declared limit. A nonsensical explicit 0 is tolerated as absent.
    // Lanes are the CLA's own count, which the BPA takes as asked.
    let terms = Terms {
        lane_count: request.lane_count,
        max_bundle_size: response.max_bundle_size.and_then(NonZeroU64::new),
    };

    let session: SessionCla = Arc::new(Mutex::new(None));
    let proxy = RpcProxy::run(
        channel_sender,
        channel_receiver,
        Box::new(Handler {
            cla: session.clone(),
        }),
        Side::Client,
    );
    let sink = Arc::new(Sink { proxy });

    // The CLA runs against the proxy from here, so a build or
    // registration failure takes the session down with it.
    let registered = async {
        let cla = factory.build(Context::new(sink.clone(), Arc::new(node_ids), terms))?;
        cla.register().await?;
        Ok(cla)
    }
    .await;

    match registered {
        Ok(cla) => {
            *session.lock() = Some(cla);
            info!("Proxy CLA {name} registered");
            Ok(())
        }
        Err(e) => {
            sink.unregister().await;
            Err(e)
        }
    }
}

// Matched on the raw field, not the prost getter: the getter collapses
// unknown values to `Unspecified`, which would turn a future verdict
// variant into a silent acceptance (the CLA acks, the peer deletes its
// copy, the BPA stored nothing).
fn map_dispatch_verdict(verdict: i32) -> hardy_bpa::cla::Result<hardy_bpa::cla::Acceptance> {
    match DispatchVerdict::try_from(verdict) {
        // Unspecified = a BPA predating the verdict field, which only
        // responded on acceptance.
        Ok(DispatchVerdict::Accepted) | Ok(DispatchVerdict::Unspecified) => {
            Ok(hardy_bpa::cla::Acceptance::Accepted)
        }
        Ok(DispatchVerdict::Refused) => Ok(hardy_bpa::cla::Acceptance::Refused),
        // Fail closed on a verdict this client does not know.
        Err(_) => Err(hardy_bpa::cla::Error::Internal(
            tonic::Status::unimplemented("unknown dispatch verdict").into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // An unknown wire verdict must fail closed, never decode as Accepted:
    // an acknowledged transfer the BPA never stored is silent bundle loss.
    #[test]
    fn unknown_dispatch_verdict_fails_closed() {
        assert!(matches!(
            map_dispatch_verdict(DispatchVerdict::Accepted as i32),
            Ok(hardy_bpa::cla::Acceptance::Accepted)
        ));
        assert!(matches!(
            map_dispatch_verdict(DispatchVerdict::Unspecified as i32),
            Ok(hardy_bpa::cla::Acceptance::Accepted)
        ));
        assert!(matches!(
            map_dispatch_verdict(DispatchVerdict::Refused as i32),
            Ok(hardy_bpa::cla::Acceptance::Refused)
        ));
        assert!(map_dispatch_verdict(99).is_err());
    }
}

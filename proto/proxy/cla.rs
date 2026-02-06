use super::*;
use crate::cla::*;

async fn forward(
    cla: &dyn hardy_bpa::cla::Cla,
    request: ForwardBundleRequest,
) -> Result<ForwardBundleResponse, tonic::Status> {
    let cla_addr = request
        .address
        .ok_or(tonic::Status::invalid_argument("Missing address"))?
        .try_into()?;

    let result = match cla
        .forward(request.queue, &cla_addr, request.bundle)
        .await
        .map_err(|e| tonic::Status::from_error(e.into()))?
    {
        hardy_bpa::cla::ForwardBundleResult::Sent => forward_bundle_response::Result::Sent(()),
        hardy_bpa::cla::ForwardBundleResult::NoNeighbour => {
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
    async fn dispatch(
        &self,
        bundle: hardy_bpa::Bytes,
        peer_node: Option<&hardy_bpv7::eid::NodeId>,
        peer_addr: Option<&hardy_bpa::cla::ClaAddress>,
    ) -> hardy_bpa::cla::Result<()> {
        match self
            .call(cla_to_bpa::Msg::Dispatch(DispatchBundleRequest {
                bundle,
                peer_node_id: peer_node.map(|n| n.to_string()),
                peer_addr: peer_addr.map(|a| a.clone().into()),
            }))
            .await?
        {
            bpa_to_cla::Msg::Dispatch(_) => Ok(()),
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
        node_id: hardy_bpv7::eid::NodeId,
        cla_addr: hardy_bpa::cla::ClaAddress,
    ) -> hardy_bpa::cla::Result<bool> {
        match self
            .call(cla_to_bpa::Msg::AddPeer(AddPeerRequest {
                node_id: node_id.into(),
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
        node_id: hardy_bpv7::eid::NodeId,
        cla_addr: &hardy_bpa::cla::ClaAddress,
    ) -> hardy_bpa::cla::Result<bool> {
        match self
            .call(cla_to_bpa::Msg::RemovePeer(RemovePeerRequest {
                node_id: node_id.into(),
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

    async fn unregister(&self) {
        match self
            .call(cla_to_bpa::Msg::Unregister(UnregisterClaRequest {}))
            .await
        {
            Ok(bpa_to_cla::Msg::Unregister(_)) => {}
            Ok(msg) => {
                warn!("Unexpected response: {msg:?}");
            }
            Err(e) => {
                error!("Failed to request unregistration: {e}");
            }
        }

        self.proxy.close().await;
    }
}

struct Handler {
    cla: Weak<dyn hardy_bpa::cla::Cla>,
}

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = cla_to_bpa::Msg;
    type RMsg = bpa_to_cla::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        match msg {
            bpa_to_cla::Msg::Forward(request) => {
                if let Some(cla) = self.cla.upgrade() {
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
            bpa_to_cla::Msg::Unregister(_) => {
                if let Some(cla) = self.cla.upgrade() {
                    cla.on_unregister().await;
                }
                Some(cla_to_bpa::Msg::OnUnregister(OnUnregisterClaResponse {}))
            }
            _ => {
                warn!("Ignoring unsolicited response: {msg:?}");
                None
            }
        }
    }

    async fn on_close(&self) {
        if let Some(cla) = self.cla.upgrade() {
            cla.on_unregister().await;
        }
    }
}

pub async fn register_cla(
    grpc_addr: String,
    name: String,
    address_type: Option<hardy_bpa::cla::ClaAddressType>,
    cla: Arc<dyn hardy_bpa::cla::Cla>,
) -> hardy_bpa::cla::Result<Vec<hardy_bpv7::eid::NodeId>> {
    let mut cla_client = cla_client::ClaClient::connect(grpc_addr.clone())
        .await
        .map_err(|e| {
            error!("Failed to connect to gRPC server '{grpc_addr}': {e}");
            hardy_bpa::cla::Error::Internal(e.into())
        })?;

    // Create a channel for sending messages to the service.
    let (mut channel_sender, rx) = tokio::sync::mpsc::channel(16);

    // Call the service's streaming method
    let mut channel_receiver = cla_client
        .register(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .map_err(|e| {
            error!("CLA Registration failed: {e}");
            hardy_bpa::cla::Error::Internal(e.into())
        })?
        .into_inner();

    // Send the initial registration message.
    let response = match RpcProxy::send(
        &mut channel_sender,
        &mut channel_receiver,
        cla_to_bpa::Msg::Register(RegisterClaRequest {
            name: name.clone(),
            address_type: address_type.map(|a| {
                match a {
                    hardy_bpa::cla::ClaAddressType::Tcp => ClaAddressType::Tcp,
                    hardy_bpa::cla::ClaAddressType::Private => ClaAddressType::Private,
                }
                .into()
            }),
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

    let node_ids = response
        .node_ids
        .into_iter()
        .try_fold(Vec::new(), |mut v, node_id| {
            v.push(node_id.parse::<hardy_bpv7::eid::NodeId>()?);
            Ok::<_, hardy_bpv7::eid::Error>(v)
        })
        .map_err(|e| {
            error!("Failed to parse node IDs in response: {e}");
            hardy_bpa::cla::Error::Internal(e.into())
        })?;

    let handler = Box::new(Handler {
        cla: Arc::downgrade(&cla),
    });

    // Start the proxy
    let proxy = RpcProxy::run(channel_sender, channel_receiver, handler);

    // Call on_register()
    cla.on_register(Box::new(Sink { proxy }), node_ids.as_slice())
        .await;

    info!("Proxy CLA {name} started");
    Ok(node_ids)
}

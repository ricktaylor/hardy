use std::sync::{Arc, Weak};

use hardy_async::{CancellationToken, async_trait};
use hardy_bpa::cla::{self, Cla, ClaAddressType, ClaContext, ForwardBundleResult, context::PeerOp};
use hardy_bpv7::eid::NodeId;
use tracing::{error, info, warn};

use crate::proto::cla::{
    AddPeerRequest, DispatchBundleRequest, ForwardBundleRequest, ForwardBundleResponse,
    RegisterClaRequest, RemovePeerRequest, bpa_to_cla, cla_client, cla_to_bpa,
    forward_bundle_response,
};
use crate::proxy::{ProxyHandler, RpcProxy};

async fn forward(
    cla: &dyn Cla,
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
        ForwardBundleResult::Sent => forward_bundle_response::Result::Sent(()),
        ForwardBundleResult::NoNeighbour => forward_bundle_response::Result::NoNeighbour(()),
    };

    Ok(ForwardBundleResponse {
        result: Some(result),
    })
}

struct Handler {
    cla: Weak<dyn Cla>,
    shutdown: CancellationToken,
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
            _ => {
                warn!("Ignoring unsolicited response: {msg:?}");
                None
            }
        }
    }

    async fn on_close(&self) {
        self.shutdown.cancel();
        if let Some(cla) = self.cla.upgrade() {
            cla.on_unregister().await;
        }
    }
}

pub async fn register_cla(
    grpc_addr: String,
    name: String,
    cla: Arc<dyn Cla>,
) -> cla::Result<Vec<NodeId>> {
    let mut cla_client = cla_client::ClaClient::connect(grpc_addr.clone())
        .await
        .map_err(|e| {
            error!("Failed to connect to gRPC server '{grpc_addr}': {e}");
            cla::Error::Internal(e.into())
        })?;

    let (mut channel_sender, rx) = tokio::sync::mpsc::channel(16);

    let mut channel_receiver = cla_client
        .register(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .map_err(|e| {
            error!("CLA Registration failed: {e}");
            cla::Error::Internal(e.into())
        })?
        .into_inner();

    let response = match RpcProxy::send(
        &mut channel_sender,
        &mut channel_receiver,
        cla_to_bpa::Msg::Register(RegisterClaRequest {
            name: name.clone(),
            address_type: cla.address_type().map(|a| {
                use crate::proto::cla::ClaAddressType as ProtoClaAddressType;
                match a {
                    ClaAddressType::Tcp => ProtoClaAddressType::Tcp,
                    ClaAddressType::Private => ProtoClaAddressType::Private,
                }
                .into()
            }),
        }),
    )
    .await
    .map_err(|e| {
        error!("Failed to send registration: {e}");
        cla::Error::Internal(e.into())
    })? {
        None => return Err(cla::Error::Disconnected),
        Some(bpa_to_cla::Msg::Register(response)) => response,
        Some(msg) => {
            error!("CLA Registration failed: Unexpected response: {msg:?}");
            return Err(cla::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            ));
        }
    };

    let node_ids = response
        .node_ids
        .into_iter()
        .try_fold(Vec::new(), |mut v, node_id| {
            v.push(node_id.parse::<NodeId>()?);
            Ok::<_, hardy_bpv7::eid::Error>(v)
        })
        .map_err(|e| {
            error!("Failed to parse node IDs in response: {e}");
            cla::Error::Internal(e.into())
        })?;

    let (ingress_tx, ingress_rx) = flume::bounded(16);
    let (peer_tx, peer_rx) = flume::unbounded();
    let shutdown = hardy_async::CancellationToken::new();
    let ctx = ClaContext::new(ingress_tx, peer_tx, shutdown.clone());

    let handler = Box::new(Handler {
        cla: Arc::downgrade(&cla),
        shutdown,
    });

    let proxy = Arc::new(RpcProxy::run(channel_sender, channel_receiver, handler));

    // Bridge ingress bundles to gRPC dispatch calls
    let proxy_for_ingress = proxy.clone();
    tokio::spawn(async move {
        while let Ok(msg) = ingress_rx.recv_async().await {
            let grpc_msg = cla_to_bpa::Msg::Dispatch(DispatchBundleRequest {
                bundle: msg.data,
                peer_node_id: msg.peer_node.map(|n| n.to_string()),
                peer_addr: msg.peer_addr.map(|a| a.into()),
            });
            if proxy_for_ingress.call(grpc_msg).await.is_err() {
                break;
            }
        }
    });

    // Bridge peer ops to gRPC calls
    let proxy_for_peers = proxy.clone();
    tokio::spawn(async move {
        while let Ok(op) = peer_rx.recv_async().await {
            let grpc_msg = match op {
                PeerOp::Add(addr, ids) => cla_to_bpa::Msg::AddPeer(AddPeerRequest {
                    node_ids: ids.iter().map(|n| n.to_string()).collect(),
                    address: Some(addr.into()),
                }),
                PeerOp::Remove(addr) => cla_to_bpa::Msg::RemovePeer(RemovePeerRequest {
                    address: Some(addr.into()),
                }),
            };
            if proxy_for_peers.call(grpc_msg).await.is_err() {
                break;
            }
        }
    });

    cla.on_register(ctx, node_ids.as_slice()).await;

    info!("Proxy CLA {name} started");
    Ok(node_ids)
}

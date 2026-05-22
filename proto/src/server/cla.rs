use std::sync::Arc;

use hardy_async::sync::spin::{Mutex, Once};
use hardy_bpa::Bytes;
use hardy_bpa::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::cla::{self, Cla, ClaAddress, ClaAddressType, ForwardBundleResult, Sink};
use hardy_bpv7::eid::NodeId;
use tonic::Status;
use tracing::{error, warn};

use crate::proto::cla::{
    AddPeerRequest, AddPeerResponse, BpaToCla, ClaAddressType as ProtoClaAddressType, ClaToBpa,
    DispatchBundleRequest, DispatchBundleResponse, ForwardBundleRequest, RegisterClaResponse,
    RemovePeerRequest, RemovePeerResponse, bpa_to_cla, cla_server, cla_to_bpa,
    forward_bundle_response,
};
use crate::proxy::{ProxyHandler, RpcProxy};

type ClaLink = Arc<dyn Sink>;

struct ClaProxy {
    sink: Mutex<Option<ClaLink>>,
    proxy: Once<RpcProxy<Result<BpaToCla, Status>, ClaToBpa>>,
    address_type: std::sync::OnceLock<Option<ClaAddressType>>,
}

impl ClaProxy {
    fn sink(&self) -> Result<ClaLink, Status> {
        self.sink
            .lock()
            .clone()
            .ok_or(Status::unavailable("Unregistered"))
    }

    async fn call(&self, msg: bpa_to_cla::Msg) -> cla::Result<cla_to_bpa::Msg> {
        let proxy = self.proxy.get().ok_or_else(|| {
            error!("call made before on_register!");
            cla::Error::Disconnected
        })?;

        match proxy.call(msg).await {
            Ok(None) => Err(cla::Error::Disconnected),
            Ok(Some(msg)) => Ok(msg),
            Err(e) => Err(cla::Error::Internal(e.into())),
        }
    }

    async fn dispatch(&self, request: DispatchBundleRequest) -> Result<bpa_to_cla::Msg, Status> {
        let peer_node: Option<NodeId> = request
            .peer_node_id
            .map(|s| {
                s.parse()
                    .map_err(|e| Status::invalid_argument(format!("Invalid peer_node_id: {e}")))
            })
            .transpose()?;

        let peer_addr: Option<ClaAddress> = request.peer_addr.map(|a| a.try_into()).transpose()?;

        self.sink()?
            .dispatch(request.bundle, peer_node.as_ref(), peer_addr.as_ref())
            .await
            .map(|_| bpa_to_cla::Msg::Dispatch(DispatchBundleResponse {}))
            .map_err(|e| Status::from_error(e.into()))
    }

    async fn add_peer(&self, request: AddPeerRequest) -> Result<bpa_to_cla::Msg, Status> {
        let node_ids = request
            .node_ids
            .into_iter()
            .map(|s| {
                s.parse()
                    .map_err(|e| Status::invalid_argument(format!("Invalid node id: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let cla_addr = request
            .address
            .ok_or(Status::invalid_argument("Missing address"))?
            .try_into()?;

        self.sink()?
            .add_peer(cla_addr, &node_ids)
            .await
            .map(|added| bpa_to_cla::Msg::AddPeer(AddPeerResponse { added }))
            .map_err(|e| Status::from_error(e.into()))
    }

    async fn remove_peer(&self, request: RemovePeerRequest) -> Result<bpa_to_cla::Msg, Status> {
        let cla_addr = request
            .address
            .ok_or(Status::invalid_argument("Missing address"))?
            .try_into()?;

        self.sink()?
            .remove_peer(&cla_addr)
            .await
            .map(|removed| bpa_to_cla::Msg::RemovePeer(RemovePeerResponse { removed }))
            .map_err(|e| Status::from_error(e.into()))
    }

    async fn unregister(&self) {
        let sink = self.sink.lock().take();
        if let Some(sink) = sink {
            sink.unregister().await;
        }
    }
}

#[async_trait]
impl Cla for ClaProxy {
    async fn on_register(&self, sink: Box<dyn Sink>, _node_ids: &[NodeId]) {
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

    fn address_type(&self) -> Option<ClaAddressType> {
        self.address_type.get().copied().flatten()
    }

    async fn forward(
        &self,
        queue: Option<u32>,
        cla_addr: &ClaAddress,
        bundle: Bytes,
    ) -> cla::Result<ForwardBundleResult> {
        match self
            .call(bpa_to_cla::Msg::Forward(ForwardBundleRequest {
                bundle,
                address: Some(cla_addr.clone().into()),
                queue,
            }))
            .await?
        {
            cla_to_bpa::Msg::Forward(response) => match response.result {
                None => Err(cla::Error::Internal(
                    Status::internal("Invalid result code").into(),
                )),
                Some(forward_bundle_response::Result::Sent(_)) => Ok(ForwardBundleResult::Sent),
                Some(forward_bundle_response::Result::NoNeighbour(_)) => {
                    Ok(ForwardBundleResult::NoNeighbour)
                }
            },
            msg => {
                warn!("Unexpected response: {msg:?}");
                Err(cla::Error::Internal(
                    Status::internal(format!("Unexpected response: {msg:?}")).into(),
                ))
            }
        }
    }
}

struct Handler {
    cla: Arc<ClaProxy>,
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
        self.cla.unregister().await;
        if let Some(proxy) = self.cla.proxy.get() {
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
impl cla_server::Cla for Service {
    type RegisterStream = tokio_stream::wrappers::ReceiverStream<Result<BpaToCla, Status>>;

    async fn register(
        &self,
        request: tonic::Request<tonic::Streaming<ClaToBpa>>,
    ) -> Result<tonic::Response<Self::RegisterStream>, Status> {
        let (channel_sender, rx) = tokio::sync::mpsc::channel(self.channel_size);
        let channel_receiver = request.into_inner();

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
    mut channel_sender: tokio::sync::mpsc::Sender<Result<BpaToCla, Status>>,
    mut channel_receiver: tonic::Streaming<ClaToBpa>,
    bpa: Arc<dyn BpaRegistration>,
) {
    let cla = Arc::new(ClaProxy {
        sink: Mutex::new(None),
        proxy: Once::new(),
        address_type: std::sync::OnceLock::new(),
    });

    let result = RpcProxy::recv(&mut channel_sender, &mut channel_receiver, |msg| async {
        match msg {
            cla_to_bpa::Msg::Register(request) => {
                let address_type =
                    request
                        .address_type
                        .map(|address_type| match address_type.try_into() {
                            Ok(ProtoClaAddressType::Tcp) => ClaAddressType::Tcp,
                            Err(_) | Ok(ProtoClaAddressType::Private) => ClaAddressType::Private,
                        });
                let _ = cla.address_type.set(address_type);
                let node_ids = bpa
                    .register_cla(request.name, cla.clone(), None)
                    .await
                    .map_err(|e| Status::from_error(e.into()))?
                    .into_iter()
                    .map(|node_id| node_id.to_string())
                    .collect();

                Ok(bpa_to_cla::Msg::Register(RegisterClaResponse { node_ids }))
            }
            _ => {
                warn!("CLA sent incorrect message: {msg:?}");
                Err(Status::internal(format!("Unexpected response: {msg:?}")))
            }
        }
    })
    .await;

    if let Err(e) = result {
        warn!("CLA registration failed: {e}");
        return;
    }

    let handler = Box::new(Handler { cla: cla.clone() });
    cla.proxy
        .call_once(|| RpcProxy::run(channel_sender, channel_receiver, handler));
}

/// Create a new CLA gRPC service.
pub fn new_cla_service(
    bpa: &Arc<dyn BpaRegistration>,
    tasks: &hardy_async::TaskPool,
) -> cla_server::ClaServer<Service> {
    cla_server::ClaServer::new(Service {
        bpa: bpa.clone(),
        session_tasks: tasks.clone(),
        channel_size: 16,
    })
}

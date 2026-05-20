use std::sync::Arc;

use hardy_async::async_trait;
use hardy_async::sync::spin::{Mutex, Once};
use hardy_bpa::Bytes;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::cla::{
    self, Cla as ClaTrait, ClaAddress, ClaAddressType as BpaClaAddressType, ClaContext,
    ForwardBundleResult,
};
use hardy_bpv7::eid::NodeId;
use tracing::{error, warn};

use crate::proto::cla::{
    AddPeerRequest, AddPeerResponse, BpaToCla, ClaAddressType, ClaToBpa, DispatchBundleRequest,
    DispatchBundleResponse, ForwardBundleRequest, RegisterClaResponse, RemovePeerRequest,
    RemovePeerResponse, bpa_to_cla, cla_server, cla_to_bpa, forward_bundle_response,
};
use crate::proxy::{ProxyHandler, RpcProxy};

struct Cla {
    ctx: Mutex<Option<ClaContext>>,
    proxy: Once<RpcProxy<Result<BpaToCla, tonic::Status>, ClaToBpa>>,
    address_type: std::sync::OnceLock<Option<BpaClaAddressType>>,
}

impl Cla {
    fn ctx(&self) -> Result<ClaContext, tonic::Status> {
        self.ctx
            .lock()
            .clone()
            .ok_or(tonic::Status::unavailable("Unregistered"))
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

    async fn dispatch(
        &self,
        request: DispatchBundleRequest,
    ) -> Result<bpa_to_cla::Msg, tonic::Status> {
        let peer_node: Option<NodeId> = request
            .peer_node_id
            .map(|s| {
                s.parse().map_err(|e| {
                    tonic::Status::invalid_argument(format!("Invalid peer_node_id: {e}"))
                })
            })
            .transpose()?;

        let peer_addr: Option<ClaAddress> = request.peer_addr.map(|a| a.try_into()).transpose()?;

        self.ctx()?
            .dispatch(request.bundle, peer_node, peer_addr)
            .await;

        Ok(bpa_to_cla::Msg::Dispatch(DispatchBundleResponse {}))
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

        self.ctx()?.add_peer(cla_addr, node_ids);

        Ok(bpa_to_cla::Msg::AddPeer(AddPeerResponse { added: true }))
    }

    async fn remove_peer(
        &self,
        request: RemovePeerRequest,
    ) -> Result<bpa_to_cla::Msg, tonic::Status> {
        let cla_addr: ClaAddress = request
            .address
            .ok_or(tonic::Status::invalid_argument("Missing address"))?
            .try_into()?;

        self.ctx()?.remove_peer(cla_addr);

        Ok(bpa_to_cla::Msg::RemovePeer(RemovePeerResponse {
            removed: true,
        }))
    }

    fn disconnect(&self) {
        self.ctx.lock().take();
    }
}

#[async_trait]
impl ClaTrait for Cla {
    async fn on_register(&self, ctx: ClaContext, _node_ids: &[NodeId]) {
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

    fn address_type(&self) -> Option<BpaClaAddressType> {
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
                    tonic::Status::internal("Invalid result code").into(),
                )),
                Some(forward_bundle_response::Result::Sent(_)) => Ok(ForwardBundleResult::Sent),
                Some(forward_bundle_response::Result::NoNeighbour(_)) => {
                    Ok(ForwardBundleResult::NoNeighbour)
                }
            },
            msg => {
                warn!("Unexpected response: {msg:?}");
                Err(cla::Error::Internal(
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
        self.cla.disconnect();
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
    type RegisterStream = tokio_stream::wrappers::ReceiverStream<Result<BpaToCla, tonic::Status>>;

    async fn register(
        &self,
        request: tonic::Request<tonic::Streaming<ClaToBpa>>,
    ) -> Result<tonic::Response<Self::RegisterStream>, tonic::Status> {
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
    mut channel_sender: tokio::sync::mpsc::Sender<Result<BpaToCla, tonic::Status>>,
    mut channel_receiver: tonic::Streaming<ClaToBpa>,
    bpa: Arc<dyn BpaRegistration>,
) {
    let cla = Arc::new(Cla {
        ctx: Mutex::new(None),
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
                            Ok(ClaAddressType::Tcp) => BpaClaAddressType::Tcp,
                            Err(_) | Ok(ClaAddressType::Private) => BpaClaAddressType::Private,
                        });
                let _ = cla.address_type.set(address_type);
                let node_ids = bpa
                    .register_cla(request.name, cla.clone(), None)
                    .await
                    .map_err(|e| tonic::Status::from_error(e.into()))?
                    .into_iter()
                    .map(|node_id| node_id.to_string())
                    .collect();

                Ok(bpa_to_cla::Msg::Register(RegisterClaResponse { node_ids }))
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

    let handler = Box::new(Handler { cla: cla.clone() });
    cla.proxy
        .call_once(|| RpcProxy::run(channel_sender, channel_receiver, handler));
}

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

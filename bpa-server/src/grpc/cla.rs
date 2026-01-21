use super::*;
use hardy_bpa::async_trait;
use hardy_proto::{cla::*, proxy::*};

struct ClaInner {
    sink: Box<dyn hardy_bpa::cla::Sink>,
}

#[derive(Default)]
struct Cla {
    inner: std::sync::OnceLock<ClaInner>,
    proxy: std::sync::OnceLock<RpcProxy<Result<BpaToCla, tonic::Status>, ClaToBpa>>,
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
        request: DispatchBundleRequest,
    ) -> Result<bpa_to_cla::Msg, tonic::Status> {
        self.inner
            .wait()
            .sink
            .dispatch(request.bundle)
            .await
            .map(|_| bpa_to_cla::Msg::Dispatch(DispatchBundleResponse {}))
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    async fn add_peer(&self, request: AddPeerRequest) -> Result<bpa_to_cla::Msg, tonic::Status> {
        let node_id = request
            .node_id
            .parse()
            .map_err(|e| tonic::Status::invalid_argument(format!("Invalid endpoint id: {e}")))?;

        let cla_addr = request
            .address
            .ok_or(tonic::Status::invalid_argument("Missing address"))?
            .try_into()?;

        self.inner
            .wait()
            .sink
            .add_peer(node_id, cla_addr)
            .await
            .map(|added| bpa_to_cla::Msg::AddPeer(AddPeerResponse { added }))
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    async fn remove_peer(
        &self,
        request: RemovePeerRequest,
    ) -> Result<bpa_to_cla::Msg, tonic::Status> {
        let node_id = request
            .node_id
            .parse()
            .map_err(|e| tonic::Status::invalid_argument(format!("Invalid endpoint id: {e}")))?;

        let cla_addr = request
            .address
            .ok_or(tonic::Status::invalid_argument("Missing address"))?
            .try_into()?;

        self.inner
            .wait()
            .sink
            .remove_peer(node_id, &cla_addr)
            .await
            .map(|removed| bpa_to_cla::Msg::RemovePeer(RemovePeerResponse { removed }))
            .map_err(|e| tonic::Status::from_error(e.into()))
    }

    async fn unregister(&self) {
        if let Some(inner) = self.inner.get() {
            inner.sink.unregister().await
        }
    }
}

#[async_trait]
impl hardy_bpa::cla::Cla for Cla {
    async fn on_register(
        &self,
        sink: Box<dyn hardy_bpa::cla::Sink>,
        _node_ids: &[hardy_bpv7::eid::NodeId],
    ) {
        // Ensure single initialization
        self.inner.get_or_init(|| ClaInner { sink });
    }

    async fn on_unregister(&self) {
        match self
            .call(bpa_to_cla::Msg::Unregister(UnregisterClaResponse {}))
            .await
        {
            Ok(cla_to_bpa::Msg::Unregister(_)) => {}
            Ok(msg) => {
                warn!("Unexpected response: {msg:?}");
            }
            Err(e) => {
                error!("Failed to notify CLA of unregistration: {e}");
            }
        }

        // Close the proxy, nothing else is going to be processed
        if let Some(proxy) = self.proxy.get() {
            proxy.close().await;
        }
    }

    async fn forward(
        &self,
        queue: Option<u32>,
        cla_addr: &hardy_bpa::cla::ClaAddress,
        bundle: hardy_bpa::Bytes,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        match self
            .call(bpa_to_cla::Msg::Forward(ForwardBundleRequest {
                bundle,
                address: Some(cla_addr.clone().into()),
                queue,
            }))
            .await?
        {
            cla_to_bpa::Msg::Forward(response) => match response.result {
                None => Err(hardy_bpa::cla::Error::Internal(
                    tonic::Status::internal("Invalid result code").into(),
                )),
                Some(forward_bundle_response::Result::Sent(_)) => {
                    Ok(hardy_bpa::cla::ForwardBundleResult::Sent)
                }
                Some(forward_bundle_response::Result::NoNeighbour(_)) => {
                    Ok(hardy_bpa::cla::ForwardBundleResult::NoNeighbour)
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
            cla_to_bpa::Msg::Unregister(_) => {
                self.cla.unregister().await;
                Ok(bpa_to_cla::Msg::Unregister(UnregisterClaResponse {}))
            }
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
        // Do nothing
    }
}

pub struct Service {
    bpa: Arc<hardy_bpa::bpa::Bpa>,
    channel_size: usize,
}

#[async_trait]
impl cla_server::Cla for Service {
    type RegisterStream = tokio_stream::wrappers::ReceiverStream<Result<BpaToCla, tonic::Status>>;

    async fn register(
        &self,
        request: tonic::Request<tonic::Streaming<ClaToBpa>>,
    ) -> Result<tonic::Response<Self::RegisterStream>, tonic::Status> {
        let (mut channel_sender, rx) = tokio::sync::mpsc::channel(self.channel_size);
        let mut channel_receiver = request.into_inner();

        let cla = Arc::new(Cla::default());

        RpcProxy::recv(&mut channel_sender, &mut channel_receiver, |msg| async {
            match msg {
                cla_to_bpa::Msg::Register(request) => {
                    // Register the CLA and respond
                    let node_ids = self
                        .bpa
                        .register_cla(
                            request.name,
                            request.address_type.map(|address_type| {
                                match address_type.try_into() {
                                    Ok(ClaAddressType::Tcp) => hardy_bpa::cla::ClaAddressType::Tcp,
                                    Err(_) | Ok(ClaAddressType::Private) => {
                                        hardy_bpa::cla::ClaAddressType::Private
                                    }
                                }
                            }),
                            cla.clone(),
                            None,
                        )
                        .await
                        .map_err(|e| tonic::Status::from_error(e.into()))?
                        .into_iter()
                        .map(|node_id| node_id.to_string())
                        .collect();

                    Ok(bpa_to_cla::Msg::Register(RegisterClaResponse { node_ids }))
                }
                _ => {
                    info!("CLA sent incorrect message: {msg:?}");
                    Err(tonic::Status::internal(format!(
                        "Unexpected response: {msg:?}"
                    )))
                }
            }
        })
        .await?;

        // Start the proxy
        let handler = Box::new(Handler { cla: cla.clone() });
        cla.proxy
            .get_or_init(|| RpcProxy::run(channel_sender, channel_receiver, handler));

        Ok(tonic::Response::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
    }
}

pub fn new_service(bpa: &Arc<hardy_bpa::bpa::Bpa>) -> cla_server::ClaServer<Service> {
    cla_server::ClaServer::new(Service {
        bpa: bpa.clone(),
        channel_size: 16,
    })
}

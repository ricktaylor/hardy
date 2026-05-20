use super::*;
use futures::StreamExt;
use hardy_bpa::cla::ClaContext;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_util::bytes::Bytes;
use tokio_util::codec::Decoder;

pub struct Listener {
    pub address: SocketAddr,
    pub framing: config::Framing,
    pub max_bundle_size: u64,
    pub ctx: ClaContext,
}

impl Listener {
    pub async fn listen(self, tasks: Arc<hardy_async::TaskPool>) {
        let Ok(listener) = TcpListener::bind(self.address)
            .await
            .inspect_err(|e| error!("Failed to bind listener on {}: {e}", self.address))
        else {
            return;
        };

        info!("Listening on {} ({:?} framing)", self.address, self.framing);

        loop {
            tokio::select! {
                result = listener.accept() => match result {
                    Ok((stream, remote_addr)) => {
                        debug!(%remote_addr, "Accepted connection");
                        let ctx = self.ctx.clone();
                        let framing = self.framing.clone();
                        let max_bundle_size = self.max_bundle_size;
                        let cancel = tasks.cancel_token().clone();
                        hardy_async::spawn!(tasks, "mtcp_rx", async move {
                            handle_connection(stream, remote_addr, framing, max_bundle_size, ctx, cancel).await;
                        });
                    }
                    Err(e) => {
                        warn!("Failed to accept connection: {e}");
                    }
                },
                _ = tasks.cancel_token().cancelled() => {
                    break;
                }
            }
        }
    }
}

async fn receive_loop<S>(
    framed: &mut S,
    remote_addr: SocketAddr,
    ctx: &ClaContext,
    peer_addr: &Option<hardy_bpa::cla::ClaAddress>,
    cancel: &hardy_async::CancellationToken,
) where
    S: StreamExt<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    loop {
        tokio::select! {
            result = framed.next() => match result {
                Some(Ok(bundle)) => {
                    debug!(%remote_addr, len = bundle.len(), "Received bundle");
                    ctx.dispatch(bundle, None, peer_addr.clone());
                }
                Some(Err(e)) => {
                    debug!(%remote_addr, "Connection error: {e}");
                    return;
                }
                None => {
                    debug!(%remote_addr, "Connection closed");
                    return;
                }
            },
            _ = cancel.cancelled() => {
                debug!(%remote_addr, "Connection cancelled");
                return;
            }
        }
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    remote_addr: SocketAddr,
    framing: config::Framing,
    max_bundle_size: u64,
    ctx: ClaContext,
    cancel: hardy_async::CancellationToken,
) {
    stream
        .set_nodelay(true)
        .inspect_err(|e| warn!("Failed to set TCP_NODELAY: {e}"))
        .ok();

    let peer_addr = Some(hardy_bpa::cla::ClaAddress::Tcp(remote_addr));

    match framing {
        config::Framing::Mtcp => {
            let mut framed = codec::MtcpCodec::new(max_bundle_size).framed(stream);
            receive_loop(&mut framed, remote_addr, &ctx, &peer_addr, &cancel).await;
        }
        config::Framing::Stcp => {
            let mut framed = codec::StcpCodec::new(max_bundle_size).framed(stream);
            receive_loop(&mut framed, remote_addr, &ctx, &peer_addr, &cancel).await;
        }
    }
}

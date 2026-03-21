use super::*;
use futures::StreamExt;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_util::bytes::Bytes;
use tokio_util::codec::Decoder;

pub struct Listener {
    pub address: SocketAddr,
    pub framing: config::Framing,
    pub max_bundle_size: u64,
    pub sink: Arc<dyn hardy_bpa::cla::Sink>,
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
                        let sink = self.sink.clone();
                        let framing = self.framing.clone();
                        let max_bundle_size = self.max_bundle_size;
                        let cancel = tasks.cancel_token().clone();
                        hardy_async::spawn!(tasks, "mtcp_rx", async move {
                            handle_connection(stream, remote_addr, framing, max_bundle_size, sink, cancel).await;
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
    sink: &Arc<dyn hardy_bpa::cla::Sink>,
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
                    if let Err(e) = sink.dispatch(bundle, None, peer_addr.as_ref()).await {
                        warn!(%remote_addr, "Dispatch failed: {e:?}");
                        return;
                    }
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
    sink: Arc<dyn hardy_bpa::cla::Sink>,
    cancel: hardy_async::CancellationToken,
) {
    let peer_addr = Some(hardy_bpa::cla::ClaAddress::Tcp(remote_addr));

    match framing {
        config::Framing::Mtcp => {
            let mut framed = codec::MtcpCodec::new(max_bundle_size).framed(stream);
            receive_loop(&mut framed, remote_addr, &sink, &peer_addr, &cancel).await;
        }
        config::Framing::Stcp => {
            let mut framed = codec::StcpCodec::new(max_bundle_size).framed(stream);
            receive_loop(&mut framed, remote_addr, &sink, &peer_addr, &cancel).await;
        }
    }
}

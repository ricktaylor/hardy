use super::*;
use futures::SinkExt;
use tokio::net::TcpStream;
use tokio_util::bytes::Bytes;
use tokio_util::codec::Decoder;

/// Send a single bundle to a remote peer via a fresh TCP connection.
///
/// Connect-per-bundle: opens a connection, sends the bundle, closes.
/// This is the simplest approach and is compatible with both ION (STCP)
/// and D3TN (MTCP) receivers.
pub async fn forward(
    remote_addr: &std::net::SocketAddr,
    framing: &config::Framing,
    bundle: Bytes,
) -> Result<(), std::io::Error> {
    let stream = TcpStream::connect(remote_addr)
        .await
        .inspect_err(|e| debug!("Failed to connect to {remote_addr}: {e}"))?;

    stream
        .set_nodelay(true)
        .inspect_err(|e| debug!("Failed to set TCP_NODELAY: {e}"))
        .ok();

    match framing {
        config::Framing::Mtcp => {
            let mut framed = codec::MtcpCodec::new(0).framed(stream);
            framed.send(bundle).await?;
            framed.close().await?;
        }
        config::Framing::Stcp => {
            let mut framed = codec::StcpCodec::new(0).framed(stream);
            framed.send(bundle).await?;
            framed.close().await?;
        }
    }

    Ok(())
}

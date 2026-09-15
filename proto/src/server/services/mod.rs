//! The four APIs, and the machinery they share: the session handshake and the
//! waits on a request stream.

pub mod cla;
pub mod endpoint;
pub mod routing;

use core::time::Duration;

use hardy_async::CancellationToken;
use hardy_bpv7::eid::Service;
use tokio::{
    sync::mpsc::{Receiver, WeakSender},
    time::sleep,
};
use tokio_stream::{Once, StreamExt, adapters::Chain, once, wrappers::ReceiverStream};
use tonic::{Code, Status, Streaming};
use tracing::warn;

use crate::{
    grammar::{Register, Unregister},
    server::watchdog::{Stage, Watchdog},
};

/// The response stream of a `Subscribe` call: the `Registration` event, then
/// the session's events.
pub type EventStream<T> = Chain<Once<Result<T, Status>>, ReceiverStream<Result<T, Status>>>;

/// Returns an [`EventStream`] that yields `first` before anything on
/// `events_rx`.
///
/// The `Registration` event cannot simply be sent on the channel first: the BPA
/// may announce parked bundles from inside its `register_*` call, so the
/// channel may already hold deliveries by the time the registration exists, and
/// the client must not see them before the token that lets it collect them.
pub fn prepend<T>(first: T, events_rx: Receiver<Result<T, Status>>) -> EventStream<T> {
    once(Ok(first)).chain(ReceiverStream::new(events_rx))
}

/// Reads the `Register` that must open a `Subscribe` stream.
///
/// # Errors
///
/// Returns `UNAVAILABLE` if `cancel` fires first, `DEADLINE_EXCEEDED` if
/// `handshake` elapses first, the stream's own status if it fails, and
/// `INVALID_ARGUMENT` if the stream ends or yields any other message first.
pub async fn expect_register<R: Register>(
    requests: &mut Streaming<R>,
    cancel: &CancellationToken,
    handshake: Duration,
) -> Result<R::Registration, Status> {
    let first = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(Status::unavailable("server shutting down")),
        message = requests.message() => message,
        _ = sleep(handshake) => {
            warn!("subscription handshake timed out");
            return Err(Status::deadline_exceeded("no Register"));
        }
    };
    first?
        .and_then(Register::into_register)
        .ok_or_else(|| Status::invalid_argument("the first message must be Register"))
}

/// Reads the message that must open a data-plane call, under the `handshake`
/// bound.
///
/// `what` names the expected message in the statuses, as `SendMetadata`.
///
/// # Errors
///
/// Returns `UNAVAILABLE` if `cancel` fires first, `DEADLINE_EXCEEDED` if
/// `handshake` elapses first, the stream's own status if it fails, and
/// `INVALID_ARGUMENT` if the stream ends first.
pub async fn expect_first<M>(
    requests: &mut Streaming<M>,
    cancel: &CancellationToken,
    handshake: Duration,
    what: &str,
) -> Result<M, Status> {
    let first = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(Status::unavailable("server shutting down")),
        message = requests.message() => message?,
        _ = sleep(handshake) => {
            warn!("{what} handshake timed out");
            return Err(Status::deadline_exceeded(format!("no {what}")));
        }
    };
    first.ok_or_else(|| Status::invalid_argument(format!("the first message must be {what}")))
}

/// Waits for the client to end its registration, by `Unregister` or by closing
/// its request stream.
///
/// Any other message is ignored; the first one is logged.
///
/// # Errors
///
/// Returns `ABORTED` if the stream fails.
pub async fn wait_for_unregister<R: Unregister>(requests: &mut Streaming<R>) -> Result<(), Status> {
    let mut warned = false;

    loop {
        match requests.message().await {
            Ok(Some(request)) if request.is_unregister() => return Ok(()),
            Ok(Some(_)) if !warned => {
                warned = true;
                warn!("ignoring unexpected message on the session stream");
            }
            Ok(Some(_)) => {}
            Ok(None) => return Ok(()),
            Err(e) => {
                warn!("subscription stream failed: {e}");
                return Err(Status::aborted("request stream failed"));
            }
        }
    }
}

/// Whether `status` ends the session, rather than just the exchange it was
/// raised on.
///
/// A stall and a closed registration both mean the session is going; anything
/// else is one exchange failing on a session that stays up. The client SDK
/// draws the same line.
pub fn is_disconnect(status: &Status) -> bool {
    matches!(status.code(), Code::DeadlineExceeded | Code::Unavailable)
}

/// Pushes `event` onto a session's event stream.
///
/// # Errors
///
/// Returns `UNAVAILABLE` if the session has ended, because `cancel` fired or
/// the stream is gone, and `DEADLINE_EXCEEDED` at [`Stage::Event`] if the
/// client leaves no room on the stream for the idle bound.
pub async fn send_event<T>(
    events_tx: &WeakSender<Result<T, Status>>,
    cancel: &CancellationToken,
    watchdog: &Watchdog,
    event: T,
) -> Result<(), Status> {
    let Some(events_tx) = events_tx.upgrade() else {
        return Err(Status::unavailable("registration closed"));
    };
    let permit = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(Status::unavailable("registration closed")),
        permit = events_tx.reserve() => {
            permit.map_err(|_| Status::unavailable("registration closed"))?
        }
        stalled = watchdog.idle(Stage::Event) => return Err(stalled),
    };
    permit.send(Ok(event));
    Ok(())
}

/// Forms the subject of a session token: the API and the service id, or
/// `dynamic` when the BPA chooses the service number.
fn token_subject(api: &str, service: Option<&Service>) -> String {
    match service {
        Some(Service::Ipn(n)) => format!("{api}:ipn:{n}"),
        Some(Service::Dtn(name)) => format!("{api}:dtn:{name}"),
        None => format!("{api}:dynamic"),
    }
}

#[cfg(test)]
pub mod tests {
    use core::future::Future;
    use std::{net::SocketAddr, sync::Arc, time::Duration};

    use hardy_bpa::{bpa::Bpa, node_ids::NodeIds};
    use hardy_bpv7::eid::{IpnNodeId, NodeId};
    use tokio::net::TcpListener;
    use tonic::transport::server::{Router, TcpIncoming};

    pub async fn timeout<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .expect("test timed out")
    }

    pub fn ipn1() -> NodeIds {
        NodeIds::try_from(
            [NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            })]
            .as_slice(),
        )
        .unwrap()
    }

    pub async fn build_bpa(node_ids: NodeIds, status_reports: bool) -> Arc<Bpa> {
        let bpa = Arc::new(
            Bpa::builder()
                .node_ids(node_ids)
                .status_reports(status_reports)
                .build()
                .await
                .unwrap(),
        );
        bpa.start(false).await;
        bpa
    }

    pub async fn serve(router: Router) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let incoming = TcpIncoming::from(listener).with_nodelay(Some(true));
        tokio::spawn(router.serve_with_incoming(incoming));
        address
    }
}

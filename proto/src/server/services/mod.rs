//! The four APIs, and the machinery they share: the session handshake and the
//! waits on a request stream.

pub mod cla;
pub mod endpoint;
pub mod routing;

use core::time::Duration;

use hardy_async::CancellationToken;
use hardy_bpv7::eid::{DtnNodeId, Service};
use tokio::{
    sync::{
        Semaphore, SemaphorePermit,
        mpsc::{Receiver, WeakSender},
    },
    time::sleep,
};
use tokio_stream::{Once, StreamExt, adapters::Chain, once, wrappers::ReceiverStream};
use tonic::{Code, Status, Streaming};
use tracing::{debug, warn};

use crate::{
    grammar::{Register, Unregister},
    timeouts::{Stage, Timeouts},
};

/// The longest name, in bytes, a client may register a component under.
///
/// The BPA holds the name for the life of the session and the server logs it,
/// and neither is the place to find out that a client sent a megabyte of it.
const MAX_NAME_LEN: usize = 256;

/// Returns `name`, having checked that it is one a client may register a
/// component under.
///
/// This is where a registration name is checked: the BPA takes it as a
/// registry key without one.
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if `name` is empty, longer than
/// [`MAX_NAME_LEN`] bytes, or carries a control character, which would let a
/// client choose the shape of a log line about it. `field` names the field in
/// the message, and no string the client sent is echoed back.
fn registered_name(field: &str, name: String) -> Result<String, Status> {
    if name.is_empty() {
        return Err(Status::invalid_argument(format!(
            "{field} must not be empty"
        )));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "{field} exceeds the maximum of {MAX_NAME_LEN} bytes"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(Status::invalid_argument(format!(
            "{field} must not carry control characters"
        )));
    }
    Ok(name)
}

/// Returns the `dtn` demux `name` as the service id to register under.
///
/// This is where the demux is checked: the BPA copies it into the node's
/// endpoint id without looking at it.
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if the demux is empty, which names the node
/// itself rather than a service on it, if it is longer than
/// [`MAX_NAME_LEN`] bytes, or if it is not a `dtn` service name.
fn dtn_service_id(name: String) -> Result<Service, Status> {
    let name = registered_name("Register.dtn", name)?;
    if !DtnNodeId::is_valid_service_name(&name) {
        return Err(Status::invalid_argument(
            "Register.dtn is not a valid dtn service name",
        ));
    }
    Ok(Service::Dtn(name.into()))
}

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
            // Nothing has authenticated yet, so this is not worth more than a
            // debug line: a peer can trigger it as often as it likes.
            debug!("subscription handshake timed out");
            return Err(Status::deadline_exceeded("timed out waiting for Register"));
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
            debug!("{what} handshake timed out");
            return Err(Status::deadline_exceeded(format!("timed out waiting for {what}")));
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
            Err(error) => {
                debug!("subscription stream failed: {error}");
                return Err(Status::aborted("request stream failed"));
            }
        }
    }
}

/// Takes a slot on `transfers`, the session's bound on the inbound transfers
/// it has open, waiting at most the idle bound for one to end.
///
/// A call parked here holds only its metadata, and the client is the one
/// waiting, so the wait is not charged against the session: it fails this
/// call alone.
///
/// # Errors
///
/// Returns `UNAVAILABLE` if the session ends first, and `RESOURCE_EXHAUSTED`
/// if no transfer ends within [`Limits::idle`](crate::server::Limits::idle).
pub async fn transfer_slot<'a>(
    transfers: &'a Semaphore,
    timeouts: &Timeouts,
) -> Result<SemaphorePermit<'a>, Status> {
    tokio::select! {
        biased;
        _ = timeouts.cancelled() => Err(Status::unavailable("registration closed")),
        permit = transfers.acquire() => {
            permit.map_err(|_| Status::unavailable("registration closed"))
        }
        () = sleep(timeouts.limits().idle) => {
            Err(Status::resource_exhausted("too many transfers open on this session"))
        }
    }
}

/// Whether `status` ends the session, rather than just the exchange it was
/// raised on.
///
/// A stall and a closed registration both mean the session is going; anything
/// else is one exchange failing on a session that stays up. The client SDK
/// draws the same line, and adds `UNAUTHENTICATED`, which a server raises
/// only before a session is found and so never inside one.
pub fn is_disconnect(status: &Status) -> bool {
    matches!(status.code(), Code::DeadlineExceeded | Code::Unavailable)
}

/// Pushes `event` onto a session's event stream.
///
/// # Errors
///
/// Returns `UNAVAILABLE` if the session has ended, or the stream is gone, and
/// `DEADLINE_EXCEEDED` at [`Stage::Event`] if the client leaves no room on the
/// stream for the idle bound.
pub async fn send_event<T>(
    events_tx: &WeakSender<Result<T, Status>>,
    timeouts: &Timeouts,
    event: T,
) -> Result<(), Status> {
    let Some(events_tx) = events_tx.upgrade() else {
        return Err(Status::unavailable("registration closed"));
    };
    let permit = timeouts
        .bound(Stage::Event, async {
            events_tx
                .reserve()
                .await
                .map_err(|_| Status::unavailable("registration closed"))
        })
        .await?;
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

    // The timeout only bounds a regression; correct code completes at once.
    // `tokio::time::timeout` is qualified because this helper takes its name.
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

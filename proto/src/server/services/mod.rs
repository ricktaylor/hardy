// The rpc services, one file per wire surface, with `application.rs` as
// the template. The shared machinery lives beside them in `server`:
// `subscribe`, `session`, `announce`, `adapter`. What is left here is
// what the surfaces share: the error-to-status mapping, the
// session-token naming, and the client's verdict on a collection.

pub mod application;
pub mod cla;
pub mod routing;
pub mod service;

use hardy_bpa::services::Error;
use hardy_bpv7::eid::Service;
use tonic::{Status, Streaming};
use tracing::{debug, error, warn};

use crate::{
    grammar::{Ack, Cancel},
    status::embed_service_error,
};

// The one point where BPA service errors become gRPC statuses. The
// typed discriminator is embedded so the SDK can recover the exact
// variant past the coarse code.
fn service_status(error: Error) -> Status {
    let status = match &error {
        Error::ServiceIdInUse(_) | Error::DuplicateBundle => {
            Status::already_exists(error.to_string())
        }
        Error::AdministrativeEndpoint(_)
        | Error::InvalidDestination(_)
        | Error::InvalidBundle(_)
        | Error::PayloadUnderrun { .. } => Status::invalid_argument(error.to_string()),
        Error::NodeId(_) => Status::failed_precondition(error.to_string()),
        // Three variants the BPA never constructs, mapped for
        // exhaustiveness alone (see [`docs/TODO.md`](../../docs/TODO.md)).
        Error::DtnInvalidServiceName(_) => Status::invalid_argument(error.to_string()),
        Error::NoIpnNodeId | Error::NoDtnNodeId => Status::failed_precondition(error.to_string()),
        Error::PayloadTooLarge { .. } | Error::PayloadUnaddressable { .. } => {
            Status::resource_exhausted(error.to_string())
        }
        Error::Disconnected => Status::unavailable("Unregistered"),
        Error::StreamCancelled => Status::cancelled(error.to_string()),
        Error::Dropped(_) => Status::aborted(error.to_string()),
        // The chain may carry host detail an untrusted peer must never
        // see, so it is logged here and the status left generic. The
        // embedded kind still tells the SDK it was internal.
        Error::Internal(e) => {
            error!("internal service error: {e}");
            Status::internal("internal error")
        }
    };
    embed_service_error(status, &error)
}

// The cleartext `sub` of a session token, for observability only. The
// surface is part of it because an application and a service can ask
// for the same service id, and are otherwise indistinguishable in a log
// line.
fn session_sub(surface: &str, service: Option<&Service>) -> String {
    match service {
        Some(Service::Ipn(n)) => format!("{surface}:ipn:{n}"),
        Some(Service::Dtn(name)) => format!("{surface}:dtn:{name}"),
        None => format!("{surface}:dynamic"),
    }
}

// The client's verdict on a collection: an in-band `Ack` commits it,
// everything else abandons it and parks the bundle. Silence must never
// commit, since a crashed client is silent, so a half-close without an
// ack abandons too.
//
// The announcer races this against the transfer, because a verdict may
// arrive at any point of one and always ends it, then awaits the same
// future once the last chunk is on the wire.
async fn verdict<Req: Ack + Cancel>(mut requests: Streaming<Req>) -> Result<(), Status> {
    loop {
        match requests.message().await {
            Ok(Some(req)) if req.is_ack() => return Ok(()),
            Ok(Some(req)) if req.is_cancel() => {
                return Err(Status::cancelled("Collection abandoned"));
            }
            // Not Debug-formatted: a stray metadata message carries the
            // session token, which must never reach the logs.
            Ok(Some(_)) => warn!("Ignoring unexpected message on the Receive request side"),
            Ok(None) => return Err(Status::cancelled("Collection abandoned")),
            Err(e) => {
                debug!("Receive stream failed: {e}");
                return Err(Status::aborted("Receive stream failed"));
            }
        }
    }
}

// The harness plumbing shared by the four surfaces' inline wire tests.
// Surface-specific fixtures stay local to each surface.
#[cfg(test)]
pub mod tests {
    use core::future::Future;
    use std::{borrow::Cow, net::SocketAddr, sync::Arc, time::Duration};

    use hardy_bpa::{Bytes, bpa::Bpa, node_ids::NodeIds};
    use hardy_bpv7::{
        builder::Builder,
        creation_timestamp::CreationTimestamp,
        eid::{IpnNodeId, NodeId},
    };
    use tokio::{net::TcpListener, sync::broadcast::Receiver as TornReceiver};
    use tonic::transport::server::{Router, TcpIncoming};

    use crate::token::Token;

    // A generous hang failsafe on an event-driven wait; the timeout only
    // bounds a regression.
    pub async fn timeout<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .expect("test timed out")
    }

    // The single-node configuration every surface's default harness uses.
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

    pub fn build_bundle(source: &str, destination: &str, payload: &[u8]) -> Bytes {
        let (_, data) = Builder::new(source.parse().unwrap(), destination.parse().unwrap())
            .with_payload(Cow::Borrowed(payload))
            .build(CreationTimestamp::now())
            .unwrap();
        Bytes::from(data)
    }

    // A started BPA behind the surface.
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

    // Serves `router` on a fresh port-0 listener and returns its address.
    pub async fn serve(router: Router) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let incoming = TcpIncoming::from(listener).with_nodelay(Some(true));
        tokio::spawn(router.serve_with_incoming(incoming));
        address
    }

    // Awaits the teardown barrier for `token`: once it fires the session
    // is fully retired, so a later call is rejected without a race.
    // Subscribe before triggering teardown. The timeout only bounds a
    // regression.
    pub async fn wait_torn_down(torn: &mut TornReceiver<Token>, token: &Bytes) {
        timeout(async { while Bytes::from(torn.recv().await.unwrap()) != *token {} }).await;
    }
}

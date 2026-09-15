// The rpc service implementations, one per wire surface.

pub mod application;
pub mod cla;
pub mod routing;
pub mod service;

use hardy_bpa::services::Error;
use hardy_bpv7::eid::Service;
use tonic::Status;
use tracing::error;

use crate::status::embed_service_error;

// Maps a BPA service error to a gRPC status, embedding the typed
// discriminator for the SDK.
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
        // Never constructed by the BPA (see `docs/TODO.md`).
        Error::DtnInvalidServiceName(_) => Status::invalid_argument(error.to_string()),
        Error::NoIpnNodeId | Error::NoDtnNodeId => Status::failed_precondition(error.to_string()),
        Error::PayloadTooLarge { .. } | Error::PayloadUnaddressable { .. } => {
            Status::resource_exhausted(error.to_string())
        }
        Error::Disconnected => Status::unavailable("Unregistered"),
        Error::StreamCancelled => Status::cancelled(error.to_string()),
        Error::Dropped(_) => Status::aborted(error.to_string()),
        // May carry host detail: logged here, redacted to a generic
        // status on the wire.
        Error::Internal(e) => {
            error!("internal service error: {e}");
            Status::internal("internal error")
        }
    };
    embed_service_error(status, &error)
}

// The cleartext `sub` claim of a session token. The surface prefix is
// needed: an application and a service can share a service id.
fn session_sub(surface: &str, service: Option<&Service>) -> String {
    match service {
        Some(Service::Ipn(n)) => format!("{surface}:ipn:{n}"),
        Some(Service::Dtn(name)) => format!("{surface}:dtn:{name}"),
        None => format!("{surface}:dynamic"),
    }
}

// Shared test harness for the four surfaces' wire tests.
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
    use tokio::{
        net::TcpListener,
        sync::{broadcast, broadcast::Receiver as TornReceiver},
    };
    use tonic::transport::server::{Router, TcpIncoming};

    use crate::token::Token;

    // The test barriers each surface embeds behind `#[cfg(test)]`.
    #[derive(Clone)]
    pub struct Hooks {
        // Fires a token once its session is fully retired.
        pub torn_down: broadcast::Sender<Token>,
        // Fires as a subscription task reaches its first read.
        pub opened: broadcast::Sender<()>,
    }

    impl Default for Hooks {
        fn default() -> Self {
            Self {
                torn_down: broadcast::channel(256).0,
                opened: broadcast::channel(256).0,
            }
        }
    }

    // A generous hang failsafe on an event-driven wait; the timeout only
    // bounds a regression.
    pub async fn timeout<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .expect("test timed out")
    }

    // Node ids for a single `ipn:1` node.
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

    // Waits until `token`'s session is fully retired. Subscribe before
    // triggering teardown. The timeout only bounds a regression.
    pub async fn wait_torn_down(torn: &mut TornReceiver<Token>, token: &Bytes) {
        timeout(async { while Bytes::from(torn.recv().await.unwrap()) != *token {} }).await;
    }
}

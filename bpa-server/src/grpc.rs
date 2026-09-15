// The gRPC front end of the BPA server: composes the configured
// registration surfaces (`hardy-proto` provides one per component) onto a
// tonic router, binds the listener, and serves it with a bounded graceful
// drain. `new` is the composition step, `serve` the running one.
//
// There is deliberately no extension seam for foreign gRPC services: the
// BPA's extension point is `hardy_bpa`'s registration traits, of which
// these four surfaces are already the clients. Extensions register
// against the BPA, not against this transport.

use std::{
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use hardy_async::{CancellationToken, TaskPool};
use hardy_bpa::bpa::Bpa;
use hardy_proto::{
    DEFAULT_MAX_FRAME_SIZE,
    server::{
        ApplicationServiceImpl, ClaServiceImpl, Limits, RoutingServiceImpl, ServiceServiceImpl,
    },
};
use tokio::time::sleep;
use tonic::{
    service::Routes,
    transport::{
        Server,
        server::{Router, TcpIncoming},
    },
};
use tonic_health::{
    ServingStatus,
    server::{HealthReporter, health_reporter},
};
use tracing::{error, info, warn};

use crate::{config::grpc::GrpcService, error::Error};

// The listen address used when `grpc.address` is absent.
const DEFAULT_ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 50051);

// Concurrent HTTP/2 streams one connection may hold open: far above
// what a component opens (a subscription and a few doors), far below
// what exhausts a host, given an open door buffers chunks.
const MAX_CONCURRENT_STREAMS: u32 = 256;

// The composed gRPC router and its bound listener, built but not yet
// serving. Constructed by [`new`](Self::new) and consumed by
// [`serve`](Self::serve).
pub struct GrpcServer {
    router: Router,
    incoming: TcpIncoming,
    reporter: HealthReporter,
    address: SocketAddr,
    drain_timeout: Duration,
}

impl GrpcServer {
    // Composes the given surfaces, adds the health service, and binds the
    // listener. Binding here means a bad address fails startup rather than
    // the serve task. The listener is therefore accepting before the BPA
    // has started: until [`serve`](Self::serve) runs, a connection sits in
    // the kernel backlog and gets no answer, which is what a client dialling
    // a node that is still recovering its store sees.
    pub fn new(
        address: Option<SocketAddr>,
        services: Vec<GrpcService>,
        drain_timeout: Duration,
        limits: Limits,
        bpa: &Arc<Bpa>,
        tasks: &TaskPool,
    ) -> Result<Self, Error> {
        // Mount each listed surface: the caller guarantees the list is
        // non-empty with no repeats, and the exhaustive match means a new
        // `GrpcService` variant fails to compile here until it is wired.
        // `into_server` sizes each mount for the wire contract.
        let mut routes = Routes::builder();
        for service in &services {
            match service {
                GrpcService::Application => routes.add_service(
                    ApplicationServiceImpl::with_limits(bpa.clone(), tasks.clone(), limits)
                        .into_server(),
                ),
                GrpcService::Service => routes.add_service(
                    ServiceServiceImpl::with_limits(bpa.clone(), tasks.clone(), limits)
                        .into_server(),
                ),
                GrpcService::Cla => routes.add_service(
                    ClaServiceImpl::with_limits(bpa.clone(), tasks.clone(), limits).into_server(),
                ),
                GrpcService::Routing => routes.add_service(
                    RoutingServiceImpl::with_limits(bpa.clone(), tasks.clone(), limits)
                        .into_server(),
                ),
            };
        }

        let (reporter, health_service) = health_reporter();

        let requested = address.unwrap_or(DEFAULT_ADDRESS);
        // `serve_with_incoming` ignores the tonic `Server` TCP settings, so
        // restate its `TCP_NODELAY` default here.
        let incoming = TcpIncoming::bind(requested)
            .map_err(|source| Error::Bind {
                address: requested,
                source,
            })?
            .with_nodelay(Some(true));
        // An ephemeral `:0` reads back to the real port here.
        let address = incoming.local_addr().unwrap_or(requested);

        // HTTP/2 keepalive bounds how long a silently dead peer can hold
        // sessions and parked door calls; graceful ends are caught by the
        // streams themselves. The flow-control window is adaptive
        // (auto-sized to the connection's bandwidth-delay product): the
        // fixed ~64 KiB default caps a transfer at window/RTT, throttling
        // GB-scale bundles on any link with non-trivial latency. The frame
        // cap carries a whole chunk in as few DATA frames as possible,
        // matching the client SDK's default endpoint. The stream cap
        // bounds one connection's fan-out, which nothing else limits.
        let router = Server::builder()
            .http2_keepalive_interval(Some(Duration::from_secs(30)))
            .http2_keepalive_timeout(Some(Duration::from_secs(10)))
            .http2_adaptive_window(Some(true))
            .max_frame_size(Some(DEFAULT_MAX_FRAME_SIZE))
            .max_concurrent_streams(Some(MAX_CONCURRENT_STREAMS))
            .add_routes(routes.routes())
            .add_service(health_service);

        info!("gRPC server hosting {services:?}, bound on {address}");

        Ok(Self {
            router,
            incoming,
            reporter,
            address,
            drain_timeout,
        })
    }

    // The resolved listen address; an ephemeral `:0` reads back as the real
    // bound port. Only the lifecycle test needs to dial the bound port.
    #[cfg(test)]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    // Serves until `cancel` fires, then drains open connections up to the
    // configured timeout before returning. Marks the server `Serving` at
    // entry, so a health check is answered `SERVING` only once the router
    // is accepting.
    // Returns `Err` on a transport failure so the caller can key its exit
    // status on it; a clean cancel-driven shutdown returns `Ok`.
    pub async fn serve(self, cancel: CancellationToken) -> Result<(), Error> {
        let Self {
            router,
            incoming,
            reporter,
            address,
            drain_timeout,
        } = self;

        reporter
            .set_service_status("", ServingStatus::Serving)
            .await;
        info!("gRPC server listening on {address}");

        let mut server =
            core::pin::pin!(router.serve_with_incoming_shutdown(incoming, cancel.cancelled()));
        tokio::select! {
            biased;
            result = &mut server => {
                if let Err(e) = result {
                    error!("gRPC server failed: {e}, shutting down");
                    // Unblock the composition root's shutdown wait, then hand
                    // the failure back so the process exits non-zero.
                    cancel.cancel();
                    return Err(Error::Serve(e));
                }
            }
            // The graceful drain is shutdown's one unbounded wait: a client
            // holding an unread response stream keeps its connection open
            // indefinitely, so the drain gets a deadline. Connections
            // abandoned here die with the process.
            _ = async {
                cancel.cancelled().await;
                // Tonic stops accepting the moment the token fires, so this
                // only answers health checks that arrive on a connection
                // already open and still draining. The drain then races the
                // deadline.
                reporter
                    .set_service_status("", ServingStatus::NotServing)
                    .await;
                sleep(drain_timeout).await;
            } => {
                // A zero deadline is the configured immediate cut, not a
                // failure to drain.
                if !drain_timeout.is_zero() {
                    warn!(
                        "gRPC connections did not drain within {drain_timeout:?}, abandoning them"
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv6Addr, sync::Arc, time::Duration};

    use hardy_async::{CancellationToken, TaskPool};
    use hardy_bpa::bpa::Bpa;
    use hardy_proto::server::Limits;
    use tonic::transport::Channel;
    use tonic_health::pb::{
        HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
    };

    use super::GrpcServer;
    use crate::{config::grpc::GrpcService, error::Error};

    // Bounds a hung shutdown only; the wait it wraps is event-driven.
    const REGRESSION_BOUND: Duration = Duration::from_secs(10);

    async fn minimal_bpa() -> Arc<Bpa> {
        Arc::new(Bpa::builder().build().await.unwrap())
    }

    // A successful SERVING check is the event that proves the listener is
    // bound, accepting, and has flipped readiness on.
    async fn assert_serving(channel: Channel) {
        let status = HealthClient::new(channel)
            .check(HealthCheckRequest {
                service: String::new(),
            })
            .await
            .unwrap()
            .into_inner()
            .status;
        assert_eq!(status, ServingStatus::Serving as i32);
    }

    // Cancelling must drive `serve` to return `Ok` (a clean shutdown, not a
    // transport failure).
    async fn cancel_and_join(
        cancel: CancellationToken,
        served: tokio::task::JoinHandle<Result<(), Error>>,
    ) {
        cancel.cancel();
        tokio::time::timeout(REGRESSION_BOUND, served)
            .await
            .expect("the timeout only bounds a regression")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn serves_health_and_returns_on_cancel() {
        let bpa = minimal_bpa().await;
        let tasks = TaskPool::new();
        let server = GrpcServer::new(
            // An ephemeral port bound before the serve task spawns, so the
            // dial below cannot race the listener into existence.
            Some((Ipv6Addr::LOCALHOST, 0).into()),
            vec![GrpcService::Application],
            // Do not wait on the still-open health connection at shutdown.
            Duration::ZERO,
            Limits::default(),
            &bpa,
            &tasks,
        )
        .unwrap();
        let address = server.address();

        let cancel = tasks.cancel_token().clone();
        let served = tokio::spawn(server.serve(cancel.clone()));

        let channel = Channel::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        assert_serving(channel).await;

        cancel_and_join(cancel, served).await;
    }
}

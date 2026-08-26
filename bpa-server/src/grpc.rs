// The gRPC front end of the BPA server: composes the configured
// registration surfaces (`hardy-proto` provides one per component) onto a
// tonic router, binds the listener, and serves it with a bounded graceful
// drain. `new` is the composition step, `serve` the running one.
//
// There is deliberately no extension seam for foreign gRPC services: the
// BPA's extension point is `hardy_bpa`'s registration traits, of which
// these four surfaces are already the clients, and a foreign service would
// need this crate's `Signer` (the wire-auth internal). Extensions register
// against the BPA, not against this transport.

use std::{
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use hardy_async::{CancellationToken, TaskPool};
use hardy_bpa::bpa::Bpa;
use hardy_proto::{
    MAX_MESSAGE_SIZE,
    application::application_service_server::ApplicationServiceServer,
    cla::cla_service_server::ClaServiceServer,
    routing::routing_agent_service_server::RoutingAgentServiceServer,
    server::{
        ApplicationServiceImpl, ClaServiceImpl, RoutingAgentServiceImpl, ServiceServiceImpl, Signer,
    },
    service::service_service_server::ServiceServiceServer,
};
use tonic::{
    service::Routes,
    transport::{
        Server, ServerTlsConfig,
        server::{Router, TcpIncoming},
    },
};
use tonic_health::{
    ServingStatus,
    server::{HealthReporter, health_reporter},
};
use tracing::{error, info, warn};

use crate::config::GrpcService;
use crate::error::Error;

// The listen address used when `grpc.address` is absent.
const DEFAULT_ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 50051);

// The composed gRPC router and its bound listener, built but not yet
// serving. Constructed by [`new`](Self::new) and consumed by
// [`serve`](Self::serve).
pub struct GrpcServer {
    router: Router,
    incoming: TcpIncoming,
    // Carried from `new` to `serve` so readiness flips to `Serving` at
    // accept time, not while this value is still inert.
    reporter: HealthReporter,
    address: SocketAddr,
    drain_timeout: Duration,
}

impl GrpcServer {
    // Composes the given surfaces, adds the health service, and binds the
    // listener. Binding here means a bad address fails startup rather than
    // the serve task.
    pub fn new(
        address: Option<SocketAddr>,
        services: Vec<GrpcService>,
        drain_timeout: Duration,
        tls: Option<ServerTlsConfig>,
        bpa: &Arc<Bpa>,
        tasks: &TaskPool,
    ) -> Result<Self, Error> {
        // One signing identity for the whole server: every surface's
        // sessions mint their tokens with it.
        let signer = Signer::new();

        // Mount each listed surface: the caller guarantees the list is
        // non-empty with no repeats, and the exhaustive match means a new
        // `GrpcService` variant fails to compile here until it is wired.
        let mut routes = Routes::builder();
        for service in &services {
            match service {
                GrpcService::Application => routes.add_service(
                    ApplicationServiceServer::new(ApplicationServiceImpl::new(
                        bpa.clone(),
                        tasks.clone(),
                        signer.clone(),
                    ))
                    .max_encoding_message_size(MAX_MESSAGE_SIZE)
                    .max_decoding_message_size(MAX_MESSAGE_SIZE),
                ),
                GrpcService::Service => routes.add_service(
                    ServiceServiceServer::new(ServiceServiceImpl::new(
                        bpa.clone(),
                        tasks.clone(),
                        signer.clone(),
                    ))
                    .max_encoding_message_size(MAX_MESSAGE_SIZE)
                    .max_decoding_message_size(MAX_MESSAGE_SIZE),
                ),
                GrpcService::Cla => routes.add_service(
                    ClaServiceServer::new(ClaServiceImpl::new(
                        bpa.clone(),
                        tasks.clone(),
                        signer.clone(),
                    ))
                    .max_encoding_message_size(MAX_MESSAGE_SIZE)
                    .max_decoding_message_size(MAX_MESSAGE_SIZE),
                ),
                GrpcService::Routing => routes.add_service(
                    RoutingAgentServiceServer::new(RoutingAgentServiceImpl::new(
                        bpa.clone(),
                        tasks.clone(),
                        signer.clone(),
                    ))
                    .max_encoding_message_size(MAX_MESSAGE_SIZE)
                    .max_decoding_message_size(MAX_MESSAGE_SIZE),
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
        // streams themselves.
        let mut builder = Server::builder()
            .http2_keepalive_interval(Some(Duration::from_secs(30)))
            .http2_keepalive_timeout(Some(Duration::from_secs(10)));

        let tls_enabled = tls.is_some();
        if let Some(tls) = tls {
            builder = builder.tls_config(tls)?;
        }

        let router = builder
            .add_routes(routes.routes())
            .add_service(health_service);

        info!(
            "gRPC server hosting {services:?}, bound on {address}{}",
            if tls_enabled { " over TLS" } else { "" }
        );

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
    // entry so health readiness is truthful only once it is accepting.
    pub async fn serve(self, cancel: CancellationToken) {
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
                    error!("gRPC server failed: {e}");
                }
            }
            // The graceful drain is shutdown's one unbounded wait: a client
            // holding an unread response stream keeps its connection open
            // indefinitely, so the drain gets a deadline. Connections
            // abandoned here die with the process.
            _ = async {
                cancel.cancelled().await;
                tokio::time::sleep(drain_timeout).await;
            } => {
                warn!(
                    "gRPC connections did not drain within {drain_timeout:?}, abandoning them"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, Ipv6Addr},
        sync::Arc,
        time::Duration,
    };

    use hardy_async::{CancellationToken, TaskPool};
    use hardy_bpa::bpa::Bpa;
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity, ServerTlsConfig};
    use tonic_health::pb::{
        HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
    };

    use super::GrpcServer;
    use crate::config::GrpcService;

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

    // Cancelling must drive `serve` to return.
    async fn cancel_and_join(cancel: CancellationToken, served: tokio::task::JoinHandle<()>) {
        cancel.cancel();
        tokio::time::timeout(REGRESSION_BOUND, served)
            .await
            .expect("the timeout only bounds a regression")
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
            None,
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

    #[tokio::test]
    async fn serves_over_tls() {
        let bpa = minimal_bpa().await;
        let tasks = TaskPool::new();

        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let tls = ServerTlsConfig::new()
            .identity(Identity::from_pem(cert.pem(), signing_key.serialize_pem()));

        let server = GrpcServer::new(
            Some((Ipv4Addr::LOCALHOST, 0).into()),
            vec![GrpcService::Application],
            Duration::ZERO,
            Some(tls),
            &bpa,
            &tasks,
        )
        .unwrap();
        let address = server.address();

        let cancel = tasks.cancel_token().clone();
        let served = tokio::spawn(server.serve(cancel.clone()));

        // The client trusts the self-signed cert as its own CA and verifies
        // it against the SAN, so a successful check proves the TLS handshake.
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(cert.pem()))
            .domain_name("localhost");
        let channel = Channel::from_shared(format!("https://{address}"))
            .unwrap()
            .tls_config(tls)
            .unwrap()
            .connect()
            .await
            .unwrap();
        assert_serving(channel).await;

        cancel_and_join(cancel, served).await;
    }
}

/*!
The client SDK: a local component registers against a remote BPA over
the v1 wire with the same traits a local [`Bpa`](hardy_bpa::bpa::Bpa)
uses, and the SDK carries the sessions, tokens, and data-plane calls.

All four surfaces are served: applications, low-level services,
convergence-layer adapters, and routing agents.
*/

mod adapter;
mod collector;
mod services;

use core::{
    num::NonZeroUsize,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use std::sync::Arc;

use hardy_async::TaskPool;
use hardy_bpa::{cla, routing};
use hardy_bpv7::eid::{Eid, NodeId, Service};
use thiserror::Error;
use tonic::transport::{Channel, Endpoint};
#[cfg(feature = "instrument")]
use tracing::instrument;

use crate::DEFAULT_MAX_FRAME_SIZE;

// The request channel of one data-plane transfer (Send/Dispatch/Receive/
// Forward): the metadata message, then chunks written one at a time under
// backpressure. The capacity is load-bearing, not a tuning knob:
// [`adapter::Reader`]'s drop relies on there being room for every message
// this side queues plus the in-band cancel, so `try_send(cancel)` on drop
// is reliable rather than best-effort. Do not lower it below `queued + 1`.
pub(crate) const TRANSFER_REQUEST_CAPACITY: usize = 2;

// The request channel of a Subscribe session: the Register handshake plus
// a later Unregister, with headroom.
pub(crate) const SUBSCRIBE_REQUEST_CAPACITY: usize = 4;

/// Errors configuring a [`BpaClient`]. Construction never dials the
/// endpoint (connections are established lazily), so only configuration
/// can fail here; DNS, TCP, TLS, and HTTP/2 connection failures surface
/// later, from the `register_*` calls and the sinks' operations, as the
/// surface's own errors.
#[derive(Debug, Error)]
pub enum EndpointError {
    /// The endpoint does not convert to a tonic
    /// [`Endpoint`](tonic::transport::Endpoint): an invalid URI, or an
    /// unsupported scheme.
    #[error("Invalid endpoint: {0}")]
    InvalidEndpoint(#[source] Box<dyn core::error::Error + Send + Sync>),
}

/// A client of a remote BPA: components register against it with the
/// same traits a local `Bpa` takes, and unregister by dropping their
/// sink or calling its `unregister` (completion is asynchronous, and
/// `on_unregister` marks it). Registrations are sharded round-robin
/// across a pool of lazily-established connections (one by default): a
/// single session's event stream, its token-gated data-plane calls, and
/// its in-band cancels all stay on one connection, while separate
/// sessions spread across the pool so no single HTTP/2 state machine or
/// TCP flow bounds aggregate throughput. Sharding happens only when the
/// session is created: an existing session is never migrated (its token
/// and in-band cancels are per-connection state), so one very busy
/// session can saturate its connection while others sit idle. See
/// [`new_pool`].
///
/// [`new_pool`]: BpaClient::new_pool
///
/// # Lifecycle
///
/// The sink a component receives in `on_register` is its control
/// handle: it must be stored, and every way a registration ends
/// converges on `on_unregister`.
///
/// ```ignore
/// let tasks = TaskPool::new();
/// let app = Arc::new(MyApp::new());
/// let bpa = BpaClient::new("http://[::1]:50051", tasks.clone())?;
/// let eid = bpa.register_application(Service::Ipn(42), app.clone()).await?;
///
/// // The application acts through its stored sink:
/// app.sink().send(destination, lifetime, None, None, &mut payload).await?;
///
/// // and unregisters explicitly through it,
/// app.sink().unregister().await;
/// // or implicitly, by dropping it (half-closing the session, which
/// // the BPA treats exactly as an Unregister).
/// drop(app);
/// ```
///
/// `on_unregister` fires on the component exactly once, whichever way
/// the session ends: explicit `unregister`, dropping the sink,
/// connection loss, BPA shutdown, or the shutdown of the client's own
/// [`TaskPool`]. (A pool shutdown racing a fresh registration is that
/// last ending arriving early: the component may see `on_register`
/// followed promptly by `on_unregister`.) The server closing the stream
/// is the single source of truth, so a silent peer is only detected as
/// fast as the transport reports it. [`new`] arms HTTP/2 keepalive to
/// bound that detection; [`with_endpoint`] leaves it to the
/// [`Endpoint`].
///
/// [`new`]: BpaClient::new
/// [`with_endpoint`]: BpaClient::with_endpoint
///
/// A registration is never re-created automatically: connection loss
/// terminates it like any other ending, through `on_unregister`, and
/// its session token is dead afterwards. The component (or its host)
/// calls `register_*` again to resume, and deliveries that were
/// announced but never acknowledged are announced to the new
/// registration.
#[derive(Clone, Debug)]
pub struct BpaClient {
    // The connection pool; sessions are sharded round-robin across it by
    // `next_channel`. A `Channel` clone shares its connection, so a whole
    // session pins to one entry.
    channels: Arc<[Channel]>,
    next: Arc<AtomicUsize>,
    tasks: TaskPool,
}

impl BpaClient {
    /// A client of the BPA server at `endpoint` over a single connection:
    /// anything convertible to a tonic [`Endpoint`], such as
    /// `"http://[::1]:50051"`. Equivalent to [`new_pool`] with one
    /// connection.
    ///
    /// The session event loops run on `tasks`: shutting the pool down
    /// ends every registration made through this client, and the pool
    /// is usually the host's, so the sessions join its shutdown
    /// sequence.
    ///
    /// The connection carries the transport defaults of
    /// [`default_endpoint`]. For different settings, start from
    /// [`default_endpoint`] (or a bare `Endpoint`) and use
    /// [`with_endpoint`].
    ///
    /// [`new_pool`]: BpaClient::new_pool
    /// [`with_endpoint`]: BpaClient::with_endpoint
    /// [`default_endpoint`]: BpaClient::default_endpoint
    pub fn new<D>(endpoint: D, tasks: TaskPool) -> Result<Self, EndpointError>
    where
        D: TryInto<Endpoint>,
        D::Error: Into<Box<dyn core::error::Error + Send + Sync>>,
    {
        Self::new_pool(endpoint, NonZeroUsize::MIN, tasks)
    }

    /// The SDK's transport defaults applied to `endpoint`: HTTP/2
    /// keepalive (a ping every 30 seconds, 10 seconds to answer, active
    /// while idle) so a silently dead peer ends its sessions within
    /// about a minute even when the event streams are quiet, an adaptive
    /// flow-control window so a large transfer is not throttled to the
    /// fixed default window per round-trip, and a chunk-sized DATA frame
    /// cap so a transfer is not fragmented into many small frames.
    /// Keepalive is transport liveness only: it detects a peer that is
    /// unreachable at the HTTP/2 level, not a BPA that is unhealthy or
    /// stalled.
    ///
    /// [`new`] and [`new_pool`] connect with exactly this. To adjust one
    /// setting without silently losing the others (say, the keepalive
    /// cadence, since setting it again overrides these), start here,
    /// reconfigure, and construct with [`with_endpoint`].
    ///
    /// [`new`]: BpaClient::new
    /// [`new_pool`]: BpaClient::new_pool
    /// [`with_endpoint`]: BpaClient::with_endpoint
    pub fn default_endpoint<D>(endpoint: D) -> Result<Endpoint, EndpointError>
    where
        D: TryInto<Endpoint>,
        D::Error: Into<Box<dyn core::error::Error + Send + Sync>>,
    {
        Ok(endpoint
            .try_into()
            .map_err(|e| EndpointError::InvalidEndpoint(e.into()))?
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true)
            // Auto-size the flow-control window to the connection's
            // bandwidth-delay product: the fixed ~64 KiB default caps a
            // transfer at window/RTT, throttling GB-scale bundles on any
            // link with non-trivial latency.
            .http2_adaptive_window(true)
            // Carry a whole chunk in as few HTTP/2 DATA frames as
            // possible: the ~16 KiB default fragments each chunk into
            // many frames, all per-frame bookkeeping on a GB transfer.
            .max_frame_size(DEFAULT_MAX_FRAME_SIZE))
    }

    /// A client of the BPA server at `endpoint` over a pool of
    /// `connections` connections, with the same transport defaults as
    /// [`new`]. Sessions shard round-robin across the pool (see
    /// [`BpaClient`]); use this to drive many concurrent large transfers,
    /// since one connection is one HTTP/2 state machine over one TCP flow.
    /// Keep the count small (a handful): each is a full connection, and
    /// the pool is allocated up front.
    ///
    /// [`new`]: BpaClient::new
    pub fn new_pool<D>(
        endpoint: D,
        connections: NonZeroUsize,
        tasks: TaskPool,
    ) -> Result<Self, EndpointError>
    where
        D: TryInto<Endpoint>,
        D::Error: Into<Box<dyn core::error::Error + Send + Sync>>,
    {
        Ok(Self::with_endpoint_pool(
            Self::default_endpoint(endpoint)?,
            connections,
            tasks,
        ))
    }

    /// A client over `endpoint` exactly as configured (no transport
    /// defaults, keepalive included) over a single connection. Without
    /// keepalive, a silently dead peer is only detected when the
    /// operating system gives up on the connection. Equivalent to
    /// [`with_endpoint_pool`] with one connection.
    ///
    /// [`with_endpoint_pool`]: BpaClient::with_endpoint_pool
    pub fn with_endpoint(endpoint: Endpoint, tasks: TaskPool) -> Self {
        Self::with_endpoint_pool(endpoint, NonZeroUsize::MIN, tasks)
    }

    /// A client over `endpoint` exactly as configured, over a pool of
    /// `connections` connections sharded per session. The endpoint's own
    /// settings apply identically to every connection. Keep the count
    /// small (a handful): each is a full connection, and the pool is
    /// allocated up front.
    pub fn with_endpoint_pool(
        endpoint: Endpoint,
        connections: NonZeroUsize,
        tasks: TaskPool,
    ) -> Self {
        // Each `connect_lazy` is an independent connection (cloning a
        // `Channel` would instead share one), so the pool is genuinely
        // `connections` connections, each established on first use.
        let channels = (0..connections.get())
            .map(|_| endpoint.connect_lazy())
            .collect::<Vec<_>>()
            .into();
        Self {
            channels,
            next: Arc::new(AtomicUsize::new(0)),
            tasks,
        }
    }

    // The next connection in round-robin order. Cloning a `Channel` shares
    // its underlying connection, so every call a session makes off this
    // clone stays on one HTTP/2 state machine (required: the session token
    // and its in-band cancel are peer-connection state).
    fn next_channel(&self) -> Channel {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.channels.len();
        self.channels[index].clone()
    }

    /// The size of the connection pool, for diagnostics and sizing
    /// decisions: the `connections` given to [`new_pool`] or
    /// [`with_endpoint_pool`], or one.
    ///
    /// [`new_pool`]: BpaClient::new_pool
    /// [`with_endpoint_pool`]: BpaClient::with_endpoint_pool
    pub fn connection_count(&self) -> usize {
        self.channels.len()
    }

    /// Registers an application under an explicit service id. The
    /// returned EID is the endpoint the registration is bound to.
    ///
    /// The application holds the sink it is given for its active
    /// lifetime; dropping the sink, or calling its `unregister`,
    /// terminates the registration.
    ///
    /// # Delivery commitment over the wire
    ///
    /// A delivery commits only when the SDK acknowledges it, which it
    /// does after
    /// [`on_deliver`](hardy_bpa::services::Application::on_deliver)
    /// returns `Ok` for a fully received stream. Returning `Err` parks
    /// the bundle for a later registration, whether or not the stream
    /// was received in full; so does a connection lost before the
    /// acknowledgement reached the BPA, which the application then
    /// observes as a re-delivery of a bundle it already accepted.
    /// Deliveries are therefore at-least-once: accept idempotently,
    /// keyed on the bundle id.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_application(
        &self,
        service_id: Service,
        application: Arc<dyn hardy_bpa::services::Application>,
    ) -> hardy_bpa::services::Result<Eid> {
        if self.tasks.cancel_token().is_cancelled() {
            return Err(hardy_bpa::services::Error::Disconnected);
        }
        let (eid, sink, collector, events) =
            services::application::subscribe(self.next_channel(), Some(service_id)).await?;
        application.on_register(&eid, Box::new(sink)).await;

        let cancel = self.tasks.cancel_token().clone();
        hardy_async::spawn!(self.tasks, "application_session", async move {
            services::application::run_session(events, collector, application, cancel).await
        });
        Ok(eid)
    }

    /// Registers an application under a BPA-assigned service id. The
    /// delivery-commitment contract of
    /// [`register_application`](Self::register_application) applies.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_dynamic_application(
        &self,
        application: Arc<dyn hardy_bpa::services::Application>,
    ) -> hardy_bpa::services::Result<Eid> {
        if self.tasks.cancel_token().is_cancelled() {
            return Err(hardy_bpa::services::Error::Disconnected);
        }
        let (eid, sink, collector, events) =
            services::application::subscribe(self.next_channel(), None).await?;
        application.on_register(&eid, Box::new(sink)).await;

        let cancel = self.tasks.cancel_token().clone();
        hardy_async::spawn!(self.tasks, "application_session", async move {
            services::application::run_session(events, collector, application, cancel).await
        });
        Ok(eid)
    }

    /// Registers a low-level service under an explicit service id: it
    /// exchanges whole BPv7 bundles, built and parsed by the service
    /// itself. The returned EID is the endpoint the registration is
    /// bound to.
    ///
    /// # Delivery commitment over the wire
    ///
    /// A delivery commits only when the SDK acknowledges it, which it
    /// does after
    /// [`on_deliver`](hardy_bpa::services::Service::on_deliver)
    /// returns `Ok` for a fully received stream. Returning `Err` parks
    /// the bundle for a later registration, whether or not the stream
    /// was received in full; so does a connection lost before the
    /// acknowledgement reached the BPA, which the service then observes
    /// as a re-delivery of a bundle it already accepted. Deliveries are
    /// therefore at-least-once: accept idempotently, keyed on the
    /// bundle id.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_service(
        &self,
        service_id: Service,
        service: Arc<dyn hardy_bpa::services::Service>,
    ) -> hardy_bpa::services::Result<Eid> {
        if self.tasks.cancel_token().is_cancelled() {
            return Err(hardy_bpa::services::Error::Disconnected);
        }
        let (eid, sink, collector, events) =
            services::service::subscribe(self.next_channel(), Some(service_id)).await?;
        service.on_register(&eid, Box::new(sink)).await;

        let cancel = self.tasks.cancel_token().clone();
        hardy_async::spawn!(self.tasks, "service_session", async move {
            services::service::run_session(events, collector, service, cancel).await
        });
        Ok(eid)
    }

    /// Registers a low-level service under a BPA-assigned service id.
    /// The delivery-commitment contract of
    /// [`register_service`](Self::register_service) applies.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_dynamic_service(
        &self,
        service: Arc<dyn hardy_bpa::services::Service>,
    ) -> hardy_bpa::services::Result<Eid> {
        if self.tasks.cancel_token().is_cancelled() {
            return Err(hardy_bpa::services::Error::Disconnected);
        }
        let (eid, sink, collector, events) =
            services::service::subscribe(self.next_channel(), None).await?;
        service.on_register(&eid, Box::new(sink)).await;

        let cancel = self.tasks.cancel_token().clone();
        hardy_async::spawn!(self.tasks, "service_session", async move {
            services::service::run_session(events, collector, service, cancel).await
        });
        Ok(eid)
    }

    /// Registers a routing agent: it pushes routes into the BPA's RIB via
    /// the sink it is given, and the BPA never calls back. The returned
    /// node ids are the BPA's own; the agent also receives them in
    /// `on_register`. Dropping the sink, or calling its `unregister`,
    /// withdraws the agent's routes.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_routing_agent(
        &self,
        name: String,
        agent: Arc<dyn routing::RoutingAgent>,
    ) -> routing::Result<Vec<NodeId>> {
        if self.tasks.cancel_token().is_cancelled() {
            return Err(routing::Error::Disconnected);
        }
        let (node_ids, sink, events) =
            services::routing::subscribe(self.next_channel(), name).await?;
        agent.on_register(Box::new(sink), &node_ids).await;

        let cancel = self.tasks.cancel_token().clone();
        hardy_async::spawn!(self.tasks, "routing_session", async move {
            services::routing::run_session(events, agent, cancel).await
        });
        Ok(node_ids)
    }

    /// Registers a convergence-layer adapter: the BPA forwards bundles
    /// to it, and it dispatches received bundles and manages peers
    /// through the sink it is given. The returned node ids are the BPA's
    /// own; the CLA also receives them in `on_register`. Its address
    /// type and lane count are read from the trait, as a local
    /// registration reads them. Dropping the sink, or calling its
    /// `unregister`, ends the registration.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_cla(
        &self,
        name: String,
        convergence_layer: Arc<dyn cla::Cla>,
    ) -> cla::Result<Vec<NodeId>> {
        if self.tasks.cancel_token().is_cancelled() {
            return Err(cla::Error::Disconnected);
        }
        let services::cla::Registered {
            node_ids,
            sink,
            events,
            client,
            token,
        } = services::cla::subscribe(
            self.next_channel(),
            name,
            convergence_layer.address_type(),
            convergence_layer.lane_count(),
        )
        .await?;
        convergence_layer
            .on_register(Box::new(sink), &node_ids)
            .await;

        let cancel = self.tasks.cancel_token().clone();
        hardy_async::spawn!(self.tasks, "cla_session", async move {
            services::cla::run_session(events, convergence_layer, cancel, client, token).await
        });
        Ok(node_ids)
    }
}

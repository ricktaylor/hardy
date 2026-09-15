use core::{
    error::Error,
    future::Future,
    num::NonZeroUsize,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll},
    time::Duration,
};
use std::sync::Arc;

use hardy_async::{JoinHandle, TaskPool};
use hardy_bpa::{cla, routing, services};
use hardy_bpv7::eid::{Eid, NodeId, Service};
use tokio::sync::oneshot;
use tonic::transport::{Channel, Endpoint};
#[cfg(feature = "instrument")]
use tracing::instrument;

use super::services::{
    cla::ClaSession,
    endpoint::{application::ApplicationSession, service::ServiceSession},
    routing::RoutingSession,
};
use crate::DEFAULT_MAX_FRAME_SIZE;

/// The reason a [`BpaClient`] could not be created from an endpoint
/// description.
#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    /// The description did not convert into a tonic [`Endpoint`]; the source is the
    /// conversion's error, typically a malformed URI.
    #[error("invalid endpoint")]
    InvalidEndpoint(#[source] Box<dyn Error + Send + Sync>),
}

/// A handle to a live registration on a remote BPA.
///
/// The `register_*` methods of [`BpaClient`] return one. It carries the
/// identity the BPA assigned, which is the endpoint id for an application or
/// service and the BPA's node ids for a CLA or routing agent, and it resolves
/// when the session ends, either through [`join`](RegistrationHandle::join) or
/// by being awaited. `Ok(())` means the session ended cleanly: the component
/// unregistered through its sink, or the client's task pool shut down, or the
/// BPA closed the stream without a status. `Err` carries the component's error
/// type: `Disconnected` when the server ended the session for a reason of its
/// own, such as its shutdown, a stall, or the BPA unregistering the component,
/// or when the BPA became unreachable; `Internal` carrying the gRPC status for
/// anything else, including a transport failure mid-session. The component's
/// `on_unregister` has run by the time the handle resolves.
///
/// Dropping the handle detaches the session, which runs on until it ends on its
/// own, and nothing then observes how it ended.
///
/// # Panics
///
/// Awaiting the handle panics if the session task neither returned nor
/// unwound, which the task pool does not allow: it aborts the process when one
/// of its tasks panics or is aborted.
#[must_use = "dropping the handle detaches the session; await it to observe how the session ends"]
#[derive(Debug)]
pub struct RegistrationHandle<Identity, E> {
    identity: Identity,
    session: JoinHandle<Result<(), E>>,
}

impl<Identity, E> RegistrationHandle<Identity, E> {
    /// Returns the identity the BPA assigned to the registration.
    pub fn id(&self) -> &Identity {
        &self.identity
    }
}

impl<Identity: Unpin, E> RegistrationHandle<Identity, E> {
    /// Waits for the session to end and returns how it ended.
    ///
    /// Awaiting the handle directly is the same.
    ///
    /// # Errors
    ///
    /// Returns the error the session ended with, as described on
    /// [`RegistrationHandle`].
    pub async fn join(self) -> Result<(), E> {
        self.await
    }
}

/// Resolves when the session ends, with how it ended.
impl<Identity: Unpin, E> Future for RegistrationHandle<Identity, E> {
    type Output = Result<(), E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().session)
            .poll(cx)
            .map(|joined| {
                joined.unwrap_or_else(|error| {
                    panic!("the session task did not run to completion: {error}")
                })
            })
    }
}

/// A client of one remote BPA, through which local components register over the
/// wire.
///
/// A client holds one or more lazily connected channels to the BPA and a
/// [`TaskPool`] on which it runs the sessions it opens. Nothing connects until
/// the first registration. Each registration takes the next channel in turn
/// and keeps every call of its session on it, so a client with several
/// connections keeps busy components from sharing one HTTP/2 connection's
/// flow-control window; a single component gains nothing from more than one.
///
/// Cloning is cheap and shares the channels and the pool. Shutting the pool
/// down ends every session opened through the client, and a client whose pool
/// has shut down refuses new registrations with the component's `Disconnected`
/// error. A registration runs on the pool rather than on the caller, so
/// nothing is left half-registered: giving up on a `register_*` call
/// unregisters whatever it had reached, and no registration outlives the
/// shutdown that raced it.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
///
/// use hardy_async::TaskPool;
/// use hardy_bpa::routing::RoutingAgent;
/// use hardy_proto::client::BpaClient;
///
/// # async fn run(agent: Arc<dyn RoutingAgent>) -> Result<(), Box<dyn std::error::Error>> {
/// let tasks = TaskPool::new();
/// let client = BpaClient::new("http://[::1]:50051", tasks.clone())?;
///
/// let registration = client.register_routing_agent("my-agent".into(), agent).await?;
/// println!("routing for node ids {:?}", registration.id());
///
/// tasks.shutdown().await;
/// registration.await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct BpaClient {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    channels: Box<[Channel]>,
    next: AtomicUsize,
    tasks: TaskPool,
}

impl BpaClient {
    /// Creates a client with one connection, configured by
    /// [`default_endpoint`](BpaClient::default_endpoint).
    ///
    /// `endpoint` is anything that converts into an [`Endpoint`], such as a
    /// `"http://host:port"` string. Sessions run on `tasks`. Nothing connects until
    /// the first registration.
    ///
    /// # Errors
    ///
    /// Returns [`EndpointError::InvalidEndpoint`] if `endpoint` does not convert.
    pub fn new<D>(endpoint: D, tasks: TaskPool) -> Result<Self, EndpointError>
    where
        D: TryInto<Endpoint>,
        D::Error: Into<Box<dyn Error + Send + Sync>>,
    {
        Self::with_connections(endpoint, NonZeroUsize::MIN, tasks)
    }

    /// Converts `endpoint` into an [`Endpoint`] carrying the SDK's default
    /// connection settings.
    ///
    /// The settings are HTTP/2 keep-alive pings every 30 seconds, each to be
    /// answered within 10 seconds and sent even while the connection is idle;
    /// adaptive HTTP/2 flow-control windows; and a maximum frame size of
    /// [`DEFAULT_MAX_FRAME_SIZE`]. Keep-alive is what detects a peer that vanished
    /// without closing the connection, because a session stream that is merely
    /// quiet looks the same as one whose peer is gone. A caller that wants other
    /// settings adjusts the returned endpoint and passes it to
    /// [`with_endpoint`](BpaClient::with_endpoint).
    ///
    /// # Errors
    ///
    /// Returns [`EndpointError::InvalidEndpoint`] if `endpoint` does not convert.
    pub fn default_endpoint<D>(endpoint: D) -> Result<Endpoint, EndpointError>
    where
        D: TryInto<Endpoint>,
        D::Error: Into<Box<dyn Error + Send + Sync>>,
    {
        Ok(endpoint
            .try_into()
            .map_err(|error| EndpointError::InvalidEndpoint(error.into()))?
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true)
            .http2_adaptive_window(true)
            .max_frame_size(DEFAULT_MAX_FRAME_SIZE))
    }

    /// Creates a client with `connections` connections, each configured by
    /// [`default_endpoint`](BpaClient::default_endpoint).
    ///
    /// A registration takes the next connection in turn and keeps every call
    /// of its session on it, so several connections spread sessions, not the
    /// calls of one session.
    ///
    /// # Errors
    ///
    /// Returns [`EndpointError::InvalidEndpoint`] if `endpoint` does not convert.
    pub fn with_connections<D>(
        endpoint: D,
        connections: NonZeroUsize,
        tasks: TaskPool,
    ) -> Result<Self, EndpointError>
    where
        D: TryInto<Endpoint>,
        D::Error: Into<Box<dyn Error + Send + Sync>>,
    {
        Ok(Self::with_endpoint_connections(
            Self::default_endpoint(endpoint)?,
            connections,
            tasks,
        ))
    }

    /// Creates a client with one connection to `endpoint`, exactly as configured.
    ///
    /// No default settings are applied;
    /// [`default_endpoint`](BpaClient::default_endpoint) produces them for a caller
    /// that wants to start from the defaults.
    pub fn with_endpoint(endpoint: Endpoint, tasks: TaskPool) -> Self {
        Self::with_endpoint_connections(endpoint, NonZeroUsize::MIN, tasks)
    }

    /// Creates a client with `connections` connections to `endpoint`, exactly as
    /// configured.
    ///
    /// Each connection is opened lazily, on the first registration that uses it.
    /// As with [`with_connections`](BpaClient::with_connections), a session
    /// stays on the connection its registration took.
    pub fn with_endpoint_connections(
        endpoint: Endpoint,
        connections: NonZeroUsize,
        tasks: TaskPool,
    ) -> Self {
        let channels = (0..connections.get())
            .map(|_| endpoint.connect_lazy())
            .collect::<Box<[_]>>();
        Self {
            inner: Arc::new(Inner {
                channels,
                next: AtomicUsize::new(0),
                tasks,
            }),
        }
    }

    /// Returns the next connection in round-robin order.
    fn next_channel(&self) -> Channel {
        let index = self.inner.next.fetch_add(1, Ordering::Relaxed) % self.inner.channels.len();
        self.inner.channels[index].clone()
    }

    /// Returns the number of connections the client spreads its registrations
    /// across.
    pub fn connection_count(&self) -> usize {
        self.inner.channels.len()
    }

    /// Registers `application` on the BPA under `service_id` and starts its
    /// session.
    ///
    /// The handle carries the endpoint id the BPA formed from its node id and
    /// `service_id`; bundles the application sends carry it as their source. Before
    /// this returns, the application's `on_register` has run with a sink that
    /// speaks the wire. From then on deliveries arrive through `on_deliver`, each
    /// collected on its own task, and bundle status reports through
    /// `on_status_notify`, until the application unregisters, the BPA ends the
    /// session, or the client's task pool shuts down.
    ///
    /// # Errors
    ///
    /// Returns `Disconnected` if the client's task pool has shut down or the BPA is
    /// unreachable, `ServiceIdInUse` if another registration holds `service_id`,
    /// and otherwise the error the BPA reported, or `Internal` carrying the gRPC
    /// status when the SDK cannot rebuild it.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_application(
        &self,
        service_id: Service,
        application: Arc<dyn services::Application>,
    ) -> services::Result<RegistrationHandle<Eid, services::Error>> {
        self.register_application_inner(Some(service_id), application)
            .await
    }

    /// Registers `application` on the BPA under a service number of the BPA's
    /// choosing and starts its session.
    ///
    /// The handle carries the endpoint id the BPA assigned. In every other respect
    /// this is [`register_application`](BpaClient::register_application).
    ///
    /// # Errors
    ///
    /// Returns `Disconnected` if the client's task pool has shut down or the BPA is
    /// unreachable, and otherwise the error the BPA reported, or `Internal`
    /// carrying the gRPC status when the SDK cannot rebuild it.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_dynamic_application(
        &self,
        application: Arc<dyn services::Application>,
    ) -> services::Result<RegistrationHandle<Eid, services::Error>> {
        self.register_application_inner(None, application).await
    }

    /// Registers `application` under `service_id`, or under a service number
    /// of the BPA's choosing if `None`, and starts its session.
    ///
    /// # Errors
    ///
    /// As [`register_application`](BpaClient::register_application).
    async fn register_application_inner(
        &self,
        service_id: Option<Service>,
        application: Arc<dyn services::Application>,
    ) -> services::Result<RegistrationHandle<Eid, services::Error>> {
        let channel = self.next_channel();
        // A session whose caller gives up before it has the handle is
        // cancelled, so no registration is left running unobserved.
        let cancel = self.inner.tasks.cancel_token().child_token();
        let guard = cancel.clone().drop_guard();
        let (registered_tx, registered_rx) = oneshot::channel();
        let session = hardy_async::spawn!(self.inner.tasks, "application_session", async move {
            let (eid, session) =
                match ApplicationSession::subscribe(channel, service_id, application, cancel).await
                {
                    Ok(registered) => registered,
                    Err(error) => {
                        let _ = registered_tx.send(Err(error));
                        return Ok(());
                    }
                };
            let _ = registered_tx.send(Ok(eid));
            session.handle_events().await
        });

        let identity = registered_rx
            .await
            .map_err(|_| services::Error::Disconnected)??;

        // The caller has the handle, so ending the session is its call now.
        guard.disarm();
        Ok(RegistrationHandle { identity, session })
    }

    /// Registers `service` on the BPA under `service_id` and starts its session.
    ///
    /// The handle carries the endpoint id the BPA formed from its node id and
    /// `service_id`. Before this returns, the service's `on_register` has run with
    /// a sink that speaks the wire. From then on complete bundles arrive through
    /// `on_deliver`, each collected on its own task, and bundle status reports
    /// through `on_status_notify`, until the service unregisters, the BPA ends the
    /// session, or the client's task pool shuts down.
    ///
    /// # Errors
    ///
    /// Returns `Disconnected` if the client's task pool has shut down or the BPA is
    /// unreachable, `ServiceIdInUse` if another registration holds `service_id`,
    /// and otherwise the error the BPA reported, or `Internal` carrying the gRPC
    /// status when the SDK cannot rebuild it.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_service(
        &self,
        service_id: Service,
        service: Arc<dyn services::Service>,
    ) -> services::Result<RegistrationHandle<Eid, services::Error>> {
        self.register_service_inner(Some(service_id), service).await
    }

    /// Registers `service` on the BPA under a service number of the BPA's choosing
    /// and starts its session.
    ///
    /// The handle carries the endpoint id the BPA assigned. In every other respect
    /// this is [`register_service`](BpaClient::register_service).
    ///
    /// # Errors
    ///
    /// Returns `Disconnected` if the client's task pool has shut down or the BPA is
    /// unreachable, and otherwise the error the BPA reported, or `Internal`
    /// carrying the gRPC status when the SDK cannot rebuild it.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_dynamic_service(
        &self,
        service: Arc<dyn services::Service>,
    ) -> services::Result<RegistrationHandle<Eid, services::Error>> {
        self.register_service_inner(None, service).await
    }

    /// Registers `service` under `service_id`, or under a service number of
    /// the BPA's choosing if `None`, and starts its session.
    ///
    /// # Errors
    ///
    /// As [`register_service`](BpaClient::register_service).
    async fn register_service_inner(
        &self,
        service_id: Option<Service>,
        service: Arc<dyn services::Service>,
    ) -> services::Result<RegistrationHandle<Eid, services::Error>> {
        let channel = self.next_channel();
        // A session whose caller gives up before it has the handle is
        // cancelled, so no registration is left running unobserved.
        let cancel = self.inner.tasks.cancel_token().child_token();
        let guard = cancel.clone().drop_guard();
        let (registered_tx, registered_rx) = oneshot::channel();
        let session = hardy_async::spawn!(self.inner.tasks, "service_session", async move {
            let (eid, session) =
                match ServiceSession::subscribe(channel, service_id, service, cancel).await {
                    Ok(registered) => registered,
                    Err(error) => {
                        let _ = registered_tx.send(Err(error));
                        return Ok(());
                    }
                };
            let _ = registered_tx.send(Ok(eid));
            session.handle_events().await
        });

        let identity = registered_rx
            .await
            .map_err(|_| services::Error::Disconnected)??;

        // The caller has the handle, so ending the session is its call now.
        guard.disarm();
        Ok(RegistrationHandle { identity, session })
    }

    /// Registers `agent` on the BPA under `name` and starts its session.
    ///
    /// The handle carries the BPA's node ids. Before this returns, the agent's
    /// `on_register` has run with a sink whose `add_route` and `remove_route` drive
    /// the BPA's routing information base. The BPA sends a routing agent no events;
    /// the session only holds the registration, and its routes, open until the
    /// agent unregisters, the BPA ends the session, or the client's task pool shuts
    /// down.
    ///
    /// # Errors
    ///
    /// Returns `Disconnected` if the client's task pool has shut down or the BPA is
    /// unreachable, `AlreadyExists` if another registration holds `name`, and
    /// otherwise the error the BPA reported, or `Internal` carrying the gRPC status
    /// when the SDK cannot rebuild it.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_routing_agent(
        &self,
        name: String,
        agent: Arc<dyn routing::RoutingAgent>,
    ) -> routing::Result<RegistrationHandle<Vec<NodeId>, routing::Error>> {
        let channel = self.next_channel();
        // A session whose caller gives up before it has the handle is
        // cancelled, so no registration is left running unobserved.
        let cancel = self.inner.tasks.cancel_token().child_token();
        let guard = cancel.clone().drop_guard();
        let (registered_tx, registered_rx) = oneshot::channel();
        let session = hardy_async::spawn!(self.inner.tasks, "routing_session", async move {
            let (node_ids, session) =
                match RoutingSession::subscribe(channel, name, agent, cancel).await {
                    Ok(registered) => registered,
                    Err(error) => {
                        let _ = registered_tx.send(Err(error));
                        return Ok(());
                    }
                };
            let _ = registered_tx.send(Ok(node_ids));
            session.handle_events().await
        });

        let identity = registered_rx
            .await
            .map_err(|_| routing::Error::Disconnected)??;

        // The caller has the handle, so ending the session is its call now.
        guard.disarm();
        Ok(RegistrationHandle { identity, session })
    }

    /// Registers `convergence_layer` on the BPA under `name`, with the declarations
    /// in `init`, and starts its session.
    ///
    /// The handle carries the BPA's node ids. Before this returns, the CLA's
    /// `on_register` has run with a sink that speaks the wire and with the bundle
    /// size limit the BPA agreed to. From then on each `Forwarding` event becomes a
    /// call to the CLA's `forward`, with the announced bundle streamed as its
    /// source; forwardings run concurrently, one task each. The session lasts until
    /// the CLA unregisters, the BPA ends the session, or the client's task pool
    /// shuts down.
    ///
    /// # Errors
    ///
    /// Returns `Disconnected` if the client's task pool has shut down or the BPA is
    /// unreachable, `AlreadyExists` if another registration holds `name`,
    /// `Internal` if `init` declares more than the 256 lanes the wire allows, and
    /// otherwise the error the BPA reported, or `Internal` carrying the gRPC status
    /// when the SDK cannot rebuild it.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_cla(
        &self,
        name: String,
        convergence_layer: Arc<dyn cla::Cla>,
        init: cla::ClaInit,
    ) -> cla::Result<RegistrationHandle<Vec<NodeId>, cla::Error>> {
        let channel = self.next_channel();
        // A session whose caller gives up before it has the handle is
        // cancelled, so no registration is left running unobserved.
        let cancel = self.inner.tasks.cancel_token().child_token();
        let guard = cancel.clone().drop_guard();
        let (registered_tx, registered_rx) = oneshot::channel();
        let session = hardy_async::spawn!(self.inner.tasks, "cla_session", async move {
            let (node_ids, session) =
                match ClaSession::subscribe(channel, name, convergence_layer, init, cancel).await {
                    Ok(registered) => registered,
                    Err(error) => {
                        let _ = registered_tx.send(Err(error));
                        return Ok(());
                    }
                };
            let _ = registered_tx.send(Ok(node_ids));
            session.handle_events().await
        });

        let identity = registered_rx
            .await
            .map_err(|_| cla::Error::Disconnected)??;

        // The caller has the handle, so ending the session is its call now.
        guard.disarm();
        Ok(RegistrationHandle { identity, session })
    }
}

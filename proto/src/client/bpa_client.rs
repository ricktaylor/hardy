// `BpaClient`, its connection pool, the `register_*` methods, and the
// handle each registration returns.

use core::{
    future::{Future, IntoFuture},
    num::NonZeroUsize,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use std::sync::Arc;

use hardy_async::{JoinHandle, TaskPool};
use hardy_bpa::{cla, routing, services};
use hardy_bpv7::eid::{Eid, NodeId, Service};
use thiserror::Error;
use tonic::transport::{Channel, Endpoint};
#[cfg(feature = "instrument")]
use tracing::instrument;

use super::services::{
    application::ApplicationSession, cla::ClaSession, routing::RoutingSession,
    service::ServiceSession,
};
use crate::DEFAULT_MAX_FRAME_SIZE;

/// Errors configuring a [`BpaClient`].
///
/// Construction never dials; connection failures surface later, from
/// `register_*` calls and sink operations.
#[derive(Debug, Error)]
pub enum EndpointError {
    /// The endpoint does not convert to a tonic [`Endpoint`].
    #[error("invalid endpoint")]
    InvalidEndpoint(#[source] Box<dyn core::error::Error + Send + Sync>),
}

/// A live registration: the identity it bound and a handle to its
/// running session.
///
/// [`id`](Self::id) is available as soon as `register_*` returns.
/// Awaiting the handle yields how the session ended: `Ok(())` if this
/// side asked for the ending, `Err` otherwise. Dropping the handle
/// detaches the session, which keeps running on the client's task
/// pool.
#[must_use = "dropping the handle detaches the session; await it to observe how the session ends"]
#[derive(Debug)]
pub struct RegistrationHandle<Id, E> {
    id: Id,
    session: JoinHandle<Result<(), E>>,
}

impl<Id, E> RegistrationHandle<Id, E> {
    /// Returns the identity the registration bound: the endpoint for
    /// an application or service, or the BPA's node ids for a CLA or
    /// routing agent.
    pub fn id(&self) -> &Id {
        &self.id
    }

    /// Waits for the session to end, consuming the handle; see
    /// [`RegistrationHandle`] for the result's meaning.
    pub async fn join(self) -> Result<(), E>
    where
        E: From<Box<dyn core::error::Error + Send + Sync>>,
    {
        // A task panic aborts the process, so a `JoinError` here can
        // only mean the session task was aborted.
        match self.session.await {
            Ok(result) => result,
            Err(e) => Err(E::from(Box::new(e))),
        }
    }
}

impl<Id, E> IntoFuture for RegistrationHandle<Id, E>
where
    Id: Send + 'static,
    E: From<Box<dyn core::error::Error + Send + Sync>> + Send + 'static,
{
    type Output = Result<(), E>;
    type IntoFuture = Pin<Box<dyn Future<Output = Result<(), E>> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.join())
    }
}

/// A client of a remote BPA: components register with the same traits
/// a local `Bpa` takes, and each `register_*` call returns a
/// [`RegistrationHandle`].
///
/// Registrations are assigned round-robin to a pool of lazily-connected
/// channels (one by default; see [`new_pool`]), and each session stays
/// on its channel. A component must keep the sink it receives in
/// `on_register`: dropping the sink, or calling its `unregister`, ends
/// the session, and every ending is reported through a single
/// `on_unregister` call. The SDK never reconnects: after a session
/// ends, the caller registers again. [`new`] enables HTTP/2 keepalive;
/// [`with_endpoint`] leaves keepalive to the [`Endpoint`].
///
/// [`new`]: BpaClient::new
/// [`new_pool`]: BpaClient::new_pool
/// [`with_endpoint`]: BpaClient::with_endpoint
#[derive(Clone, Debug)]
pub struct BpaClient {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    // Clones of a `Channel` share its connection.
    channels: Box<[Channel]>,
    next: AtomicUsize,
    tasks: TaskPool,
}

impl BpaClient {
    /// Creates a client with a single connection to `endpoint`, using
    /// the transport defaults of [`default_endpoint`].
    ///
    /// Session event loops run on `tasks`: shutting the pool down ends
    /// every registration made through this client.
    ///
    /// [`default_endpoint`]: BpaClient::default_endpoint
    pub fn new<D>(endpoint: D, tasks: TaskPool) -> Result<Self, EndpointError>
    where
        D: TryInto<Endpoint>,
        D::Error: Into<Box<dyn core::error::Error + Send + Sync>>,
    {
        Self::new_pool(endpoint, NonZeroUsize::MIN, tasks)
    }

    /// Applies the SDK's transport defaults to `endpoint`: HTTP/2
    /// keepalive, an adaptive flow-control window, and a chunk-sized
    /// DATA frame cap.
    ///
    /// To override a setting, reconfigure the returned [`Endpoint`]
    /// and construct the client with [`with_endpoint`].
    ///
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
            // The default fixed ~64 KiB window caps throughput at
            // window/RTT.
            .http2_adaptive_window(true)
            // Fit a whole chunk in as few DATA frames as possible.
            .max_frame_size(DEFAULT_MAX_FRAME_SIZE))
    }

    /// Creates a client with a pool of `connections` connections, using
    /// the transport defaults of [`default_endpoint`].
    ///
    /// [`default_endpoint`]: BpaClient::default_endpoint
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

    /// Creates a client with a single connection, using `endpoint`
    /// exactly as configured; no transport defaults are applied.
    pub fn with_endpoint(endpoint: Endpoint, tasks: TaskPool) -> Self {
        Self::with_endpoint_pool(endpoint, NonZeroUsize::MIN, tasks)
    }

    /// Creates a client with a pool of `connections` connections,
    /// using `endpoint` exactly as configured; no transport defaults
    /// are applied.
    pub fn with_endpoint_pool(
        endpoint: Endpoint,
        connections: NonZeroUsize,
        tasks: TaskPool,
    ) -> Self {
        // Each `connect_lazy` call creates an independent connection.
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

    fn next_channel(&self) -> Channel {
        let index = self.inner.next.fetch_add(1, Ordering::Relaxed) % self.inner.channels.len();
        self.inner.channels[index].clone()
    }

    /// Returns the number of connections in the pool.
    pub fn connection_count(&self) -> usize {
        self.inner.channels.len()
    }

    /// Registers an application under an explicit service id; the
    /// handle's [`id`](RegistrationHandle::id) is the bound endpoint.
    ///
    /// Deliveries are at-least-once: a delivery commits only on the
    /// SDK's acknowledgement, so accept idempotently, keyed on the
    /// bundle id.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_application(
        &self,
        service_id: Service,
        application: Arc<dyn services::Application>,
    ) -> services::Result<RegistrationHandle<Eid, services::Error>> {
        if self.inner.tasks.cancel_token().is_cancelled() {
            return Err(services::Error::Disconnected);
        }
        let (eid, session) = ApplicationSession::subscribe(
            self.next_channel(),
            Some(service_id),
            application,
            self.inner.tasks.cancel_token().clone(),
        )
        .await?;
        let session = hardy_async::spawn!(self.inner.tasks, "application_session", async move {
            session.handle_events().await
        });
        Ok(RegistrationHandle { id: eid, session })
    }

    /// Registers an application under a BPA-assigned service id. The
    /// contract of [`register_application`](Self::register_application)
    /// applies.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_dynamic_application(
        &self,
        application: Arc<dyn services::Application>,
    ) -> services::Result<RegistrationHandle<Eid, services::Error>> {
        if self.inner.tasks.cancel_token().is_cancelled() {
            return Err(services::Error::Disconnected);
        }
        let (eid, session) = ApplicationSession::subscribe(
            self.next_channel(),
            None,
            application,
            self.inner.tasks.cancel_token().clone(),
        )
        .await?;
        let session = hardy_async::spawn!(self.inner.tasks, "application_session", async move {
            session.handle_events().await
        });
        Ok(RegistrationHandle { id: eid, session })
    }

    /// Registers a low-level service under an explicit service id: it
    /// exchanges whole BPv7 bundles. The contract of
    /// [`register_application`](Self::register_application) applies.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_service(
        &self,
        service_id: Service,
        service: Arc<dyn services::Service>,
    ) -> services::Result<RegistrationHandle<Eid, services::Error>> {
        if self.inner.tasks.cancel_token().is_cancelled() {
            return Err(services::Error::Disconnected);
        }
        let (eid, session) = ServiceSession::subscribe(
            self.next_channel(),
            Some(service_id),
            service,
            self.inner.tasks.cancel_token().clone(),
        )
        .await?;
        let session = hardy_async::spawn!(self.inner.tasks, "service_session", async move {
            session.handle_events().await
        });
        Ok(RegistrationHandle { id: eid, session })
    }

    /// Registers a low-level service under a BPA-assigned service id. The
    /// contract of [`register_service`](Self::register_service) applies.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_dynamic_service(
        &self,
        service: Arc<dyn services::Service>,
    ) -> services::Result<RegistrationHandle<Eid, services::Error>> {
        if self.inner.tasks.cancel_token().is_cancelled() {
            return Err(services::Error::Disconnected);
        }
        let (eid, session) = ServiceSession::subscribe(
            self.next_channel(),
            None,
            service,
            self.inner.tasks.cancel_token().clone(),
        )
        .await?;
        let session = hardy_async::spawn!(self.inner.tasks, "service_session", async move {
            session.handle_events().await
        });
        Ok(RegistrationHandle { id: eid, session })
    }

    /// Registers a routing agent; the handle's
    /// [`id`](RegistrationHandle::id) is the BPA's node ids. Dropping
    /// the sink, or calling its `unregister`, withdraws the agent's
    /// routes and ends the session.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_routing_agent(
        &self,
        name: String,
        agent: Arc<dyn routing::RoutingAgent>,
    ) -> routing::Result<RegistrationHandle<Vec<NodeId>, routing::Error>> {
        if self.inner.tasks.cancel_token().is_cancelled() {
            return Err(routing::Error::Disconnected);
        }
        let (node_ids, session) = RoutingSession::subscribe(
            self.next_channel(),
            name,
            agent,
            self.inner.tasks.cancel_token().clone(),
        )
        .await?;
        let session = hardy_async::spawn!(self.inner.tasks, "routing_session", async move {
            session.handle_events().await
        });
        Ok(RegistrationHandle {
            id: node_ids,
            session,
        })
    }

    /// Registers a convergence-layer adapter; the handle's
    /// [`id`](RegistrationHandle::id) is the BPA's node ids.
    ///
    /// Unlike a local registration, no egress policy can be supplied:
    /// a policy is an in-process trait object that cannot cross the
    /// wire.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_cla(
        &self,
        name: String,
        convergence_layer: Arc<dyn cla::Cla>,
        init: cla::ClaInit,
    ) -> cla::Result<RegistrationHandle<Vec<NodeId>, cla::Error>> {
        if self.inner.tasks.cancel_token().is_cancelled() {
            return Err(cla::Error::Disconnected);
        }
        let (node_ids, session) = ClaSession::subscribe(
            self.next_channel(),
            name,
            convergence_layer,
            init,
            self.inner.tasks.cancel_token().clone(),
        )
        .await?;
        let session = hardy_async::spawn!(self.inner.tasks, "cla_session", async move {
            session.handle_events().await
        });
        Ok(RegistrationHandle {
            id: node_ids,
            session,
        })
    }
}

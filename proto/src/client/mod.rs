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

use core::time::Duration;
use std::sync::Arc;

use hardy_async::TaskPool;
use hardy_bpa::{cla, routing};
use hardy_bpv7::eid::{Eid, NodeId, Service};
use tonic::transport::{Channel, Endpoint};
#[cfg(feature = "instrument")]
use tracing::instrument;

/// A client of a remote BPA: components register against it with the
/// same traits a local `Bpa` takes, and unregister by dropping their
/// sink or calling its `unregister` (completion is asynchronous, and
/// `on_unregister` marks it). All registrations multiplex one
/// lazily-established connection.
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
/// app.sink().send(destination, lifetime, None, &mut payload).await?;
///
/// // and unregisters explicitly through it,
/// app.sink().unregister().await;
/// // or implicitly, by dropping it (half-closing the session, which
/// // the BPA treats exactly as an Unregister).
/// drop(app);
/// ```
///
/// `on_unregister` fires on the component whichever way the session
/// ends, including BPA shutdown, connection loss, and the shutdown of
/// the client's own [`TaskPool`]; the server
/// closing the stream is the single source of truth, so a silent peer
/// is only detected as fast as the transport reports it. [`new`]
/// arms HTTP/2 keepalive to bound that detection; [`with_endpoint`]
/// leaves it to the [`Endpoint`].
///
/// [`new`]: BpaClient::new
/// [`with_endpoint`]: BpaClient::with_endpoint
///
/// Afterwards the session token is dead; registering again resumes,
/// and deliveries that were announced but never collected are
/// announced to the new registration.
#[derive(Clone, Debug)]
pub struct BpaClient {
    channel: Channel,
    tasks: TaskPool,
}

impl BpaClient {
    /// A client of the BPA server at `endpoint`: anything convertible
    /// to a tonic [`Endpoint`], such as `"http://[::1]:50051"`.
    ///
    /// The session event loops run on `tasks`: shutting the pool down
    /// ends every registration made through this client, and the pool
    /// is usually the host's, so the sessions join its shutdown
    /// sequence.
    ///
    /// The connection is armed with HTTP/2 keepalive (a ping every
    /// 30 seconds, 10 seconds to answer, active while idle), so a
    /// silently dead peer ends its sessions within about a minute
    /// even when the event streams are quiet. For different transport
    /// settings, configure an `Endpoint` and use [`with_endpoint`];
    /// keepalive set here would override the endpoint's own.
    ///
    /// [`with_endpoint`]: BpaClient::with_endpoint
    pub fn new<D>(endpoint: D, tasks: TaskPool) -> hardy_bpa::services::Result<Self>
    where
        D: TryInto<Endpoint>,
        D::Error: Into<Box<dyn core::error::Error + Send + Sync>>,
    {
        Ok(Self::with_endpoint(
            endpoint
                .try_into()
                .map_err(|e| hardy_bpa::services::Error::Internal(e.into()))?
                .http2_keep_alive_interval(Duration::from_secs(30))
                .keep_alive_timeout(Duration::from_secs(10))
                .keep_alive_while_idle(true),
            tasks,
        ))
    }

    /// A client over `endpoint` exactly as configured: no transport
    /// defaults are applied, keepalive included. Without keepalive, a
    /// silently dead peer is only detected when the operating system
    /// gives up on the connection.
    pub fn with_endpoint(endpoint: Endpoint, tasks: TaskPool) -> Self {
        Self {
            channel: endpoint.connect_lazy(),
            tasks,
        }
    }

    /// Registers an application under an explicit service id. The
    /// returned EID is the endpoint the registration is bound to.
    ///
    /// The application holds the sink it is given for its active
    /// lifetime; dropping the sink, or calling its `unregister`,
    /// terminates the registration.
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
            services::application::subscribe(self.channel.clone(), Some(service_id)).await?;
        application.on_register(&eid, Box::new(sink)).await;

        let cancel = self.tasks.cancel_token().clone();
        hardy_async::spawn!(self.tasks, "application_session", async move {
            services::application::run_session(events, collector, application, cancel).await
        });
        Ok(eid)
    }

    /// Registers an application under a BPA-assigned service id.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_dynamic_application(
        &self,
        application: Arc<dyn hardy_bpa::services::Application>,
    ) -> hardy_bpa::services::Result<Eid> {
        if self.tasks.cancel_token().is_cancelled() {
            return Err(hardy_bpa::services::Error::Disconnected);
        }
        let (eid, sink, collector, events) =
            services::application::subscribe(self.channel.clone(), None).await?;
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
            services::service::subscribe(self.channel.clone(), Some(service_id)).await?;
        service.on_register(&eid, Box::new(sink)).await;

        let cancel = self.tasks.cancel_token().clone();
        hardy_async::spawn!(self.tasks, "service_session", async move {
            services::service::run_session(events, collector, service, cancel).await
        });
        Ok(eid)
    }

    /// Registers a low-level service under a BPA-assigned service id.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn register_dynamic_service(
        &self,
        service: Arc<dyn hardy_bpa::services::Service>,
    ) -> hardy_bpa::services::Result<Eid> {
        if self.tasks.cancel_token().is_cancelled() {
            return Err(hardy_bpa::services::Error::Disconnected);
        }
        let (eid, sink, collector, events) =
            services::service::subscribe(self.channel.clone(), None).await?;
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
            services::routing::subscribe(self.channel.clone(), name).await?;
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
            self.channel.clone(),
            name,
            convergence_layer.address_type(),
            convergence_layer.lane_count(),
        )
        .await?;
        convergence_layer
            .on_register(Box::new(sink), &node_ids)
            .await;

        let cancel = self.tasks.cancel_token().clone();
        let tasks = self.tasks.clone();
        hardy_async::spawn!(self.tasks, "cla_session", async move {
            services::cla::run_session(events, convergence_layer, cancel, client, token, tasks)
                .await
        });
        Ok(node_ids)
    }
}

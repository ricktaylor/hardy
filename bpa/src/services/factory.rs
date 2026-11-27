//! Constructing a service or application at the endpoints the BPA resolves
//! for it.

use core::fmt::{self, Debug, Formatter};

use alloc::{boxed::Box, vec::Vec};
use hardy_bpv7::eid::Eid;

use super::{Application, ApplicationSink, Result, Service, ServiceSink};
use crate::Arc;

/// One endpoint a service is registered at, with that endpoint's sink.
pub struct ServiceEndpoint {
    /// The endpoint, and the source of every bundle sent through `sink`.
    pub eid: Eid,

    /// The endpoint's channel back to the BPA.
    ///
    /// The same channel [`Service::on_register`] delivers for this
    /// endpoint: a service keeps whichever handle suits it, and the
    /// endpoint unregisters when the last of them is dropped. The sink is
    /// not live until the build returns, so [`ServiceFactory::build`] must
    /// not use it.
    pub sink: Arc<dyn ServiceSink>,
}

impl Debug for ServiceEndpoint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceEndpoint")
            .field("eid", &self.eid)
            .finish_non_exhaustive()
    }
}

/// Everything the BPA hands a [`ServiceFactory`] when it builds the service.
#[derive(Debug)]
pub struct ServiceContext {
    /// Every endpoint the service is registered at, in the order its
    /// service ids were given; one for a single-id registration.
    pub endpoints: Vec<ServiceEndpoint>,
}

impl ServiceContext {
    /// Mints a context. The BPA does this; a service's tests mint one to
    /// build the service at the endpoints they exercise.
    pub fn new(endpoints: Vec<ServiceEndpoint>) -> Self {
        Self { endpoints }
    }
}

/// One endpoint an application is registered at, with that endpoint's
/// sink, as a [`ServiceEndpoint`] is for a service.
pub struct ApplicationEndpoint {
    /// The endpoint, and the source of every bundle sent through `sink`.
    pub eid: Eid,

    /// The endpoint's channel back to the BPA, shared with the handle
    /// [`Application::on_register`] delivers for it, as for
    /// [`ServiceEndpoint::sink`].
    pub sink: Arc<dyn ApplicationSink>,
}

impl Debug for ApplicationEndpoint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplicationEndpoint")
            .field("eid", &self.eid)
            .finish_non_exhaustive()
    }
}

/// Everything the BPA hands an [`ApplicationFactory`] when it builds the
/// application.
#[derive(Debug)]
pub struct ApplicationContext {
    /// Every endpoint the application is registered at, in the order its
    /// service ids were given; one for a single-id registration.
    pub endpoints: Vec<ApplicationEndpoint>,
}

impl ApplicationContext {
    /// Mints a context. The BPA does this; an application's tests mint one
    /// to build the application at the endpoints they exercise.
    pub fn new(endpoints: Vec<ApplicationEndpoint>) -> Self {
        Self { endpoints }
    }
}

/// Builds a low-level [`Service`] when the BPA registers it.
///
/// [`BpaBuilder::service`](crate::builder::BpaBuilder::service) and
/// [`service_endpoints`](crate::builder::BpaBuilder::service_endpoints)
/// take a factory, and the BPA runs it once at
/// [`Bpa::start`](crate::bpa::Bpa::start), with every resolved endpoint
/// and its sink in hand; then [`Service::on_register`] runs once per
/// endpoint as for any service. An already built service is a factory
/// whose `build` returns it.
pub trait ServiceFactory: Send {
    /// Builds the service at the endpoints the BPA resolved.
    ///
    /// Wiring only: no I/O, no spawned tasks, and no use of the sinks.
    /// `Err` means the service cannot work at those endpoints.
    fn build(self, cx: ServiceContext) -> Result<Arc<dyn Service>>;
}

impl<S: Service + 'static> ServiceFactory for Arc<S> {
    /// Returns the service, which takes its sinks in
    /// [`Service::on_register`].
    fn build(self, _cx: ServiceContext) -> Result<Arc<dyn Service>> {
        Ok(self)
    }
}

impl ServiceFactory for Arc<dyn Service> {
    /// Returns the service, which takes its sinks in
    /// [`Service::on_register`].
    fn build(self, _cx: ServiceContext) -> Result<Arc<dyn Service>> {
        Ok(self)
    }
}

/// Builds a high-level [`Application`] when the BPA registers it, as a
/// [`ServiceFactory`] builds a service.
pub trait ApplicationFactory: Send {
    /// Builds the application at the endpoints the BPA resolved.
    ///
    /// Wiring only: no I/O, no spawned tasks, and no use of the sinks.
    /// `Err` means the application cannot work at those endpoints.
    fn build(self, cx: ApplicationContext) -> Result<Arc<dyn Application>>;
}

impl<A: Application + 'static> ApplicationFactory for Arc<A> {
    /// Returns the application, which takes its sinks in
    /// [`Application::on_register`].
    fn build(self, _cx: ApplicationContext) -> Result<Arc<dyn Application>> {
        Ok(self)
    }
}

impl ApplicationFactory for Arc<dyn Application> {
    /// Returns the application, which takes its sinks in
    /// [`Application::on_register`].
    fn build(self, _cx: ApplicationContext) -> Result<Arc<dyn Application>> {
        Ok(self)
    }
}

// A service or application factory with its type erased, so the registry
// holds both kinds together.
pub enum BoxFactory {
    Service(Box<dyn FnOnce(ServiceContext) -> Result<Arc<dyn Service>> + Send>),
    Application(Box<dyn FnOnce(ApplicationContext) -> Result<Arc<dyn Application>> + Send>),
}

impl BoxFactory {
    pub fn service<F: ServiceFactory + 'static>(factory: F) -> Self {
        Self::Service(Box::new(|cx| factory.build(cx)))
    }

    pub fn application<F: ApplicationFactory + 'static>(factory: F) -> Self {
        Self::Application(Box::new(|cx| factory.build(cx)))
    }
}

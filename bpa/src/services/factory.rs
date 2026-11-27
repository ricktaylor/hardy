//! Constructing a service or application at the endpoint the BPA resolves
//! for it.

use core::fmt::{self, Debug, Formatter};

use alloc::boxed::Box;
use hardy_bpv7::eid::Eid;

use super::{Application, ApplicationSink, Result, Service, ServiceSink};
use crate::Arc;

/// Everything the BPA hands a [`ServiceFactory`] when it builds the service.
pub struct ServiceContext {
    /// The channel back to the BPA.
    ///
    /// The same channel [`Service::on_register`] delivers: a service keeps
    /// whichever handle suits it, and unregisters when it drops the last of
    /// them. The sink is not live until the build returns, so
    /// [`ServiceFactory::build`] must not use it.
    pub sink: Arc<dyn ServiceSink>,

    /// The endpoint the service is registered at, and the source of every
    /// bundle it sends.
    pub endpoint: Eid,
}

impl ServiceContext {
    /// Mints a context. The BPA does this; a service's tests mint one to
    /// build the service at the endpoint they exercise.
    pub fn new(sink: Arc<dyn ServiceSink>, endpoint: Eid) -> Self {
        Self { sink, endpoint }
    }
}

impl Debug for ServiceContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceContext")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// Everything the BPA hands an [`ApplicationFactory`] when it builds the
/// application.
pub struct ApplicationContext {
    /// The channel back to the BPA, shared with the handle
    /// [`Application::on_register`] delivers, as for
    /// [`ServiceContext::sink`].
    pub sink: Arc<dyn ApplicationSink>,

    /// The endpoint the application is registered at, and the source of
    /// every bundle it sends.
    pub endpoint: Eid,
}

impl ApplicationContext {
    /// Mints a context. The BPA does this; an application's tests mint one
    /// to build the application at the endpoint they exercise.
    pub fn new(sink: Arc<dyn ApplicationSink>, endpoint: Eid) -> Self {
        Self { sink, endpoint }
    }
}

impl Debug for ApplicationContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplicationContext")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// Builds a low-level [`Service`] when the BPA registers it.
///
/// [`BpaBuilder::service`](crate::builder::BpaBuilder::service) takes a
/// factory, and the BPA runs it at [`Bpa::start`](crate::bpa::Bpa::start)
/// with the resolved endpoint and the sink in hand; then
/// [`Service::on_register`] runs as for any service. An already built
/// service is a factory whose `build` returns it.
pub trait ServiceFactory: Send {
    /// Builds the service at the endpoint the BPA resolved.
    ///
    /// Wiring only: no I/O, no spawned tasks, and no use of `cx.sink`.
    /// `Err` means the service cannot work at that endpoint.
    fn build(self, cx: ServiceContext) -> Result<Arc<dyn Service>>;
}

impl<S: Service + 'static> ServiceFactory for Arc<S> {
    /// Returns the service, which takes its sink in
    /// [`Service::on_register`].
    fn build(self, _cx: ServiceContext) -> Result<Arc<dyn Service>> {
        Ok(self)
    }
}

impl ServiceFactory for Arc<dyn Service> {
    /// Returns the service, which takes its sink in
    /// [`Service::on_register`].
    fn build(self, _cx: ServiceContext) -> Result<Arc<dyn Service>> {
        Ok(self)
    }
}

/// Builds a high-level [`Application`] when the BPA registers it, as a
/// [`ServiceFactory`] builds a service.
pub trait ApplicationFactory: Send {
    /// Builds the application at the endpoint the BPA resolved.
    ///
    /// Wiring only: no I/O, no spawned tasks, and no use of `cx.sink`.
    /// `Err` means the application cannot work at that endpoint.
    fn build(self, cx: ApplicationContext) -> Result<Arc<dyn Application>>;
}

impl<A: Application + 'static> ApplicationFactory for Arc<A> {
    /// Returns the application, which takes its sink in
    /// [`Application::on_register`].
    fn build(self, _cx: ApplicationContext) -> Result<Arc<dyn Application>> {
        Ok(self)
    }
}

impl ApplicationFactory for Arc<dyn Application> {
    /// Returns the application, which takes its sink in
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

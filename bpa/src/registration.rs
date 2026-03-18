//! Trait for registering CLAs, services, and applications with a BPA.

use hardy_async::async_trait;
use hardy_bpv7::eid::{Eid, NodeId, Service as Bpv7Service};

use crate::cla::{Cla, ClaAddressType, Result as ClaResult};
use crate::policy::EgressPolicy;
use crate::services::{Application, Result as ServiceResult, Service};
use crate::{Arc, Vec};

/// Trait for registering CLAs, services, and applications with a BPA.
///
/// This trait abstracts the registration interface, allowing components
/// to work with either a local [`Bpa`](crate::bpa::Bpa) instance or a remote BPA via gRPC.
///
/// # Component Lifecycle
///
/// Components follow a consistent lifecycle pattern:
///
/// 1. **Construction**: `new(&Config) -> Result<Self, Error>` validates configuration
///    eagerly. Errors surface at construction time rather than during registration.
///
/// 2. **Registration**: `register(&Arc<Self>, &dyn BpaRegistration)` calls the
///    appropriate `register_*` method. The BPA calls `on_register()` on the component,
///    providing a Sink for communication back to the BPA.
///
/// 3. **Active**: Component uses Sink methods to interact with the BPA. The Sink
///    remains valid until unregistration.
///
/// 4. **Unregistration**: Either the component calls `sink.unregister()`, or the BPA
///    initiates shutdown and calls `on_unregister()`.
///
/// # Sink Storage Requirement
///
/// **Components MUST store the Sink for their entire active lifetime.**
///
/// The Sink is provided in `on_register()` and must be retained (typically in
/// a `spin::Once<T>` or `OnceLock<T>`) until unregistration. If `on_register()`
/// returns without storing the Sink, the Sink is dropped and the component is
/// automatically unregistered.
///
/// ```ignore
/// pub struct MyComponent {
///     sink: spin::Once<Arc<dyn Sink>>,
///     // ... other fields
/// }
///
/// impl MyTrait for MyComponent {
///     fn on_register(&self, sink: Arc<dyn Sink>) {
///         // MUST store the sink - dropping it triggers unregistration
///         self.sink.set(sink);
///     }
/// }
/// ```
///
/// # Post-Disconnection Behaviour
///
/// After unregistration, the Sink remains stored but becomes non-functional:
/// all operations return `Error::Disconnected`. Components don't need defensive
/// patterns like `Option<Sink>` with `take()` in `on_unregister()` - the Sink
/// can remain stored and post-disconnection calls simply fail gracefully.
///
/// This means `on_unregister()` only handles component-specific cleanup (stopping
/// tasks, closing connections), not Sink lifecycle management.
///
/// # Recommended Implementation Pattern
///
/// ```ignore
/// impl MyComponent {
///     /// Creates a new component. Validates configuration eagerly.
///     pub fn new(config: &Config) -> Result<Self, Error> {
///         // Validate and prepare resources
///         Ok(Self { sink: spin::Once::new(), /* ... */ })
///     }
///
///     /// Registers with the BPA. Returns after Sink is stored.
///     pub async fn register(
///         self: &Arc<Self>,
///         bpa: &dyn BpaRegistration,
///     ) -> Result<(), Error> {
///         bpa.register_xxx(/* ... */, self.clone(), /* ... */).await?;
///         Ok(())
///     }
///
///     /// Explicit unregistration.
///     pub async fn unregister(&self) {
///         if let Some(sink) = self.sink.get() {
///             sink.unregister().await;
///         }
///     }
/// }
/// ```
///
/// # For CLA Implementors
///
/// CLAs receive callbacks via the [`cla::Sink`] trait, which is provided
/// in [`cla::Cla::on_register`]. Key Sink methods:
///
/// - `dispatch()` - Submit received bundles to the BPA
/// - `add_peer()` / `remove_peer()` - Manage peer connections (keyed by CL address)
/// - `unregister()` - Disconnect from the BPA
///
/// # For Service Implementors
///
/// Services receive [`services::ServiceSink`] (low-level, full bundle access) or
/// [`services::ApplicationSink`] (high-level, payload-only), provided in their
/// respective `on_register` methods.
#[async_trait]
pub trait BpaRegistration: Send + Sync {
    /// Register a Convergence Layer Adapter with the BPA.
    ///
    /// The CLA will receive a [`cla::Sink`] via [`cla::Cla::on_register`]
    /// for communicating back to the BPA.
    ///
    /// # Arguments
    ///
    /// * `name` - Unique name for this CLA instance
    /// * `address_type` - The address type this CLA handles (e.g., TCP)
    /// * `cla` - The CLA implementation
    /// * `policy` - Optional egress policy for traffic shaping
    ///
    /// # Returns
    ///
    /// The BPA's node IDs on success
    async fn register_cla(
        &self,
        name: String,
        address_type: Option<ClaAddressType>,
        cla: Arc<dyn Cla>,
        policy: Option<Arc<dyn EgressPolicy>>,
    ) -> ClaResult<Vec<NodeId>>;

    /// Register a low-level Service with full bundle access.
    ///
    /// The service will receive a [`services::ServiceSink`] via
    /// [`services::Service::on_register`] for sending bundles.
    ///
    /// # Arguments
    ///
    /// * `service_id` - Optional service identifier. If None, one is assigned.
    /// * `service` - The service implementation
    ///
    /// # Returns
    ///
    /// The endpoint ID assigned to this service
    async fn register_service(
        &self,
        service_id: Option<Bpv7Service>,
        service: Arc<dyn Service>,
    ) -> ServiceResult<Eid>;

    /// Register a high-level Application with payload-only access.
    ///
    /// The application will receive an [`services::ApplicationSink`] via
    /// [`services::Application::on_register`] for sending payloads.
    ///
    /// # Arguments
    ///
    /// * `service_id` - Optional service identifier. If None, one is assigned.
    /// * `application` - The application implementation
    ///
    /// # Returns
    ///
    /// The endpoint ID assigned to this application
    async fn register_application(
        &self,
        service_id: Option<Bpv7Service>,
        application: Arc<dyn Application>,
    ) -> ServiceResult<Eid>;
}

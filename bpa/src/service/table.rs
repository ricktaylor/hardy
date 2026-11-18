use core::time::Duration;

// `closeable::Receiver` is the receiving end of a delivery queue, not the
// stream trait of the same name this module hands to services.
use hardy_async::{
    CancellationToken, TaskPool, async_trait, closeable::Receiver as QueueReceiver,
    sync::spin::Mutex,
};
// `eid::Service` is the service number inside an EID, not the trait of the
// same name this module registers; it is aliased to the word the code
// already uses for it.
use hardy_bpv7::{
    bundle::Id,
    eid::{Eid, Service as ServiceId},
    status_report::ReasonCode,
};
use portable_atomic::{AtomicU32, Ordering};
use time::OffsetDateTime;
use tracing::{error, info};

// This module's own `Sink` implements both service sink traits, so it takes
// the bare name here; the traits stay module-qualified at their few use
// sites.
use super::{BoxFactory, Context, Error, Result, Segment, SendOptions, StatusNotify, raw};
use crate::{
    Arc, Bytes, HashMap, Weak,
    bundle::Bundle,
    dispatcher::Dispatcher,
    hash_map::Entry,
    node_ids::{NodeIds, service_is_admin_endpoint},
    routing::Rib,
    service,
    storage::channel::{SendError, Sender},
    stream::{CancellableReceiver, Receiver},
};

// The service map is behind a spin lock. Its critical section is a lookup,
// the opening of a delivery queue and an insert, all O(1), with the
// service itself built outside it, so the lock is never held long enough
// to repay an OS mutex's park. The queue opens inside because a number
// already taken must leave no queue behind.

/// One registered service: what it was built as, and the queue the BPA
/// delivers to it through.
pub struct Record {
    pub kind: Kind,
    pub service_id: ServiceId,
    // Cancelled at unregistration; every in-flight stream of this
    // registration races it and fails immediately.
    pub cancel: CancellationToken,
    // The canonical registration EID — the exact key the DeliverPending
    // claim, the unregister sweep, and poll_service_waiting match on.
    eid: Eid,
    // Both ends of this registration's delivery queue (the local
    // analogue of a peer's egress queues): the dispatcher fills it
    // through `deliver` and drains it through `next_delivery`.
    tx: Sender,
    rx: QueueReceiver<Bundle>,
}

impl Record {
    /// A registration of `kind` at `eid`, delivered to through `queue`.
    pub fn new(
        kind: Kind,
        service_id: ServiceId,
        eid: Eid,
        (tx, rx): (Sender, QueueReceiver<Bundle>),
    ) -> Arc<Self> {
        Arc::new(Self {
            kind,
            service_id,
            cancel: CancellationToken::new(),
            eid,
            tx,
            rx,
        })
    }

    /// Queues a bundle into this service's delivery channel.
    ///
    /// `Err(bundle)` hands ownership back for parking (the mirror of
    /// `Peer::forward`): the queue has closed (unregistration).
    #[expect(
        clippy::result_large_err,
        reason = "Err(bundle) hands ownership back to the caller; boxing it to shrink the Err variant would allocate on every call"
    )]
    pub async fn deliver(&self, bundle: Bundle) -> core::result::Result<(), Bundle> {
        self.tx.send(bundle).await.map_err(|SendError(b)| b)
    }

    /// Pulls the next bundle queued for delivery, or `None` once the
    /// queue has closed.
    pub async fn next_delivery(&self) -> Option<Bundle> {
        self.rx.recv().await.ok()
    }

    /// The canonical registration EID.
    pub fn eid(&self) -> &Eid {
        &self.eid
    }

    fn close_queue(&self) {
        self.tx.close();
    }
}

impl PartialEq for Record {
    fn eq(&self, other: &Self) -> bool {
        self.service_id == other.service_id
    }
}

impl Eq for Record {}

impl PartialOrd for Record {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Record {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.service_id.cmp(&other.service_id)
    }
}

impl core::hash::Hash for Record {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.service_id.hash(state);
    }
}

impl core::fmt::Debug for Record {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Record")
            .field("eid", &self.eid)
            .finish_non_exhaustive()
    }
}

/// The BPA's implementation of both service sink traits.
// The back-edge is weak. `Bpa` owns the table for the life of the BPA, so
// a strong edge would add no reachability, only a cycle through the
// service that holds this sink, and a cycle of `Arc`s never drops. A
// failed upgrade is the condition the trait already names,
// `Error::Disconnected`.
struct Sink {
    table: Weak<Table>,
    service_id: ServiceId,
    // The registration EID, and the source of every bundle sent here.
    eid: Eid,
}

impl Sink {
    fn new(table: Weak<Table>, service_id: ServiceId, eid: Eid) -> Arc<Self> {
        Arc::new(Self {
            table,
            service_id,
            eid,
        })
    }

    // The registration behind this sink, gone once the service has been
    // unregistered.
    fn record(&self) -> Result<(Arc<Table>, Arc<Record>)> {
        let table = self.table.upgrade().ok_or(Error::Disconnected)?;
        let record = table.record(&self.service_id).ok_or(Error::Disconnected)?;
        Ok((table, record))
    }

    async fn unregister_inner(&self) {
        if let Some(table) = self.table.upgrade() {
            table.unregister(&self.service_id).await;
        }
    }
}

#[async_trait]
impl raw::Sink for Sink {
    async fn unregister(&self) {
        self.unregister_inner().await
    }

    async fn send(&self, stream: &mut dyn Receiver<Segment>) -> Result<Id> {
        let (table, record) = self.record()?;
        let dispatcher = table.dispatcher()?;

        // A service that unregisters mid-send must not originate its
        // bundle: the registration's token races every pull, so teardown
        // wakes this stream immediately, even parked behind a stalled
        // producer, and the send surfaces as cancelled. Sink-side, so the
        // dispatcher's stream consumers stay registration-agnostic.
        let mut stream = CancellableReceiver {
            inner: stream,
            token: record.cancel.clone(),
        };
        dispatcher.originate_raw(&self.eid, &mut stream).await
    }
}

#[async_trait]
impl service::Sink for Sink {
    async fn unregister(&self) {
        self.unregister_inner().await
    }

    async fn send(
        &self,
        destination: Eid,
        data: Bytes,
        lifetime: Duration,
        options: Option<SendOptions>,
    ) -> Result<Id> {
        let (table, _) = self.record()?;
        table
            .dispatcher()?
            .originate(self.eid.clone(), destination, data, lifetime, options)
            .await
    }

    async fn send_streamed(
        &self,
        destination: Eid,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
        lifetime: Duration,
        options: Option<SendOptions>,
    ) -> Result<Id> {
        let (table, record) = self.record()?;
        let dispatcher = table.dispatcher()?;

        // Registration teardown wakes an in-flight send exactly as on the
        // raw door: the token races every pull, sink-side.
        let mut stream = CancellableReceiver {
            inner: stream,
            token: record.cancel.clone(),
        };
        dispatcher
            .originate_streamed(
                self.eid.clone(),
                destination,
                total_len,
                &mut stream,
                lifetime,
                options,
            )
            .await
    }
}

/// Which of the two service contracts a service is declared against.
///
/// The doors differ in the factory they carry and in nothing else, so a
/// declared service is this and its service number.
pub enum FactoryKind<'a> {
    /// A service that receives payloads only.
    Payload(BoxFactory<'a>),
    /// A service that receives whole bundles.
    Raw(raw::BoxFactory<'a>),
}

impl FactoryKind<'_> {
    // Calls the factory this carries, under the contract it names.
    fn build(self, sink: Arc<Sink>, eid: Eid) -> Result<Kind> {
        match self {
            Self::Payload(factory) => Ok(Kind::Payload(factory.build(Context::new(sink, eid))?)),
            Self::Raw(factory) => Ok(Kind::Raw(factory.build(raw::Context::new(sink, eid))?)),
        }
    }
}

/// A registered service, under the contract it was built against.
///
/// Delivery is the only thing the two contracts disagree on, and the
/// dispatcher settles it by what it hands over: a whole bundle on one
/// side, a payload on the other. Everything else is the same call on
/// either, so this answers it.
pub enum Kind {
    /// A service that receives payloads only.
    Payload(Arc<dyn service::Service>),
    /// A service that receives whole bundles.
    Raw(Arc<dyn raw::Service>),
}

impl Kind {
    async fn start(&self) -> Result<()> {
        match self {
            Self::Payload(service) => service.start().await,
            Self::Raw(service) => service.start().await,
        }
    }

    async fn shutdown(&self) -> Result<()> {
        match self {
            Self::Payload(service) => service.shutdown().await,
            Self::Raw(service) => service.shutdown().await,
        }
    }

    /// Passes a status report on to the service it was raised for.
    pub async fn report_status(
        &self,
        bundle_id: &Id,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
        timestamp: Option<OffsetDateTime>,
    ) {
        match self {
            Self::Payload(service) => {
                service
                    .report_status(bundle_id, from, kind, reason, timestamp)
                    .await
            }
            Self::Raw(service) => {
                service
                    .report_status(bundle_id, from, kind, reason, timestamp)
                    .await
            }
        }
    }
}

/// Base for dynamically assigned IPN service numbers.
/// Starts high to avoid collisions with explicitly assigned IDs.
const DYNAMIC_SERVICE_BASE: u32 = 0x8000_0000;

// Where a registration stands.
//
// `Building` holds the service id while the factory runs, before there
// is a record to hold; `Attached` is the registration itself. Both hold
// the id against reuse. There is no detaching state: `unregister` takes
// the record out under the lock and the withdrawal that follows no
// longer holds the id.
enum Slot {
    Building,
    Attached(Arc<Record>),
}

/// The services of one BPA: the only place a service is built, and the
/// authority on which ones are live.
pub struct Table {
    node_ids: Arc<NodeIds>,
    // The dispatcher owns the BPA's delivery machinery and outlives this
    // table, so the edge back is weak; a failed upgrade is
    // `Error::Disconnected`.
    dispatcher: Weak<Dispatcher>,
    rib: Arc<Rib>,
    services: Mutex<HashMap<ServiceId, Slot>>,
    next_dynamic: AtomicU32,
    tasks: TaskPool,
}

impl Table {
    /// The table of one BPA, holding the `services` declared for it,
    /// each built and filed inert.
    ///
    /// The map is complete before it reaches the table, so there is no
    /// moment at which the table is live but incomplete, and a service
    /// the BPA cannot honour is reported from `Bpa::build` rather than
    /// from `start`. A service that cannot be built takes the ones
    /// before it down with it, leaving nothing behind.
    // The dispatcher is the one argument taken by reference: the table
    // holds a `Weak` to it, and keeps the others.
    pub fn new(
        node_ids: Arc<NodeIds>,
        dispatcher: &Arc<Dispatcher>,
        rib: Arc<Rib>,
        services: Vec<(Option<ServiceId>, FactoryKind<'static>)>,
    ) -> Result<Arc<Self>> {
        let table = Arc::new(Self {
            node_ids,
            dispatcher: Arc::downgrade(dispatcher),
            rib,
            services: Mutex::new(HashMap::new()),
            next_dynamic: AtomicU32::new(DYNAMIC_SERVICE_BASE),
            tasks: TaskPool::new(),
        });

        let mut built = HashMap::with_capacity(services.len());
        for (id, factory) in services {
            // An allocated service number collides with nothing by
            // construction; an explicit one may name the administrative
            // endpoint, which is structurally registered (RFC 9758
            // Section 5.7).
            let service_id = id.unwrap_or_else(|| {
                ServiceId::Ipn(table.next_dynamic.fetch_add(1, Ordering::Relaxed))
            });
            if service_is_admin_endpoint(&service_id) {
                return Err(Error::AdministrativeEndpoint(service_id.to_string()));
            }
            if built.contains_key(&service_id) {
                return Err(Error::ServiceIdInUse(service_id.to_string()));
            }

            let eid = table.node_ids.resolve_eid(&service_id)?;
            let sink = Sink::new(Arc::downgrade(&table), service_id.clone(), eid.clone());
            let service = factory.build(sink, eid.clone())?;
            info!("Attached service: {service_id}");

            // The queue's creation-time poll recovers any DeliverPending
            // bundles left over from an earlier registration on this
            // EID; they wait in it until `start` drains them.
            let record = Record::new(
                service,
                service_id.clone(),
                eid.clone(),
                dispatcher.new_delivery_channel(&eid),
            );
            built.insert(service_id, Slot::Attached(record));
        }

        metrics::gauge!("bpa.service.registered").increment(built.len() as f64);
        *table.services.lock() = built;
        Ok(table)
    }

    /// Puts every attached service into service, in service number
    /// order.
    ///
    /// Called by `Bpa::start` once storage recovery has completed: a
    /// delivery queue draining earlier would offer a service bundles the
    /// consistency check has yet to settle. A service that cannot run is
    /// unregistered and logged rather than keeping the whole BPA down.
    pub async fn start(self: &Arc<Self>) {
        let mut attached = self
            .services
            .lock()
            .values()
            .filter_map(|slot| match slot {
                Slot::Attached(record) => Some(record.clone()),
                Slot::Building => None,
            })
            .collect::<Vec<_>>();
        attached.sort();

        for record in attached {
            if let Err(e) = self.publish(record.clone()).await {
                error!("Service {} failed to start: {e}", record.service_id);
            }
        }
    }

    pub async fn shutdown(&self) {
        let records = self
            .services
            .lock()
            .drain()
            .filter_map(|(_, slot)| match slot {
                Slot::Attached(record) => Some(record),
                Slot::Building => None,
            })
            .collect::<Vec<_>>();

        if !records.is_empty() {
            metrics::gauge!("bpa.service.registered").decrement(records.len() as f64);
        }

        for record in records {
            self.withdraw(record).await;
        }

        self.tasks.shutdown().await;
    }

    /// Attaches the service and puts it into service: the runtime
    /// door, where the two halves are one call. A service that cannot
    /// run is unregistered again, so a failure leaves nothing behind.
    ///
    /// The service is filed under `id`, or under a freshly allocated
    /// service number when `id` is `None`. The factory is the
    /// embedder's code and may do anything, so it runs outside the
    /// lock, and against a reservation: the id is claimed first, so no
    /// service is ever built only to be discarded because another
    /// registration took the id, and none can be reached while it is
    /// still being built.
    pub async fn register(
        self: &Arc<Self>,
        id: Option<ServiceId>,
        factory: FactoryKind<'_>,
    ) -> Result<()> {
        // An allocated service number collides with nothing by
        // construction; an explicit one may name the administrative
        // endpoint, which is structurally registered (RFC 9758
        // Section 5.7).
        let service_id =
            id.unwrap_or_else(|| ServiceId::Ipn(self.next_dynamic.fetch_add(1, Ordering::Relaxed)));
        if service_is_admin_endpoint(&service_id) {
            return Err(Error::AdministrativeEndpoint(service_id.to_string()));
        }

        let eid = self.node_ids.resolve_eid(&service_id)?;

        match self.services.lock().entry(service_id.clone()) {
            Entry::Vacant(slot) => {
                slot.insert(Slot::Building);
            }
            Entry::Occupied(_) => return Err(Error::ServiceIdInUse(service_id.to_string())),
        }

        let sink = Sink::new(Arc::downgrade(self), service_id.clone(), eid.clone());
        let built = factory.build(sink, eid.clone()).and_then(|service| {
            // The queue's creation-time poll recovers any DeliverPending
            // bundles left over from an earlier registration on this EID;
            // they wait in it until `publish` drains them.
            Ok(Record::new(
                service,
                service_id.clone(),
                eid.clone(),
                self.dispatcher()?.new_delivery_channel(&eid),
            ))
        });

        let record = match built {
            Ok(record) => record,
            Err(e) => {
                // The reservation existed only for this build.
                self.services.lock().remove(&service_id);
                return Err(e);
            }
        };

        {
            let mut services = self.services.lock();
            if !matches!(services.get(&service_id), Some(Slot::Building)) {
                // An `unregister` arrived while the factory ran and took
                // the reservation with it. Nothing reached the service
                // and it was never started, so dropping it is the whole
                // teardown.
                return Err(Error::Disconnected);
            }
            services.insert(service_id, Slot::Attached(record.clone()));
        }

        metrics::gauge!("bpa.service.registered").increment(1.0);
        info!("Attached service: {}", record.service_id);

        self.publish(record).await
    }

    /// Unregisters the service at `service_id` and withdraws everything
    /// it published.
    pub async fn unregister(&self, service_id: &ServiceId) {
        // A reservation is taken out too: the build that holds it reads
        // its absence and takes the service straight back down instead
        // of filing it.
        let Some(Slot::Attached(record)) = self.services.lock().remove(service_id) else {
            return;
        };

        metrics::gauge!("bpa.service.registered").decrement(1.0);
        self.withdraw(record).await;
    }

    // The registration at `service_id`, if there is one.
    fn record(&self, service_id: &ServiceId) -> Option<Arc<Record>> {
        self.services
            .lock()
            .get(service_id)
            .and_then(|slot| match slot {
                Slot::Attached(record) => Some(record.clone()),
                Slot::Building => None,
            })
    }

    // Puts an attached service into service: starts it, drains its
    // delivery queue and publishes its route. One that cannot run is
    // unregistered again.
    async fn publish(self: &Arc<Self>, record: Arc<Record>) -> Result<()> {
        let dispatcher = self.dispatcher()?;

        // The service runs before anything can reach it: its queue was
        // opened at attachment, and may already hold bundles the
        // channel's creation-time poll recovered.
        if let Err(e) = record.kind.start().await {
            self.unregister(&record.service_id).await;
            return Err(e);
        }

        dispatcher.start_delivery_queue(record.clone());
        let _ = self
            .rib
            .add_service(record.eid.clone(), record.clone())
            .await;

        // The post-registration poll is spawned, never awaited inline: a
        // service whose event buffer drains only after registration
        // returns, the gRPC-bridge shape, would deadlock registration
        // against its own announcements. Overlapping polls are safe: the
        // poll claims each bundle out of WaitingForService with a status
        // CAS.
        let eid = record.eid.clone();
        hardy_async::spawn!(self.tasks, "post_registration_poll", async move {
            dispatcher.poll_service_waiting(&eid).await;
        });

        info!("Registered service: {}", record.service_id);
        Ok(())
    }

    fn dispatcher(&self) -> Result<Arc<Dispatcher>> {
        self.dispatcher.upgrade().ok_or(Error::Disconnected)
    }

    // Tears a registration down: wake its streams, close its queue,
    // withdraw its route, stop the service.
    async fn withdraw(&self, record: Arc<Record>) {
        // First: wake every in-flight stream of this registration.
        record.cancel.cancel();

        // Close the delivery queue: the consumer finishes its in-flight
        // delivery and exits; queued bundles are swept to WaitingForService
        // by the RIB removal below.
        record.close_queue();

        self.rib.remove_service(&record.eid, record.clone()).await;

        if let Err(e) = record.kind.shutdown().await {
            error!("Service {} failed to shut down cleanly: {e}", record.eid);
        }

        info!("Unregistered service: {}", record.eid);
    }
}

// The gauge counts what the map holds: `unregister` takes services out
// one at a time, and dropping the table takes whatever is left. A table
// dropped before it is ever started, which is how a failed `Bpa::build`
// unwinds, is accounted for here and needs nothing else: a service that
// never ran holds nothing but itself.
impl Drop for Table {
    fn drop(&mut self) {
        let attached = self.services.get_mut().len();
        if attached > 0 {
            metrics::gauge!("bpa.service.registered").decrement(attached as f64);
        }
    }
}

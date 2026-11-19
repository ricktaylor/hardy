use hardy_async::sync::spin::Mutex;
use hardy_eid_patterns::EidPattern;
use tracing::{error, info};

// `Sink` names a type in this module, so the trait of the same name is
// used qualified, as `routing::Sink`, and `routing::Agent` with it.
use super::{BoxFactory, Context};
use crate::{
    Arc, HashMap, Weak, async_trait,
    hash_map::Entry,
    node_ids::NodeIds,
    routing::{self, Error, Result, RouteAction, rib::Rib},
};

// The map is behind a spin lock. Every critical section is an O(1) map
// operation, and nothing is awaited while the lock is held; the embedder's
// factory runs outside the table's lock, against a reservation.

// A name is claimed, rather than occupied by an agent, while its
// registration builds and while its agent is torn down.
enum Claim {
    Building,
    Unregistering,
}

// The occupant of a name: a registered agent, or a claim on the name.
enum Slot {
    Claimed(Claim),
    Attached(Arc<dyn routing::Agent>),
}

impl Slot {
    fn attached(&self) -> Option<&Arc<dyn routing::Agent>> {
        match self {
            Slot::Attached(agent) => Some(agent),
            Slot::Claimed(_) => None,
        }
    }
}

/// The registered routing agents of a BPA.
pub struct Table {
    node_ids: Arc<NodeIds>,
    rib: Arc<Rib>,
    agents: Mutex<HashMap<String, Slot>>,
}

impl Table {
    /// Creates an empty table. Agents are added by
    /// [`register`](Self::register) only.
    pub fn new(node_ids: Arc<NodeIds>, rib: Arc<Rib>) -> Arc<Self> {
        Arc::new(Self {
            node_ids,
            rib,
            agents: Mutex::new(HashMap::new()),
        })
    }

    /// Builds the agent, files it under `name`, and calls
    /// [`Agent::register`](routing::Agent::register).
    ///
    /// The factory runs outside the table's lock, against a reservation of
    /// `name`, so no other registration can take the name and no caller
    /// can reach an agent that is still being built.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DuplicateName`] or [`Error::Unregistering`] if
    /// `name` is taken, or the error returned by the factory or by
    /// [`Agent::register`](routing::Agent::register). An agent whose
    /// `register` fails is unregistered again, so a failed call registers
    /// nothing.
    pub async fn register(self: &Arc<Self>, name: &str, factory: BoxFactory<'_>) -> Result<()> {
        let reservation = self.reserve(name)?;

        // Dropping the reservation releases the name.
        let agent = factory.build(Context::new(
            Arc::new(Sink {
                table: Arc::downgrade(self),
                name: Arc::from(name),
            }),
            self.node_ids.clone(),
        ))?;
        reservation.publish(agent.clone())?;

        metrics::gauge!("bpa.routing.agents").increment(1.0);
        info!("Registered routing agent: {name}");

        if let Err(e) = agent.register().await {
            self.unregister(name).await;
            return Err(e);
        }
        Ok(())
    }

    /// Unregisters the agent filed under `name` and withdraws every route
    /// it added.
    ///
    /// Returns `true` if this call performed the unregistration, and
    /// `false` if `name` is absent, already unregistering, or still
    /// building. An agent that is still building is torn down by
    /// [`register`](Self::register) instead.
    pub async fn unregister(&self, name: &str) -> bool {
        let agent = {
            let mut agents = self.agents.lock();
            let Some(slot) = agents.get_mut(name) else {
                return false;
            };

            // Claim the name for the teardown, so that nothing registers
            // over routes that are still being withdrawn.
            match core::mem::replace(slot, Slot::Claimed(Claim::Unregistering)) {
                Slot::Attached(agent) => agent,
                // A registration holds the name. The claim just written
                // tells it to tear the agent down instead of filing it.
                Slot::Claimed(_) => return false,
            }
        };

        if let Err(e) = agent.unregister().await {
            error!("Routing agent {name} failed to shut down cleanly: {e}");
        }

        // The agent has stopped, so the sweep cannot race a route added on
        // the way out.
        self.rib.remove_by_source(name).await;

        self.agents.lock().remove(name);
        metrics::gauge!("bpa.routing.agents").decrement(1.0);
        info!("Unregistered routing agent: {name}");

        true
    }

    pub async fn shutdown(&self) {
        // Outstanding claims become teardowns, so a registration still
        // inside its factory releases its name instead of filing into a
        // table that is shutting down.
        let names = {
            let mut agents = self.agents.lock();
            agents
                .iter_mut()
                .filter_map(|(name, slot)| match slot {
                    Slot::Attached(_) => Some(name.clone()),
                    Slot::Claimed(claim) => {
                        *claim = Claim::Unregistering;
                        None
                    }
                })
                .collect::<Vec<_>>()
        };

        for name in names {
            self.unregister(&name).await;
        }
    }

    // Claims `name`, failing if an agent or another claim holds it.
    fn reserve<'a>(&'a self, name: &'a str) -> Result<Reservation<'a>> {
        match self.agents.lock().entry(name.to_owned()) {
            Entry::Vacant(slot) => {
                slot.insert(Slot::Claimed(Claim::Building));
                Ok(Reservation {
                    table: self,
                    name,
                    claimed: true,
                })
            }
            Entry::Occupied(slot) => Err(match slot.get() {
                Slot::Attached(_) | Slot::Claimed(Claim::Building) => {
                    Error::DuplicateName(name.to_owned())
                }
                Slot::Claimed(Claim::Unregistering) => Error::Unregistering(name.to_owned()),
            }),
        }
    }

    // A name held only by a claim is not attached.
    fn is_attached(&self, name: &str) -> bool {
        self.agents
            .lock()
            .get(name)
            .is_some_and(|slot| slot.attached().is_some())
    }
}

// `unregister` decrements the gauge per agent; this accounts for the
// agents still registered when the table is dropped.
impl Drop for Table {
    fn drop(&mut self) {
        let attached = self
            .agents
            .get_mut()
            .values()
            .filter_map(Slot::attached)
            .count();
        if attached > 0 {
            metrics::gauge!("bpa.routing.agents").decrement(attached as f64);
        }
    }
}

// A claim on a name, released on drop unless `publish` consumes it.
#[must_use = "an unused reservation releases its name immediately"]
struct Reservation<'a> {
    table: &'a Table,
    name: &'a str,
    claimed: bool,
}

impl Reservation<'_> {
    // Files `agent` under the reserved name.
    //
    // Fails with `Error::Unregistering` if an `unregister` arrived while
    // the factory ran. The agent is then dropped having never been
    // published or started, which completes the teardown.
    fn publish(mut self, agent: Arc<dyn routing::Agent>) -> Result<()> {
        let mut agents = self.table.agents.lock();
        self.claimed = false;
        if let Some(slot @ Slot::Claimed(Claim::Building)) = agents.get_mut(self.name) {
            *slot = Slot::Attached(agent);
            return Ok(());
        }

        agents.remove(self.name);
        Err(Error::Unregistering(self.name.to_owned()))
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.claimed {
            self.table.agents.lock().remove(self.name);
        }
    }
}

// Holds the agent's name and a weak reference to the table, and
// re-resolves on each call, so the table remains the only authority on
// liveness. A failed resolution maps to `Error::Disconnected`.
struct Sink {
    table: Weak<Table>,
    name: Arc<str>,
}

impl Sink {
    fn resolve(&self) -> Result<Arc<Table>> {
        let table = self.table.upgrade().ok_or(Error::Disconnected)?;
        if table.is_attached(&self.name) {
            Ok(table)
        } else {
            Err(Error::Disconnected)
        }
    }
}

#[async_trait]
impl routing::Sink for Sink {
    async fn unregister(&self) {
        if let Some(table) = self.table.upgrade() {
            table.unregister(&self.name).await;
        }
    }

    async fn add_route(
        &self,
        pattern: EidPattern,
        action: RouteAction,
        priority: u32,
    ) -> Result<bool> {
        self.resolve()?
            .rib
            .add(pattern, self.name.to_string(), action.into(), priority)
            .await
    }

    async fn remove_route(
        &self,
        pattern: &EidPattern,
        action: &RouteAction,
        priority: u32,
    ) -> Result<bool> {
        Ok(self
            .resolve()?
            .rib
            .remove(pattern, &self.name, action.clone().into(), priority)
            .await)
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroUsize;

    use hardy_bpv7::eid::IpnNodeId;
    use hardy_eid_patterns::EidPattern;

    use super::{Rib, Table};
    use crate::{
        Arc,
        node_ids::NodeIds,
        routing::{
            Agent, BoxFactory, Context, Error, Factory, Result, RouteAction, Sink, action::Action,
            factory_fn,
        },
        storage::{BundleMemStorage, MetadataMemStorage, store::Store},
    };

    // An agent that keeps only its sink, so that the tests can drive the
    // table through the agent side of the contract.
    struct TestAgent {
        sink: Arc<dyn Sink>,
    }

    impl TestAgent {
        fn factory() -> impl Factory<Component = Self> {
            factory_fn(|cx: Context| Ok(Arc::new(Self { sink: cx.sink })))
        }
    }

    impl Agent for TestAgent {}

    fn make_table() -> (Arc<Rib>, Arc<Table>) {
        let node_ids = Arc::new(NodeIds {
            ipn: Some(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            dtn: None,
        });
        let store = Arc::new(Store::new(
            NonZeroUsize::new(16).unwrap(),
            Arc::new(MetadataMemStorage::new(None)),
            Arc::new(BundleMemStorage::new(None, None)),
        ));
        let rib = Arc::new(Rib::new(node_ids.clone(), store, 1));
        let table = Table::new(node_ids, rib.clone());
        (rib, table)
    }

    async fn register(table: &Arc<Table>, name: &str) -> Arc<TestAgent> {
        let mut agent = None;
        table
            .register(
                name,
                BoxFactory::capturing(TestAgent::factory(), &mut agent),
            )
            .await
            .expect("Registration should succeed");
        agent.expect("Registration builds the factory")
    }

    #[tokio::test]
    async fn duplicate_name_is_refused() {
        let (_rib, table) = make_table();
        register(&table, "test-agent").await;

        let Err(err) = table
            .register("test-agent", BoxFactory::new(TestAgent::factory()))
            .await
        else {
            panic!("A second agent claiming the name should be refused");
        };
        assert!(matches!(err, Error::DuplicateName(ref name) if name == "test-agent"));

        table.shutdown().await;
    }

    // The factory must not run under the table's lock: a factory is free
    // to call back into the table.
    #[tokio::test]
    async fn the_factory_runs_outside_the_table_lock() {
        let (_rib, table) = make_table();
        register(&table, "neighbour").await;

        let probe = Arc::downgrade(&table);
        table
            .register(
                "probe",
                BoxFactory::new(factory_fn(move |cx: Context| {
                    let table = probe.upgrade().expect("The table outlives the build");
                    assert!(
                        table.agents.try_lock().is_some(),
                        "The table's lock is free while the factory runs"
                    );
                    assert!(
                        table.is_attached("neighbour"),
                        "The table is readable from inside a factory"
                    );
                    Ok(Arc::new(TestAgent { sink: cx.sink }))
                })),
            )
            .await
            .expect("Registration should succeed");

        table.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_build_releases_the_name() {
        let (_rib, table) = make_table();

        let Err(err) = table
            .register(
                "flaky",
                BoxFactory::new(factory_fn(|_cx: Context| -> Result<Arc<TestAgent>> {
                    Err(Error::Internal("The factory refused".into()))
                })),
            )
            .await
        else {
            panic!("A factory that fails should fail the registration");
        };
        assert!(matches!(err, Error::Internal(_)));

        register(&table, "flaky").await;

        table.shutdown().await;
    }

    // Unregistration withdraws the agent's routes, disconnects its sink
    // and releases its name.
    #[tokio::test]
    async fn unregistering_sweeps_routes_and_releases_the_name() {
        let (rib, table) = make_table();
        let agent = register(&table, "sweep").await;

        let pattern: EidPattern = "ipn:0.9.*".parse().unwrap();
        let action = RouteAction::Via("ipn:0.2.0".parse().unwrap());
        assert!(
            agent
                .sink
                .add_route(pattern.clone(), action.clone(), 1)
                .await
                .unwrap()
        );

        agent.sink.unregister().await;

        assert!(
            !rib.remove(&pattern, "sweep", Action::Route(action.clone()), 1)
                .await,
            "Unregistering should have swept the agent's routes"
        );
        assert!(
            matches!(
                agent.sink.add_route(pattern, action, 1).await,
                Err(Error::Disconnected)
            ),
            "The sink of an unregistered agent is disconnected"
        );

        register(&table, "sweep").await;

        table.shutdown().await;
    }
}

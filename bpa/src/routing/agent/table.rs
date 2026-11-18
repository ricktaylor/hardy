use hardy_async::sync::spin::Mutex;
use hardy_eid_patterns::EidPattern;
use tracing::{error, info};

// `routing::Agent` and `routing::Sink` are the traits this module's slots
// and `Sink` are built around, so both names are taken here; the two
// traits stay module-qualified at their few use sites.
use super::{BoxFactory, Context};
use crate::{
    Arc, HashMap, Weak, async_trait,
    hash_map::Entry,
    node_ids::NodeIds,
    routing::{self, Error, Result, RouteAction, rib::Rib},
};

// The agent map is behind a spin lock. Every critical section is an O(1)
// lookup or insert with nothing awaited, blocked or iterated inside it,
// so the lock is never held long enough to repay an OS mutex's park. In
// particular the embedder's factory runs outside it, against a
// reservation.

// Where an agent stands.
//
// `Building` holds the name while the factory runs, before there is an
// agent to hold; `Attached` is usable; `Detaching` marks the window
// between `detach` taking the agent and the teardown it awaits
// completing. Only `Attached` is usable, and all three hold the name
// against reuse.
enum Slot {
    Building,
    Attached(Arc<dyn routing::Agent>),
    Detaching,
}

/// The routing agents of one BPA: the only place an agent is built, and
/// the authority on which ones are live.
pub struct Table {
    node_ids: Arc<NodeIds>,
    rib: Arc<Rib>,
    agents: Mutex<HashMap<String, Slot>>,
}

impl Table {
    /// The table of one BPA, holding the `agents` declared for it, each
    /// built and filed inert.
    ///
    /// The map is complete before it reaches the table, so there is no
    /// moment at which the table is live but incomplete, and an agent
    /// that cannot be built takes the ones before it down with it,
    /// leaving nothing behind.
    pub fn new(
        node_ids: Arc<NodeIds>,
        rib: Arc<Rib>,
        agents: Vec<(String, BoxFactory<'static>)>,
    ) -> Result<Arc<Self>> {
        let table = Arc::new(Self {
            node_ids,
            rib,
            agents: Mutex::new(HashMap::new()),
        });

        let mut built = HashMap::with_capacity(agents.len());
        for (name, factory) in agents {
            if built.contains_key(&name) {
                return Err(Error::DuplicateName(name));
            }

            let sink = Arc::new(Sink {
                table: Arc::downgrade(&table),
                name: Arc::from(name.as_str()),
            });
            let agent = factory.build(Context::new(sink, table.node_ids.clone()))?;
            info!("Attached routing agent: {name}");
            built.insert(name, Slot::Attached(agent));
        }

        metrics::gauge!("bpa.routing.agents").increment(built.len() as f64);
        *table.agents.lock() = built;
        Ok(table)
    }

    /// Attaches the agent and starts it: the runtime door, where the two
    /// halves are one call. An agent that cannot run is detached again,
    /// so a failure leaves nothing behind.
    ///
    /// The factory is the embedder's code and may do anything, so it
    /// runs outside the lock: the name is reserved first, and the
    /// reservation is resolved into an agent or released again
    /// afterwards. No one else can claim the name in between, and no
    /// one can reach an agent that is still being built.
    pub async fn register(self: &Arc<Self>, name: &str, factory: BoxFactory<'_>) -> Result<()> {
        match self.agents.lock().entry(name.to_owned()) {
            Entry::Vacant(slot) => {
                slot.insert(Slot::Building);
            }
            Entry::Occupied(slot) => {
                return Err(match slot.get() {
                    Slot::Building | Slot::Attached(_) => Error::DuplicateName(name.to_owned()),
                    Slot::Detaching => Error::Detaching(name.to_owned()),
                });
            }
        }

        let built = factory.build(Context::new(
            Arc::new(Sink {
                table: Arc::downgrade(self),
                name: Arc::from(name),
            }),
            self.node_ids.clone(),
        ));

        let agent = match built {
            Ok(agent) => agent,
            Err(e) => {
                // The reservation existed only for this build.
                self.agents.lock().remove(name);
                return Err(e);
            }
        };

        {
            let mut agents = self.agents.lock();
            if !matches!(agents.get(name), Some(Slot::Building)) {
                // A `detach` arrived while the factory ran and left
                // `Detaching` in the reservation. Nothing reached the
                // agent and it was never started, so releasing the name
                // and dropping it is the whole teardown.
                agents.remove(name);
                return Err(Error::Detaching(name.to_owned()));
            }
            agents.insert(name.to_owned(), Slot::Attached(agent.clone()));
        }

        metrics::gauge!("bpa.routing.agents").increment(1.0);
        info!("Attached routing agent: {name}");

        if let Err(e) = agent.start().await {
            self.detach(name).await;
            return Err(e);
        }
        Ok(())
    }

    /// Starts every attached agent; each one that cannot run at all is
    /// detached, so the table holds only working agents when this
    /// returns.
    ///
    /// Called by `Bpa::start` once storage recovery has completed: an
    /// agent installing routes earlier would race the consistency check.
    pub async fn start(&self) {
        let attached = self
            .agents
            .lock()
            .iter()
            .filter_map(|(name, slot)| match slot {
                Slot::Attached(agent) => Some((name.clone(), agent.clone())),
                Slot::Building | Slot::Detaching => None,
            })
            .collect::<Vec<_>>();

        for (name, agent) in attached {
            if let Err(e) = agent.start().await {
                error!("Routing agent {name} failed to start: {e}");
                self.detach(&name).await;
            }
        }
    }

    /// Tears the named agent down and withdraws every route it added.
    ///
    /// Returns whether this call did the detaching: a name that is
    /// absent, already being detached by another caller, or still being
    /// built, answers `false`. In the last case the build itself
    /// completes the detachment.
    pub async fn detach(&self, name: &str) -> bool {
        let agent = {
            let mut agents = self.agents.lock();
            let Some(slot) = agents.get_mut(name) else {
                return false;
            };
            match core::mem::replace(slot, Slot::Detaching) {
                Slot::Attached(agent) => agent,
                // The factory is still running. Leaving `Detaching`
                // behind is enough: `attach` reads it and takes the
                // agent straight back down instead of filing it.
                Slot::Building => return false,
                // Another caller is already taking this one down, and
                // the replace put back what it found.
                Slot::Detaching => return false,
            }
        };

        if let Err(e) = agent.shutdown().await {
            error!("Routing agent {name} failed to shut down cleanly: {e}");
        }

        // The agent is quiet by now, so the sweep cannot race a route it
        // adds on the way out.
        self.rib.remove_by_source(name).await;

        self.agents.lock().remove(name);
        metrics::gauge!("bpa.routing.agents").decrement(1.0);
        info!("Detached routing agent: {name}");

        true
    }

    pub async fn shutdown(&self) {
        let names = self.agents.lock().keys().cloned().collect::<Vec<_>>();
        for name in names {
            self.detach(&name).await;
        }
    }

    // Whether an agent is attached under `name`. A name a build still
    // holds, and one a detaching agent still holds, both answer `false`:
    // only an attached agent is usable.
    fn is_attached(&self, name: &str) -> bool {
        self.agents
            .lock()
            .get(name)
            .is_some_and(|slot| matches!(slot, Slot::Attached(_)))
    }
}

// The gauge counts what the map holds: `detach` takes agents out one
// at a time, and dropping the table takes whatever is left. A table
// dropped before it is ever started, which is how a failed `Bpa::build`
// unwinds, is accounted for here and needs nothing else: an agent that
// never ran holds nothing but itself.
impl Drop for Table {
    fn drop(&mut self) {
        let attached = self.agents.get_mut().len();
        if attached > 0 {
            metrics::gauge!("bpa.routing.agents").decrement(attached as f64);
        }
    }
}

// An agent is named, not pointed at: the sink holds its key and a weak,
// non-owning edge to the table, and re-resolves on every operation. The
// table is then the single authority on liveness, and a failed
// resolution is the condition the trait already names,
// `Error::Disconnected`.
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
            table.detach(&self.name).await;
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

    // An agent that does nothing but keep the sink it was built with, so
    // the tests can drive the table from the agent's side of the
    // contract.
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
        let table = Table::new(node_ids, rib.clone(), Vec::new()).unwrap();
        (rib, table)
    }

    // Registers an agent and hands back the component the table now owns.
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

    // A name is exclusive: the second agent to claim one is refused.
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

    // The embedder's factory is not called under the table's lock. A
    // factory may log, allocate or reach back into the table, any of
    // which would spin against a lock `attach` was still holding.
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
                        "The table's lock is held while the factory runs"
                    );
                    // The name is reserved for the duration, so nothing
                    // else can claim it while this runs.
                    assert!(table.is_attached("neighbour"));
                    Ok(Arc::new(TestAgent { sink: cx.sink }))
                })),
            )
            .await
            .expect("Registration should succeed");

        table.shutdown().await;
    }

    // A factory that fails leaves nothing behind, so the name it
    // reserved is free for the next attempt.
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

    // Detaching an agent takes its routes with it, disconnects its sink,
    // and releases its name.
    #[tokio::test]
    async fn detaching_sweeps_routes_and_releases_the_name() {
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
            "Detaching should have swept the agent's routes"
        );
        assert!(
            matches!(
                agent.sink.add_route(pattern, action, 1).await,
                Err(Error::Disconnected)
            ),
            "The sink of a detached agent is disconnected"
        );

        register(&table, "sweep").await;

        table.shutdown().await;
    }
}

use super::*;
use hardy_eid_patterns::EidPattern;

pub(crate) struct Agent {
    agent: Arc<dyn routes::RoutingAgent>,
    pub(crate) name: String,
}

struct Sink {
    agent: Weak<Agent>,
    rib: Arc<Rib>,
}

#[async_trait]
impl routes::RoutingSink for Sink {
    async fn unregister(&self) {
        if let Some(agent) = self.agent.upgrade() {
            self.rib.unregister_agent(agent).await;
        }
    }

    async fn add_route(
        &self,
        pattern: EidPattern,
        action: routes::Action,
        priority: u32,
    ) -> routes::Result<bool> {
        let agent = self.agent.upgrade().ok_or(routes::Error::Disconnected)?;
        Ok(self
            .rib
            .add(pattern, agent.name.clone(), action, priority)
            .await)
    }

    async fn remove_route(
        &self,
        pattern: &EidPattern,
        action: &routes::Action,
        priority: u32,
    ) -> routes::Result<bool> {
        let agent = self.agent.upgrade().ok_or(routes::Error::Disconnected)?;
        Ok(self
            .rib
            .remove(pattern, &agent.name, action, priority)
            .await)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(agent) = self.agent.upgrade() {
            let rib = self.rib.clone();
            hardy_async::spawn!(self.rib.tasks, "routing_agent_drop_cleanup", async move {
                rib.unregister_agent(agent).await;
            });
        }
    }
}

impl Rib {
    pub(crate) async fn register_agent(
        self: &Arc<Self>,
        name: String,
        agent: Arc<dyn routes::RoutingAgent>,
    ) -> routes::Result<Vec<NodeId>> {
        let agent = {
            let mut agents = self.agents.lock();
            let hash_map::Entry::Vacant(e) = agents.entry(name.clone()) else {
                return Err(routes::Error::AlreadyExists(name));
            };

            info!("Registered new routing agent: {name}");

            e.insert(Arc::new(Agent { agent, name })).clone()
        };

        let node_ids: Vec<NodeId> = (&*self.node_ids).into();

        agent
            .agent
            .on_register(
                Box::new(Sink {
                    agent: Arc::downgrade(&agent),
                    rib: self.clone(),
                }),
                &node_ids,
            )
            .await;

        Ok(node_ids)
    }

    async fn unregister_agent(&self, agent: Arc<Agent>) {
        let agent = self.agents.lock().remove(&agent.name);

        if let Some(agent) = agent {
            agent.agent.on_unregister().await;
            self.remove_by_source(&agent.name).await;
            info!("Unregistered routing agent: {}", agent.name);
        }
    }

    pub(crate) async fn shutdown_agents(&self) {
        let agents = self
            .agents
            .lock()
            .drain()
            .map(|(_, v)| v)
            .collect::<Vec<_>>();

        for agent in agents {
            agent.agent.on_unregister().await;
            self.remove_by_source(&agent.name).await;
            info!("Unregistered routing agent: {}", agent.name);
        }
    }
}

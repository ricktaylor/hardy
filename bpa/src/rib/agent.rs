use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use hardy_bpv7::eid::NodeId;
use tracing::info;

use super::Rib;
use super::context::{RouteOp, RoutingContext};
use crate::hash_map;
use crate::routes::{self, RoutingAgent};

pub(crate) struct Agent {
    agent: Arc<dyn RoutingAgent>,
    pub(crate) name: String,
}

impl Rib {
    pub(crate) async fn register_agent(
        self: &Arc<Self>,
        name: String,
        agent: Arc<dyn RoutingAgent>,
    ) -> routes::Result<Vec<NodeId>> {
        let agent = {
            let mut agents = self.agents.lock();
            let hash_map::Entry::Vacant(e) = agents.entry(name.clone()) else {
                return Err(routes::Error::AlreadyExists(name));
            };

            info!("Registered new routing agent: {name}");

            e.insert(Arc::new(Agent {
                agent,
                name: name.clone(),
            }))
            .clone()
        };

        metrics::gauge!("bpa.rib.agents").increment(1.0);

        let node_ids: Vec<NodeId> = (&*self.node_ids).into();

        let (route_tx, route_rx) = flume::unbounded();
        let shutdown = self.tasks.cancel_token().child_token();

        let ctx = RoutingContext::new(route_tx, shutdown.clone());

        let rib = self.clone();
        let agent_name = name.clone();
        hardy_async::spawn!(self.tasks, "routing_agent_receiver", async move {
            use futures::FutureExt;
            loop {
                futures::select_biased! {
                    _ = shutdown.cancelled().fuse() => break,
                    op = route_rx.recv_async().fuse() => match op {
                        Ok(RouteOp::Add { pattern, action, priority }) => {
                            rib.add(pattern, agent_name.clone(), action.into(), priority)
                                .await;
                        }
                        Ok(RouteOp::Remove { pattern, action, priority }) => {
                            rib.remove(&pattern, &agent_name, action.into(), priority)
                                .await;
                        }
                        Err(_) => break,
                    },
                }
            }

            // Channel closed or shutdown: unregister
            rib.unregister_agent(&agent_name).await;
        });

        agent.agent.on_register(ctx, &node_ids).await;

        Ok(node_ids)
    }

    pub(crate) async fn unregister_agent(&self, name: &str) {
        let agent = self.agents.lock().remove(name);

        if let Some(agent) = agent {
            metrics::gauge!("bpa.rib.agents").decrement(1.0);
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

        if !agents.is_empty() {
            metrics::gauge!("bpa.rib.agents").decrement(agents.len() as f64);
        }

        for agent in agents {
            agent.agent.on_unregister().await;
            self.remove_by_source(&agent.name).await;
            info!("Unregistered routing agent: {}", agent.name);
        }
    }
}

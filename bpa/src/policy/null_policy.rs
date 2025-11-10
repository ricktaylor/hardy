use super::*;

pub struct EgressController {
    queue: Arc<dyn policy::EgressQueue>,
}

#[async_trait]
impl policy::EgressController for EgressController {
    async fn forward(&self, _queue: Option<u32>, bundle: bundle::Bundle) {
        self.queue.forward(bundle).await
    }
}

#[async_trait]
impl policy::EgressQueue for EgressController {
    async fn forward(&self, bundle: bundle::Bundle) {
        self.queue.forward(bundle).await
    }
}

#[derive(Default)]
pub struct EgressPolicy {}

impl EgressPolicy {
    pub fn new() -> Self {
        Default::default()
    }
}

#[async_trait]
impl policy::EgressPolicy for EgressPolicy {
    fn queue_count(&self) -> u32 {
        0
    }

    fn classify(&self, _flow_label: Option<u32>) -> Option<u32> {
        None
    }

    async fn new_controller(
        &self,
        queues: HashMap<Option<u32>, Arc<dyn policy::EgressQueue>>,
    ) -> Arc<dyn policy::EgressController> {
        assert!(queues.len() == 1, "Too many queues!");
        let queue = queues.get(&None).trace_expect("No None queue?!?").clone();
        Arc::new(EgressController { queue })
    }
}

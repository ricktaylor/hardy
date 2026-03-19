use hardy_async::async_trait;
use trace_err::TraceErrOption;

use super::{EgressController, EgressPolicy, EgressQueue};
use crate::bundle::Bundle;
use crate::{Arc, HashMap};

pub struct NullEgressController {
    queue: Arc<dyn EgressQueue>,
}

#[async_trait]
impl EgressController for NullEgressController {
    async fn forward(&self, _queue: Option<u32>, bundle: Bundle) {
        self.queue.forward(bundle).await
    }
}

#[async_trait]
impl EgressQueue for NullEgressController {
    async fn forward(&self, bundle: Bundle) {
        self.queue.forward(bundle).await
    }
}

#[derive(Default)]
pub struct NullEgressPolicy {}

impl NullEgressPolicy {
    pub fn new() -> Self {
        Default::default()
    }
}

#[async_trait]
impl EgressPolicy for NullEgressPolicy {
    fn queue_count(&self) -> u32 {
        0
    }

    fn classify(&self, _flow_label: Option<u32>) -> Option<u32> {
        None
    }

    async fn new_controller(
        &self,
        queues: HashMap<Option<u32>, Arc<dyn EgressQueue>>,
    ) -> Arc<dyn EgressController> {
        assert!(queues.len() == 1, "Too many queues!");
        let queue = queues.get(&None).trace_expect("No None queue?!?").clone();
        Arc::new(NullEgressController { queue })
    }
}

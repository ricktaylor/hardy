use super::*;

struct Shared {
    cla: Arc<dyn Cla>,
    dispatcher: Arc<dispatcher::Dispatcher>,
    peer: u32,
    cla_addr: ClaAddress,
}

struct EgressQueue {
    shared: Arc<Shared>,
    lane: Option<u32>,
}

#[async_trait]
impl policy::EgressQueue for EgressQueue {
    async fn forward(&self, bundle: bundle::Bundle) {
        self.shared
            .dispatcher
            .forward_bundle(
                &*self.shared.cla,
                self.shared.peer,
                self.lane,
                &self.shared.cla_addr,
                bundle,
            )
            .await
    }
}

impl EgressQueue {
    fn create(shared: Arc<Shared>, lane: Option<u32>) -> Arc<dyn policy::EgressQueue> {
        Arc::new(Self { shared, lane })
    }
}

pub fn new_queue_set(
    cla: Arc<dyn Cla>,
    dispatcher: Arc<dispatcher::Dispatcher>,
    peer: u32,
    cla_addr: ClaAddress,
    lanes: Lanes,
) -> policy::EgressQueueSet {
    // A queue is built eagerly per explicit lane, so the count a CLA
    // declares sizes an allocation here; `MAX_PINNED_LANES` bounds it.
    let lane_count = lanes.pinned();
    let shared = Arc::new(Shared {
        cla,
        dispatcher,
        peer,
        cla_addr,
    });

    policy::EgressQueueSet {
        next_free: EgressQueue::create(shared.clone(), None),
        pinned: (0..lane_count)
            .map(|i| (i, EgressQueue::create(shared.clone(), Some(i))))
            .collect(),
    }
}

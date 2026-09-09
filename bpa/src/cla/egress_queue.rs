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
    lane_count: Option<core::num::NonZeroU32>,
) -> policy::EgressQueueSet {
    // A queue is instantiated eagerly per declared lane, so the count a CLA
    // declares directly sizes an allocation here — cap it to keep an absurd
    // declaration from becoming a resource bomb. Lane indices are u32 on the
    // trait surface; an over-declared count is clamped rather than wrapped.
    const MAX_EAGER_LANE_QUEUES: u32 = 256;
    let lane_count = lane_count.map_or(0, |n| n.get()).min(MAX_EAGER_LANE_QUEUES);
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

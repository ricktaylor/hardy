use core::num::NonZeroU32;

use super::{Address, Cla};
use crate::{
    Arc, async_trait,
    bundle::Bundle,
    dispatcher::Dispatcher,
    policy::{EgressQueue, EgressQueueSet},
};

/// The parts every lane of one adjacency shares.
struct Shared {
    cla: Arc<dyn Cla>,
    dispatcher: Arc<Dispatcher>,
    peer: u32,
    cla_addr: Address,
}

/// One lane of one adjacency, or the unpinned lane when `lane` is `None`.
struct Queue {
    shared: Arc<Shared>,
    lane: Option<u32>,
}

#[async_trait]
impl EgressQueue for Queue {
    async fn forward(&self, bundle: Bundle) {
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

impl Queue {
    fn create(shared: Arc<Shared>, lane: Option<u32>) -> Arc<dyn EgressQueue> {
        Arc::new(Self { shared, lane })
    }
}

pub fn new_set(
    cla: Arc<dyn Cla>,
    dispatcher: Arc<Dispatcher>,
    peer: u32,
    cla_addr: Address,
    lane_count: Option<NonZeroU32>,
) -> EgressQueueSet {
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

    EgressQueueSet {
        next_free: Queue::create(shared.clone(), None),
        pinned: (0..lane_count)
            .map(|i| (i, Queue::create(shared.clone(), Some(i))))
            .collect(),
    }
}

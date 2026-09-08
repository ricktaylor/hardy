use super::*;

struct Shared {
    cla: Arc<dyn Cla>,
    dispatcher: Arc<dispatcher::Dispatcher>,
    peer: u32,
    cla_addr: ClaAddress,
}

struct EgressQueue {
    shared: Arc<Shared>,
    queue: Option<u32>,
}

#[async_trait]
impl policy::EgressQueue for EgressQueue {
    async fn forward(&self, bundle: bundle::Bundle) {
        self.shared
            .dispatcher
            .forward_bundle(
                &*self.shared.cla,
                self.shared.peer,
                self.queue,
                &self.shared.cla_addr,
                bundle,
            )
            .await
    }
}

impl EgressQueue {
    fn create(shared: Arc<Shared>, queue: Option<u32>) -> Arc<dyn policy::EgressQueue> {
        Arc::new(Self { shared, queue })
    }
}

pub fn new_queue_set(
    cla: Arc<dyn Cla>,
    dispatcher: Arc<dispatcher::Dispatcher>,
    peer: u32,
    cla_addr: ClaAddress,
    lane_count: Option<core::num::NonZeroU32>,
) -> HashMap<Option<u32>, Arc<dyn policy::EgressQueue>> {
    // The declared count sizes an allocation loop, so it is clamped to
    // MAX_LANE_COUNT rather than trusted.
    let lane_count = lane_count.map_or(0, |n| n.get());
    let lane_count = if lane_count > super::MAX_LANE_COUNT {
        warn!(
            "CLA declared {lane_count} egress lanes, clamping to {}",
            super::MAX_LANE_COUNT
        );
        super::MAX_LANE_COUNT
    } else {
        lane_count
    };
    let shared = Arc::new(Shared {
        cla,
        dispatcher,
        peer,
        cla_addr,
    });

    let mut h: HashMap<Option<u32>, Arc<dyn policy::EgressQueue>> =
        [(None, EgressQueue::create(shared.clone(), None))].into();
    for i in 0..lane_count {
        h.insert(Some(i), EgressQueue::create(shared.clone(), Some(i)));
    }
    h
}

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
    queue_count: u32,
) -> HashMap<Option<u32>, Arc<dyn policy::EgressQueue>> {
    let shared = Arc::new(Shared {
        cla,
        dispatcher,
        peer,
        cla_addr,
    });

    let mut h: HashMap<Option<u32>, Arc<dyn policy::EgressQueue>> =
        [(None, EgressQueue::create(shared.clone(), None))].into();
    for i in 0..queue_count {
        h.insert(Some(i), EgressQueue::create(shared.clone(), Some(i)));
    }
    h
}

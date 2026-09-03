use super::*;

/// A pass-through egress controller: every bundle transmits on the next
/// free lane.
pub struct EgressController {
    queue: Arc<dyn policy::EgressQueue>,
}

#[async_trait]
impl policy::EgressController for EgressController {
    async fn forward(&self, _queue: u32, bundle: bundle::Bundle) {
        self.queue.forward(bundle).await
    }
}

#[async_trait]
impl policy::EgressQueue for EgressController {
    async fn forward(&self, bundle: bundle::Bundle) {
        self.queue.forward(bundle).await
    }
}

/// The null egress policy: one total FIFO queue, no prioritisation, no lane
/// pinning — it applies no policy.
#[derive(Default)]
pub struct EgressPolicy {}

impl EgressPolicy {
    /// Creates a new null egress policy with default settings.
    pub fn new() -> Self {
        Default::default()
    }
}

#[async_trait]
impl policy::EgressPolicy for EgressPolicy {
    fn queue_count(&self) -> core::num::NonZeroU32 {
        core::num::NonZeroU32::MIN
    }

    fn classify(&self, _flow_label: Option<u32>) -> u32 {
        0
    }

    async fn new_controller(
        &self,
        queues: HashMap<Option<u32>, Arc<dyn policy::EgressQueue>>,
    ) -> Arc<dyn policy::EgressController> {
        // Applying no policy means imposing no lane constraint: the one
        // queue transmits with the next-free-lane directive (`None`), so a
        // multi-lane CLA still fans across its idle lanes. Any pinned
        // per-lane queues a CLA's declaration created simply sit unused —
        // pinning is what a real policy does when it wants flow affinity.
        let queue = queues
            .get(&None)
            .trace_expect("No next-free queue?!?")
            .clone();
        Arc::new(EgressController { queue })
    }
}

#[cfg(test)]
mod tests {
    use crate::policy::EgressPolicy as _;

    use super::*;

    struct CapturingQueue {
        lane: Option<u32>,
        tx: flume::Sender<Option<u32>>,
    }

    #[async_trait]
    impl policy::EgressQueue for CapturingQueue {
        async fn forward(&self, _bundle: bundle::Bundle) {
            let _ = self.tx.send(self.lane);
        }
    }

    // A CLA declaring pinned lanes must not panic the null policy (remote,
    // once the v1 wire carries `lane_count`): its single queue transmits
    // with the next-free directive, so the pinned endpoints stay idle and
    // every bundle — whatever queue index it arrives with — goes next-free.
    #[tokio::test]
    async fn declared_lanes_are_tolerated_on_the_next_free_endpoint() {
        let (tx, rx) = flume::unbounded();
        let mut queues: HashMap<Option<u32>, Arc<dyn policy::EgressQueue>> = HashMap::new();
        for lane in [None, Some(0), Some(1)] {
            queues.insert(
                lane,
                Arc::new(CapturingQueue {
                    lane,
                    tx: tx.clone(),
                }),
            );
        }
        let controller = EgressPolicy::new().new_controller(queues).await;

        let (_, data) = hardy_bpv7::builder::Builder::new(
            "ipn:0.1.1".parse().unwrap(),
            "ipn:0.2.1".parse().unwrap(),
        )
        .with_payload(b"x".as_slice().into())
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .unwrap();
        let parsed = hardy_bpv7::parse::parse(crate::Bytes::from(data)).unwrap();
        let record = bundle::Bundle {
            bpv7: parsed.bundle,
            metadata: bundle::BundleMetadata::originated(),
            status: bundle::BundleStatus::New,
        };

        controller.forward(1, record).await;
        assert_eq!(
            rx.recv().expect("an endpoint received the bundle"),
            None,
            "transmitted with the next-free-lane directive"
        );
        assert!(rx.is_empty(), "no pinned endpoint received anything");
    }
}

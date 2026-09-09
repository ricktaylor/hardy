use core::num::NonZeroU32;

use super::*;

/// A pass-through egress controller: every bundle transmits on the next
/// free lane.
pub struct FlowController {
    queue: Arc<dyn policy::EgressQueue>,
}

#[async_trait]
impl policy::FlowController for FlowController {
    fn queue_for(&self) -> u32 {
        0
    }

    async fn forward(&self, _queue: u32, bundle: bundle::Bundle) {
        self.queue.forward(bundle).await
    }
}

/// The null egress policy: one total FIFO queue, no prioritisation, no lane
/// pinning — it applies no policy.
#[derive(Default)]
pub struct FlowControllerFactory {}

impl FlowControllerFactory {
    /// Creates a new null egress policy with default settings.
    pub fn new() -> Self {
        Default::default()
    }
}

#[async_trait]
impl policy::FlowControllerFactory for FlowControllerFactory {
    fn queue_count(&self) -> NonZeroU32 {
        NonZeroU32::MIN
    }

    async fn new_controller(
        &self,
        queues: policy::EgressQueueSet,
    ) -> Arc<dyn policy::FlowController> {
        // Applying no policy means imposing no lane constraint: the one
        // queue transmits with the next-free-lane directive, so a
        // multi-lane CLA still fans across its idle lanes. Any pinned
        // per-lane queues a CLA's declaration created simply sit unused —
        // pinning is what a real policy does when it wants flow affinity.
        Arc::new(FlowController {
            queue: queues.next_free,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::policy::FlowControllerFactory as _;

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
        let queues = policy::EgressQueueSet {
            next_free: Arc::new(CapturingQueue {
                lane: None,
                tx: tx.clone(),
            }),
            pinned: [0, 1]
                .into_iter()
                .map(|lane| {
                    (
                        lane,
                        Arc::new(CapturingQueue {
                            lane: Some(lane),
                            tx: tx.clone(),
                        }) as Arc<dyn policy::EgressQueue>,
                    )
                })
                .collect(),
        };
        let controller = FlowControllerFactory::new().new_controller(queues).await;

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

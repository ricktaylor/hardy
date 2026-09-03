use tracing::warn;

use super::*;

/// A pass-through egress controller that forwards all bundles to a single queue.
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

/// A no-op egress policy: zero priority queues, all bundles go to the default FIFO queue.
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
        // A CLA may declare explicit egress lanes (`Cla::lane_count`) — a
        // CLA-side shape declaration, not this policy's contract, and on the
        // v1 wire a remote CLA's to make. This policy classifies nothing
        // onto them, so the declared lanes sit unused and every bundle
        // forwards on the default queue.
        if queues.len() > 1 {
            warn!(
                "Null egress policy ignoring {} declared CLA lanes",
                queues.len() - 1
            );
        }
        let queue = queues.get(&None).trace_expect("No None queue?!?").clone();
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

    // A CLA declaring explicit egress lanes must not panic the null policy:
    // the declaration is CLA-side shape (remote, once the v1 wire carries
    // `lane_count`), so the lanes sit unused and every bundle — whatever
    // queue index it arrives with — forwards on the default queue.
    #[tokio::test]
    async fn declared_lanes_are_tolerated_on_the_default_queue() {
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

        controller.forward(Some(1), record).await;
        assert_eq!(
            rx.recv().expect("a queue received the bundle"),
            None,
            "forwarded on the default queue"
        );
        assert!(rx.is_empty(), "no declared lane received anything");
    }
}

use super::*;

/// A no-op egress policy that uses a single FIFO queue with no prioritization.
pub mod null_policy;

// #[cfg(feature = "htb_policy")]
// pub mod htb_policy;

// #[cfg(feature = "tbf_policy")]
// pub mod tbf_policy;

/// A trait for controlling the egress of bundles through a CLA.
/// This is often implemented by a CLA itself or by a policy manager.
#[async_trait]
pub trait EgressController: Send + Sync {
    /// Forwards a bundle to a specific queue in a Controller.
    async fn forward(&self, queue: Option<u32>, bundle: bundle::Bundle);
}

/// Defines an egress policy for a CLA, managing how outgoing bundles are prioritized and scheduled.
///
/// An `EgressPolicy` allows for sophisticated traffic management, such as implementing
/// quality of service (QoS) by classifying bundles into different queues.
#[async_trait]
pub trait EgressPolicy: Send + Sync {
    /// Returns the number of egress queues this policy manages.
    /// The default is 0, for simple FIFO behavior.
    /// Any value > 0 indicates multiple priority queues with 0 highest
    fn queue_count(&self) -> u32;

    /// Classifies a bundle based on its flow label into an egress queue index.
    ///
    /// If the returned queue index > `queue_count()` then it is converted to None.
    fn classify(&self, _flow_label: Option<u32>) -> Option<u32>;

    /// Creates a new [`EgressController`] that implements this policy for a given CLA.
    ///
    /// This allows the policy to wrap the CLA's basic `forward` capability with its
    /// own logic, such as token bucket filtering or prioritized dispatching.
    async fn new_controller(
        &self,
        queues: HashMap<Option<u32>, Arc<dyn EgressQueue>>,
    ) -> Arc<dyn EgressController>;
}

/// A single egress queue that a CLA pulls bundles from for transmission.
#[async_trait]
pub trait EgressQueue: Send + Sync {
    /// Enqueues a bundle for transmission on this queue.
    async fn forward(&self, bundle: bundle::Bundle);
}

#[cfg(test)]
mod tests {
    use super::*;

    // NullPolicy.classify() should always return None (single-queue FIFO).
    #[test]
    fn test_flow_classification() {
        let policy = null_policy::EgressPolicy::new();

        // queue_count is 0 for null policy
        assert_eq!(policy.queue_count(), 0);

        // Any flow label maps to None (default queue)
        assert_eq!(policy.classify(None), None);
        assert_eq!(policy.classify(Some(0)), None);
        assert_eq!(policy.classify(Some(42)), None);
        assert_eq!(policy.classify(Some(u32::MAX)), None);
    }

    // Queue indices beyond queue_count should be treated as invalid.
    // For NullPolicy with 0 queues, classify always returns None.
    #[test]
    fn test_queue_bounds() {
        let policy = null_policy::EgressPolicy::new();

        // With queue_count=0, there's only the default (None) queue
        let count = policy.queue_count();
        assert_eq!(count, 0);

        // Verify classify never returns a queue index >= queue_count
        for label in [None, Some(0), Some(1), Some(100), Some(u32::MAX)] {
            let queue = policy.classify(label);
            if let Some(idx) = queue {
                assert!(idx < count, "Queue index {idx} exceeds queue_count {count}");
            }
        }
    }
}

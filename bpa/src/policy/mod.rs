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
pub trait FlowController: Send + Sync {
    /// The policy queue the next bundle is assigned to — the controller
    /// owns the mapping onto its own queues, per peer (this instance's
    /// scope), so per-peer scheduler state lives where it belongs. Every
    /// bundle is assigned somewhere: the returned index is in
    /// `0..queue_count()`, and an out-of-range index is a policy bug the
    /// caller clamps to queue 0 and logs. The traffic-class parameter
    /// arrives with the policy tranche (`policy_subsystem_redesign.md`:
    /// this is the interior mapping of the future `FlowController::push`).
    fn queue_for(&self) -> u32;

    /// Forwards a bundle from the given policy queue (an index in
    /// `0..queue_count()`).
    async fn forward(&self, queue: u32, bundle: bundle::Bundle);
}

/// Defines an egress policy for a CLA, managing how outgoing bundles are prioritized and scheduled.
///
/// An `FlowControllerFactory` allows for sophisticated traffic management, such as implementing
/// quality of service (QoS) by classifying bundles into different queues.
#[async_trait]
pub trait FlowControllerFactory: Send + Sync {
    /// Returns the total number of egress queues this policy manages —
    /// always at least one, since every policy has a queue for every bundle
    /// to classify into (the null policy's single FIFO). Queue indices are
    /// the policy's own naming: relative priority and scheduling between
    /// queues are internal policy decisions — index 0 is only guaranteed to
    /// exist (it is the clamp target for an out-of-range assignment).
    fn queue_count(&self) -> core::num::NonZeroU32;

    /// Creates a new [`FlowController`] that implements this policy for a given CLA.
    ///
    /// This allows the policy to wrap the CLA's basic `forward` capability with its
    /// own logic, such as token bucket filtering or prioritized dispatching.
    ///
    /// `queues` is the per-lane-directive [`EgressQueueSet`] the controller
    /// transmits through.
    async fn new_controller(&self, queues: EgressQueueSet) -> Arc<dyn FlowController>;
}

/// The queue feeding one lane directive, from which a CLA pulls bundles
/// for transmission: pinned to a declared lane, or the next free lane.
#[async_trait]
pub trait EgressQueue: Send + Sync {
    /// Enqueues a bundle for transmission under this queue's lane directive.
    async fn forward(&self, bundle: bundle::Bundle);
}

/// The egress queue set handed to a policy at controller construction, one
/// queue per lane directive: `next_free` transmits on the next free lane, and
/// `pinned` queues transmit pinned to the declared lane keying them. The
/// non-optional `next_free` field is the contract that every policy has
/// somewhere to transmit without lane pinning.
pub struct EgressQueueSet {
    /// The next-free-lane queue.
    pub next_free: Arc<dyn EgressQueue>,
    /// The pinned per-lane queues, keyed by declared lane index.
    pub pinned: HashMap<u32, Arc<dyn EgressQueue>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // The null policy is one total queue: the factory declares it, and the
    // controller's mapping assigns every bundle to it, in range of
    // `queue_count()` as the contract requires.
    #[tokio::test]
    async fn null_policy_is_one_total_queue() {
        let policy = null_policy::FlowControllerFactory::new();
        assert_eq!(policy.queue_count().get(), 1);

        struct NullQueue;
        #[async_trait]
        impl EgressQueue for NullQueue {
            async fn forward(&self, _bundle: bundle::Bundle) {}
        }
        let queues = EgressQueueSet {
            next_free: Arc::new(NullQueue),
            pinned: HashMap::new(),
        };
        let controller = policy.new_controller(queues).await;
        let queue = controller.queue_for();
        assert_eq!(queue, 0);
        assert!(queue < policy.queue_count().get());
    }
}

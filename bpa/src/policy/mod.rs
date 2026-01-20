use super::*;
use std::collections::HashMap;

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

#[async_trait]
pub trait EgressQueue: Send + Sync {
    /// Forwards a bundle.
    async fn forward(&self, bundle: bundle::Bundle);
}

#[cfg(test)]
mod tests {
    use super::*;

    // TODO: Implement test for 'Flow Classification' (Map Flow Label to Queue Index)
    #[test]
    fn test_flow_classification() {
        todo!("Verify Map Flow Label to Queue Index");
    }

    // TODO: Implement test for 'Queue Bounds' (Handle invalid queue indices)
    #[test]
    fn test_queue_bounds() {
        todo!("Verify Handle invalid queue indices");
    }
}

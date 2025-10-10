use super::*;
use std::collections::HashMap;

pub mod null_policy;

// #[cfg(feature = "htb_policy")]
// pub mod htb_policy;

// #[cfg(feature = "tbf_policy")]
// pub mod tbf_policy;

/// The EgressPolicy enum now cleanly separates the type from its configuration.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type", rename_all = "lowercase"))]
pub enum EgressPolicyConfig {
    // #[cfg(feature = "htb_policy")]
    // Htb { config: htb_policy::HtbConfig },

    // #[cfg(feature = "tbf_policy")]
    // Tbf { config: tbf_policy::TbfConfig },
}

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

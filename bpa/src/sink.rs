//! Output sink for bundle data.
//!
//! A `Sink` accepts processed bundles from the egress pipeline.
//! Implementations include CLA transports, local services, and test harnesses.

use bytes::Bytes;
use hardy_async::async_trait;

use crate::bundle::Bundle;

/// A destination that accepts egress-processed bundles.
#[async_trait]
pub trait Sink: Send + Sync {
    /// Write a bundle to this destination.
    ///
    /// The `bundle` provides metadata (source, destination, expiry, block info).
    /// The `data` is the egress-filtered bundle bytes.
    async fn write(&self, bundle: &Bundle, data: Bytes) -> Result<(), crate::Error>;
}

//! Output sink for bundle data.
//!
//! A `Sink` accepts processed bundle bytes from the egress pipeline.
//! Implementations include CLA transports, local services, and test harnesses.

use bytes::Bytes;
use hardy_async::async_trait;

/// A destination that accepts bundle bytes.
#[async_trait]
pub trait Sink: Send + Sync {
    /// Write bundle bytes to this destination.
    async fn write(&self, data: Bytes) -> Result<(), crate::Error>;
}

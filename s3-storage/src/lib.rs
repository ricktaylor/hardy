/*!
S3-compatible bundle storage backend for the Hardy BPA.

Implements the [`BundleStorage`](hardy_bpa::storage::BundleStorage) trait
using any S3-compatible object store (AWS S3, MinIO, LocalStack, etc.).
Bundles are stored as individual objects keyed by UUID, with optional key
prefixing for shared buckets. Large bundles are uploaded via the S3
multipart upload API to bypass the 5 GiB single-object limit.
*/

mod builder;
mod storage;

pub use self::builder::{PartSize, S3StorageBuilder};
pub use self::storage::S3Storage;

/// Errors returned during S3 storage construction.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The bucket name is empty.
    #[error("bucket must not be empty")]
    EmptyBucket,

    /// The multipart threshold is below the part size, so mid-sized
    /// bundles would pay the multipart round trips for a single part.
    #[error("multipart threshold {multipart_threshold} is below the part size {part_size}")]
    ThresholdBelowPartSize {
        multipart_threshold: usize,
        part_size: usize,
    },
}

/// Shorthand for results whose error is [`enum@Error`].
pub type Result<T> = std::result::Result<T, Error>;

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

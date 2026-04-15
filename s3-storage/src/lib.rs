/*!
S3-compatible bundle storage backend for the Hardy BPA.

Implements the [`BundleStorage`](hardy_bpa::storage::BundleStorage) trait
using any S3-compatible object store (AWS S3, MinIO, LocalStack, etc.).
Bundles are stored as individual objects keyed by UUID, with optional key
prefixing for shared buckets. Large bundles are uploaded via the S3
multipart upload API to bypass the 5 GiB single-object limit.
*/

mod config;
mod storage;

pub use config::Config;

use std::sync::Arc;
use tracing::info;

const DEFAULT_MULTIPART_THRESHOLD: usize = 8 * 1024 * 1024;
const DEFAULT_PART_SIZE: usize = 8 * 1024 * 1024;
const MIN_PART_SIZE: usize = 5 * 1024 * 1024;

/// Errors returned during S3 storage construction.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The supplied [`Config`] is invalid (e.g. empty bucket name or part size
    /// below the S3 minimum).
    #[error("invalid configuration: {0}")]
    Config(String),
}

/// Construct an S3 bundle storage backend from `config`.
///
/// AWS credentials are resolved via the standard credential chain
/// (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`, IAM role,
/// `~/.aws/credentials`). Do not store credentials in the config file.
pub async fn new(config: &Config) -> Result<Arc<dyn hardy_bpa::storage::BundleStorage>, Error> {
    if config.bucket.is_empty() {
        return Err(Error::Config("bucket must not be empty".into()));
    }

    let part_size = config.multipart_part_size.unwrap_or(DEFAULT_PART_SIZE);
    if part_size < MIN_PART_SIZE {
        return Err(Error::Config(format!(
            "multipart-part-size must be at least {MIN_PART_SIZE} bytes (5 MiB)"
        )));
    }

    let multipart_threshold = config
        .multipart_threshold
        .unwrap_or(DEFAULT_MULTIPART_THRESHOLD);

    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(region) = &config.region {
        loader = loader.region(aws_sdk_s3::config::Region::new(region.clone()));
    }
    let aws_cfg = loader.load().await;

    let mut s3_builder = aws_sdk_s3::config::Builder::from(&aws_cfg);
    if let Some(endpoint) = &config.endpoint_url {
        s3_builder = s3_builder.endpoint_url(endpoint);
    }
    s3_builder = s3_builder.force_path_style(config.force_path_style);
    let client = aws_sdk_s3::Client::from_conf(s3_builder.build());

    info!(
        bucket = %config.bucket,
        prefix = %config.prefix,
        "Using S3 bundle storage"
    );

    Ok(Arc::new(storage::Storage::new(
        client,
        config.bucket.clone(),
        &config.prefix,
        multipart_threshold,
        part_size,
    )))
}

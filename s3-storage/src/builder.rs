//! Builder for [`S3Storage`].

use tracing::info;

use super::{
    MultipartThreshold, PartSize, S3Storage,
    error::{Error, Result},
};

// Bundle size threshold above which multipart upload is used instead of a
// single `PutObject`: S3 enforces a 5 GiB hard limit on `PutObject`, and
// larger bundles benefit from parallel part uploads.
const DEFAULT_MULTIPART_THRESHOLD: MultipartThreshold =
    MultipartThreshold::new(8 * 1024 * 1024).unwrap();

// Size of each part in a multipart upload (all parts except the last).
const DEFAULT_PART_SIZE: PartSize = PartSize::new(8 * 1024 * 1024).unwrap();

/// Builder for [`S3Storage`], obtained from [`S3Storage::builder()`].
/// Unset knobs apply the backend's own defaults.
///
/// AWS credentials are **not** part of the builder. They are resolved via
/// the standard credential chain: `AWS_ACCESS_KEY_ID` /
/// `AWS_SECRET_ACCESS_KEY` env vars, an IAM role, or `~/.aws/credentials`.
#[must_use = "an S3StorageBuilder does nothing unless `build()` is called"]
pub struct S3StorageBuilder {
    bucket: String,
    prefix: Option<String>,
    region: Option<String>,
    endpoint_url: Option<String>,
    force_path_style: bool,
    multipart_threshold: Option<MultipartThreshold>,
    multipart_part_size: Option<PartSize>,
}

impl S3StorageBuilder {
    pub(crate) fn new(bucket: String) -> Self {
        Self {
            bucket,
            prefix: None,
            region: None,
            endpoint_url: None,
            force_path_style: false,
            multipart_threshold: None,
            multipart_part_size: None,
        }
    }

    /// Sets a key prefix for all objects stored by hardy (no leading or
    /// trailing slash), for when the bucket is shared with other
    /// applications.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Sets the AWS region (e.g. `"us-east-1"`); unset falls back to the
    /// `AWS_DEFAULT_REGION` / `AWS_REGION` env vars.
    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Sets a custom endpoint URL for S3-compatible stores (MinIO,
    /// LocalStack, etc.).
    pub fn endpoint_url(mut self, url: impl Into<String>) -> Self {
        self.endpoint_url = Some(url.into());
        self
    }

    /// Forces path-style addressing (`http://host/bucket/key` instead of
    /// `http://bucket.host/key`), required for MinIO and some
    /// S3-compatible stores.
    pub fn force_path_style(mut self) -> Self {
        self.force_path_style = true;
        self
    }

    /// Sets the bundle size threshold above which multipart upload is
    /// used instead of a single `PutObject`. Must be at least the part
    /// size.
    pub fn multipart_threshold(mut self, threshold: MultipartThreshold) -> Self {
        self.multipart_threshold = Some(threshold);
        self
    }

    /// Sets the size of each part in a multipart upload (all parts except
    /// the last).
    pub fn multipart_part_size(mut self, size: PartSize) -> Self {
        self.multipart_part_size = Some(size);
        self
    }

    /// Resolves the AWS configuration and constructs the storage.
    ///
    /// # Errors
    ///
    /// Fails if the bucket name is empty, or if the multipart threshold is
    /// below the part size (bundles in between would pay the multipart
    /// round trips for a single part).
    pub async fn build(self) -> Result<S3Storage> {
        if self.bucket.is_empty() {
            return Err(Error::EmptyBucket);
        }

        let part_size = self.multipart_part_size.unwrap_or(DEFAULT_PART_SIZE);

        let multipart_threshold = self
            .multipart_threshold
            .unwrap_or(DEFAULT_MULTIPART_THRESHOLD)
            .get();
        if multipart_threshold < part_size.get() {
            return Err(Error::ThresholdBelowPartSize {
                multipart_threshold,
                part_size: part_size.get(),
            });
        }

        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(region) = self.region {
            loader = loader.region(aws_sdk_s3::config::Region::new(region));
        }
        let aws_cfg = loader.load().await;

        let mut s3_builder = aws_sdk_s3::config::Builder::from(&aws_cfg);
        if let Some(endpoint) = &self.endpoint_url {
            s3_builder = s3_builder.endpoint_url(endpoint);
        }
        s3_builder = s3_builder.force_path_style(self.force_path_style);
        let client = aws_sdk_s3::Client::from_conf(s3_builder.build());

        let prefix = self.prefix.unwrap_or_default();
        info!(bucket = %self.bucket, %prefix, "Using S3 bundle storage");

        Ok(S3Storage::new(
            client,
            self.bucket,
            &prefix,
            multipart_threshold,
            part_size,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_bucket_is_refused() {
        assert!(matches!(
            S3Storage::builder(String::new()).build().await,
            Err(Error::EmptyBucket)
        ));
    }

    #[tokio::test]
    async fn threshold_below_part_size_is_refused() {
        assert!(matches!(
            S3Storage::builder("bucket")
                .multipart_threshold(MultipartThreshold::new(1024).unwrap())
                .build()
                .await,
            Err(Error::ThresholdBelowPartSize { .. })
        ));
    }
}

/// Configuration for the S3 bundle storage backend.
///
/// AWS credentials are **not** stored here. Use the standard credential chain:
/// `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` env vars, an IAM role,
/// or `~/.aws/credentials`.
#[derive(Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct Config {
    /// S3 bucket name.
    pub bucket: String,

    /// Key prefix for all objects stored by hardy (no leading or trailing slash).
    /// Use this when the bucket is shared with other applications.
    pub prefix: String,

    /// AWS region (e.g. `"us-east-1"`).
    /// Falls back to `AWS_DEFAULT_REGION` / `AWS_REGION` env vars if absent.
    pub region: Option<String>,

    /// Custom endpoint URL for S3-compatible stores (MinIO, LocalStack, etc.).
    pub endpoint_url: Option<String>,

    /// Force path-style addressing (`http://host/bucket/key` instead of
    /// `http://bucket.host/key`). Required for MinIO and some S3-compatible stores.
    pub force_path_style: bool,

    /// Bundle size threshold above which multipart upload is used instead of a single
    /// `PutObject`. S3 enforces a 5 GiB hard limit on `PutObject`; bundles larger than
    /// this threshold bypass that limit and benefit from parallel part uploads.
    ///
    /// Default: 8 MiB. Must be >= `multipart-part-size`.
    pub multipart_threshold: Option<usize>,

    /// Size of each part in a multipart upload (all parts except the last).
    /// S3 requires a minimum of 5 MiB per part.
    ///
    /// Default: 8 MiB.
    pub multipart_part_size: Option<usize>,
}

/*!
S3-compatible bundle storage backend for the Hardy BPA.

Implements the [`BundleStorage`](hardy_bpa::storage::BundleStorage) trait
using any S3-compatible object store (AWS S3, MinIO, LocalStack, etc.).
Bundles are stored as individual objects keyed by UUID, with optional key
prefixing for shared buckets. Large bundles are uploaded via the S3
multipart upload API to bypass the 5 GiB single-object limit.

# Feature flags

- `instrument` — adds `tracing` spans to the async internals.
- `serde` — adds `Serialize`/`Deserialize` impls to [`PartSize`], so
  consumer config schemas reject sub-minimum part sizes at
  deserialization.
*/

mod builder;
mod error;
mod storage;

pub use self::builder::S3StorageBuilder;
pub use self::error::Error;
pub use self::storage::S3Storage;

/// The size, in bytes, of each part in a multipart upload (all parts
/// except the last): within the S3 protocol bounds of [`PartSize::MIN`]
/// and [`PartSize::MAX_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartSize(usize);

impl PartSize {
    /// The S3 protocol minimum part size: 5 MiB.
    pub const MIN: PartSize = PartSize(5 * 1024 * 1024);

    /// The S3 protocol maximum part size (and single `PutObject` size):
    /// 5 GiB. In bytes rather than a `PartSize`, as the value exceeds
    /// `usize` on 32-bit targets.
    pub const MAX_BYTES: u64 = 5 * 1024 * 1024 * 1024;

    /// A part size of `bytes`, or `None` outside the S3 bounds.
    pub const fn new(bytes: usize) -> Option<Self> {
        if bytes >= Self::MIN.0 && bytes as u64 <= Self::MAX_BYTES {
            Some(Self(bytes))
        } else {
            None
        }
    }

    /// The size in bytes.
    pub const fn get(self) -> usize {
        self.0
    }
}

/// The bundle size threshold, in bytes, above which multipart upload is
/// used instead of a single `PutObject`: at most
/// [`MultipartThreshold::MAX_BYTES`], the S3 `PutObject` limit. It must
/// also be at least the part size, which is judged at build, where both
/// values are known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultipartThreshold(usize);

impl MultipartThreshold {
    /// The S3 single `PutObject` limit: 5 GiB. In bytes rather than a
    /// `MultipartThreshold`, as the value exceeds `usize` on 32-bit
    /// targets.
    pub const MAX_BYTES: u64 = PartSize::MAX_BYTES;

    /// A threshold of `bytes`, or `None` above the S3 `PutObject` limit.
    pub const fn new(bytes: usize) -> Option<Self> {
        if bytes as u64 <= Self::MAX_BYTES {
            Some(Self(bytes))
        } else {
            None
        }
    }

    /// The size in bytes.
    pub const fn get(self) -> usize {
        self.0
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for PartSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.get().serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for MultipartThreshold {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bytes = usize::deserialize(deserializer)?;
        MultipartThreshold::new(bytes).ok_or_else(|| {
            serde::de::Error::custom(
                "a multipart threshold must be at most 5 GiB (the S3 PutObject limit)",
            )
        })
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for MultipartThreshold {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.get().serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for PartSize {
    /// Deserializes from bytes, rejecting values below the S3 protocol
    /// minimum, so an undersized part size fails at parse.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bytes = usize::deserialize(deserializer)?;
        PartSize::new(bytes).ok_or_else(|| {
            serde::de::Error::custom(
                "a multipart part size must be between 5 MiB and 5 GiB (the S3 protocol bounds)",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::PartSize;

    #[test]
    fn multipart_threshold_holds_the_s3_bound() {
        use super::MultipartThreshold;

        assert_eq!(
            MultipartThreshold::new(0).map(MultipartThreshold::get),
            Some(0)
        );

        #[cfg(target_pointer_width = "64")]
        {
            assert!(
                MultipartThreshold::new((MultipartThreshold::MAX_BYTES as usize) + 1).is_none()
            );
            assert!(MultipartThreshold::new(MultipartThreshold::MAX_BYTES as usize).is_some());
        }
    }

    #[test]
    fn part_size_holds_the_s3_bounds() {
        assert!(PartSize::new(5 * 1024 * 1024 - 1).is_none());
        assert_eq!(
            PartSize::new(5 * 1024 * 1024).map(PartSize::get),
            Some(5 * 1024 * 1024)
        );

        #[cfg(target_pointer_width = "64")]
        {
            assert!(PartSize::new((PartSize::MAX_BYTES as usize) + 1).is_none());
            assert!(PartSize::new(PartSize::MAX_BYTES as usize).is_some());
        }
    }
}

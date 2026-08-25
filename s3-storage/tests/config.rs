//! Bucket and multipart configuration bounds, through the public builder API.

use hardy_s3_storage::{Error, MultipartThreshold, PartSize, S3Storage};

#[test]
fn multipart_threshold_holds_the_s3_bound() {
    assert_eq!(
        MultipartThreshold::new(0).map(MultipartThreshold::get),
        Some(0)
    );

    #[cfg(target_pointer_width = "64")]
    {
        assert!(MultipartThreshold::new((MultipartThreshold::MAX_BYTES as usize) + 1).is_none());
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

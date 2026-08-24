use super::*;

use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(1);

fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Metadata with the given status; every other field keeps its default.
///
/// `BundleMetadata` has a private field, so functional-update syntax is not
/// available outside `hardy_bpa`; the default-then-assign shape is confined
/// to this one helper.
fn metadata(status: BundleStatus) -> BundleMetadata {
    let mut metadata = BundleMetadata::default();
    metadata.status = status;
    metadata
}

/// Shared base constructor: a bundle with the unique source `ipn:{seq}.0`.
fn make(
    seq: u64,
    timestamp: CreationTimestamp,
    fragment_info: Option<hardy_bpv7::bundle::FragmentInfo>,
    lifetime: core::time::Duration,
    metadata: BundleMetadata,
) -> bundle::Bundle {
    bundle::Bundle {
        bundle: hardy_bpv7::bundle::Bundle {
            id: hardy_bpv7::bundle::Id {
                source: format!("ipn:{seq}.0").parse().unwrap(),
                timestamp,
                fragment_info,
            },
            destination: "ipn:99.0".parse().unwrap(),
            lifetime,
            ..Default::default()
        },
        metadata,
    }
}

/// Create a bundle with a unique ID, status `Waiting`, and a 1-hour lifetime.
pub fn random_bundle() -> bundle::Bundle {
    make(
        next_seq(),
        CreationTimestamp::now(),
        None,
        core::time::Duration::from_secs(3600),
        metadata(BundleStatus::Waiting),
    )
}

/// Create a bundle with a specific status and received_at timestamp.
pub fn bundle_with_status(
    status: BundleStatus,
    received_at: time::OffsetDateTime,
) -> bundle::Bundle {
    let mut metadata = metadata(status);
    metadata.read_only.received_at = received_at;

    make(
        next_seq(),
        CreationTimestamp::now(),
        None,
        core::time::Duration::from_secs(3600),
        metadata,
    )
}

/// Create a bundle with a controlled expiry.
///
/// Expiry = creation_time + lifetime.  We set the BPv7 creation timestamp
/// to `creation_time` and use the given `lifetime`.
pub fn bundle_with_expiry(
    status: BundleStatus,
    creation_time: time::OffsetDateTime,
    lifetime: core::time::Duration,
) -> bundle::Bundle {
    let seq = next_seq();

    let timestamp = CreationTimestamp::try_from(creation_time)
        .unwrap_or_else(|_| CreationTimestamp::from_parts(None, seq));

    make(seq, timestamp, None, lifetime, metadata(status))
}

/// Create a bundle with fragment info and the given AduFragment status.
pub fn bundle_with_fragment(
    status: BundleStatus,
    offset: u64,
    total_adu_length: u64,
) -> bundle::Bundle {
    make(
        next_seq(),
        CreationTimestamp::now(),
        Some(hardy_bpv7::bundle::FragmentInfo {
            offset,
            total_adu_length,
        }),
        core::time::Duration::from_secs(3600),
        metadata(status),
    )
}

/// Generate deterministic, patterned payload data of a given size.
pub fn patterned_payload(size: usize) -> Bytes {
    Bytes::from_iter((0..size).map(|i| (i % 256) as u8))
}

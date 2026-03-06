use super::*;

use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(1);

fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Create a bundle with a unique ID, status `Waiting`, and a 1-hour lifetime.
pub fn random_bundle() -> bundle::Bundle {
    let seq = next_seq();

    let bpv7 = hardy_bpv7::bundle::Bundle {
        id: hardy_bpv7::bundle::Id {
            source: format!("ipn:{seq}.0").parse().unwrap(),
            timestamp: CreationTimestamp::now(),
            fragment_info: None,
        },
        destination: "ipn:99.0".parse().unwrap(),
        lifetime: core::time::Duration::from_secs(3600),
        ..Default::default()
    };

    let mut meta = BundleMetadata::default();
    meta.status = BundleStatus::Waiting;

    bundle::Bundle {
        bundle: bpv7,
        metadata: meta,
    }
}

/// Create a bundle with a specific status and received_at timestamp.
pub fn bundle_with_status(
    status: BundleStatus,
    received_at: time::OffsetDateTime,
) -> bundle::Bundle {
    let seq = next_seq();

    let bpv7 = hardy_bpv7::bundle::Bundle {
        id: hardy_bpv7::bundle::Id {
            source: format!("ipn:{seq}.0").parse().unwrap(),
            timestamp: CreationTimestamp::now(),
            fragment_info: None,
        },
        destination: "ipn:99.0".parse().unwrap(),
        lifetime: core::time::Duration::from_secs(3600),
        ..Default::default()
    };

    let mut meta = BundleMetadata::default();
    meta.status = status;
    meta.read_only.received_at = received_at;

    bundle::Bundle {
        bundle: bpv7,
        metadata: meta,
    }
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

    let ts = CreationTimestamp::try_from(creation_time)
        .unwrap_or_else(|_| CreationTimestamp::from_parts(None, seq));

    let bpv7 = hardy_bpv7::bundle::Bundle {
        id: hardy_bpv7::bundle::Id {
            source: format!("ipn:{seq}.0").parse().unwrap(),
            timestamp: ts,
            fragment_info: None,
        },
        destination: "ipn:99.0".parse().unwrap(),
        lifetime,
        ..Default::default()
    };

    let mut meta = BundleMetadata::default();
    meta.status = status;

    bundle::Bundle {
        bundle: bpv7,
        metadata: meta,
    }
}

/// Create a bundle with fragment info and the given AduFragment status.
pub fn bundle_with_fragment(
    status: BundleStatus,
    offset: u64,
    total_adu_length: u64,
) -> bundle::Bundle {
    let seq = next_seq();

    let bpv7 = hardy_bpv7::bundle::Bundle {
        id: hardy_bpv7::bundle::Id {
            source: format!("ipn:{seq}.0").parse().unwrap(),
            timestamp: CreationTimestamp::now(),
            fragment_info: Some(hardy_bpv7::bundle::FragmentInfo {
                offset,
                total_adu_length,
            }),
        },
        destination: "ipn:99.0".parse().unwrap(),
        lifetime: core::time::Duration::from_secs(3600),
        ..Default::default()
    };

    let mut meta = BundleMetadata::default();
    meta.status = status;

    bundle::Bundle {
        bundle: bpv7,
        metadata: meta,
    }
}

/// Generate deterministic payload data of a given size.
pub fn random_payload(size: usize) -> Bytes {
    let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
    Bytes::from(data)
}

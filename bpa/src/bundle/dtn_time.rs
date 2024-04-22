use hardy_bpa_core::bundle::{Bundle, Metadata};

pub fn as_dtn_time(instant: &time::OffsetDateTime) -> u64 {
    (*instant - time::macros::datetime!(2000-01-01 00:00:00 UTC)).whole_milliseconds() as u64
}

pub fn has_bundle_expired(metadata: &Metadata, bundle: &Bundle) -> bool {
    let now = as_dtn_time(&time::OffsetDateTime::now_utc());
    let age = if bundle.id.timestamp.creation_time != 0 {
        now.checked_sub(bundle.id.timestamp.creation_time)
    } else if let Some(received_at) = &metadata.received_at {
        match (bundle.age, now.checked_sub(as_dtn_time(received_at))) {
            (None, None) => None,
            (None, Some(age)) => Some(age),
            (Some(age), None) => Some(age),
            (Some(a1), Some(a2)) => a1.checked_add(a2),
        }
    } else {
        bundle.age
    };

    if let Some(age) = age {
        age > bundle.lifetime
    } else {
        // If something is missing, or we have overflowed, then expire the bundle
        true
    }
}

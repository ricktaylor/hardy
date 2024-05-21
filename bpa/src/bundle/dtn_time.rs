use super::*;

const DTN_EPOCH: time::OffsetDateTime = time::macros::datetime!(2000-01-01 00:00:00 UTC);

pub fn to_dtn_time(instant: &time::OffsetDateTime) -> u64 {
    (*instant - DTN_EPOCH)
        .whole_milliseconds()
        .clamp(0, u64::MAX as i128) as u64
}

pub fn from_dtn_time(milliseconds: u64) -> time::OffsetDateTime {
    DTN_EPOCH.saturating_add(millis_to_duration(milliseconds))
}

fn millis_to_duration(ms: u64) -> time::Duration {
    time::Duration::saturating_seconds_f64((ms / 1_000) as f64 + ((ms % 1_0000) as f64 / 1_000f64))
}

pub fn get_bundle_creation(metadata: &Metadata, bundle: &Bundle) -> time::OffsetDateTime {
    if bundle.id.timestamp.creation_time != 0 {
        DTN_EPOCH.saturating_add(millis_to_duration(bundle.id.timestamp.creation_time))
    } else {
        metadata
            .received_at
            .unwrap_or_else(time::OffsetDateTime::now_utc)
            .saturating_sub(millis_to_duration(bundle.age.unwrap_or(0)))
    }
}

pub fn get_bundle_expiry(metadata: &Metadata, bundle: &Bundle) -> time::OffsetDateTime {
    get_bundle_creation(metadata, bundle).saturating_add(millis_to_duration(bundle.lifetime))
}

pub fn has_bundle_expired(metadata: &Metadata, bundle: &Bundle) -> bool {
    get_bundle_expiry(metadata, bundle) <= time::OffsetDateTime::now_utc()
}

use super::*;

pub fn as_dtn_time(instant: &time::OffsetDateTime) -> u64 {
    let t = (*instant - time::macros::datetime!(2000-01-01 00:00:00 UTC)).whole_milliseconds();
    if t > u64::MAX as i128 {
        u64::MAX
    } else if t < 0 {
        0
    } else {
        t as u64
    }
}

fn get_bundle_creation(metadata: &Metadata, bundle: &Bundle) -> time::OffsetDateTime {
    if bundle.id.timestamp.creation_time != 0 {
        time::OffsetDateTime::from_unix_timestamp_nanos(
            time::macros::datetime!(2000-01-01 00:00:00 UTC)
                .unix_timestamp_nanos()
                .saturating_add_unsigned(bundle.id.timestamp.creation_time as u128 * 1_000_000u128),
        )
        .unwrap_or(time::OffsetDateTime::new_utc(
            time::Date::MAX,
            time::Time::MIDNIGHT,
        ))
    } else {
        time::OffsetDateTime::from_unix_timestamp_nanos(
            metadata
                .received_at
                .unwrap_or_else(time::OffsetDateTime::now_utc)
                .unix_timestamp_nanos()
                .saturating_sub_unsigned(bundle.age.unwrap_or(0) as u128 * 1_000_000u128),
        )
        .unwrap_or(time::OffsetDateTime::new_utc(
            time::Date::MIN,
            time::Time::MIDNIGHT,
        ))
    }
}

pub fn get_bundle_expiry(metadata: &Metadata, bundle: &Bundle) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(
        get_bundle_creation(metadata, bundle)
            .unix_timestamp_nanos()
            .saturating_add_unsigned(bundle.lifetime as u128 * 1_000_000u128),
    )
    .unwrap_or(time::OffsetDateTime::new_utc(
        time::Date::MAX,
        time::Time::MIDNIGHT,
    ))
}

pub fn has_bundle_expired(metadata: &Metadata, bundle: &Bundle) -> bool {
    get_bundle_expiry(metadata, bundle) <= time::OffsetDateTime::now_utc()
}

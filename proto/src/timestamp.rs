//! Conversion between the protobuf `Timestamp` and
//! [`OffsetDateTime`](time::OffsetDateTime).

#[cfg(any(feature = "client", feature = "server"))]
use prost_types::Timestamp;
#[cfg(feature = "client")]
use time::Duration;
#[cfg(any(feature = "client", feature = "server"))]
use time::OffsetDateTime;

/// Converts a point in time to its wire form.
#[cfg(feature = "server")]
pub(crate) fn to_timestamp(t: OffsetDateTime) -> Timestamp {
    Timestamp {
        seconds: t.unix_timestamp(),
        nanos: t.nanosecond() as i32,
    }
}

/// Converts a wire timestamp to a point in time.
///
/// Returns `None` if the timestamp lies outside the range `OffsetDateTime`
/// represents, so that a peer's value can never panic this side.
#[cfg(feature = "client")]
pub(crate) fn from_timestamp(t: Timestamp) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(t.seconds)
        .ok()?
        .checked_add(Duration::nanoseconds(t.nanos.into()))
}

#[cfg(all(test, feature = "client"))]
mod tests {
    use time::{Date, Time};

    use super::*;

    #[test]
    fn an_unrepresentable_timestamp_is_skipped_not_panicked_on() {
        let max = OffsetDateTime::new_utc(Date::MAX, Time::MAX).unix_timestamp();
        assert_eq!(
            from_timestamp(Timestamp {
                seconds: max,
                nanos: i32::MAX,
            }),
            None
        );

        let t = from_timestamp(Timestamp {
            seconds: 1,
            nanos: 500,
        })
        .expect("a representable timestamp converts");
        assert_eq!(
            t,
            OffsetDateTime::from_unix_timestamp(1).unwrap() + Duration::nanoseconds(500)
        );
    }
}

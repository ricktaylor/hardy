// Both halves of the wire's timestamp conversion. `Timestamp` and
// `OffsetDateTime` are both foreign, so these are free functions rather
// than `From` impls; the two directions live together so neither end of
// the wire re-derives the other's convention.

#[cfg(any(feature = "client", feature = "server"))]
use prost_types::Timestamp;
#[cfg(feature = "client")]
use time::Duration;
#[cfg(any(feature = "client", feature = "server"))]
use time::OffsetDateTime;

// A local timestamp as the wire carries it; shared by the surfaces that
// emit status-report timestamps.
#[cfg(feature = "server")]
pub(crate) fn to_timestamp(t: OffsetDateTime) -> Timestamp {
    Timestamp {
        seconds: t.unix_timestamp(),
        nanos: t.nanosecond() as i32,
    }
}

// A wire timestamp as a local one, or `None` for a value no
// `OffsetDateTime` can hold. Both fields are peer-supplied and
// `OffsetDateTime + Duration` panics on overflow, so the sum is
// checked: a malformed timestamp skips its event, not the process.
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
        // The last instant `OffsetDateTime` can hold, plus nanoseconds:
        // the addition overflows, and the whole point of the `Option` is
        // that the event is skipped instead.
        let max = OffsetDateTime::new_utc(Date::MAX, Time::MAX).unix_timestamp();
        assert_eq!(
            from_timestamp(Timestamp {
                seconds: max,
                nanos: i32::MAX,
            }),
            None
        );

        // A representable one still converts, nanoseconds included.
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

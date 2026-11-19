use core::time::Duration;

use jiff::{
    SpanRelativeTo,
    fmt::friendly::{Designator, SpanParser, SpanPrinter},
};
use thiserror::Error;

const SECS_PER_DAY: u64 = 86_400;

#[derive(Debug, Error)]
pub enum DurationError {
    #[error(
        "months and years have no fixed length; use a unit from nanoseconds to weeks (e.g. `500ms`, `2s`, `1h 30m`, `1d`)"
    )]
    CalendarUnit,

    #[error("expected a number and a unit (e.g. `500ms`, `2s`, `1h 30m`, `1d`)")]
    Malformed,

    #[error("a duration cannot be negative")]
    Negative,

    #[error("the duration is too large")]
    TooLarge,
}

// Parses a duration argument in Jiff's friendly format (`500ms`, `1h 30m`,
// `2 days`). Days and weeks count as 24-hour multiples.
pub fn parse_duration(text: &str) -> Result<Duration, DurationError> {
    let span = SpanParser::new().parse_span(text).map_err(|e| {
        if e.is_range() {
            DurationError::TooLarge
        } else {
            DurationError::Malformed
        }
    })?;
    if span.get_years() != 0 || span.get_months() != 0 {
        return Err(DurationError::CalendarUnit);
    }
    if span.is_negative() {
        return Err(DurationError::Negative);
    }
    span.to_duration(SpanRelativeTo::days_are_24_hours())
        .ok()
        .and_then(|signed| signed.try_into().ok())
        .ok_or(DurationError::TooLarge)
}

// Formats a duration with whole days split out and the rest balanced up to
// hours (`1m 30s`, `2ms 345us`, `1d 2h`). The `HumanTime` designators spell
// microseconds as ASCII `us`, which the interop benchmark's RTT summary
// parser reads.
pub fn format_duration(duration: Duration) -> String {
    let days = duration.as_secs() / SECS_PER_DAY;
    let rest = Duration::new(duration.as_secs() % SECS_PER_DAY, duration.subsec_nanos());
    let printer = SpanPrinter::new().designator(Designator::HumanTime);
    match (days, rest.is_zero()) {
        (0, _) => printer.unsigned_duration_to_string(&rest),
        (_, true) => format!("{days}d"),
        (_, false) => format!("{days}d {}", printer.unsigned_duration_to_string(&rest)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Days and weeks are accepted as fixed 24-hour multiples.
    #[test]
    fn days_and_weeks_are_24_hour_multiples() {
        assert_eq!(
            parse_duration("2d").unwrap(),
            Duration::from_secs(2 * SECS_PER_DAY)
        );
        assert_eq!(
            parse_duration("1w").unwrap(),
            Duration::from_secs(7 * SECS_PER_DAY)
        );
    }

    // Months and years have no fixed length, so they are refused by name.
    #[test]
    fn calendar_units_are_refused() {
        for text in ["1year", "1 month", "1y 2d"] {
            assert!(
                matches!(parse_duration(text), Err(DurationError::CalendarUnit)),
                "{text:?}"
            );
        }
    }

    // A number with no unit, or text with no number, is malformed.
    #[test]
    fn missing_units_are_malformed() {
        for text in ["30", "ten minutes", ""] {
            assert!(
                matches!(parse_duration(text), Err(DurationError::Malformed)),
                "{text:?}"
            );
        }
    }

    // A duration below zero, or beyond what a span can hold, is refused.
    #[test]
    fn negative_and_oversized_durations_are_refused() {
        assert!(matches!(
            parse_duration("-5s"),
            Err(DurationError::Negative)
        ));
        assert!(matches!(
            parse_duration("20000000w"),
            Err(DurationError::TooLarge)
        ));
    }

    // Sub-millisecond parts print with ASCII `us`, whole days as a day count,
    // and the text parses back.
    #[test]
    fn formatted_durations_use_ascii_units_and_round_trip() {
        for (duration, text) in [
            (Duration::new(90, 2_345_678), "1m 30s 2ms 345us 678ns"),
            (Duration::from_secs(SECS_PER_DAY + 7_200), "1d 2h"),
            (Duration::from_secs(5 * SECS_PER_DAY), "5d"),
        ] {
            assert_eq!(format_duration(duration), text);
            assert_eq!(parse_duration(text).unwrap(), duration);
        }
    }
}

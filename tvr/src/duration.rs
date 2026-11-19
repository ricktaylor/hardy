use core::time::Duration;

use jiff::{SpanRelativeTo, fmt::friendly::SpanParser};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DurationError {
    #[error(
        "months and years have no fixed length; use a unit from nanoseconds to weeks (e.g. `500ms`, `90m`, `4h30m`, `2d`)"
    )]
    CalendarUnit,

    #[error("expected a number and a unit (e.g. `500ms`, `90m`, `4h30m`, `2d`)")]
    Malformed,

    #[error("a duration cannot be negative")]
    Negative,

    #[error("the duration is too large")]
    TooLarge,
}

// Parses a contact-plan duration in Jiff's friendly format (`90m`, `4h30m`,
// `500ms`). Days and weeks count as 24-hour multiples.
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

#[cfg(test)]
mod tests {
    use super::*;

    // Days and weeks are accepted as fixed 24-hour multiples.
    #[test]
    fn days_and_weeks_are_24_hour_multiples() {
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86_400));
        assert_eq!(
            parse_duration("1w 12h").unwrap(),
            Duration::from_secs(7 * 86_400 + 12 * 3_600)
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
        for text in ["90", "ten minutes", ""] {
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
}

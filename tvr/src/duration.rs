use core::time::Duration;

use jiff::{SpanRelativeTo, fmt::friendly::SpanParser};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DurationError {
    #[error(
        "months and years have no fixed length; use a unit from nanoseconds to weeks (e.g. `500ms`, `90m`, `4h30m`, `2d`)"
    )]
    CalendarUnit,

    #[error(transparent)]
    Jiff(#[from] jiff::Error),
}

static PARSER: SpanParser = SpanParser::new();

// Parses a contact-plan duration in Jiff's friendly format (`90m`, `4h30m`,
// `500ms`). Days and weeks count as 24-hour multiples.
pub fn parse_duration(text: &str) -> Result<Duration, DurationError> {
    let span = PARSER.parse_span(text)?;
    if span.get_years() != 0 || span.get_months() != 0 {
        return Err(DurationError::CalendarUnit);
    }
    let signed = span.to_duration(SpanRelativeTo::days_are_24_hours())?;
    Ok(Duration::try_from(signed)?)
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

    // A number with no unit, or text with no number, is refused by jiff's
    // parser and never reported as a range error.
    #[test]
    fn missing_units_are_refused() {
        for text in ["90", "ten minutes", ""] {
            assert!(
                matches!(parse_duration(text), Err(DurationError::Jiff(e)) if !e.is_range()),
                "{text:?}"
            );
        }
    }

    // A duration below zero, or beyond what a span can hold, is a range
    // error.
    #[test]
    fn negative_and_oversized_durations_are_refused() {
        for text in ["-5s", "20000000w"] {
            assert!(
                matches!(parse_duration(text), Err(DurationError::Jiff(e)) if e.is_range()),
                "{text:?}"
            );
        }
    }
}

use core::{
    fmt::{self, Display, Formatter},
    time::Duration,
};

use jiff::{
    SpanRelativeTo,
    fmt::{
        StdFmtWrite,
        friendly::{Designator, SpanParser, SpanPrinter},
    },
};
use thiserror::Error;

const SECS_PER_DAY: u64 = 86_400;

#[derive(Debug, Error)]
pub enum DurationError {
    #[error(
        "months and years have no fixed length; use a unit from nanoseconds to weeks (e.g. `24h`, `30d`, `365d`)"
    )]
    CalendarUnit,

    #[error(transparent)]
    Jiff(#[from] jiff::Error),
}

static PARSER: SpanParser = SpanParser::new();
static PRINTER: SpanPrinter = SpanPrinter::new().designator(Designator::HumanTime);

// Parses a duration argument in Jiff's friendly format (`24h`, `30days`,
// `1h 30m`). Days and weeks count as 24-hour multiples.
pub fn parse_duration(text: &str) -> Result<Duration, DurationError> {
    let span = PARSER.parse_span(text)?;
    if span.get_years() != 0 || span.get_months() != 0 {
        return Err(DurationError::CalendarUnit);
    }
    Ok(Duration::try_from(
        span.to_duration(SpanRelativeTo::days_are_24_hours())?,
    )?)
}

// Formats a duration with whole days split out and the rest balanced up to
// hours (`30d`, `2d 16m 40s`), with ASCII unit designators (`us`, not `µs`).
pub fn format_duration(duration: Duration) -> impl Display {
    Formatted(duration)
}

struct Formatted(Duration);

impl Display for Formatted {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let days = self.0.as_secs() / SECS_PER_DAY;
        let rest = Duration::new(self.0.as_secs() % SECS_PER_DAY, self.0.subsec_nanos());
        if days != 0 {
            write!(f, "{days}d")?;
            if rest.is_zero() {
                return Ok(());
            }
            f.write_str(" ")?;
        }
        PRINTER
            .print_unsigned_duration(&rest, StdFmtWrite(f))
            .map_err(|_| fmt::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Days and weeks are accepted as fixed 24-hour multiples.
    #[test]
    fn days_and_weeks_are_24_hour_multiples() {
        assert_eq!(
            parse_duration("30days").unwrap(),
            Duration::from_secs(30 * SECS_PER_DAY)
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

    // A number with no unit, or text with no number, is refused by jiff's
    // parser and never reported as a range error.
    #[test]
    fn missing_units_are_refused() {
        for text in ["30", "ten minutes", ""] {
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

    // Whole days print as a day count, and the text parses back.
    #[test]
    fn long_durations_print_days_and_round_trip() {
        for (duration, text) in [
            (Duration::from_secs(30 * SECS_PER_DAY), "30d"),
            (
                Duration::new(2 * SECS_PER_DAY + 1_000, 250_000),
                "2d 16m 40s 250us",
            ),
            (Duration::from_secs(SECS_PER_DAY - 1), "23h 59m 59s"),
        ] {
            assert_eq!(format_duration(duration).to_string(), text);
            assert_eq!(parse_duration(text).unwrap(), duration);
        }
    }
}

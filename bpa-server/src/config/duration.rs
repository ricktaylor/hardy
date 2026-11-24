use core::{
    fmt::{self, Display, Formatter},
    str::FromStr,
    time::Duration,
};

use jiff::{
    SpanRelativeTo,
    fmt::{
        StdFmtWrite,
        friendly::{Designator, SpanParser, SpanPrinter},
    },
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use thiserror::Error;

const SECS_PER_DAY: u64 = 86_400;

#[derive(Debug, Error)]
pub enum DurationError {
    #[error(
        "months and years have no fixed length; use a unit from nanoseconds to weeks (e.g. `30s`, `10m`, `1h 30m`, `1d`)"
    )]
    CalendarUnit,

    #[error("a duration must be greater than zero")]
    Zero,

    #[error(transparent)]
    Jiff(#[from] jiff::Error),
}

static PARSER: SpanParser = SpanParser::new();
static PRINTER: SpanPrinter = SpanPrinter::new().designator(Designator::HumanTime);

// A positive duration, written in Jiff's friendly format (e.g. `30s`, `10m`,
// `1h 30m`). Days and weeks count as 24-hour multiples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NonZeroDuration(Duration);

impl NonZeroDuration {
    pub fn new(duration: Duration) -> Option<Self> {
        (!duration.is_zero()).then_some(Self(duration))
    }

    pub fn get(&self) -> Duration {
        self.0
    }
}

impl FromStr for NonZeroDuration {
    type Err = DurationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let span = PARSER.parse_span(text)?;
        if span.get_years() != 0 || span.get_months() != 0 {
            return Err(DurationError::CalendarUnit);
        }
        let duration = Duration::try_from(span.to_duration(SpanRelativeTo::days_are_24_hours())?)?;
        Self::new(duration).ok_or(DurationError::Zero)
    }
}

// Whole days are split out and the rest is balanced up to hours (`10m`,
// `1d 12h`), so the text parses back to the same duration.
impl Display for NonZeroDuration {
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

impl<'de> Deserialize<'de> for NonZeroDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

impl Serialize for NonZeroDuration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_err(text: &str) -> DurationError {
        text.parse::<NonZeroDuration>().unwrap_err()
    }

    // A duration string parses, and serializes back to the same text.
    #[test]
    fn non_zero_duration_round_trips() {
        for (text, secs) in [
            ("1m 30s", 90),
            ("1d 12h", 36 * 3_600),
            ("2d", 2 * SECS_PER_DAY),
        ] {
            let duration: NonZeroDuration = serde_json::from_str(&format!("\"{text}\"")).unwrap();
            assert_eq!(duration.get(), Duration::from_secs(secs));
            assert_eq!(
                serde_json::to_string(&duration).unwrap(),
                format!("\"{text}\"")
            );
        }
    }

    // Days and weeks are accepted as fixed 24-hour multiples.
    #[test]
    fn days_and_weeks_are_24_hour_multiples() {
        let duration: NonZeroDuration = "1w 1d".parse().unwrap();
        assert_eq!(duration.get(), Duration::from_secs(8 * SECS_PER_DAY));
    }

    // A zero duration is refused as zero, not as a parse failure.
    #[test]
    fn zero_is_refused() {
        assert!(matches!(parse_err("0s"), DurationError::Zero));
    }

    // Months and years have no fixed length, so they are refused by name.
    #[test]
    fn calendar_units_are_refused() {
        for text in ["1year", "1 month", "1y 2d"] {
            assert!(
                matches!(parse_err(text), DurationError::CalendarUnit),
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
                matches!(parse_err(text), DurationError::Jiff(e) if !e.is_range()),
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
                matches!(parse_err(text), DurationError::Jiff(e) if e.is_range()),
                "{text:?}"
            );
        }
    }
}

use core::{fmt::Display, time::Duration};

use chumsky::prelude::*;
use hardy_bpv7::{eid::Eid, status_report::ReasonCode};
use hardy_eid_patterns::EidPattern;
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use tracing::warn;

use super::Extra;
use crate::{cron::CronExpr, duration::parse_duration};

// Why a contact-plan field value was refused. Each message names the
// field, so the diagnostic says which value on the line is wrong.
#[derive(Debug, Error)]
pub enum ValueError {
    #[error("invalid EID pattern: {0}")]
    Pattern(String),

    #[error("invalid next-hop EID: {0}")]
    NextHop(String),

    #[error("invalid reason code: {0}")]
    ReasonCode(String),

    #[error("invalid priority: {0}")]
    Priority(String),

    #[error("invalid RFC 3339 timestamp: {0}")]
    Timestamp(String),

    #[error("invalid cron expression: {0}")]
    Cron(String),

    #[error("invalid duration: {0}")]
    Duration(String),

    #[error("duration must be greater than zero")]
    ZeroDuration,

    #[error("invalid bandwidth: {0}")]
    Bandwidth(String),

    #[error("invalid delay: {0}")]
    Delay(String),

    #[error("delay too large: {0}us exceeds u32 range")]
    DelayTooLarge(u128),
}

// Wraps a conversion error's text in a `ValueError` variant.
fn wrap<E: Display>(variant: fn(String) -> ValueError) -> impl Fn(E) -> ValueError {
    move |e| variant(e.to_string())
}

// Converts a parsed token into a field value. A token the conversion refuses
// still counts as parsed, so the parser keeps its place on the line and the
// refusal reaches the user with the field's own message; the value is then
// `None`, and the emitted error means the plan is refused.
pub fn validated<'a, P, T>(
    parser: P,
    convert: fn(&str) -> Result<T, ValueError>,
) -> impl Parser<'a, &'a str, Option<T>, Extra<'a>>
where
    P: Parser<'a, &'a str, &'a str, Extra<'a>>,
{
    parser.validate(move |token, extra, emitter| {
        convert(token)
            .map_err(|e| emitter.emit(Rich::custom(extra.span(), e.to_string())))
            .ok()
    })
}

pub fn eid_pattern(token: &str) -> Result<EidPattern, ValueError> {
    token.parse().map_err(wrap(ValueError::Pattern))
}

pub fn next_hop(token: &str) -> Result<Eid, ValueError> {
    token.parse().map_err(wrap(ValueError::NextHop))
}

pub fn reason_code(token: &str) -> Result<ReasonCode, ValueError> {
    let code: u64 = token.parse().map_err(wrap(ValueError::ReasonCode))?;
    code.try_into().map_err(wrap(ValueError::ReasonCode))
}

pub fn priority(token: &str) -> Result<u32, ValueError> {
    token.parse().map_err(wrap(ValueError::Priority))
}

// An RFC 3339 timestamp, converted to UTC (schedule evaluation uses UTC).
pub fn timestamp(token: &str) -> Result<OffsetDateTime, ValueError> {
    let dt = OffsetDateTime::parse(token, &Rfc3339).map_err(wrap(ValueError::Timestamp))?;
    if dt.offset() != UtcOffset::UTC {
        warn!("timestamp '{token}' is not UTC — converting to UTC (schedule evaluation uses UTC)");
    }
    Ok(dt.to_offset(UtcOffset::UTC))
}

pub fn cron(token: &str) -> Result<CronExpr, ValueError> {
    CronExpr::parse(token).map_err(ValueError::Cron)
}

pub fn duration(token: &str) -> Result<Duration, ValueError> {
    let d = parse_duration(token).map_err(wrap(ValueError::Duration))?;
    if d.is_zero() {
        return Err(ValueError::ZeroDuration);
    }
    Ok(d)
}

pub fn bandwidth(token: &str) -> Result<u64, ValueError> {
    parse_bandwidth(token).map_err(ValueError::Bandwidth)
}

// A one-way delay in microseconds.
pub fn delay(token: &str) -> Result<u32, ValueError> {
    let us = parse_duration(token)
        .map_err(wrap(ValueError::Delay))?
        .as_micros();
    u32::try_from(us).map_err(|_| ValueError::DelayTooLarge(us))
}

// Parse a bandwidth value with optional SI suffix.
//
// Accepts: `10G`, `256K`, `1M`, `9600` (bare = bps).
// Case-insensitive suffixes: `K` (×1,000), `M` (×1,000,000),
// `G` (×1,000,000,000), `T` (×1,000,000,000,000).
fn parse_bandwidth(s: &str) -> Result<u64, String> {
    let s_upper = s.to_ascii_uppercase();
    let (num_part, multiplier) =
        if let Some(num) = s_upper.strip_suffix("TBPS").or(s_upper.strip_suffix('T')) {
            (num, 1_000_000_000_000u64)
        } else if let Some(num) = s_upper.strip_suffix("GBPS").or(s_upper.strip_suffix('G')) {
            (num, 1_000_000_000)
        } else if let Some(num) = s_upper.strip_suffix("MBPS").or(s_upper.strip_suffix('M')) {
            (num, 1_000_000)
        } else if let Some(num) = s_upper.strip_suffix("KBPS").or(s_upper.strip_suffix('K')) {
            (num, 1_000)
        } else if let Some(num) = s_upper.strip_suffix("BPS") {
            (num, 1)
        } else {
            (s_upper.as_str(), 1)
        };

    let n: u64 = num_part
        .parse()
        .map_err(|_| format!("'{s}' is not a valid bandwidth"))?;

    n.checked_mul(multiplier)
        .ok_or_else(|| format!("'{s}' overflows u64"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each refused value maps to the variant naming its field.
    #[test]
    fn refused_values_name_their_field() {
        assert!(matches!(
            eid_pattern("not-a-pattern"),
            Err(ValueError::Pattern(_))
        ));
        assert!(matches!(
            next_hop("not-an-eid"),
            Err(ValueError::NextHop(_))
        ));
        assert!(matches!(reason_code("255"), Err(ValueError::ReasonCode(_))));
        assert!(matches!(
            priority("99999999999"),
            Err(ValueError::Priority(_))
        ));
        assert!(matches!(
            timestamp("yesterday"),
            Err(ValueError::Timestamp(_))
        ));
        assert!(matches!(cron("0 8 *"), Err(ValueError::Cron(_))));
        assert!(matches!(duration("1month"), Err(ValueError::Duration(_))));
        assert!(matches!(duration("0m"), Err(ValueError::ZeroDuration)));
        assert!(matches!(bandwidth("10X"), Err(ValueError::Bandwidth(_))));
        assert!(matches!(delay("30"), Err(ValueError::Delay(_))));
        assert!(matches!(
            delay("2h"),
            Err(ValueError::DelayTooLarge(7_200_000_000))
        ));
    }

    // A non-UTC timestamp is accepted and converted to UTC.
    #[test]
    fn timestamps_convert_to_utc() {
        let dt = timestamp("2026-03-27T10:00:00+02:00").unwrap();
        assert_eq!(dt, timestamp("2026-03-27T08:00:00Z").unwrap());
        assert_eq!(dt.offset(), UtcOffset::UTC);
    }
}

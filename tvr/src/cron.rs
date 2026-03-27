/// Cron expression with optional seconds field.
///
/// Accepts 5 fields (`minute hour dom month dow`) or 6 fields
/// (`second minute hour dom month dow`), plus `@` shortcuts.
///
/// Each field is a bitset of matching values, giving O(1) match checks.
///
/// Supports:
/// - Numeric values, ranges, steps, lists: `*`, `N`, `N-M`, `*/S`, `N-M/S`, `N,M,...`
/// - Named weekdays: `SUN`–`SAT` (case-insensitive)
/// - Named months: `JAN`–`DEC` (case-insensitive)
/// - Shortcuts: `@yearly`, `@annually`, `@monthly`, `@weekly`, `@daily`, `@midnight`, `@hourly`
#[derive(Debug, Clone, Eq)]
pub struct CronExpr {
    /// Matching seconds (bits 0–59)
    pub second: u64,
    /// Matching minutes (bits 0–59)
    pub minute: u64,
    /// Matching hours (bits 0–23)
    pub hour: u32,
    /// Matching days of month (bits 1–31)
    pub dom: u32,
    /// Matching months (bits 1–12)
    pub month: u16,
    /// Matching days of week (bits 0–6, 0 = Sunday)
    pub dow: u8,
    /// Original expression for display (excluded from equality)
    source: String,
}

impl PartialEq for CronExpr {
    fn eq(&self, other: &Self) -> bool {
        self.second == other.second
            && self.minute == other.minute
            && self.hour == other.hour
            && self.dom == other.dom
            && self.month == other.month
            && self.dow == other.dow
    }
}

impl core::fmt::Display for CronExpr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.source)
    }
}

// ── Named value tables ──────────────────────────────────────────────

const DOW_NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
const MONTH_NAMES: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

/// Bitsets covering all values for a field
const ALL_HOURS: u32 = (1 << 24) - 1;
const ALL_DOM: u32 = !1u32; // bits 1-31 (excludes bit 0)
const ALL_MONTHS: u16 = 0x1FFE; // bits 1-12
const ALL_DOW: u8 = 0x7F; // bits 0-6

impl CronExpr {
    /// Parse a cron expression.
    ///
    /// Accepts:
    /// - 5 fields: `minute hour dom month dow`
    /// - 6 fields: `second minute hour dom month dow`
    /// - Shortcuts: `@yearly`, `@monthly`, `@weekly`, `@daily`, `@hourly`
    ///
    /// Returns a human-readable error message on failure.
    pub fn parse(s: &str) -> Result<Self, String> {
        let trimmed = s.trim();

        // Handle @ shortcuts
        if let Some(shortcut) = trimmed.strip_prefix('@') {
            return Self::parse_shortcut(shortcut, s);
        }

        let fields: Vec<&str> = trimmed.split_whitespace().collect();

        match fields.len() {
            5 => Self::parse_5(fields, s),
            6 => Self::parse_6(fields, s),
            n => Err(format!("expected 5 or 6 fields (or @shortcut), got {n}")),
        }
    }

    fn parse_5(fields: Vec<&str>, source: &str) -> Result<Self, String> {
        let minute = parse_field(fields[0], 0, 59, "minute", &[])?;
        let hour = parse_field(fields[1], 0, 23, "hour", &[])? as u32;
        let dom = parse_field(fields[2], 1, 31, "dom", &[])? as u32;
        let month = parse_field(fields[3], 1, 12, "month", &MONTH_NAMES)? as u16;
        let dow = parse_dow(fields[4])?;

        Ok(Self {
            second: 1, // bit 0 only — fires at :00
            minute,
            hour,
            dom,
            month,
            dow,
            source: source.to_string(),
        })
    }

    fn parse_6(fields: Vec<&str>, source: &str) -> Result<Self, String> {
        let second = parse_field(fields[0], 0, 59, "second", &[])?;
        let minute = parse_field(fields[1], 0, 59, "minute", &[])?;
        let hour = parse_field(fields[2], 0, 23, "hour", &[])? as u32;
        let dom = parse_field(fields[3], 1, 31, "dom", &[])? as u32;
        let month = parse_field(fields[4], 1, 12, "month", &MONTH_NAMES)? as u16;
        let dow = parse_dow(fields[5])?;

        Ok(Self {
            second,
            minute,
            hour,
            dom,
            month,
            dow,
            source: source.to_string(),
        })
    }

    fn parse_shortcut(shortcut: &str, source: &str) -> Result<Self, String> {
        let expr = match shortcut.to_ascii_lowercase().as_str() {
            "yearly" | "annually" => Self {
                second: 1,
                minute: 1,
                hour: 1,
                dom: 1 << 1,
                month: 1 << 1,
                dow: ALL_DOW,
                source: source.to_string(),
            },
            "monthly" => Self {
                second: 1,
                minute: 1,
                hour: 1,
                dom: 1 << 1,
                month: ALL_MONTHS,
                dow: ALL_DOW,
                source: source.to_string(),
            },
            "weekly" => Self {
                second: 1,
                minute: 1,
                hour: 1,
                dom: ALL_DOM,
                month: ALL_MONTHS,
                dow: 1, // Sunday
                source: source.to_string(),
            },
            "daily" | "midnight" => Self {
                second: 1,
                minute: 1,
                hour: 1,
                dom: ALL_DOM,
                month: ALL_MONTHS,
                dow: ALL_DOW,
                source: source.to_string(),
            },
            "hourly" => Self {
                second: 1,
                minute: 1,
                hour: ALL_HOURS,
                dom: ALL_DOM,
                month: ALL_MONTHS,
                dow: ALL_DOW,
                source: source.to_string(),
            },
            _ => return Err(format!("unknown shortcut '@{shortcut}'")),
        };
        Ok(expr)
    }

    /// Does this expression match the given datetime?
    /// Whether this expression has second-level granularity.
    fn has_seconds(&self) -> bool {
        self.second != 1 // anything other than "only second 0"
    }

    /// Find the next datetime at or after `after` that matches this expression.
    ///
    /// Returns `None` if no match is found within ~4 years (to prevent
    /// infinite loops on impossible expressions like dom=31 + month=Feb).
    pub fn next_after(&self, after: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
        let mut dt = after.replace_nanosecond(0).ok()?;
        if !self.has_seconds() {
            dt = dt.replace_second(0).ok()?;
        }

        let limit = after + time::Duration::days(366 * 4);

        while dt <= limit {
            if (self.month & (1 << dt.month() as u16)) == 0 {
                dt = advance_month(dt)?;
                continue;
            }
            if (self.dom & (1 << dt.day() as u32)) == 0 {
                dt = advance_day(dt)?;
                continue;
            }
            if (self.dow & (1 << dt.weekday().number_days_from_sunday())) == 0 {
                dt = advance_day(dt)?;
                continue;
            }
            if (self.hour & (1 << dt.hour() as u32)) == 0 {
                dt = advance_hour(dt)?;
                continue;
            }
            if (self.minute & (1u64 << dt.minute() as u32)) == 0 {
                dt = advance_minute(dt)?;
                continue;
            }
            if (self.second & (1u64 << dt.second() as u32)) == 0 {
                dt += time::Duration::seconds(1);
                continue;
            }
            return Some(dt);
        }

        None
    }

    /// Find the last datetime at or before `before` that matches this expression.
    ///
    /// Returns `None` if no match is found within ~4 years back.
    pub fn prev_before(&self, before: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
        let mut dt = before.replace_nanosecond(0).ok()?;
        if !self.has_seconds() {
            dt = dt.replace_second(0).ok()?;
        }

        let limit = before - time::Duration::days(366 * 4);

        while dt >= limit {
            if (self.month & (1 << dt.month() as u16)) == 0 {
                dt = retreat_month(dt)?;
                continue;
            }
            if (self.dom & (1 << dt.day() as u32)) == 0 {
                dt = retreat_day(dt)?;
                continue;
            }
            if (self.dow & (1 << dt.weekday().number_days_from_sunday())) == 0 {
                dt = retreat_day(dt)?;
                continue;
            }
            if (self.hour & (1 << dt.hour() as u32)) == 0 {
                dt = retreat_hour(dt)?;
                continue;
            }
            if (self.minute & (1u64 << dt.minute() as u32)) == 0 {
                dt = retreat_minute(dt)?;
                continue;
            }
            if (self.second & (1u64 << dt.second() as u32)) == 0 {
                dt -= time::Duration::seconds(1);
                continue;
            }
            return Some(dt);
        }

        None
    }
}

// ── Field parsing ───────────────────────────────────────────────────

/// Parse a single cron field into a bitset.
///
/// `names` provides optional named aliases (e.g. `JAN`–`DEC` for months).
/// Names are 1-indexed: the first name maps to value `min`.
fn parse_field(field: &str, min: u32, max: u32, name: &str, names: &[&str]) -> Result<u64, String> {
    let mut bits: u64 = 0;

    for item in field.split(',') {
        if item.is_empty() {
            return Err(format!("{name}: empty item in list"));
        }
        bits |= parse_item(item, min, max, name, names)?;
    }

    if bits == 0 {
        return Err(format!("{name}: field matches nothing"));
    }

    Ok(bits)
}

/// Try to parse a value as a number or a named alias.
fn parse_value(s: &str, min: u32, _max: u32, name: &str, names: &[&str]) -> Result<u32, String> {
    // Try named alias first
    if !names.is_empty() {
        let upper = s.to_ascii_uppercase();
        if let Some(pos) = names.iter().position(|n| *n == upper) {
            return Ok(min + pos as u32);
        }
    }
    // Then try numeric
    s.parse::<u32>()
        .map_err(|_| format!("{name}: invalid value '{s}'"))
}

fn parse_item(item: &str, min: u32, max: u32, name: &str, names: &[&str]) -> Result<u64, String> {
    // Split on '/' for step
    let (range_part, step) = match item.split_once('/') {
        Some((r, s)) => {
            let step: u32 = s
                .parse()
                .map_err(|_| format!("{name}: invalid step '{s}'"))?;
            if step == 0 {
                return Err(format!("{name}: step must be > 0"));
            }
            (r, Some(step))
        }
        None => (item, None),
    };

    // Parse the range part
    let (start, end) = if range_part == "*" {
        (min, max)
    } else if let Some((a, b)) = range_part.split_once('-') {
        let a = parse_value(a, min, max, name, names)?;
        let b = parse_value(b, min, max, name, names)?;
        if a < min || a > max {
            return Err(format!("{name}: {a} out of range {min}-{max}"));
        }
        if b < min || b > max {
            return Err(format!("{name}: {b} out of range {min}-{max}"));
        }
        if a > b {
            return Err(format!("{name}: range {a}-{b} is empty"));
        }
        (a, b)
    } else {
        let v = parse_value(range_part, min, max, name, names)?;
        if v < min || v > max {
            return Err(format!("{name}: {v} out of range {min}-{max}"));
        }
        match step {
            Some(_) => (v, max),
            None => return Ok(1u64 << v),
        }
    };

    // Generate bits
    let step = step.unwrap_or(1);
    let mut bits: u64 = 0;
    let mut v = start;
    while v <= end {
        bits |= 1u64 << v;
        v += step;
    }

    Ok(bits)
}

/// Parse day-of-week field with special handling for Sunday alias (0 and 7 both valid).
fn parse_dow(field: &str) -> Result<u8, String> {
    let bits = parse_field(field, 0, 7, "dow", &DOW_NAMES)?;
    // Normalize: bit 7 (Sunday alias) folds into bit 0
    Ok(((bits & 0x7F) | ((bits >> 7) & 1)) as u8)
}

// ── Time advancement helpers ────────────────────────────────────────

fn advance_minute(dt: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
    let dt = dt.replace_second(0).ok()?;
    Some(dt + time::Duration::minutes(1))
}

fn advance_hour(dt: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
    let dt = dt.replace_second(0).ok()?.replace_minute(0).ok()?;
    Some(dt + time::Duration::hours(1))
}

fn advance_day(dt: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
    let dt = dt
        .replace_second(0)
        .ok()?
        .replace_minute(0)
        .ok()?
        .replace_hour(0)
        .ok()?;
    Some(dt + time::Duration::days(1))
}

fn advance_month(dt: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
    let dt = dt
        .replace_second(0)
        .ok()?
        .replace_minute(0)
        .ok()?
        .replace_hour(0)
        .ok()?
        .replace_day(1)
        .ok()?;
    let m = dt.month().next();
    let y = if m == time::Month::January {
        dt.year() + 1
    } else {
        dt.year()
    };
    dt.replace_month(m).ok()?.replace_year(y).ok()
}

fn retreat_minute(dt: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
    let dt = dt.replace_second(59).ok()?;
    Some(dt - time::Duration::minutes(1))
}

fn retreat_hour(dt: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
    let dt = dt.replace_second(59).ok()?.replace_minute(59).ok()?;
    Some(dt - time::Duration::hours(1))
}

fn retreat_day(dt: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
    let dt = dt
        .replace_second(59)
        .ok()?
        .replace_minute(59)
        .ok()?
        .replace_hour(23)
        .ok()?;
    Some(dt - time::Duration::days(1))
}

fn retreat_month(dt: time::OffsetDateTime) -> Option<time::OffsetDateTime> {
    let m = dt.month().previous();
    let y = if m == time::Month::December {
        dt.year() - 1
    } else {
        dt.year()
    };
    let last_day = days_in_month(y, m);
    dt.replace_month(m)
        .ok()?
        .replace_year(y)
        .ok()?
        .replace_day(last_day)
        .ok()?
        .replace_hour(23)
        .ok()?
        .replace_minute(59)
        .ok()?
        .replace_second(59)
        .ok()
}

fn days_in_month(year: i32, month: time::Month) -> u8 {
    match month {
        time::Month::February => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        time::Month::April | time::Month::June | time::Month::September | time::Month::November => {
            30
        }
        _ => 31,
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use time::macros::datetime;

    impl CronExpr {
        fn matches(&self, dt: &time::OffsetDateTime) -> bool {
            let sec = dt.second() as u32;
            let min = dt.minute() as u32;
            let hr = dt.hour() as u32;
            let d = dt.day() as u32;
            let m = dt.month() as u16;
            let wd = dt.weekday().number_days_from_sunday();

            (self.second & (1u64 << sec)) != 0
                && (self.minute & (1u64 << min)) != 0
                && (self.hour & (1 << hr)) != 0
                && (self.dom & (1 << d)) != 0
                && (self.month & (1 << m)) != 0
                && (self.dow & (1 << wd)) != 0
        }
    }

    fn parse_ok(s: &str) -> CronExpr {
        CronExpr::parse(s).unwrap_or_else(|e| panic!("should parse '{s}': {e}"))
    }

    fn parse_err(s: &str) {
        assert!(CronExpr::parse(s).is_err(), "should fail: '{s}'");
    }

    // ── 5-field parsing ─────────────────────────────────────────────

    #[test]
    fn every_minute() {
        let c = parse_ok("* * * * *");
        assert_eq!(c.second, 1); // 5-field: second defaults to 0 only
        assert_eq!(c.minute, (1u64 << 60) - 1);
        assert_eq!(c.hour, ALL_HOURS);
        assert_eq!(c.dow, ALL_DOW);
    }

    #[test]
    fn specific_values() {
        let c = parse_ok("0 8 * * *");
        assert_eq!(c.minute, 1); // bit 0 only
        assert_eq!(c.hour, 1 << 8);
    }

    #[test]
    fn range() {
        let c = parse_ok("0 9-17 * * 1-5");
        assert_eq!(c.hour, 0b111111111 << 9); // bits 9-17
        assert_eq!(c.dow, 0b0111110); // bits 1-5
    }

    #[test]
    fn step() {
        let c = parse_ok("*/15 * * * *");
        assert_eq!(
            c.minute,
            (1u64 << 0) | (1u64 << 15) | (1u64 << 30) | (1u64 << 45)
        );
    }

    #[test]
    fn range_with_step() {
        let c = parse_ok("0 8-18/2 * * *");
        let expected = (1u32 << 8) | (1 << 10) | (1 << 12) | (1 << 14) | (1 << 16) | (1 << 18);
        assert_eq!(c.hour, expected);
    }

    #[test]
    fn list() {
        let c = parse_ok("0,30 * * * *");
        assert_eq!(c.minute, (1u64 << 0) | (1u64 << 30));
    }

    // ── 6-field parsing (with seconds) ──────────────────────────────

    #[test]
    fn six_field_with_seconds() {
        let c = parse_ok("30 0 8 * * *");
        assert_eq!(c.second, 1u64 << 30);
        assert_eq!(c.minute, 1); // bit 0
        assert_eq!(c.hour, 1 << 8);
    }

    #[test]
    fn six_field_every_10_seconds() {
        let c = parse_ok("*/10 * * * * *");
        assert_eq!(
            c.second,
            (1u64 << 0) | (1u64 << 10) | (1u64 << 20) | (1u64 << 30) | (1u64 << 40) | (1u64 << 50)
        );
        assert_eq!(c.minute, (1u64 << 60) - 1);
    }

    // ── Named days of week ──────────────────────────────────────────

    #[test]
    fn named_weekdays() {
        let c = parse_ok("0 9 * * MON-FRI");
        assert_eq!(c.dow, 0b0111110); // bits 1-5
    }

    #[test]
    fn named_weekday_list() {
        let c = parse_ok("0 9 * * MON,WED,FRI");
        assert_eq!(c.dow, (1 << 1) | (1 << 3) | (1 << 5));
    }

    #[test]
    fn named_weekday_case_insensitive() {
        let c1 = parse_ok("0 9 * * mon-fri");
        let c2 = parse_ok("0 9 * * MON-FRI");
        assert_eq!(c1.dow, c2.dow);
    }

    #[test]
    fn named_sunday() {
        let c = parse_ok("0 0 * * SUN");
        assert_eq!(c.dow, 1); // bit 0
    }

    // ── Named months ────────────────────────────────────────────────

    #[test]
    fn named_months() {
        let c = parse_ok("0 8 * MAR-OCT *");
        // MAR=3, OCT=10 → bits 3-10
        let expected: u16 = (3..=10).fold(0, |acc, b| acc | (1 << b));
        assert_eq!(c.month, expected);
    }

    #[test]
    fn named_month_list() {
        let c = parse_ok("0 8 * JAN,JUN,DEC *");
        assert_eq!(c.month, (1 << 1) | (1 << 6) | (1 << 12));
    }

    // ── Day of week Sunday alias ────────────────────────────────────

    #[test]
    fn dow_sunday_alias_numeric() {
        let c0 = parse_ok("0 0 * * 0");
        let c7 = parse_ok("0 0 * * 7");
        assert_eq!(c0.dow, c7.dow);
        assert_eq!(c0.dow, 1); // bit 0 = Sunday
    }

    // ── @ shortcuts ─────────────────────────────────────────────────

    #[test]
    fn shortcut_daily() {
        let c = parse_ok("@daily");
        assert_eq!(c.second, 1);
        assert_eq!(c.minute, 1);
        assert_eq!(c.hour, 1);
        assert_eq!(c.dom, ALL_DOM);
        assert_eq!(c.month, ALL_MONTHS);
        assert_eq!(c.dow, ALL_DOW);
    }

    #[test]
    fn shortcut_midnight() {
        let daily = parse_ok("@daily");
        let midnight = parse_ok("@midnight");
        assert_eq!(daily, midnight);
    }

    #[test]
    fn shortcut_hourly() {
        let c = parse_ok("@hourly");
        assert_eq!(c.minute, 1);
        assert_eq!(c.hour, ALL_HOURS);
    }

    #[test]
    fn shortcut_weekly() {
        let c = parse_ok("@weekly");
        assert_eq!(c.dow, 1); // Sunday
        assert_eq!(c.hour, 1);
    }

    #[test]
    fn shortcut_monthly() {
        let c = parse_ok("@monthly");
        assert_eq!(c.dom, 1 << 1); // 1st
        assert_eq!(c.month, ALL_MONTHS);
    }

    #[test]
    fn shortcut_yearly() {
        let c = parse_ok("@yearly");
        assert_eq!(c.dom, 1 << 1);
        assert_eq!(c.month, 1 << 1); // January
    }

    #[test]
    fn shortcut_annually() {
        let yearly = parse_ok("@yearly");
        let annually = parse_ok("@annually");
        assert_eq!(yearly, annually);
    }

    #[test]
    fn shortcut_unknown() {
        parse_err("@every");
        parse_err("@secondly");
    }

    // ── Validation ──────────────────────────────────────────────────

    #[test]
    fn invalid_field_count() {
        parse_err("* * *");
        parse_err("* * * * * * *");
    }

    #[test]
    fn out_of_range() {
        parse_err("60 * * * *");
        parse_err("* 24 * * *");
        parse_err("* * 0 * *");
        parse_err("* * 32 * *");
        parse_err("* * * 0 *");
        parse_err("* * * 13 *");
        parse_err("* * * * 8");
    }

    #[test]
    fn empty_range() {
        parse_err("* * * * 5-3");
    }

    #[test]
    fn zero_step() {
        parse_err("*/0 * * * *");
    }

    // ── Matching ────────────────────────────────────────────────────

    #[test]
    fn matches_specific() {
        let c = parse_ok("0 8 * * *");
        assert!(c.matches(&datetime!(2026-03-27 08:00:00 UTC)));
        assert!(!c.matches(&datetime!(2026-03-27 08:01:00 UTC)));
        assert!(!c.matches(&datetime!(2026-03-27 07:00:00 UTC)));
    }

    #[test]
    fn matches_with_seconds() {
        let c = parse_ok("30 0 8 * * *");
        assert!(c.matches(&datetime!(2026-03-27 08:00:30 UTC)));
        assert!(!c.matches(&datetime!(2026-03-27 08:00:00 UTC)));
        assert!(!c.matches(&datetime!(2026-03-27 08:00:31 UTC)));
    }

    #[test]
    fn matches_weekday() {
        let c = parse_ok("0 9 * * MON-FRI");
        // 2026-03-27 is Friday
        assert!(c.matches(&datetime!(2026-03-27 09:00:00 UTC)));
        // 2026-03-28 is Saturday
        assert!(!c.matches(&datetime!(2026-03-28 09:00:00 UTC)));
    }

    // ── next_after ──────────────────────────────────────────────────

    #[test]
    fn next_after_same_minute() {
        let c = parse_ok("0 8 * * *");
        let dt = datetime!(2026-03-27 08:00:00 UTC);
        assert_eq!(c.next_after(dt), Some(datetime!(2026-03-27 08:00:00 UTC)));
    }

    #[test]
    fn next_after_later_today() {
        let c = parse_ok("30 8 * * *");
        let dt = datetime!(2026-03-27 08:00:00 UTC);
        assert_eq!(c.next_after(dt), Some(datetime!(2026-03-27 08:30:00 UTC)));
    }

    #[test]
    fn next_after_tomorrow() {
        let c = parse_ok("0 8 * * *");
        let dt = datetime!(2026-03-27 09:00:00 UTC);
        assert_eq!(c.next_after(dt), Some(datetime!(2026-03-28 08:00:00 UTC)));
    }

    #[test]
    fn next_after_skips_weekend() {
        let c = parse_ok("0 9 * * 1-5");
        // Friday 2026-03-27 after 09:00 → next is Monday 2026-03-30
        let dt = datetime!(2026-03-27 10:00:00 UTC);
        assert_eq!(c.next_after(dt), Some(datetime!(2026-03-30 09:00:00 UTC)));
    }

    #[test]
    fn next_after_month_rollover() {
        let c = parse_ok("0 0 1 * *");
        let dt = datetime!(2026-03-02 00:00:00 UTC);
        assert_eq!(c.next_after(dt), Some(datetime!(2026-04-01 00:00:00 UTC)));
    }

    #[test]
    fn next_after_with_seconds() {
        let c = parse_ok("*/30 * * * * *"); // every 30 seconds
        let dt = datetime!(2026-03-27 08:00:01 UTC);
        assert_eq!(c.next_after(dt), Some(datetime!(2026-03-27 08:00:30 UTC)));
    }

    // ── prev_before ─────────────────────────────────────────────────

    #[test]
    fn prev_before_same_minute() {
        let c = parse_ok("0 8 * * *");
        let dt = datetime!(2026-03-27 08:00:00 UTC);
        assert_eq!(c.prev_before(dt), Some(datetime!(2026-03-27 08:00:00 UTC)));
    }

    #[test]
    fn prev_before_earlier_today() {
        let c = parse_ok("0 8 * * *");
        let dt = datetime!(2026-03-27 09:00:00 UTC);
        assert_eq!(c.prev_before(dt), Some(datetime!(2026-03-27 08:00:00 UTC)));
    }

    #[test]
    fn prev_before_yesterday() {
        let c = parse_ok("0 8 * * *");
        let dt = datetime!(2026-03-27 07:00:00 UTC);
        assert_eq!(c.prev_before(dt), Some(datetime!(2026-03-26 08:00:00 UTC)));
    }

    #[test]
    fn prev_before_skips_weekend() {
        let c = parse_ok("0 17 * * MON-FRI");
        // Sunday 2026-03-29 → previous is Friday 2026-03-27
        let dt = datetime!(2026-03-29 12:00:00 UTC);
        assert_eq!(c.prev_before(dt), Some(datetime!(2026-03-27 17:00:00 UTC)));
    }

    #[test]
    fn prev_before_with_seconds() {
        let c = parse_ok("*/30 * * * * *"); // every 30 seconds
        let dt = datetime!(2026-03-27 08:00:29 UTC);
        assert_eq!(c.prev_before(dt), Some(datetime!(2026-03-27 08:00:00 UTC)));
    }

    // ── Display ─────────────────────────────────────────────────────

    #[test]
    fn display_preserves_source() {
        let c = parse_ok("*/15 8-17 * * MON-FRI");
        assert_eq!(c.to_string(), "*/15 8-17 * * MON-FRI");
    }

    #[test]
    fn display_preserves_shortcut() {
        let c = parse_ok("@daily");
        assert_eq!(c.to_string(), "@daily");
    }
}

use crate::contacts::{Contact, Schedule};
use chumsky::prelude::*;
use hardy_bpa::routes::Action;
use hardy_eid_patterns as eid_patterns;
use std::path::PathBuf;
use tracing::{debug, error};

type Span = SimpleSpan<usize>;
type Extra<'a> = extra::Err<Rich<'a, char, Span>>;

// ── Shared primitives ───────────────────────────────────────────────

fn keyword<'a>(word: &'a str) -> impl Parser<'a, &'a str, (), Extra<'a>> {
    just(word).ignored()
}

/// A non-whitespace token (used for EIDs, patterns, etc.)
fn non_ws_token<'a>() -> impl Parser<'a, &'a str, &'a str, Extra<'a>> {
    any()
        .filter(|c: &char| !c.is_whitespace())
        .repeated()
        .at_least(1)
        .to_slice()
}

// ── Pattern ─────────────────────────────────────────────────────────

fn pattern<'a>() -> impl Parser<'a, &'a str, eid_patterns::EidPattern, Extra<'a>> {
    non_ws_token()
        .try_map(|s: &str, span| {
            s.parse()
                .map_err(|e| Rich::custom(span, format!("invalid EID pattern: {e}")))
        })
        .labelled("EID pattern")
}

// ── Actions (via / drop — no reflect) ───────────────────────────────

fn drop_action<'a>() -> impl Parser<'a, &'a str, Action, Extra<'a>> {
    keyword("drop")
        .then(
            text::inline_whitespace()
                .at_least(1)
                .ignore_then(text::int(10))
                .try_map(|s: &str, span| {
                    let code: u64 = s
                        .parse()
                        .map_err(|e| Rich::custom(span, format!("invalid reason code: {e}")))?;
                    code.try_into()
                        .map_err(|e| Rich::custom(span, format!("invalid reason code: {e}")))
                })
                .or_not(),
        )
        .map(|(_, reason)| Action::Drop(reason))
        .labelled("drop action")
}

fn via_action<'a>() -> impl Parser<'a, &'a str, Action, Extra<'a>> {
    keyword("via")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(
            non_ws_token()
                .try_map(|s: &str, span| {
                    s.parse()
                        .map_err(|e| Rich::custom(span, format!("invalid next-hop EID: {e}")))
                })
                .labelled("next-hop EID"),
        )
        .map(Action::Via)
        .labelled("via action")
}

fn action<'a>() -> impl Parser<'a, &'a str, Action, Extra<'a>> {
    choice((drop_action(), via_action())).labelled("action")
}

// ── Named fields (keyword-value pairs) ──────────────────────────────

fn priority_field<'a>() -> impl Parser<'a, &'a str, u32, Extra<'a>> {
    keyword("priority")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(
            text::int(10)
                .try_map(|s: &str, span| {
                    s.parse()
                        .map_err(|e| Rich::custom(span, format!("invalid priority: {e}")))
                })
                .labelled("priority value"),
        )
        .labelled("priority")
}

/// RFC 3339 timestamp (e.g. "2026-03-27T08:00:00Z")
fn rfc3339_timestamp<'a>() -> impl Parser<'a, &'a str, time::OffsetDateTime, Extra<'a>> {
    non_ws_token()
        .try_map(|s: &str, span| {
            time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                .map_err(|e| Rich::custom(span, format!("invalid RFC 3339 timestamp: {e}")))
        })
        .labelled("RFC 3339 timestamp")
}

fn start_field<'a>() -> impl Parser<'a, &'a str, time::OffsetDateTime, Extra<'a>> {
    keyword("start")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(rfc3339_timestamp())
        .labelled("start time")
}

fn end_field<'a>() -> impl Parser<'a, &'a str, time::OffsetDateTime, Extra<'a>> {
    keyword("end")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(rfc3339_timestamp())
        .labelled("end time")
}

fn until_field<'a>() -> impl Parser<'a, &'a str, time::OffsetDateTime, Extra<'a>> {
    keyword("until")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(rfc3339_timestamp())
        .labelled("until time")
}

/// Quoted string (for cron expressions): `"0 8 * * *"`
fn quoted_string<'a>() -> impl Parser<'a, &'a str, &'a str, Extra<'a>> {
    just('"')
        .ignore_then(
            any()
                .filter(|c: &char| *c != '"' && *c != '\n')
                .repeated()
                .to_slice(),
        )
        .then_ignore(just('"'))
        .labelled("quoted string")
}

fn cron_field<'a>() -> impl Parser<'a, &'a str, &'a str, Extra<'a>> {
    keyword("cron")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(quoted_string())
        .labelled("cron expression")
}

/// Duration value: e.g. "90m", "2h", "4h30m", "1h15m30s"
/// Supports: Nh, Nm, Ns and combinations thereof.
fn duration_value<'a>() -> impl Parser<'a, &'a str, std::time::Duration, Extra<'a>> {
    non_ws_token()
        .try_map(|s: &str, span| parse_duration(s, span))
        .labelled("duration")
}

fn duration_field<'a>() -> impl Parser<'a, &'a str, std::time::Duration, Extra<'a>> {
    keyword("duration")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(duration_value())
        .labelled("duration field")
}

fn bps_field<'a>() -> impl Parser<'a, &'a str, u64, Extra<'a>> {
    keyword("bps")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(
            text::int(10)
                .try_map(|s: &str, span| {
                    s.parse()
                        .map_err(|e| Rich::custom(span, format!("invalid bandwidth: {e}")))
                })
                .labelled("bandwidth value"),
        )
        .labelled("bandwidth")
}

fn delay_field<'a>() -> impl Parser<'a, &'a str, u32, Extra<'a>> {
    keyword("delay")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(
            text::int(10)
                .try_map(|s: &str, span| {
                    s.parse()
                        .map_err(|e| Rich::custom(span, format!("invalid delay: {e}")))
                })
                .labelled("delay value"),
        )
        .labelled("delay")
}

// ── Contact fields (order-independent keyword fields) ───────────────

/// All optional fields that can appear after the action, in any order.
#[derive(Default)]
struct ContactFields<'a> {
    priority: Option<u32>,
    start: Option<time::OffsetDateTime>,
    end: Option<time::OffsetDateTime>,
    cron: Option<&'a str>,
    duration: Option<std::time::Duration>,
    until: Option<time::OffsetDateTime>,
    bps: Option<u64>,
    delay: Option<u32>,
}

/// A single optional field — parsed one at a time, accumulated into ContactFields.
enum Field<'a> {
    Priority(u32),
    Start(time::OffsetDateTime),
    End(time::OffsetDateTime),
    Cron(&'a str),
    Duration(std::time::Duration),
    Until(time::OffsetDateTime),
    Bps(u64),
    Delay(u32),
}

fn field<'a>() -> impl Parser<'a, &'a str, Field<'a>, Extra<'a>> {
    choice((
        priority_field().map(Field::Priority),
        start_field().map(Field::Start),
        end_field().map(Field::End),
        cron_field().map(Field::Cron),
        duration_field().map(Field::Duration),
        until_field().map(Field::Until),
        bps_field().map(Field::Bps),
        delay_field().map(Field::Delay),
    ))
    .labelled("field")
}

fn contact_fields<'a>() -> impl Parser<'a, &'a str, ContactFields<'a>, Extra<'a>> {
    text::inline_whitespace()
        .at_least(1)
        .ignore_then(field())
        .repeated()
        .collect::<Vec<_>>()
        .try_map(|fields, span| {
            let mut cf = ContactFields::default();
            for f in fields {
                match f {
                    Field::Priority(v) => {
                        if cf.priority.is_some() {
                            return Err(Rich::custom(span, "duplicate 'priority' field"));
                        }
                        cf.priority = Some(v);
                    }
                    Field::Start(v) => {
                        if cf.start.is_some() {
                            return Err(Rich::custom(span, "duplicate 'start' field"));
                        }
                        cf.start = Some(v);
                    }
                    Field::End(v) => {
                        if cf.end.is_some() {
                            return Err(Rich::custom(span, "duplicate 'end' field"));
                        }
                        cf.end = Some(v);
                    }
                    Field::Cron(v) => {
                        if cf.cron.is_some() {
                            return Err(Rich::custom(span, "duplicate 'cron' field"));
                        }
                        cf.cron = Some(v);
                    }
                    Field::Duration(v) => {
                        if cf.duration.is_some() {
                            return Err(Rich::custom(span, "duplicate 'duration' field"));
                        }
                        cf.duration = Some(v);
                    }
                    Field::Until(v) => {
                        if cf.until.is_some() {
                            return Err(Rich::custom(span, "duplicate 'until' field"));
                        }
                        cf.until = Some(v);
                    }
                    Field::Bps(v) => {
                        if cf.bps.is_some() {
                            return Err(Rich::custom(span, "duplicate 'bps' field"));
                        }
                        cf.bps = Some(v);
                    }
                    Field::Delay(v) => {
                        if cf.delay.is_some() {
                            return Err(Rich::custom(span, "duplicate 'delay' field"));
                        }
                        cf.delay = Some(v);
                    }
                }
            }
            Ok(cf)
        })
}

// ── Contact line ────────────────────────────────────────────────────

fn contact<'a>() -> impl Parser<'a, &'a str, Contact, Extra<'a>> {
    pattern()
        .then(
            text::inline_whitespace()
                .at_least(1)
                .ignore_then(action())
                .labelled("action"),
        )
        .then(contact_fields())
        .try_map(|((pattern, action), fields), span| {
            let schedule = resolve_schedule(&fields, span)?;

            Ok(Contact {
                pattern,
                action,
                priority: fields.priority,
                schedule,
                bandwidth_bps: fields.bps,
                delay_us: fields.delay,
            })
        })
        .labelled("contact")
}

/// Resolve schedule from parsed fields. Validates mutual exclusivity of
/// one-shot (start/end) vs recurring (cron/duration/until).
fn resolve_schedule<'a>(
    fields: &ContactFields<'a>,
    span: Span,
) -> Result<Schedule, Rich<'a, char, Span>> {
    let has_oneshot = fields.start.is_some() || fields.end.is_some();
    let has_recurring = fields.cron.is_some() || fields.duration.is_some();

    if has_oneshot && has_recurring {
        return Err(Rich::custom(
            span,
            "cannot mix one-shot (start/end) and recurring (cron/duration) fields",
        ));
    }

    if fields.until.is_some() && !has_recurring {
        return Err(Rich::custom(
            span,
            "'until' requires 'cron' (recurring schedule)",
        ));
    }

    if has_recurring {
        let cron = fields
            .cron
            .ok_or_else(|| Rich::custom(span, "'duration' requires 'cron' expression"))?;
        let duration = fields
            .duration
            .ok_or_else(|| Rich::custom(span, "'cron' requires 'duration' field"))?;
        Ok(Schedule::Recurring {
            cron: cron.to_string(),
            duration,
            until: fields.until,
        })
    } else if has_oneshot {
        // Validate end > start if both present
        if let (Some(start), Some(end)) = (fields.start, fields.end)
            && end <= start
        {
            return Err(Rich::custom(span, "'end' must be after 'start'"));
        }
        Ok(Schedule::OneShot {
            start: fields.start,
            end: fields.end,
        })
    } else {
        Ok(Schedule::Permanent)
    }
}

// ── File structure ──────────────────────────────────────────────────

fn line<'a>() -> impl Parser<'a, &'a str, Option<Contact>, Extra<'a>> {
    text::inline_whitespace()
        .ignore_then(choice((
            just('#')
                .then(any().and_is(just('\n').not()).repeated())
                .ignored()
                .to(None),
            contact().map(Some),
            empty().to(None),
        )))
        .then_ignore(text::inline_whitespace())
}

fn contacts<'a>() -> impl Parser<'a, &'a str, Vec<Contact>, Extra<'a>> {
    line()
        .separated_by(just('\n'))
        .allow_trailing()
        .collect::<Vec<_>>()
        .map(|v| v.into_iter().flatten().collect())
        .then_ignore(end())
}

// ── Duration parsing ────────────────────────────────────────────────

/// Parse a duration string like "90m", "2h", "4h30m", "1h15m30s".
fn parse_duration<'a>(s: &str, span: Span) -> Result<std::time::Duration, Rich<'a, char, Span>> {
    let mut total_secs: u64 = 0;
    let mut current_num = String::new();
    let mut found_unit = false;

    for c in s.chars() {
        if c.is_ascii_digit() {
            current_num.push(c);
        } else {
            if current_num.is_empty() {
                return Err(Rich::custom(
                    span,
                    format!("invalid duration '{s}': expected number before '{c}'"),
                ));
            }
            let n: u64 = current_num
                .parse()
                .map_err(|e| Rich::custom(span, format!("invalid duration '{s}': {e}")))?;
            current_num.clear();
            match c {
                'h' => total_secs += n * 3600,
                'm' => total_secs += n * 60,
                's' => total_secs += n,
                _ => {
                    return Err(Rich::custom(
                        span,
                        format!("invalid duration '{s}': unknown unit '{c}' (expected h/m/s)"),
                    ));
                }
            }
            found_unit = true;
        }
    }

    if !current_num.is_empty() {
        return Err(Rich::custom(
            span,
            format!("invalid duration '{s}': missing unit suffix (expected h/m/s)"),
        ));
    }

    if !found_unit {
        return Err(Rich::custom(span, format!("invalid duration '{s}': empty")));
    }

    if total_secs == 0 {
        return Err(Rich::custom(
            span,
            format!("invalid duration '{s}': must be greater than zero"),
        ));
    }

    Ok(std::time::Duration::from_secs(total_secs))
}

// ── Error formatting ────────────────────────────────────────────────

/// Format a parse error with line number, column, source context, and a caret.
fn format_error(input: &str, error: &Rich<'_, char, Span>) -> String {
    let offset = error.span().start;

    let mut line_num = 1;
    let mut line_start = 0;
    for (i, ch) in input.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line_num += 1;
            line_start = i + 1;
        }
    }
    let col = offset - line_start + 1;

    let line_end = input[line_start..]
        .find('\n')
        .map(|i| line_start + i)
        .unwrap_or(input.len());
    let source_line = &input[line_start..line_end];

    let caret = format!("{:>width$}", "^", width = col);

    format!("line {line_num}: {error}\n  {source_line}\n  {caret}")
}

// ── Public API ──────────────────────────────────────────────────────

/// Parse a contact plan string into a list of contacts.
pub fn parse_contacts(input: &str) -> Result<Vec<Contact>, Vec<String>> {
    contacts()
        .parse(input)
        .into_result()
        .map_err(|errors| errors.iter().map(|e| format_error(input, e)).collect())
}

/// Load and parse a contact plan file.
pub async fn load_contacts(
    path: &PathBuf,
    ignore_errors: bool,
    watching: bool,
) -> anyhow::Result<Vec<Contact>> {
    match tokio::fs::read_to_string(path).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && ignore_errors && watching => {
            debug!("Contact plan file: '{}' not found", path.display());
            Ok(Vec::new())
        }
        Err(e) if ignore_errors => {
            error!("Failed to read contact plan file '{}': {e}", path.display(),);
            Ok(Vec::new())
        }
        Err(e) => Err(anyhow::anyhow!(
            "Failed to read contact plan file '{}': {e}",
            path.display()
        )),
        Ok(input) => match parse_contacts(&input) {
            Err(errors) if ignore_errors => {
                for e in &errors {
                    error!("{}:{e}", path.display());
                }
                Ok(Vec::new())
            }
            Err(errors) => {
                let msg = errors.join("\n");
                Err(anyhow::anyhow!(
                    "Failed to parse '{}':\n{msg}",
                    path.display()
                ))
            }
            Ok(v) => Ok(v),
        },
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn parse_ok(input: &str) -> Vec<Contact> {
        parse_contacts(input).unwrap_or_else(|errors| {
            panic!("Should parse '{input}', got errors:\n{}", errors.join("\n"))
        })
    }

    fn parse_err(input: &str) {
        assert!(
            parse_contacts(input).is_err(),
            "Parsing '{input}' should have failed"
        );
    }

    // ── Static route compatibility ──────────────────────────────────

    #[test]
    fn simple_via() {
        let c = parse_ok("ipn:*.*.* via ipn:0.1.0");
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0].action, Action::Via(_)));
        assert_eq!(c[0].priority, None);
        assert_eq!(c[0].schedule, Schedule::Permanent);
    }

    #[test]
    fn simple_drop() {
        let c = parse_ok("ipn:99.*.* drop");
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0].action, Action::Drop(None)));
        assert_eq!(c[0].schedule, Schedule::Permanent);
    }

    #[test]
    fn drop_with_reason() {
        let c = parse_ok("ipn:99.*.* drop 3");
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0].action, Action::Drop(Some(_))));
    }

    #[test]
    fn via_with_priority() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 priority 10");
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0].action, Action::Via(_)));
        assert_eq!(c[0].priority, Some(10));
        assert_eq!(c[0].schedule, Schedule::Permanent);
    }

    #[test]
    fn reflect_not_supported() {
        parse_err("dtn://**/** reflect");
    }

    // ── One-shot schedule ───────────────────────────────────────────

    #[test]
    fn oneshot_start_end() {
        let c =
            parse_ok("ipn:2.*.* via ipn:2.1.0 start 2026-03-27T08:00:00Z end 2026-03-27T09:30:00Z");
        assert_eq!(c.len(), 1);
        match &c[0].schedule {
            Schedule::OneShot { start, end } => {
                assert!(start.is_some());
                assert!(end.is_some());
            }
            _ => panic!("expected OneShot schedule"),
        }
    }

    #[test]
    fn oneshot_start_only() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 start 2026-03-27T08:00:00Z");
        assert_eq!(c.len(), 1);
        match &c[0].schedule {
            Schedule::OneShot { start, end } => {
                assert!(start.is_some());
                assert!(end.is_none());
            }
            _ => panic!("expected OneShot schedule"),
        }
    }

    #[test]
    fn oneshot_end_only() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 end 2026-03-27T09:30:00Z");
        assert_eq!(c.len(), 1);
        match &c[0].schedule {
            Schedule::OneShot { start, end } => {
                assert!(start.is_none());
                assert!(end.is_some());
            }
            _ => panic!("expected OneShot schedule"),
        }
    }

    #[test]
    fn oneshot_end_before_start() {
        parse_err("ipn:2.*.* via ipn:2.1.0 start 2026-03-27T09:30:00Z end 2026-03-27T08:00:00Z");
    }

    #[test]
    fn oneshot_with_bps() {
        let c = parse_ok(
            "ipn:2.*.* via ipn:2.1.0 start 2026-03-27T08:00:00Z end 2026-03-27T09:30:00Z bps 256000",
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].bandwidth_bps, Some(256000));
    }

    // ── Recurring schedule ──────────────────────────────────────────

    #[test]
    fn recurring_cron_duration() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 90m");
        assert_eq!(c.len(), 1);
        match &c[0].schedule {
            Schedule::Recurring {
                cron,
                duration,
                until,
            } => {
                assert_eq!(cron, "0 8 * * *");
                assert_eq!(*duration, std::time::Duration::from_secs(90 * 60));
                assert!(until.is_none());
            }
            _ => panic!("expected Recurring schedule"),
        }
    }

    #[test]
    fn recurring_with_until() {
        let c = parse_ok(
            "ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 90m until 2026-06-30T00:00:00Z",
        );
        assert_eq!(c.len(), 1);
        match &c[0].schedule {
            Schedule::Recurring { until, .. } => {
                assert!(until.is_some());
            }
            _ => panic!("expected Recurring schedule"),
        }
    }

    #[test]
    fn recurring_with_bps_and_priority() {
        let c = parse_ok(
            "ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 90m bps 256000 priority 10",
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].bandwidth_bps, Some(256000));
        assert_eq!(c[0].priority, Some(10));
    }

    #[test]
    fn cron_without_duration() {
        parse_err("ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\"");
    }

    #[test]
    fn duration_without_cron() {
        parse_err("ipn:2.*.* via ipn:2.1.0 duration 90m");
    }

    #[test]
    fn mixed_oneshot_and_recurring() {
        parse_err(
            "ipn:2.*.* via ipn:2.1.0 start 2026-03-27T08:00:00Z cron \"0 8 * * *\" duration 90m",
        );
    }

    #[test]
    fn until_without_cron() {
        parse_err("ipn:2.*.* via ipn:2.1.0 until 2026-06-30T00:00:00Z");
    }

    // ── Duration parsing ────────────────────────────────────────────

    #[test]
    fn duration_minutes() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 90m");
        match &c[0].schedule {
            Schedule::Recurring { duration, .. } => {
                assert_eq!(*duration, std::time::Duration::from_secs(5400));
            }
            _ => panic!("expected Recurring"),
        }
    }

    #[test]
    fn duration_hours() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 2h");
        match &c[0].schedule {
            Schedule::Recurring { duration, .. } => {
                assert_eq!(*duration, std::time::Duration::from_secs(7200));
            }
            _ => panic!("expected Recurring"),
        }
    }

    #[test]
    fn duration_compound() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 4h30m");
        match &c[0].schedule {
            Schedule::Recurring { duration, .. } => {
                assert_eq!(
                    *duration,
                    std::time::Duration::from_secs(4 * 3600 + 30 * 60)
                );
            }
            _ => panic!("expected Recurring"),
        }
    }

    #[test]
    fn duration_hms() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 1h15m30s");
        match &c[0].schedule {
            Schedule::Recurring { duration, .. } => {
                assert_eq!(
                    *duration,
                    std::time::Duration::from_secs(3600 + 15 * 60 + 30)
                );
            }
            _ => panic!("expected Recurring"),
        }
    }

    #[test]
    fn duration_invalid_no_unit() {
        parse_err("ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 90");
    }

    #[test]
    fn duration_zero() {
        parse_err("ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 0m");
    }

    // ── Link properties ─────────────────────────────────────────────

    #[test]
    fn bps_field() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bps 1000000");
        assert_eq!(c[0].bandwidth_bps, Some(1000000));
    }

    #[test]
    fn delay_field() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 delay 500000");
        assert_eq!(c[0].delay_us, Some(500000));
    }

    #[test]
    fn all_link_properties() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bps 256000 delay 500000 priority 10");
        assert_eq!(c[0].bandwidth_bps, Some(256000));
        assert_eq!(c[0].delay_us, Some(500000));
        assert_eq!(c[0].priority, Some(10));
    }

    // ── Field ordering ──────────────────────────────────────────────

    #[test]
    fn fields_any_order() {
        // priority before schedule
        let c = parse_ok(
            "ipn:2.*.* via ipn:2.1.0 priority 10 start 2026-03-27T08:00:00Z end 2026-03-27T09:30:00Z",
        );
        assert_eq!(c[0].priority, Some(10));
        assert!(matches!(c[0].schedule, Schedule::OneShot { .. }));

        // bps before cron
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bps 256000 cron \"0 8 * * *\" duration 90m");
        assert_eq!(c[0].bandwidth_bps, Some(256000));
        assert!(matches!(c[0].schedule, Schedule::Recurring { .. }));
    }

    // ── Comments and blank lines ────────────────────────────────────

    #[test]
    fn comments() {
        parse_ok("#");
        parse_ok("#\n");
        parse_ok("# This is a comment");
        parse_ok("# Single Mars relay pass");
    }

    #[test]
    fn blank_lines() {
        parse_ok("");
        parse_ok("\n");
        parse_ok("      ");
        parse_ok("      \n");
        parse_ok("   \n   \n   ");
    }

    // ── Multiple contacts ───────────────────────────────────────────

    #[test]
    fn multiple_contacts() {
        let c = parse_ok(
            "ipn:2.*.* via ipn:2.1.0 priority 10\nipn:3.*.* via ipn:3.1.0\nipn:99.*.* drop",
        );
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn mixed_with_comments_and_blanks() {
        let input = "\
# Mars relay
ipn:2.*.* via ipn:2.1.0 start 2026-03-27T08:00:00Z end 2026-03-27T09:30:00Z bps 256000

# Daily pass
ipn:4.*.* via ipn:4.1.0 cron \"*/93 * * * *\" duration 12m bps 1000000

# Permanent ground link
ipn:3.*.* via ipn:3.1.0 priority 10
";
        let c = parse_ok(input);
        assert_eq!(c.len(), 3);
    }

    // ── Duplicate fields ────────────────────────────────────────────

    #[test]
    fn duplicate_priority() {
        parse_err("ipn:2.*.* via ipn:2.1.0 priority 10 priority 20");
    }

    #[test]
    fn duplicate_start() {
        parse_err("ipn:2.*.* via ipn:2.1.0 start 2026-03-27T08:00:00Z start 2026-03-27T09:00:00Z");
    }

    // ── Scheduled drop ──────────────────────────────────────────────

    #[test]
    fn scheduled_drop() {
        let c = parse_ok("ipn:6.*.* drop cron \"0 2 * * 0\" duration 4h priority 0");
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0].action, Action::Drop(None)));
        assert_eq!(c[0].priority, Some(0));
        assert!(matches!(c[0].schedule, Schedule::Recurring { .. }));
    }

    #[test]
    fn drop_with_reason_and_schedule() {
        let c = parse_ok(
            "ipn:7.*.* drop 3 start 2026-04-01T00:00:00Z end 2026-04-02T00:00:00Z priority 0",
        );
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0].action, Action::Drop(Some(_))));
        assert!(matches!(c[0].schedule, Schedule::OneShot { .. }));
    }

    // ── Invalid inputs ──────────────────────────────────────────────

    #[test]
    fn invalid_inputs() {
        parse_err("Broken");
        parse_err("ipn:*.*.* Broken");
        parse_err("ipn:*.*.* via Broken");
    }

    // ── Error formatting ────────────────────────────────────────────

    #[test]
    fn error_messages_are_useful() {
        let input = "ipn:*.*.* Broken";
        let errors = parse_contacts(input).unwrap_err();

        assert!(
            errors[0].contains("line 1"),
            "Should include line number, got: {}",
            errors[0]
        );
        assert!(
            errors[0].contains("Broken"),
            "Should include source context, got: {}",
            errors[0]
        );
        assert!(
            errors[0].contains('^'),
            "Should include caret indicator, got: {}",
            errors[0]
        );
    }

    #[test]
    fn multiline_error_shows_correct_line() {
        let input = "ipn:*.*.* via ipn:0.1.0\nBroken line here\nipn:2.*.* drop";
        let errors = parse_contacts(input).unwrap_err();

        assert!(
            errors[0].contains("line 2"),
            "Should point to line 2, got: {}",
            errors[0]
        );
        assert!(
            errors[0].contains("Broken line here"),
            "Should show the offending line, got: {}",
            errors[0]
        );
    }
}

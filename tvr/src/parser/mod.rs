use std::{path::PathBuf, time::Duration};

use chumsky::{input::Emitter, prelude::*};
use hardy_bpa::routing::RouteAction;
use hardy_eid_patterns::EidPattern;
use time::OffsetDateTime;
use tracing::{debug, error};

use self::value::validated;
use crate::{
    contacts::{Contact, Schedule},
    cron::CronExpr,
};

mod value;

type Span = SimpleSpan<usize>;
type Extra<'a> = extra::Err<Rich<'a, char, Span>>;

// ── Shared primitives ───────────────────────────────────────────────

fn keyword<'a>(word: &'a str) -> impl Parser<'a, &'a str, (), Extra<'a>> {
    any()
        .filter(|c: &char| c.is_ascii_alphabetic())
        .repeated()
        .exactly(word.len())
        .to_slice()
        .try_map(move |s: &str, span| {
            if s.eq_ignore_ascii_case(word) {
                Ok(())
            } else {
                Err(Rich::custom(span, format!("expected '{word}'")))
            }
        })
}

// A non-whitespace token (used for EIDs, patterns, etc.)
fn non_ws_token<'a>() -> impl Parser<'a, &'a str, &'a str, Extra<'a>> {
    any()
        .filter(|c: &char| !c.is_whitespace())
        .repeated()
        .at_least(1)
        .to_slice()
}

// ── Pattern ─────────────────────────────────────────────────────────

fn pattern<'a>() -> impl Parser<'a, &'a str, Option<EidPattern>, Extra<'a>> {
    validated(non_ws_token(), value::eid_pattern).labelled("EID pattern")
}

// ── Actions (via / drop — no reflect) ───────────────────────────────

fn drop_action<'a>() -> impl Parser<'a, &'a str, RouteAction, Extra<'a>> {
    keyword("drop")
        .then(
            text::inline_whitespace()
                .at_least(1)
                .ignore_then(validated(text::int(10), value::reason_code))
                .or_not()
                .map(Option::flatten),
        )
        .map(|(_, reason)| RouteAction::Drop(reason))
        .labelled("drop action")
}

// The next hop is `None` when its EID was refused (the error is emitted).
fn via_action<'a>() -> impl Parser<'a, &'a str, Option<RouteAction>, Extra<'a>> {
    keyword("via")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(validated(non_ws_token(), value::next_hop).labelled("next-hop EID"))
        .map(|eid| eid.map(RouteAction::Via))
        .labelled("via action")
}

fn action<'a>() -> impl Parser<'a, &'a str, Option<RouteAction>, Extra<'a>> {
    choice((drop_action().map(Some), via_action())).labelled("action")
}

// ── Named fields (keyword-value pairs) ──────────────────────────────

// Each field parser yields `None` for a value it refused; the refusal has
// already been emitted with the field's own message.

fn priority_field<'a>() -> impl Parser<'a, &'a str, Option<u32>, Extra<'a>> {
    keyword("priority")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(validated(text::int(10), value::priority).labelled("priority value"))
        .labelled("priority")
}

// RFC 3339 timestamp (e.g. "2026-03-27T08:00:00Z"), converted to UTC.
fn rfc3339_timestamp<'a>() -> impl Parser<'a, &'a str, Option<OffsetDateTime>, Extra<'a>> {
    validated(non_ws_token(), value::timestamp).labelled("RFC 3339 timestamp")
}

fn start_field<'a>() -> impl Parser<'a, &'a str, Option<OffsetDateTime>, Extra<'a>> {
    keyword("start")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(rfc3339_timestamp())
        .labelled("start time")
}

fn end_field<'a>() -> impl Parser<'a, &'a str, Option<OffsetDateTime>, Extra<'a>> {
    keyword("end")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(rfc3339_timestamp())
        .labelled("end time")
}

fn until_field<'a>() -> impl Parser<'a, &'a str, Option<OffsetDateTime>, Extra<'a>> {
    keyword("until")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(rfc3339_timestamp())
        .labelled("until time")
}

// Quoted string (for cron expressions): `"0 8 * * *"`
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

fn cron_field<'a>() -> impl Parser<'a, &'a str, Option<CronExpr>, Extra<'a>> {
    keyword("cron")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(validated(quoted_string(), value::cron).labelled("cron expression"))
        .labelled("cron")
}

// Duration value: e.g. "90m", "2h", "4h30m", "1h15m30s"
fn duration_value<'a>() -> impl Parser<'a, &'a str, Option<Duration>, Extra<'a>> {
    validated(non_ws_token(), value::duration).labelled("duration")
}

fn duration_field<'a>() -> impl Parser<'a, &'a str, Option<Duration>, Extra<'a>> {
    keyword("duration")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(duration_value())
        .labelled("duration field")
}

fn bandwidth_field<'a>() -> impl Parser<'a, &'a str, Option<u64>, Extra<'a>> {
    keyword("bandwidth")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(validated(non_ws_token(), value::bandwidth).labelled("bandwidth value"))
        .labelled("bandwidth")
}

fn delay_field<'a>() -> impl Parser<'a, &'a str, Option<u32>, Extra<'a>> {
    keyword("delay")
        .then(text::inline_whitespace().at_least(1))
        .ignore_then(validated(non_ws_token(), value::delay).labelled("delay value"))
        .labelled("delay")
}

// ── Contact fields (order-independent keyword fields) ───────────────

// All optional fields that can appear after the action, in any order.
// `refused` is set when a field's value was refused (and its error emitted).
#[derive(Default)]
struct ContactFields {
    refused: bool,
    priority: Option<u32>,
    start: Option<OffsetDateTime>,
    end: Option<OffsetDateTime>,
    cron: Option<CronExpr>,
    duration: Option<Duration>,
    until: Option<OffsetDateTime>,
    bps: Option<u64>,
    delay: Option<u32>,
}

// A single optional field, parsed one at a time and accumulated into ContactFields.
enum Field {
    Priority(u32),
    Start(OffsetDateTime),
    End(OffsetDateTime),
    Cron(CronExpr),
    Duration(Duration),
    Until(OffsetDateTime),
    Bps(u64),
    Delay(u32),
}

// A field and where it sits on the line; `None` when its value was refused.
fn field<'a>() -> impl Parser<'a, &'a str, (Option<Field>, Span), Extra<'a>> {
    choice((
        priority_field().map(|v| v.map(Field::Priority)),
        start_field().map(|v| v.map(Field::Start)),
        end_field().map(|v| v.map(Field::End)),
        cron_field().map(|v| v.map(Field::Cron)),
        duration_field().map(|v| v.map(Field::Duration)),
        until_field().map(|v| v.map(Field::Until)),
        bandwidth_field().map(|v| v.map(Field::Bps)),
        delay_field().map(|v| v.map(Field::Delay)),
    ))
    .map_with(|field, extra| (field, extra.span()))
    .labelled("field")
}

// Stores one field's value, emitting an error at the field when it repeats
// an earlier one.
fn set_once<T>(
    slot: &mut Option<T>,
    value: T,
    name: &str,
    span: Span,
    emitter: &mut Emitter<Rich<'_, char, Span>>,
) {
    if slot.is_some() {
        emitter.emit(Rich::custom(span, format!("duplicate '{name}' field")));
    }
    *slot = Some(value);
}

fn contact_fields<'a>() -> impl Parser<'a, &'a str, ContactFields, Extra<'a>> {
    text::inline_whitespace()
        .at_least(1)
        .ignore_then(field())
        .repeated()
        .collect::<Vec<_>>()
        .validate(|fields, _, emitter| {
            let mut cf = ContactFields::default();
            for (field, span) in fields {
                let Some(field) = field else {
                    cf.refused = true;
                    continue;
                };
                match field {
                    Field::Priority(v) => set_once(&mut cf.priority, v, "priority", span, emitter),
                    Field::Start(v) => set_once(&mut cf.start, v, "start", span, emitter),
                    Field::End(v) => set_once(&mut cf.end, v, "end", span, emitter),
                    Field::Cron(v) => set_once(&mut cf.cron, v, "cron", span, emitter),
                    Field::Duration(v) => set_once(&mut cf.duration, v, "duration", span, emitter),
                    Field::Until(v) => set_once(&mut cf.until, v, "until", span, emitter),
                    Field::Bps(v) => set_once(&mut cf.bps, v, "bandwidth", span, emitter),
                    Field::Delay(v) => set_once(&mut cf.delay, v, "delay", span, emitter),
                }
            }
            cf
        })
}

// ── Contact line ────────────────────────────────────────────────────

// A contact line, or `None` when any part of it was refused; every refusal
// has been emitted, so the plan as a whole is refused.
fn contact<'a>() -> impl Parser<'a, &'a str, Option<Contact>, Extra<'a>> {
    pattern()
        .then(
            text::inline_whitespace()
                .at_least(1)
                .ignore_then(action())
                .labelled("action"),
        )
        .then(contact_fields())
        .validate(|((pattern, action), fields), extra, emitter| {
            // A refused field already has its error; checking the schedule
            // without it would only report the gap it left.
            if fields.refused {
                return None;
            }
            let schedule = resolve_schedule(&fields, extra.span())
                .map_err(|e| emitter.emit(e))
                .ok()?;
            Some(Contact {
                pattern: pattern?,
                action: action?,
                priority: fields.priority,
                schedule,
                bandwidth_bps: fields.bps,
                delay_us: fields.delay,
            })
        })
        .labelled("contact")
}

// Resolve schedule from parsed fields. Validates mutual exclusivity of
// one-shot (start/end) vs recurring (cron/duration/until).
fn resolve_schedule<'a>(
    fields: &ContactFields,
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
            .clone()
            .ok_or_else(|| Rich::custom(span, "'duration' requires 'cron' expression"))?;
        let duration = fields
            .duration
            .ok_or_else(|| Rich::custom(span, "'cron' requires 'duration' field"))?;
        Ok(Schedule::Recurring {
            cron,
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
                .then(any().and_is(text::newline().not()).repeated())
                .ignored()
                .to(None),
            contact(),
            empty().to(None),
        )))
        .then_ignore(text::inline_whitespace())
}

fn contacts<'a>() -> impl Parser<'a, &'a str, Vec<Contact>, Extra<'a>> {
    line()
        .separated_by(text::newline())
        .allow_trailing()
        .collect::<Vec<_>>()
        .map(|v| v.into_iter().flatten().collect())
        .then_ignore(end())
}

// ── Error formatting ────────────────────────────────────────────────

// Format a parse error with line number, column, source context, and a caret.
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

// Parse a contact plan string into a list of contacts.
pub fn parse_contacts(input: &str) -> Result<Vec<Contact>, Vec<String>> {
    contacts()
        .parse(input)
        .into_result()
        .map_err(|errors| errors.iter().map(|e| format_error(input, e)).collect())
}

// Load and parse a contact plan file.
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

    // The single diagnostic for a one-line plan: its message, and the
    // column (from 1) the caret points at. The diagnostic ends with the source
    // line and a caret line, so those are read from the end: a message can
    // span several lines of its own.
    fn only_error(input: &str) -> (String, usize) {
        let errors = parse_contacts(input).unwrap_err();
        assert_eq!(
            errors.len(),
            1,
            "expected one error for {input:?}, got {errors:#?}"
        );
        let (body, caret) = errors[0].rsplit_once('\n').unwrap();
        let (message, _source) = body.rsplit_once('\n').unwrap();
        let message = message.strip_prefix("line 1: ").unwrap().to_string();
        (message, caret.len() - "  ".len())
    }

    // The column (from 1) where `needle` starts in `input`.
    fn column_of(input: &str, needle: &str) -> usize {
        input.find(needle).unwrap() + 1
    }

    // ── Static route compatibility ──────────────────────────────────

    #[test]
    fn simple_via() {
        let c = parse_ok("ipn:*.*.* via ipn:0.1.0");
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0].action, RouteAction::Via(_)));
        assert_eq!(c[0].priority, None);
        assert_eq!(c[0].schedule, Schedule::Permanent);
    }

    #[test]
    fn simple_drop() {
        let c = parse_ok("ipn:99.*.* drop");
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0].action, RouteAction::Drop(None)));
        assert_eq!(c[0].schedule, Schedule::Permanent);
    }

    #[test]
    fn drop_with_reason() {
        let c = parse_ok("ipn:99.*.* drop 3");
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0].action, RouteAction::Drop(Some(_))));
    }

    #[test]
    fn via_with_priority() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 priority 10");
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0].action, RouteAction::Via(_)));
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
            "ipn:2.*.* via ipn:2.1.0 start 2026-03-27T08:00:00Z end 2026-03-27T09:30:00Z bandwidth 256K",
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
                assert_eq!(cron.to_string(), "0 8 * * *");
                assert_eq!(*duration, Duration::from_secs(90 * 60));
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
            "ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 90m bandwidth 256K priority 10",
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].bandwidth_bps, Some(256000));
        assert_eq!(c[0].priority, Some(10));
    }

    #[test]
    fn invalid_cron_expression() {
        parse_err("ipn:2.*.* via ipn:2.1.0 cron \"0 8 *\" duration 90m");
        parse_err("ipn:2.*.* via ipn:2.1.0 cron \"60 8 * * *\" duration 90m");
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
                assert_eq!(*duration, Duration::from_secs(5400));
            }
            _ => panic!("expected Recurring"),
        }
    }

    #[test]
    fn duration_hours() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 2h");
        match &c[0].schedule {
            Schedule::Recurring { duration, .. } => {
                assert_eq!(*duration, Duration::from_secs(7200));
            }
            _ => panic!("expected Recurring"),
        }
    }

    #[test]
    fn duration_compound() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 4h30m");
        match &c[0].schedule {
            Schedule::Recurring { duration, .. } => {
                assert_eq!(*duration, Duration::from_secs(4 * 3600 + 30 * 60));
            }
            _ => panic!("expected Recurring"),
        }
    }

    #[test]
    fn duration_hms() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 1h15m30s");
        match &c[0].schedule {
            Schedule::Recurring { duration, .. } => {
                assert_eq!(*duration, Duration::from_secs(3600 + 15 * 60 + 30));
            }
            _ => panic!("expected Recurring"),
        }
    }

    #[test]
    fn duration_invalid_no_unit() {
        parse_err("ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 90");
    }

    // A zero duration is reported as such, at the value, with no follow-on
    // complaint that the cron has no duration.
    #[test]
    fn duration_zero() {
        let input = "ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 0m";
        let (message, column) = only_error(input);
        assert_eq!(message, value::ValueError::ZeroDuration.to_string());
        assert_eq!(column, column_of(input, "0m"));
    }

    // ── Link properties ─────────────────────────────────────────────

    #[test]
    fn bandwidth_bare_number() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bandwidth 9600");
        assert_eq!(c[0].bandwidth_bps, Some(9600));
    }

    #[test]
    fn bandwidth_si_suffixes() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bandwidth 256K");
        assert_eq!(c[0].bandwidth_bps, Some(256_000));

        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bandwidth 1M");
        assert_eq!(c[0].bandwidth_bps, Some(1_000_000));

        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bandwidth 10G");
        assert_eq!(c[0].bandwidth_bps, Some(10_000_000_000));

        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bandwidth 1T");
        assert_eq!(c[0].bandwidth_bps, Some(1_000_000_000_000));
    }

    #[test]
    fn bandwidth_long_suffixes() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bandwidth 256Kbps");
        assert_eq!(c[0].bandwidth_bps, Some(256_000));

        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bandwidth 10Gbps");
        assert_eq!(c[0].bandwidth_bps, Some(10_000_000_000));

        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bandwidth 100bps");
        assert_eq!(c[0].bandwidth_bps, Some(100));
    }

    #[test]
    fn bandwidth_case_insensitive() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bandwidth 10g");
        assert_eq!(c[0].bandwidth_bps, Some(10_000_000_000));

        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bandwidth 256kbps");
        assert_eq!(c[0].bandwidth_bps, Some(256_000));
    }

    #[test]
    fn delay_duration_units() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 delay 500ms");
        assert_eq!(c[0].delay_us, Some(500_000));

        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 delay 1s");
        assert_eq!(c[0].delay_us, Some(1_000_000));

        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 delay 250us");
        assert_eq!(c[0].delay_us, Some(250));
    }

    #[test]
    fn all_link_properties() {
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bandwidth 256K delay 500ms priority 10");
        assert_eq!(c[0].bandwidth_bps, Some(256_000));
        assert_eq!(c[0].delay_us, Some(500_000));
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

        // bandwidth before cron
        let c = parse_ok("ipn:2.*.* via ipn:2.1.0 bandwidth 256K cron \"0 8 * * *\" duration 90m");
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
ipn:2.*.* via ipn:2.1.0 start 2026-03-27T08:00:00Z end 2026-03-27T09:30:00Z bandwidth 256K

# Daily pass
ipn:4.*.* via ipn:4.1.0 cron \"*/93 * * * *\" duration 12m bandwidth 1M

# Permanent ground link
ipn:3.*.* via ipn:3.1.0 priority 10
";
        let c = parse_ok(input);
        assert_eq!(c.len(), 3);
    }

    // ── Duplicate fields ────────────────────────────────────────────

    #[test]
    fn duplicate_priority() {
        let input = "ipn:2.*.* via ipn:2.1.0 priority 10 priority 20";
        let (message, column) = only_error(input);
        assert_eq!(message, "duplicate 'priority' field");
        assert_eq!(column, column_of(input, "priority 20"));
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
        assert!(matches!(c[0].action, RouteAction::Drop(None)));
        assert_eq!(c[0].priority, Some(0));
        assert!(matches!(c[0].schedule, Schedule::Recurring { .. }));
    }

    #[test]
    fn drop_with_reason_and_schedule() {
        let c = parse_ok(
            "ipn:7.*.* drop 3 start 2026-04-01T00:00:00Z end 2026-04-02T00:00:00Z priority 0",
        );
        assert_eq!(c.len(), 1);
        assert!(matches!(c[0].action, RouteAction::Drop(Some(_))));
        assert!(matches!(c[0].schedule, Schedule::OneShot { .. }));
    }

    // ── Invalid inputs ──────────────────────────────────────────────

    #[test]
    fn invalid_inputs() {
        parse_err("Broken");
        parse_err("ipn:*.*.* Broken");
        parse_err("ipn:*.*.* via Broken");
    }

    // ── Field diagnostics ───────────────────────────────────────────

    // Each refused value is reported with its field's own message, with the
    // caret on the value rather than at the start of the line.
    #[test]
    fn refused_values_report_their_own_message() {
        let cases = [
            (
                "notapattern via ipn:2.1.0",
                "notapattern",
                value::eid_pattern("notapattern").err(),
            ),
            (
                "ipn:2.*.* via notaneid",
                "notaneid",
                value::next_hop("notaneid").err(),
            ),
            ("ipn:2.*.* drop 255", "255", value::reason_code("255").err()),
            (
                "ipn:2.*.* via ipn:2.1.0 priority 99999999999",
                "99999999999",
                value::priority("99999999999").err(),
            ),
            (
                "ipn:2.*.* via ipn:2.1.0 start notatime",
                "notatime",
                value::timestamp("notatime").err(),
            ),
            (
                "ipn:2.*.* via ipn:2.1.0 cron \"bogus\" duration 1h",
                "\"bogus\"",
                value::cron("bogus").err(),
            ),
            (
                "ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\" duration 1month",
                "1month",
                value::duration("1month").err(),
            ),
            (
                "ipn:2.*.* via ipn:2.1.0 bandwidth 10X",
                "10X",
                value::bandwidth("10X").err(),
            ),
            (
                "ipn:2.*.* via ipn:2.1.0 delay 30",
                "30",
                value::delay("30").err(),
            ),
            (
                "ipn:2.*.* via ipn:2.1.0 delay 2h",
                "2h",
                value::delay("2h").err(),
            ),
        ];
        for (input, token, expected) in cases {
            let (message, column) = only_error(input);
            assert_eq!(message, expected.unwrap().to_string(), "{input:?}");
            assert_eq!(column, column_of(input, token), "{input:?}");
        }
    }

    // Every refused value in a plan is reported, across fields and lines.
    #[test]
    fn every_refused_value_is_reported() {
        let input = "ipn:2.*.* via ipn:2.1.0 bandwidth 10X delay 30\nipn:3.*.* drop 255";
        let errors = parse_contacts(input).unwrap_err();
        let firsts: Vec<_> = errors.iter().map(|e| e.lines().next().unwrap()).collect();
        assert_eq!(
            firsts,
            [
                format!("line 1: {}", value::bandwidth("10X").unwrap_err()),
                format!("line 1: {}", value::delay("30").unwrap_err()),
                format!("line 2: {}", value::reason_code("255").unwrap_err()),
            ]
        );
    }

    // A schedule rule broken by values that all parsed is still reported.
    #[test]
    fn schedule_rules_are_reported() {
        let (message, _) = only_error("ipn:2.*.* via ipn:2.1.0 cron \"0 8 * * *\"");
        assert_eq!(message, "'cron' requires 'duration' field");
        let (message, _) = only_error(
            "ipn:2.*.* via ipn:2.1.0 start 2026-03-27T09:30:00Z end 2026-03-27T08:00:00Z",
        );
        assert_eq!(message, "'end' must be after 'start'");
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

use super::*;
use chumsky::prelude::*;

type Span = SimpleSpan<usize>;
type Extra<'a> = extra::Err<Rich<'a, char, Span>>;

fn pattern<'a>() -> impl Parser<'a, &'a str, eid_patterns::EidPattern, Extra<'a>> {
    any()
        .filter(|c: &char| !c.is_whitespace())
        .repeated()
        .at_least(1)
        .to_slice()
        .try_map(|s: &str, span| {
            s.parse()
                .map_err(|e| Rich::custom(span, format!("invalid EID pattern: {e}")))
        })
        .labelled("EID pattern")
}

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
            any()
                .filter(|c: &char| !c.is_whitespace())
                .repeated()
                .at_least(1)
                .to_slice()
                .try_map(|s: &str, span| {
                    s.parse()
                        .map_err(|e| Rich::custom(span, format!("invalid next-hop EID: {e}")))
                })
                .labelled("next-hop EID"),
        )
        .map(Action::Via)
        .labelled("via action")
}

fn reflect_action<'a>() -> impl Parser<'a, &'a str, Action, Extra<'a>> {
    keyword("reflect")
        .to(Action::Reflect)
        .labelled("reflect action")
}

fn action<'a>() -> impl Parser<'a, &'a str, Action, Extra<'a>> {
    choice((drop_action(), via_action(), reflect_action())).labelled("action")
}

fn priority<'a>() -> impl Parser<'a, &'a str, u32, Extra<'a>> {
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

fn route<'a>() -> impl Parser<'a, &'a str, StaticRoute, Extra<'a>> {
    pattern()
        .then(
            text::inline_whitespace()
                .at_least(1)
                .ignore_then(action())
                .labelled("action"),
        )
        .then(
            text::inline_whitespace()
                .at_least(1)
                .ignore_then(priority())
                .or_not(),
        )
        .map(|((pattern, action), priority)| StaticRoute {
            pattern,
            action,
            priority,
        })
        .labelled("route")
}

fn line<'a>() -> impl Parser<'a, &'a str, Option<StaticRoute>, Extra<'a>> {
    text::inline_whitespace()
        .ignore_then(choice((
            just('#')
                .then(any().and_is(text::newline().not()).repeated())
                .ignored()
                .to(None),
            route().map(Some),
            empty().to(None),
        )))
        .then_ignore(text::inline_whitespace())
}

fn routes<'a>() -> impl Parser<'a, &'a str, Vec<StaticRoute>, Extra<'a>> {
    line()
        .separated_by(text::newline())
        .allow_trailing()
        .collect::<Vec<_>>()
        .map(|v| v.into_iter().flatten().collect())
        .then_ignore(end())
}

/// Format a parse error with line number, column, source context, and a caret.
///
/// Example output:
/// ```text
/// line 3: expected action
///   ipn:*.*.* Broken
///             ^
/// ```
fn format_error(input: &str, error: &Rich<'_, char, Span>) -> String {
    let offset = error.span().start;

    // Compute line number (1-based) and column (1-based)
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

    // Extract the source line
    let line_end = input[line_start..]
        .find('\n')
        .map(|i| line_start + i)
        .unwrap_or(input.len());
    let source_line = &input[line_start..line_end];

    // Build the caret indicator
    let caret = format!("{:>width$}", "^", width = col);

    format!("line {line_num}: {error}\n  {source_line}\n  {caret}")
}

pub async fn load_routes(
    routes_file: &PathBuf,
    ignore_errors: bool,
    watching: bool,
) -> anyhow::Result<Vec<StaticRoute>> {
    match tokio::fs::read_to_string(routes_file).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && ignore_errors && watching => {
            debug!("Static routes file: '{}' not found", routes_file.display());
            Ok(Vec::new())
        }
        Err(e) if ignore_errors => {
            error!(
                "Failed to read from static routes file '{}': {e}",
                routes_file.display(),
            );
            Ok(Vec::new())
        }
        Err(e) => Err(anyhow::anyhow!(
            "Failed to read from static routes file '{}': {e}",
            routes_file.display()
        )),
        Ok(input) => match routes().parse(&input).into_result() {
            Err(errors) if ignore_errors => {
                for e in &errors {
                    error!(
                        "{}:{}\n{}",
                        routes_file.display(),
                        format_error(&input, e),
                        ""
                    );
                }
                Ok(Vec::new())
            }
            Err(errors) => {
                let msg = errors
                    .iter()
                    .map(|e| format_error(&input, e))
                    .collect::<Vec<_>>()
                    .join("\n");
                Err(anyhow::anyhow!(
                    "Failed to parse '{}':\n{msg}",
                    routes_file.display()
                ))
            }
            Ok(v) => Ok(v),
        },
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn parse_ok(input: &str) -> Vec<StaticRoute> {
        routes()
            .parse(input)
            .into_result()
            .unwrap_or_else(|errors| {
                panic!(
                    "Should parse '{input}', got errors: {:?}",
                    errors.iter().map(|e| format!("{e}")).collect::<Vec<_>>()
                )
            })
    }

    fn parse_err(input: &str) {
        assert!(
            routes().parse(input).into_result().is_err(),
            "Parsing '{input}' should have failed"
        );
    }

    #[test]
    fn simple_route() {
        let routes = parse_ok("ipn:*.*.* via ipn:0.1.0");
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0].action, Action::Via(_)));
        assert_eq!(routes[0].priority, None);
    }

    #[test]
    fn route_with_priority() {
        let routes = parse_ok("dtn://**/** reflect priority 1200");
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0].action, Action::Reflect));
        assert_eq!(routes[0].priority, Some(1200));
    }

    #[test]
    fn drop_action() {
        let routes = parse_ok("ipn:99.*.* drop");
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0].action, Action::Drop(None)));
    }

    #[test]
    fn drop_with_reason() {
        let routes = parse_ok("ipn:99.*.* drop 3");
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0].action, Action::Drop(Some(_))));
    }

    #[test]
    fn drop_with_priority() {
        let routes = parse_ok("ipn:99.*.* drop priority 5");
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0].action, Action::Drop(None)));
        assert_eq!(routes[0].priority, Some(5));
    }

    #[test]
    fn drop_with_reason_and_priority() {
        let routes = parse_ok("ipn:99.*.* drop 3 priority 5");
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0].action, Action::Drop(Some(_))));
        assert_eq!(routes[0].priority, Some(5));
    }

    #[test]
    fn via_with_priority() {
        let routes = parse_ok("ipn:*.*.* via ipn:0.1.0 priority 42");
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0].action, Action::Via(_)));
        assert_eq!(routes[0].priority, Some(42));
    }

    #[test]
    fn case_insensitive_keywords() {
        // All-caps
        let routes = parse_ok("ipn:*.*.* VIA ipn:0.1.0");
        assert!(matches!(routes[0].action, Action::Via(_)));

        let routes = parse_ok("ipn:99.*.* DROP");
        assert!(matches!(routes[0].action, Action::Drop(None)));

        let routes = parse_ok("dtn://**/** REFLECT");
        assert!(matches!(routes[0].action, Action::Reflect));

        let routes = parse_ok("ipn:*.*.* via ipn:0.1.0 PRIORITY 42");
        assert_eq!(routes[0].priority, Some(42));

        // Mixed case
        let routes = parse_ok("ipn:*.*.* Via ipn:0.1.0");
        assert!(matches!(routes[0].action, Action::Via(_)));

        let routes = parse_ok("ipn:99.*.* Drop 3 Priority 5");
        assert!(matches!(routes[0].action, Action::Drop(Some(_))));
        assert_eq!(routes[0].priority, Some(5));
    }

    #[test]
    fn invalid_inputs() {
        parse_err("Broken");
        parse_err("ipn:*.*.* Broken");
        parse_err("ipn:*.*.* via Broken");
    }

    #[test]
    fn comments() {
        parse_ok("#");
        parse_ok("#\n");
        parse_ok("#      ");
        parse_ok("#      \n");
    }

    #[test]
    fn blank_lines() {
        parse_ok("");
        parse_ok("\n");
        parse_ok("      ");
        parse_ok("      \n");
        parse_ok("   \n   \n   ");
    }

    #[test]
    fn crlf_line_endings() {
        let routes = parse_ok("ipn:*.*.* via ipn:0.1.0\r\ndtn://**/** reflect priority 1200");
        assert_eq!(routes.len(), 2);

        parse_ok("# comment\r\nipn:99.*.* drop");
        parse_ok("\r\n\r\n");
    }

    #[test]
    fn multiple_routes() {
        let routes = parse_ok("ipn:*.*.* via ipn:0.1.0\ndtn://**/** reflect priority 1200");
        assert_eq!(routes.len(), 2);
    }

    #[test]
    fn error_messages_are_useful() {
        let input = "ipn:*.*.* Broken";
        let result = routes().parse(input).into_result();
        let errors = result.unwrap_err();
        let formatted = format_error(input, &errors[0]);

        // Should include line number, source context, and caret
        assert!(
            formatted.contains("line 1"),
            "Should include line number, got: {formatted}"
        );
        assert!(
            formatted.contains("Broken"),
            "Should include source context, got: {formatted}"
        );
        assert!(
            formatted.contains('^'),
            "Should include caret indicator, got: {formatted}"
        );
    }

    #[test]
    fn multiline_error_shows_correct_line() {
        let input = "ipn:*.*.* via ipn:0.1.0\nBroken line here\nipn:2.*.* drop";
        let result = routes().parse(input).into_result();
        let errors = result.unwrap_err();
        let formatted = format_error(input, &errors[0]);

        assert!(
            formatted.contains("line 2"),
            "Should point to line 2, got: {formatted}"
        );
        assert!(
            formatted.contains("Broken line here"),
            "Should show the offending line, got: {formatted}"
        );
    }
}

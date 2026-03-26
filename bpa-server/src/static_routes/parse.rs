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
    just(word).ignored()
}

fn drop_action<'a>() -> impl Parser<'a, &'a str, Action, Extra<'a>> {
    keyword("drop")
        .then(
            any()
                .filter(|c: &char| c.is_ascii_digit())
                .repeated()
                .at_least(1)
                .to_slice()
                .try_map(|s: &str, span| {
                    let code: u64 = s
                        .parse()
                        .map_err(|e| Rich::custom(span, format!("invalid reason code: {e}")))?;
                    code.try_into()
                        .map_err(|e| Rich::custom(span, format!("invalid reason code: {e}")))
                })
                .padded_by(inline_whitespace())
                .or_not(),
        )
        .map(|(_, reason)| Action::Drop(reason))
        .labelled("drop action")
}

fn via_action<'a>() -> impl Parser<'a, &'a str, Action, Extra<'a>> {
    keyword("via")
        .then(required_whitespace())
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
        .then(required_whitespace())
        .ignore_then(
            any()
                .filter(|c: &char| c.is_ascii_digit())
                .repeated()
                .at_least(1)
                .to_slice()
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
            required_whitespace()
                .ignore_then(action())
                .labelled("action"),
        )
        .then(required_whitespace().ignore_then(priority()).or_not())
        .map(|((pattern, action), priority)| StaticRoute {
            pattern,
            action,
            priority,
        })
        .labelled("route")
}

fn line<'a>() -> impl Parser<'a, &'a str, Option<StaticRoute>, Extra<'a>> {
    inline_whitespace()
        .ignore_then(choice((
            just('#')
                .then(any().and_is(just('\n').not()).repeated())
                .ignored()
                .to(None),
            route().map(Some),
            empty().to(None),
        )))
        .then_ignore(inline_whitespace())
}

fn routes<'a>() -> impl Parser<'a, &'a str, Vec<StaticRoute>, Extra<'a>> {
    line()
        .separated_by(just('\n'))
        .allow_trailing()
        .collect::<Vec<_>>()
        .map(|v| v.into_iter().flatten().collect())
        .then_ignore(end())
}

/// Inline whitespace (spaces and tabs, not newlines) — zero or more
fn inline_whitespace<'a>() -> impl Parser<'a, &'a str, (), Extra<'a>> {
    any()
        .filter(|c: &char| *c == ' ' || *c == '\t')
        .repeated()
        .ignored()
}

/// At least one inline whitespace character
fn required_whitespace<'a>() -> impl Parser<'a, &'a str, (), Extra<'a>> {
    any()
        .filter(|c: &char| *c == ' ' || *c == '\t')
        .repeated()
        .at_least(1)
        .ignored()
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
                        "Failed to parse static routes file '{}': {e}",
                        routes_file.display()
                    );
                }
                Ok(Vec::new())
            }
            Err(errors) => {
                let msg = errors
                    .iter()
                    .map(|e| format!("{e}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                Err(anyhow::anyhow!(
                    "Failed to parse static routes file '{}': {msg}",
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
    fn multiple_routes() {
        let routes = parse_ok("ipn:*.*.* via ipn:0.1.0\ndtn://**/** reflect priority 1200");
        assert_eq!(routes.len(), 2);
    }

    #[test]
    fn error_messages_are_useful() {
        let result = routes().parse("ipn:*.*.* Broken").into_result();
        let errors = result.unwrap_err();
        let msg = format!("{}", errors[0]);
        // Should mention what was expected, not just "parse error"
        assert!(
            msg.contains("expected") || msg.contains("action"),
            "Error should be descriptive, got: {msg}"
        );
    }
}

use super::*;
use winnow::{
    ModalResult, Parser,
    ascii::{Caseless, dec_uint, line_ending, space0, space1, till_line_ending},
    combinator::{alt, cut_err, eof, opt, preceded, separated, terminated, trace},
    stream::AsChar,
    token::{rest, take_till},
};

fn parse_priority(input: &mut &[u8]) -> ModalResult<u32> {
    preceded(Caseless("priority"), preceded(space1, dec_uint)).parse_next(input)
}

fn parse_drop(input: &mut &[u8]) -> ModalResult<Action> {
    preceded(
        Caseless("drop"),
        opt(preceded(space1, dec_uint.try_map(|v: u64| v.try_into()))),
    )
    .map(Action::Drop)
    .parse_next(input)
}

fn parse_via(input: &mut &[u8]) -> ModalResult<Action> {
    preceded(
        Caseless("via"),
        preceded(space1, take_till(1.., AsChar::is_space).parse_to()),
    )
    .map(Action::Via)
    .parse_next(input)
}

fn parse_reflect(input: &mut &[u8]) -> ModalResult<Action> {
    Caseless("reflect")
        .map(|_| Action::Reflect)
        .parse_next(input)
}

fn parse_action(input: &mut &[u8]) -> ModalResult<StaticRoute> {
    (
        alt((parse_drop, parse_via, parse_reflect)),
        opt(preceded(space1, parse_priority)),
    )
        .map(|(action, priority)| StaticRoute { priority, action })
        .parse_next(input)
}

fn parse_pattern(input: &mut &[u8]) -> ModalResult<eid_patterns::EidPattern> {
    take_till(1.., AsChar::is_space)
        .parse_to()
        .parse_next(input)
}

fn parse_route(input: &mut &[u8]) -> ModalResult<(eid_patterns::EidPattern, StaticRoute)> {
    cut_err((parse_pattern, preceded(space1, parse_action))).parse_next(input)
}

fn parse_line(input: &mut &[u8]) -> ModalResult<Option<(eid_patterns::EidPattern, StaticRoute)>> {
    preceded(
        space0,
        alt((
            eof.map(|_| None),
            ('#', rest).map(|_| None),
            terminated(parse_route, space0).map(Some),
        )),
    )
    .parse_next(input)
}

#[allow(clippy::type_complexity)]
fn parse_routes(input: &mut &[u8]) -> ModalResult<Vec<(eid_patterns::EidPattern, StaticRoute)>> {
    separated(0.., till_line_ending.and_then(parse_line), line_ending)
        .map(|v: Vec<Option<(eid_patterns::EidPattern, StaticRoute)>>| {
            v.into_iter().flatten().collect()
        })
        .parse_next(input)
}

pub async fn load_routes(
    routes_file: &PathBuf,
    ignore_errors: bool,
    watching: bool,
) -> anyhow::Result<Vec<(eid_patterns::EidPattern, StaticRoute)>> {
    match tokio::fs::read(routes_file).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && ignore_errors && watching => {
            trace!("Static routes file: '{}' not found", routes_file.display());
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
        Ok(input) => {
            // Using the `trace` combinator for powerful debugging
            match trace("parse_routes", parse_routes).parse(&input) {
                Err(e) if ignore_errors => {
                    error!(
                        "Failed to parse static routes file '{}': {e}",
                        routes_file.display()
                    );
                    Ok(Vec::new())
                }
                Err(e) => Err(anyhow::anyhow!(
                    "Failed to parse static routes file '{}': {e}",
                    routes_file.display()
                )),
                Ok(v) => Ok(v),
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test() {
        parse_routes
            .parse(b"ipn:*.*.* via ipn:0.1.0")
            .expect("Should parse a simple valid route");

        parse_routes
            .parse(b"dtn://**/** reflect priority 1200")
            .expect("Should parse a route with action and priority");

        parse_routes
            .parse(b"Broken")
            .expect_err("Parsing 'Broken' should have failed");
        parse_routes
            .parse(b"ipn:*.*.* Broken")
            .expect_err("Parsing 'ipn:*.*.* Broken' should have failed");
        parse_routes
            .parse(b"ipn:*.*.* via Broken")
            .expect_err("Parsing 'ipn:*.*.* via Broken' should have failed");

        parse_routes
            .parse(b"#")
            .expect("Should parse a comment-only line");
        parse_routes
            .parse(b"#\n")
            .expect("Should parse a comment with a newline");
        parse_routes
            .parse(b"#      ")
            .expect("Should parse a comment with trailing spaces");
        parse_routes
            .parse(b"#      \n")
            .expect("Should parse a comment with trailing spaces and a newline");

        parse_routes
            .parse(b"")
            .expect("Should parse an empty string");
        parse_routes
            .parse(b"\n")
            .expect("Should parse a newline character");
        parse_routes
            .parse(b"      ")
            .expect("Should parse a line with only spaces");
        parse_routes
            .parse(b"      \n")
            .expect("Should parse a line with only spaces and a newline");
        parse_routes
            .parse(b"   \n   \n   ")
            .expect("Should parse multiple blank/whitespace lines");

        parse_routes
            .parse(b"ipn:*.*.* via ipn:0.1.0\ndtn://**/** reflect priority 1200")
            .expect("Should parse multiple valid route lines");
    }
}

use super::*;
use winnow::{
    ModalResult, Parser,
    ascii::{Caseless, dec_uint, line_ending, space0, space1, till_line_ending},
    combinator::{alt, opt, separated, separated_pair},
    stream::AsChar,
    token::{rest, take_till},
};

fn parse_priority(input: &mut &[u8]) -> ModalResult<u32> {
    (Caseless("priority"), space1, dec_uint)
        .map(|(_, _, v)| v)
        .parse_next(input)
}

fn parse_drop(input: &mut &[u8]) -> ModalResult<Action> {
    ((
        Caseless("drop"),
        opt((space1, dec_uint.try_map(|v: u64| v.try_into()))),
    )
        .map(|(_, v)| Action::Drop(v.map(|v| v.1))))
    .parse_next(input)
}

fn parse_via(input: &mut &[u8]) -> ModalResult<Action> {
    (separated_pair(
        Caseless("via"),
        space1,
        take_till(1.., AsChar::is_space).parse_to(),
    )
    .map(|(_, v)| Action::Via(v)))
    .parse_next(input)
}

fn parse_store(input: &mut &[u8]) -> ModalResult<Action> {
    (separated_pair(
        (Caseless("store"), opt((space1, Caseless("until")))),
        space1,
        take_till(1.., AsChar::is_space).try_map(|s| {
            time::OffsetDateTime::parse(
                &String::from_utf8_lossy(s),
                &time::format_description::well_known::Rfc3339,
            )
        }),
    )
    .map(|(_, v)| Action::Store(v)))
    .parse_next(input)
}

fn parse_action(input: &mut &[u8]) -> ModalResult<(Action, Option<u32>)> {
    (
        alt((parse_drop, parse_via, parse_store)),
        opt((space1, parse_priority).map(|(_, v)| v)),
    )
        .parse_next(input)
}

fn parse_pattern(input: &mut &[u8]) -> ModalResult<bpv7::EidPattern> {
    take_till(1.., AsChar::is_space)
        .parse_to()
        .parse_next(input)
}

fn parse_route(input: &mut &[u8]) -> ModalResult<(bpv7::EidPattern, StaticRoute)> {
    separated_pair(parse_pattern, space1, parse_action)
        .map(|(pattern, (action, priority))| (pattern, StaticRoute { priority, action }))
        .parse_next(input)
}

fn parse_line(input: &mut &[u8]) -> ModalResult<Option<(bpv7::EidPattern, StaticRoute)>> {
    alt((
        (space0, opt(parse_route), space0).map(|(_, v, _)| v),
        ('#', rest).map(|_| None),
    ))
    .parse_next(input)
}

#[allow(clippy::type_complexity)]
fn parse_routes(input: &mut &[u8]) -> ModalResult<Vec<(bpv7::EidPattern, StaticRoute)>> {
    separated(0.., till_line_ending.and_then(parse_line), line_ending)
        .map(|v: Vec<Option<(bpv7::EidPattern, StaticRoute)>>| v.into_iter().flatten().collect())
        .parse_next(input)
}

pub async fn load_routes(
    routes_file: &PathBuf,
    ignore_errors: bool,
    watching: bool,
) -> Result<Vec<(bpv7::EidPattern, StaticRoute)>, Error> {
    match tokio::fs::read(routes_file).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && ignore_errors && watching => {
            trace!(
                "Static routes file: '{}' not found",
                routes_file.to_string_lossy()
            );
            Ok(Vec::new())
        }
        Err(e) if ignore_errors => {
            error!(
                "Failed to open static routes file '{}': {e}",
                routes_file.to_string_lossy(),
            );
            Ok(Vec::new())
        }
        r => match parse_routes.parse(r?.as_ref()) {
            Err(e) if ignore_errors => {
                error!(
                    "Failed to parse static routes file '{}': {e}",
                    routes_file.to_string_lossy()
                );
                Ok(Vec::new())
            }
            Err(e) => Err(anyhow::format_err!("{e}").into()),
            Ok(v) => Ok(v),
        },
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test() {
        parse_routes
            .parse(b"ipn:*.*.* via ipn:0.1.0")
            .expect("Failed");

        parse_routes
            .parse(b"dtn://**/** store until 2025-01-02T11:12:13Z priority 1200")
            .expect("Failed");

        parse_routes.parse(b"#").expect("Failed");
        parse_routes.parse(b"#\n").expect("Failed");
        parse_routes.parse(b"#      ").expect("Failed");
        parse_routes.parse(b"#      \n").expect("Failed");

        parse_routes.parse(b"").expect("Failed");
        parse_routes.parse(b"\n").expect("Failed");
        parse_routes.parse(b"      ").expect("Failed");
        parse_routes.parse(b"      \n").expect("Failed");

        parse_routes.parse(b"   \n   \n   ").expect("Failed");

        parse_routes
            .parse(b"ipn:*.*.* via ipn:0.1.0\ndtn://**/** store 2025-01-02T11:12:13Z priority 1200")
            .expect("Failed");
    }
}

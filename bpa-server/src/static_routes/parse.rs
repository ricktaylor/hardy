use super::*;
use winnow::{
    ModalResult, Parser,
    ascii::{Caseless, dec_uint, line_ending, space0, space1},
    combinator::{alt, eof, opt, repeat_till, separated_pair},
    stream::AsChar,
    token::take_till,
};

fn parse_priority(input: &mut &[u8]) -> ModalResult<u32> {
    (space1, Caseless("priority"), space1, dec_uint)
        .map(|(_, _, _, v)| v)
        .parse_next(input)
}

fn parse_drop(input: &mut &[u8]) -> ModalResult<(Action, Option<u32>)> {
    (
        (
            Caseless("drop"),
            opt((space1, dec_uint.try_map(|v: u64| v.try_into()))),
        )
            .map(|v| Action::Drop(v.1.map(|v| v.1))),
        opt(parse_priority),
    )
        .parse_next(input)
}

fn parse_via(input: &mut &[u8]) -> ModalResult<(Action, Option<u32>)> {
    (
        (separated_pair(
            Caseless("via"),
            space1,
            take_till(1.., |c: u8| c.is_space() || c.is_newline()).parse_to(),
        )
        .map(|v| Action::Via(v.1))),
        opt(parse_priority),
    )
        .parse_next(input)
}

fn parse_store(input: &mut &[u8]) -> ModalResult<(Action, Option<u32>)> {
    (
        (separated_pair(
            (Caseless("store"), opt((space1, Caseless("until")))),
            space1,
            take_till(1.., |c: u8| c.is_space() || c.is_newline()).try_map(|s| {
                time::OffsetDateTime::parse(
                    &String::from_utf8_lossy(s),
                    &time::format_description::well_known::Rfc3339,
                )
            }),
        )
        .map(|v| Action::Store(v.1))),
        opt(parse_priority),
    )
        .parse_next(input)
}

fn parse_action(input: &mut &[u8]) -> ModalResult<(Action, Option<u32>)> {
    alt((parse_drop, parse_via, parse_store)).parse_next(input)
}

fn parse_pattern(input: &mut &[u8]) -> ModalResult<bpv7::EidPattern> {
    take_till(1.., AsChar::is_space)
        .parse_to()
        .parse_next(input)
}

fn parse_route(input: &mut &[u8]) -> ModalResult<(bpv7::EidPattern, StaticRoute)> {
    (
        space0,
        separated_pair(parse_pattern, space1, parse_action),
        space0,
        line_ending,
    )
        .parse_next(input)
        .map(|(_, (pattern, (action, priority)), _, _)| (pattern, StaticRoute { priority, action }))
}

#[allow(clippy::type_complexity)]
fn parse_routes(input: &mut &[u8]) -> ModalResult<Vec<(bpv7::EidPattern, StaticRoute)>> {
    repeat_till(
        0..,
        alt((
            (parse_route, line_ending).map(|v| Some(v.0)),
            ('#', take_till(0.., AsChar::is_newline)).map(|_| None),
            (space0, line_ending).map(|_| None),
        )),
        eof,
    )
    .map(
        |(v, _): (Vec<Option<(bpv7::EidPattern, StaticRoute)>>, &[u8])| {
            v.into_iter().flatten().collect()
        },
    )
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

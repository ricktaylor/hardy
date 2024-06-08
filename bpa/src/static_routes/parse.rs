use super::*;
use std::collections::HashSet;
use thiserror::Error;
use time::macros::format_description;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Error, Debug)]
enum ParseError {
    #[error("Expecting an action")]
    MissingAction,

    #[error("Only one of {0:?} allowed")]
    MultipleActions(Vec<String>),

    #[error("Invalid argument {0}")]
    InvalidArgument(String),

    #[error("Expecting a '{0}' parameter")]
    MissingParameter(&'static str),

    #[error(transparent)]
    Eid(#[from] bundle::EidError),

    #[error(transparent)]
    Pattern(#[from] bundle::EidPatternError),

    #[error(transparent)]
    Time(#[from] time::error::Parse),

    #[error(transparent)]
    Integer(#[from] std::num::ParseIntError),

    #[error(transparent)]
    StatusReport(#[from] bundle::StatusReportError),
}

#[derive(Debug)]
struct RouteLine(Option<(bundle::EidPattern, StaticRoute)>);

enum ArgOption {
    //None,
    Optional,
    One,
}
struct Arg {
    name: &'static str,
    arg: ArgOption,
    group: Option<usize>,
}

fn parse_args(
    mut parts: std::iter::Peekable<std::str::SplitWhitespace>,
    args: &'static [Arg],
) -> Result<HashMap<String, Option<String>>, ParseError> {
    let mut out = HashMap::new();
    let mut groups = HashSet::new();
    while let Some(s) = parts.next() {
        let Some(arg) = args.iter().find(|arg| arg.name.starts_with(s)) else {
            return Err(ParseError::InvalidArgument(s.to_string()));
        };

        if let Some(group) = arg.group {
            if !groups.insert(group) {
                return Err(ParseError::MultipleActions(
                    args.iter()
                        .filter_map(|arg| {
                            if arg.group == Some(group) {
                                Some(arg.name.to_string())
                            } else {
                                None
                            }
                        })
                        .collect(),
                ));
            }
        }

        out.insert(
            arg.name.to_string(),
            match arg.arg {
                //ArgOption::None => None,
                ArgOption::Optional => {
                    if let Some(n) = parts.peek() {
                        if args.iter().any(|arg| arg.name.starts_with(n)) {
                            // Next param is an arg
                            None
                        } else {
                            parts.next().map(|s| s.to_string())
                        }
                    } else {
                        parts.next().map(|s| s.to_string())
                    }
                }
                ArgOption::One => {
                    let Some(s) = parts.next() else {
                        return Err(ParseError::MissingParameter(arg.name));
                    };
                    Some(s.to_string())
                }
            },
        );
    }
    Ok(out)
}

impl std::str::FromStr for RouteLine {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split_whitespace();

        let pattern = match parts.next() {
            None => return Ok(Self(None)),
            Some(s) if s.starts_with('#') => return Ok(Self(None)),
            Some(s) => s.parse::<bundle::EidPattern>()?,
        };

        let parts = parse_args(
            parts.peekable(),
            &[
                Arg {
                    name: "drop",
                    arg: ArgOption::Optional,
                    group: Some(0),
                },
                Arg {
                    name: "via",
                    arg: ArgOption::One,
                    group: Some(0),
                },
                Arg {
                    name: "wait",
                    arg: ArgOption::One,
                    group: Some(0),
                },
                Arg {
                    name: "priority",
                    arg: ArgOption::One,
                    group: None,
                },
            ],
        )?;

        Ok(Self(Some((
            pattern,
            StaticRoute {
                priority: if let Some(priority) = parts.get("priority").unwrap_or(&None) {
                    Some(priority.parse::<u32>()?)
                } else {
                    None
                },
                action: if let Some(drop) = parts.get("drop") {
                    fib::Action::Drop(if let Some(reason) = drop {
                        Some(reason.parse::<u64>()?.try_into()?)
                    } else {
                        None
                    })
                } else if let Some(Some(via)) = parts.get("via") {
                    fib::Action::Via(via.parse()?)
                } else if let Some(Some(until)) = parts.get("wait") {
                    fib::Action::Wait(time::OffsetDateTime::parse(until,
                        format_description!("[year]-[month]-[day] [hour]:[minute]:[second] [offset_hour sign:mandatory]:[offset_minute]:[offset_second]"))?)
                } else {
                    return Err(ParseError::MissingAction);
                },
            },
        ))))
    }
}

pub async fn load_routes(
    routes_file: &PathBuf,
    ignore_errors: bool,
    watching: bool,
) -> Result<Vec<(bundle::EidPattern, StaticRoute)>, Error> {
    let file = match tokio::fs::File::open(routes_file).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && ignore_errors && watching => {
            trace!(
                "Static routes file: '{}' not found",
                routes_file.to_string_lossy()
            );
            return Ok(Vec::new());
        }
        Err(e) if ignore_errors => {
            error!(
                "Failed to open static routes file '{}': {}",
                routes_file.to_string_lossy(),
                e.to_string()
            );
            return Ok(Vec::new());
        }
        r => r?,
    };

    let mut routes = Vec::new();
    let mut lines = BufReader::new(file).lines();
    let mut idx: usize = 1;
    while let Some(line) = lines.next_line().await? {
        match line.parse::<RouteLine>() {
            Err(e) if ignore_errors => error!(
                "Failed to parse '{line}' at line {idx} in static routes file '{}': {}",
                routes_file.to_string_lossy(),
                e.to_string()
            ),
            Err(e) => return Err(e.into()),
            Ok(RouteLine(Some(line))) => routes.push(line),
            _ => {}
        }
        idx += 1;
    }
    Ok(routes)
}

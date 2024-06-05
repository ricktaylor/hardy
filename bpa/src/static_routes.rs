use super::*;
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    num::ParseIntError,
    path::PathBuf,
};
use thiserror::Error;
use time::macros::format_description;
use tokio::io::{AsyncBufReadExt, BufReader};
use utils::settings;

#[derive(Clone, Deserialize)]
struct Config {
    #[serde(default = "Config::default_path")]
    route_file: PathBuf,

    #[serde(default = "Config::default_priority")]
    priority: u32,
}

impl Config {
    fn default_path() -> PathBuf {
        settings::config_dir().join("static_routes")
    }

    fn default_priority() -> u32 {
        100
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct StaticRoute {
    priority: Option<u32>,
    action: fib::Action,
}

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
    Integer(#[from] ParseIntError),

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

#[derive(Clone)]
pub struct StaticRoutes {
    config: Config,
    fib: fib::Fib,
    routes: HashMap<bundle::EidPattern, StaticRoute>,
}

impl StaticRoutes {
    async fn init(
        mut self,
        _task_set: &mut tokio::task::JoinSet<()>,
        _cancel_token: tokio_util::sync::CancellationToken,
    ) {
        info!(
            "Loading static routes from '{}'",
            self.config.route_file.to_string_lossy()
        );

        self.refresh_routes().await.trace_expect(&format!(
            "Failed to read static_routes file '{}'",
            self.config.route_file.to_string_lossy()
        ));

        // Set up file watcher
        //let self_cloned = self.clone();
        //task_set.spawn_blocking(move || self_cloned.watch());
    }

    async fn refresh_routes(&mut self) -> Result<(), Error> {
        // Reload the routes
        let mut drop_routes = Vec::new();
        let mut add_routes = Vec::new();
        for r in self.load_routes().await? {
            if let Some(v2) = self.routes.get(&r.0) {
                if &r.1 != v2 {
                    drop_routes.push(r.0.clone());
                    add_routes.push(r);
                }
            } else {
                add_routes.push(r);
            }
        }

        // Drop routes
        for k in drop_routes {
            self.routes.remove(&k);
            self.fib.remove(String::new(), &k).await;
        }

        // Add routes
        for (k, v) in add_routes {
            if let Err(e) = self
                .fib
                .add(
                    String::new(),
                    &k,
                    v.priority.unwrap_or(self.config.priority),
                    v.action.clone(),
                )
                .await
            {
                error!("Failed to insert static route: {k:?}: {}", e.to_string());
            } else {
                self.routes.insert(k, v);
            }
        }
        Ok(())
    }

    async fn load_routes(&mut self) -> Result<Vec<(bundle::EidPattern, StaticRoute)>, Error> {
        let file = match tokio::fs::File::open(&self.config.route_file).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                trace!(
                    "Static routes file: '{}' not found",
                    self.config.route_file.to_string_lossy()
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
                Err(e) => error!(
                    "Failed to parse '{line}' at line {idx} in static routes file '{}': {}",
                    self.config.route_file.to_string_lossy(),
                    e.to_string()
                ),
                Ok(RouteLine(Some(line))) => routes.push(line),
                _ => {}
            }
            idx += 1;
        }
        Ok(routes)
    }

    /*fn watch(mut self) {
        todo!()
    }*/
}

#[instrument(skip_all)]
pub async fn init(
    config: &config::Config,
    fib: fib::Fib,
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    if let Some(config) =
        settings::get_with_default::<Option<Config>, _>(config, "static_routes", None)
            .trace_expect("Invalid 'static_routes' section in configuration")
    {
        StaticRoutes {
            config,
            fib,
            routes: HashMap::new(),
        }
        .init(task_set, cancel_token)
        .await;
    } else {
        info!("No static routes configured");
    }
}

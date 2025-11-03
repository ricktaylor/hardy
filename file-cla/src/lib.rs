use hardy_bpv7::eid::Eid;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use trace_err::*;
use tracing::{debug, error, info, warn};

#[cfg(feature = "tracing")]
use tracing::Instrument;

mod cla;
mod watcher;

#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Config {
    pub outbox: PathBuf,
    pub peers: HashMap<Eid, PathBuf>,
}

struct ClaInner {
    inboxes: HashSet<String>,
}

pub struct Cla {
    _name: String,
    config: Config,
    inner: std::sync::OnceLock<ClaInner>,
    cancel_token: tokio_util::sync::CancellationToken,
    task_tracker: tokio_util::task::TaskTracker,
}

impl Cla {
    pub fn new(name: String, config: Config) -> Self {
        Self {
            config,
            _name: name,
            inner: std::sync::OnceLock::new(),
            cancel_token: tokio_util::sync::CancellationToken::new(),
            task_tracker: tokio_util::task::TaskTracker::new(),
        }
    }
}

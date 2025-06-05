mod cla;
mod codec;
mod connect;
mod connection;
mod listen;
mod session;
mod transport;

pub mod config;

use hardy_bpv7::prelude as bpv7;
use trace_err::*;
use tracing::{error, info, trace, warn};

pub async fn new(
    config: &config::Config,
) -> hardy_bpa::cla::Result<std::sync::Arc<dyn hardy_bpa::cla::Cla>> {
    if config.session_defaults.contact_timeout > 60 {
        warn!("RFC9174 specifies contact timeout SHOULD be a maximum of 60 seconds");
    }

    match config.session_defaults.keepalive_interval {
        None | Some(0) => info!("Session keepalive disabled"),
        Some(x) if x < 15 => {
            warn!("RFC9174 specifies contact timeout SHOULD be a minimum of 15 seconds")
        }
        Some(x) if x > 600 => {
            warn!("RFC9174 specifies keepalive SHOULD be a maximum of 600 seconds")
        }
        _ => {}
    }

    Ok(std::sync::Arc::new(cla::Cla::new(config.clone())))
}

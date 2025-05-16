use super::*;
use serde::{Deserialize, Serialize};

mod application;
mod cla;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub address: core::net::SocketAddr,

    #[serde(default)]
    pub services: Vec<String>,
}

fn from_timestamp(t: prost_types::Timestamp) -> Result<time::OffsetDateTime, Error> {
    Ok(time::OffsetDateTime::from_unix_timestamp(t.seconds)
        .map_err(time::error::ComponentRange::from)?
        + time::Duration::nanoseconds(t.nanos.into()))
}

fn to_timestamp(t: time::OffsetDateTime) -> prost_types::Timestamp {
    let t = t - time::OffsetDateTime::UNIX_EPOCH;
    prost_types::Timestamp {
        seconds: t.whole_seconds(),
        nanos: t.subsec_nanoseconds(),
    }
}

pub fn init(
    config: &Config,
    bpa: &Arc<hardy_bpa::bpa::Bpa>,
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: &tokio_util::sync::CancellationToken,
) {
    if config.services.is_empty() {
        return;
    }

    // Add gRPC services to HTTP router
    let mut routes = tonic::service::Routes::builder();
    for service in &config.services {
        match service.as_str() {
            "application" => {
                routes.add_service(application::new_service(bpa.clone()));
            }
            "cla" => {
                routes.add_service(cla::new_service(bpa.clone()));
            }
            s => {
                warn!("Ignoring unknown gRPC service {s}");
            }
        }
    }

    // Start serving
    let addr = config.address;
    let cancel_token = cancel_token.clone();
    task_set.spawn(async move {
        tonic::transport::Server::builder()
            .add_routes(routes.routes())
            .serve_with_shutdown(addr, cancel_token.cancelled())
            .await
            .trace_expect("Failed to start gRPC server")
    });

    info!(
        "gRPC server hosting {:?}, listening on {}",
        config.services, config.address
    )
}

use super::*;
use std::net::SocketAddr;
use utils::settings;

mod cla;

#[instrument(skip_all)]
pub fn init(
    config: &config::Config,
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    // Get listen address from config
    let grpc_address = settings::get_with_default::<SocketAddr, SocketAddr>(
        config,
        "internal_grpc_address",
        "[::1]:50051".parse().unwrap(),
    )
    .trace_expect("Invalid 'internal_grpc_address' value in configuration");

    // Add gRPC services to HTTP router
    let router = tonic::transport::Server::builder().add_service(cla::new_service(config));

    // Start serving
    task_set.spawn(async move {
        router
            .serve_with_shutdown(grpc_address, async {
                cancel_token.cancelled().await;
            })
            .await
            .trace_expect("Failed to start gRPC server")
    });

    info!("gRPC server listening on {grpc_address}")
}

/*pub fn from_timestamp(
    t: prost_types::Timestamp,
) -> Result<time::OffsetDateTime, time::error::ComponentRange> {
    Ok(time::OffsetDateTime::from_unix_timestamp(t.seconds)?
        + time::Duration::nanoseconds(t.nanos.into()))
}*/

pub fn to_timestamp(t: time::OffsetDateTime) -> prost_types::Timestamp {
    let t = t - time::OffsetDateTime::UNIX_EPOCH;
    prost_types::Timestamp {
        seconds: t.whole_seconds(),
        nanos: t.subsec_nanoseconds(),
    }
}

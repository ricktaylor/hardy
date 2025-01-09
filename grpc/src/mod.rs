use super::*;

mod application_sink;
mod cla_sink;

#[instrument(skip_all)]
pub fn init(
    config: &config::Config,
    bpa: Arc<hardy_bpa::bpa::Bpa>,
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    // Get listen address from config
    let grpc_address =
        settings::get_with_default::<String, _>(config, "grpc_address", "[::1]:50051")
            .trace_expect("Invalid 'grpc_address' value in configuration")
            .parse()
            .trace_expect("Invalid gRPC address and/or port in configuration");

    // Add gRPC services to HTTP router
    let router = tonic::transport::Server::builder()
        .add_service(cla_sink::new_service(config, bpa.clone()))
        .add_service(application_sink::new_service(config, bpa));

    // Start serving
    task_set.spawn(async move {
        router
            .serve_with_shutdown(grpc_address, cancel_token.cancelled())
            .await
            .trace_expect("Failed to start gRPC server")
    });

    info!("gRPC server listening on {grpc_address}")
}

pub fn from_timestamp(t: prost_types::Timestamp) -> Result<time::OffsetDateTime, Error> {
    Ok(time::OffsetDateTime::from_unix_timestamp(t.seconds)
        .map_err(time::error::ComponentRange::from)?
        + time::Duration::nanoseconds(t.nanos.into()))
}

pub fn to_timestamp(t: time::OffsetDateTime) -> prost_types::Timestamp {
    let t = t - time::OffsetDateTime::UNIX_EPOCH;
    prost_types::Timestamp {
        seconds: t.whole_seconds(),
        nanos: t.subsec_nanoseconds(),
    }
}

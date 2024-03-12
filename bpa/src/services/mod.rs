use super::*;

mod cla_sink;

pub fn init(
    config: &settings::Config,
    cla_registry: cla::ClaRegistry,
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: &tokio_util::sync::CancellationToken,
) {
    // Start serving
    let addr = format!("{}:{}", config.grpc_addr, config.grpc_port)
        .parse()
        .log_expect("Invalid gRPC address and/or port in configuration");

    let router = tonic::transport::Server::builder()
        .add_service(cla_sink::new_service(config, cla_registry));

    let cancel_token_cloned = cancel_token.clone();
    task_set.spawn(async move {
        router
            .serve_with_shutdown(addr, async {
                cancel_token_cloned.cancelled().await;
            })
            .await
            .log_expect("Failed to start gRPC server")
    });
}

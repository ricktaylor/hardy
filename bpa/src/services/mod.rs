use super::*;

mod cla_sink;

pub fn init(
    config: &settings::Config,
    cache: cache::Cache,
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    // Get listen address from config
    let addr = format!("{}:{}", config.grpc_addr, config.grpc_port)
        .parse()
        .log_expect("Invalid gRPC address and/or port in configuration");

    // Add gRPC services to HTTP router
    let router =
        tonic::transport::Server::builder().add_service(cla_sink::new_service(config, cache));

    // Start serving
    task_set.spawn(async move {
        router
            .serve_with_shutdown(addr, async {
                cancel_token.cancelled().await;
            })
            .await
            .log_expect("Failed to start gRPC server")
    });
}

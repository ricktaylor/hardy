use super::*;
use utils::settings;

mod application_sink;
mod cla_sink;

#[instrument(skip_all)]
pub fn init(
    config: &config::Config,
    cla_registry: cla_registry::ClaRegistry,
    app_registry: app_registry::AppRegistry,
    ingress: ingress::Ingress,
    dispatcher: dispatcher::Dispatcher,
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
        .add_service(cla_sink::new_service(config, cla_registry, ingress))
        .add_service(application_sink::new_service(
            config,
            app_registry,
            dispatcher,
        ));

    // Start serving
    task_set.spawn(async move {
        router
            .serve_with_shutdown(grpc_address, async {
                cancel_token.cancelled().await;
            })
            .await
            .trace_expect("Failed to start gRPC server")
    });

    info!("gRPC server listening on {}", grpc_address)
}

use super::*;
use anyhow::anyhow;
use std::sync::Arc;

mod cla_sink;

pub fn init<M: storage::MetadataStorage + Send + Sync, B: storage::BundleStorage + Send + Sync>(
    config: &config::Config,
    ingress: Arc<ingress::Ingress<M, B>>,
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<(), anyhow::Error> {
    let grpc_address: String = config
        .get("grpc_address")
        .map_err(|e| anyhow!("Invalid gRPC address in configuration: {}", e))?;

    // Get listen address from config
    let addr = grpc_address
        .parse()
        .map_err(|e| anyhow!("Invalid gRPC address and/or port in configuration: {}", e))?;

    // Add gRPC services to HTTP router
    let router =
        tonic::transport::Server::builder().add_service(cla_sink::new_service(config, ingress));

    // Start serving
    task_set.spawn(async move {
        router
            .serve_with_shutdown(addr, async {
                cancel_token.cancelled().await;
            })
            .await
            .log_expect("Failed to start gRPC server")
    });

    log::info!("Start gRPC server on {}", grpc_address);
    Ok(())
}

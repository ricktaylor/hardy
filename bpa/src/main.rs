use log_err::*;
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;

mod cla;
mod logger;
mod services;
mod settings;

#[tokio::main]
async fn main() {
    let Some(config) = settings::init() else {
        return;
    };

    logger::init(&config);

    // Init services
    let services = services::init(
        &config,
        cla::ClaRegistry::new(&config),
        tonic::transport::Server::builder(),
    );

    // Start serving
    let mut set = tokio::task::JoinSet::new();

    let addr = format!("{}:{}", config.grpc_addr, config.grpc_port)
        .parse()
        .log_expect("Invalid gRPC address and/or port in configuration");

    let cancel_token = CancellationToken::new();
    let cancel_token_cloned = cancel_token.clone();

    set.spawn(async move {
        services
            .serve_with_shutdown(addr, async {
                cancel_token.cancelled().await;
            })
            .await
            .log_expect("Failed to start gRPC server")
    });

    set.spawn(async move {
        if signal(SignalKind::terminate())
            .expect("Failed to register signal handlers")
            .recv()
            .await
            .is_some()
        {
            cancel_token_cloned.cancel();
        }
    });

    while set.join_next().await.is_some() {}
}

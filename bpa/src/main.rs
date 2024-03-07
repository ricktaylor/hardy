use log_err::*;

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

    // And finally serve
    let addr = format!("{}:{}", config.grpc_addr, config.grpc_port)
        .parse()
        .log_expect("Invalid gRPC address and/or port in configuration");

    tokio::spawn(async move {
        services
            .serve(addr)
            .await
            .log_expect("Failed to start gRPC server")
    })
    .await
    .log_expect("Failed to run tasks")
}

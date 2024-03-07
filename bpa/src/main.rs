use log_err::*;

mod logger;
mod services;
mod settings;

async fn run(config: &settings::Config) {
    // Init services
    let services = services::init(config, tonic::transport::Server::builder());

    // And finally serve
    let addr = format!("{}:{}", config.grpc_addr, config.grpc_port)
        .parse()
        .log_expect("Invalid gRPC address and/or port in configuration");
    
    services.serve(addr).await.log_expect("Failed to start gRPC server")
}

fn main() {
    let Some(config) = settings::init() else {
        return;
    };

    logger::init(&config);

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .log_expect("Failed to start tokio runtime")
        .block_on(run(&config))
}

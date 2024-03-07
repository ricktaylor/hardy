use log_err::*;

mod cla;
mod logger;
mod settings;

async fn run(config: &settings::Config) {
    let addr = format!("{}:{}", config.grpc_addr, config.grpc_port)
        .parse()
        .log_expect("Invalid gRPC address and/or port in configuration");

    tonic::transport::Server::builder()
        .add_service(cla::new_service(config))
        .serve(addr)
        .await
        .log_expect("Failed to start gRPC server")
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

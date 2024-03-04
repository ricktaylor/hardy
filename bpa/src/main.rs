mod cla;
mod settings;

async fn run() {
    let config = settings::Config::get();

    let addr = format!("{}:{}", config.grpc_addr, config.grpc_port)
        .parse()
        .expect("Invalid gRPC address and/or port in configuration");

    tonic::transport::Server::builder()
        .add_service(cla::new_service())
        .serve(addr)
        .await
        .expect("Failed to start gRPC server")
}

fn main() {
    if settings::init().is_none() {
        return;
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to start tokio runtime")
        .block_on(run())
}

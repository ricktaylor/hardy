mod cla;
mod settings;

async fn run() {
    let config = settings::Config::get();

    let addr = format!("{}:{}", config.grpc_addr, config.grpc_port)
        .parse()
        .unwrap();

    tonic::transport::Server::builder()
        .add_service(cla::new_service())
        .serve(addr)
        .await
        .unwrap()
}

fn main() {
    settings::init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run())
}

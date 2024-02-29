//use config::Config;

mod cla;

async fn run() {
    let addr = "[::1]:50051".parse().unwrap();

    tonic::transport::Server::builder()
        .add_service(cla::new_service())
        .serve(addr).await.unwrap()
}


fn main() {
    //Config::builder().
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run())
}

use super::*;

mod cla_sink;

pub fn init(
    config: &settings::Config,
    cla_registry: cla::ClaRegistry,
    mut server: tonic::transport::Server,
) -> tonic::transport::server::Router {
    server.add_service(cla_sink::new_service(config, cla_registry))
}

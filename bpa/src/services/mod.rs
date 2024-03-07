use super::settings;

mod cla_sink;

pub fn init(
    config: &settings::Config,
    mut server: tonic::transport::Server,
) -> tonic::transport::server::Router {
    server.add_service(cla_sink::new_service(config))
}

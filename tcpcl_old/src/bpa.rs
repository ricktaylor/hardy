use super::*;
use hardy_proto::cla::*;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::bytes::Bytes;
use utils::settings;

type Channel = Arc<Mutex<cla_sink_client::ClaSinkClient<tonic::transport::Channel>>>;

#[derive(Clone)]
struct Config {
    bpa_address: String,
    external_address: String,
    ident: String,
}

impl Config {
    fn new(config: &config::Config) -> Self {
        // Get external address from config
        let mut external_address: String =
            settings::get_with_default(config, "external_address", String::new())
                .trace_expect("Invalid 'external_address' value in configuration");
        if external_address.is_empty() {
            let internal_address = settings::get_with_default::<SocketAddr, SocketAddr>(
                config,
                "internal_grpc_address",
                "[::1]:50051".parse().unwrap(),
            )
            .trace_expect("Invalid 'internal_grpc_address' value in configuration");

            external_address = format!("http://{}", internal_address);

            info!(
                "No 'external_grpc_address' found in configuration, using 'internal_grpc_address': {external_address}"
            );
        }

        Self {
            external_address,
            bpa_address: config
                .get::<String>("bpa_address")
                .trace_expect("Invalid or missing 'bpa_address' value in configuration"),
            ident: settings::get_with_default(config, "instance_id", "TCPCLv4")
                .trace_expect("Invalid 'instance_id' value in configuration"),
        }
    }
}

#[derive(Clone)]
struct BpaEndpoint {
    channel: Channel,
    handle: u32,
}

#[derive(Clone)]
pub struct Bpa {
    config: Config,
    endpoint: Option<BpaEndpoint>,
}

impl Bpa {
    pub fn new(config: &config::Config) -> Self {
        Self {
            config: Config::new(config),
            endpoint: None,
        }
    }

    pub async fn connect(&mut self) {
        if self.endpoint.is_none() {
            self.endpoint = Some(BpaEndpoint::connect(&self.config).await);
        }
    }

    pub async fn disconnect(&self) {
        if let Some(endpoint) = &self.endpoint {
            endpoint.disconnect().await;
        }
    }

    pub async fn send(&self, bundle: Bytes) -> Result<(), tonic::Status> {
        self.endpoint
            .as_ref()
            .trace_expect("Called send on disconnected BPA endpoint")
            .send(bundle)
            .await
    }
}

impl BpaEndpoint {
    async fn connect(config: &Config) -> Self {
        let mut channel = cla_sink_client::ClaSinkClient::connect(config.bpa_address.clone())
            .await
            .trace_expect("Failed to connect to BPA server");

        // Register with BPA
        let handle = channel
            .register_cla(RegisterClaRequest {
                ident: config.ident.clone(),
                name: "TCPCLv4".to_string(),
                grpc_address: config.external_address.clone(),
            })
            .await
            .trace_expect("Failed to register with BPA")
            .into_inner()
            .handle;

        Self {
            channel: Arc::new(Mutex::new(channel)),
            handle,
        }
    }

    async fn disconnect(&self) {
        if let Err(e) = self
            .channel
            .lock()
            .await
            .unregister_cla(UnregisterClaRequest {
                handle: self.handle,
            })
            .await
        {
            error!("Failed to unregister with BPA: {e}")
        }
    }

    pub async fn send(&self, bundle: Bytes) -> Result<(), tonic::Status> {
        self.channel
            .lock()
            .await
            .receive_bundle(DispatchBundleRequest {
                handle: self.handle,
                source: Bytes::new(),
                bundle,
            })
            .await
            .map(|_| ())
    }
}

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    /// The address of the BPA gRPC server (e.g. http://127.0.0.1:50051)
    pub bpa_address: String,

    /// The name of this CLA instance to register with the BPA
    pub cla_name: String,

    /// TCPCLv4 configuration
    #[serde(flatten)]
    pub tcpcl: hardy_tcpclv4::config::Config,
}

pub fn load(path: Option<PathBuf>) -> anyhow::Result<Config> {
    let mut builder = config::Config::builder();

    if let Some(path) = path {
        builder = builder.add_source(config::File::from(path));
    } else {
        // Optional default config file in current directory
        builder = builder.add_source(
            config::File::from(std::path::Path::new("hardy-tcpclv4.toml")).required(false),
        );
    }

    // Allow environment variables to override
    builder = builder.add_source(config::Environment::with_prefix("HARDY_TCPCLV4"));

    builder.build()?.try_deserialize().map_err(Into::into)
}

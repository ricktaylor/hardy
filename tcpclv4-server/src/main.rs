use clap::Parser;
use std::path::PathBuf;
use tracing::info;

mod config;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let config = config::load(args.config)?;

    info!("Starting TCPCLv4 Server (CLA: {})", config.cla_name);
    info!("Connecting to BPA at {}", config.bpa_address);

    // Create the CLA instance
    // This will validate the TCPCL configuration and prepare the internal state
    let _cla = hardy_tcpclv4::Cla::new(config.cla_name.clone(), config.tcpcl);

    // TODO: Connect to BPA via gRPC and register the CLA
    // let mut client = hardy_proto::cla::cla_client::ClaClient::connect(config.bpa_address).await?;

    // For now, just keep the process alive to allow the CLA to run (if it started listeners)
    tokio::signal::ctrl_c().await?;
    info!("Shutting down");
    Ok(())
}

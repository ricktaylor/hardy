use clap::Parser;
use hardy_async::TaskPool;
use hardy_bpa::bpa::BpaRegistration;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::lookup_host;
use tracing::{error, info, warn};

mod config;

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    // Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = config::Config::load(args.config)?;

    #[cfg(feature = "otel")]
    let _guard = hardy_otel::init(PKG_NAME, PKG_VERSION, config.log_level);

    #[cfg(not(feature = "otel"))]
    {
        use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};
        let filter = EnvFilter::builder()
            .with_default_directive(
                tracing_subscriber::filter::LevelFilter::from_level(config.log_level).into(),
            )
            .from_env_lossy();
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_filter(filter))
            .init();
    }

    info!("{} version {} starting...", PKG_NAME, PKG_VERSION);

    inner_main(config).await.inspect_err(|e| error!("{e}"))
}

async fn connect_peer(
    cla: &hardy_tcpclv4::Cla,
    peer: &str,
    cancel: hardy_async::CancellationToken,
) {
    loop {
        match lookup_host(peer).await {
            Ok(mut addrs) => {
                if let Some(addr) = addrs.next() {
                    info!("Connecting to peer {peer} ({addr})");
                    match cla.connect(&addr).await {
                        Ok(()) => {
                            info!("Connected to peer {peer}");
                            return;
                        }
                        Err(e) => warn!("Failed to connect to peer {peer}: {e}"),
                    }
                } else {
                    warn!("No addresses resolved for peer {peer}");
                }
            }
            Err(e) => warn!("Failed to resolve peer {peer}: {e}"),
        }

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            _ = cancel.cancelled() => return,
        }
    }
}

async fn inner_main(config: config::Config) -> anyhow::Result<()> {
    let cla = Arc::new(hardy_tcpclv4::Cla::new(&config.tcpcl)?);

    info!("Connecting to BPA at {}", config.bpa_address);

    let remote_bpa = hardy_proto::client::RemoteBpa::new(config.bpa_address);

    let node_ids = remote_bpa
        .register_cla(config.cla_name.clone(), cla.clone(), None)
        .await
        .map_err(|e| anyhow::anyhow!("CLA registration failed: {e}"))?;

    info!(
        "CLA {} registered, node IDs: {:?}",
        config.cla_name,
        node_ids.iter().map(|n| n.to_string()).collect::<Vec<_>>()
    );

    let tasks = TaskPool::new();
    hardy_async::signal::listen_for_cancel(&tasks);

    for peer in config.peers {
        let cla = cla.clone();
        let cancel = tasks.cancel_token().clone();
        hardy_async::spawn!(tasks, "peer_connect", async move {
            connect_peer(&cla, &peer, cancel).await;
        });
    }

    info!("Started successfully");

    tasks.cancel_token().cancelled().await;

    // Gracefully unregister from the BPA before shutting down
    cla.unregister().await;

    tasks.shutdown().await;

    info!("Stopped");

    Ok(())
}

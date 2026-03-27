use clap::Parser;
use hardy_async::TaskPool;
use hardy_bpa::bpa::BpaRegistration;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};

mod config;
mod contacts;
mod cron;
mod parser;
mod scheduler;
mod server;

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(author, version, about = "Time-Variant Routing agent for Hardy DTN")]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = config::load(args.config)?;

    let log_level = std::env::var("HARDY_TVR_LOG_LEVEL")
        .ok()
        .and_then(|s| s.parse::<tracing::Level>().ok())
        .or(config.log_level)
        .unwrap_or(tracing::Level::ERROR);

    #[cfg(feature = "otel")]
    let _guard = hardy_otel::init(PKG_NAME, PKG_VERSION, log_level);

    #[cfg(not(feature = "otel"))]
    {
        use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};
        let filter = EnvFilter::builder()
            .with_default_directive(
                tracing_subscriber::filter::LevelFilter::from_level(log_level).into(),
            )
            .from_env_lossy();
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_filter(filter))
            .init();
    }

    info!("{} version {} starting...", PKG_NAME, PKG_VERSION);

    inner_main(config).await.inspect_err(|e| error!("{e}"))
}

async fn inner_main(config: config::Config) -> anyhow::Result<()> {
    // Create scheduler channel (handle available immediately, task starts after registration)
    let (scheduler_handle, scheduler_rx) = scheduler::channel();

    // Create the routing agent
    let agent = Arc::new(contacts::TvrAgent::new(
        config.priority,
        scheduler_handle.clone(),
    ));

    // Connect to BPA and register as a RoutingAgent
    info!("Connecting to BPA at {}", config.bpa_address);

    let remote_bpa = hardy_proto::client::RemoteBpa::new(config.bpa_address);

    let node_ids = remote_bpa
        .register_routing_agent(config.agent_name.clone(), agent.clone())
        .await
        .map_err(|e| anyhow::anyhow!("RoutingAgent registration failed: {e}"))?;

    info!(
        "Routing agent '{}' registered, node IDs: {:?}",
        config.agent_name,
        node_ids.iter().map(|n| n.to_string()).collect::<Vec<_>>()
    );

    let tasks = TaskPool::new();
    hardy_async::signal::listen_for_cancel(&tasks);

    // Start scheduler task (sink is now available after registration)
    {
        let sink = agent.sink().expect("sink should be set after registration");
        scheduler::start(scheduler_rx, sink, &tasks);
    }

    // Start TVR gRPC session server
    server::start(config.grpc_listen, &agent, &tasks).await;

    // Load contact plan file if configured
    if let Some(contact_plan) = &config.contact_plan {
        info!("Loading contact plan from '{}'", contact_plan.display());
        match parser::load_contacts(contact_plan, false, config.watch).await {
            Ok(contacts) => {
                let source = format!("file:{}", contact_plan.display());
                if let Some(result) = scheduler_handle
                    .replace_contacts(&source, contacts, config.priority)
                    .await
                {
                    info!(
                        "Loaded contact plan: {} added, {} active",
                        result.added,
                        result.added - result.removed, // all new on first load
                    );
                }
            }
            Err(e) => {
                error!("Failed to load contact plan: {e}");
            }
        }
        // TODO: start file watcher if config.watch
    }

    info!("Started successfully");

    tasks.cancel_token().cancelled().await;

    // Gracefully unregister from the BPA
    agent.unregister().await;

    tasks.shutdown().await;

    info!("Stopped");

    Ok(())
}

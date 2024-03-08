use log_err::*;
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;
use std::sync::Arc;

mod cla;
mod logger;
mod services;
mod settings;

#[tokio::main]
async fn main() {
    // load config
    let Some(config) = settings::init() else {
        return;
    };

    // Init logger
    logger::init(&config);

    // Setup CLA registry
    let cla_registry = cla::ClaRegistry::new(&config);

    // Prep graceful shutdown
    let mut set = tokio::task::JoinSet::new();
    let cancel_token = CancellationToken::new();

    // Init services
    services::init(&config,cla_registry,&mut set,&cancel_token);
    
    // And finally set up signal handler
    set.spawn(async move {
        if signal(SignalKind::terminate())
            .expect("Failed to register signal handlers")
            .recv()
            .await
            .is_some()
        {
            cancel_token.cancel();
        }
    });

    // Wait for all tasks to finish
    while set.join_next().await.is_some() {}
}

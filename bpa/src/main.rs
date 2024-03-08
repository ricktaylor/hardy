use log_err::*;
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;

mod cla;
mod logger;
mod services;
mod settings;

// Buildtime info
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

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
    let mut task_set = tokio::task::JoinSet::new();
    let cancel_token = CancellationToken::new();

    // Init services
    services::init(&config, cla_registry, &mut task_set, &cancel_token);

    // And finally set up signal handler
    task_set.spawn(async move {
        if signal(SignalKind::terminate())
            .expect("Failed to register signal handlers")
            .recv()
            .await
            .is_some()
        {
            // Signal stop
            log::info!("{} stopping...", built_info::PKG_NAME);
            cancel_token.cancel();
        }
    });

    log::info!("{} started", built_info::PKG_NAME);

    // Wait for all tasks to finish
    while task_set.join_next().await.is_some() {}

    log::info!("{} stopped", built_info::PKG_NAME);
}

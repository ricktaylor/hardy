use log_err::*;
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;

mod cache;
mod cla;
mod database;
mod logger;
mod services;
mod settings;

// Buildtime info
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

fn listen_for_cancel(
    task_set: &mut tokio::task::JoinSet<()>,
) -> tokio_util::sync::CancellationToken {
    let cancel_token = CancellationToken::new();

    let mut term_handler =
        signal(SignalKind::terminate()).log_expect("Failed to register signal handlers");

    let cancel_token_cloned = cancel_token.clone();
    task_set.spawn(async move {
        tokio::select! {
            Some(_) = term_handler.recv() =>
                {
                    // Signal stop
                    log::info!("{} stopping...", built_info::PKG_NAME);
                    cancel_token_cloned.cancel();
                }
            _ = cancel_token_cloned.cancelled() => {}
        }
    });

    cancel_token
}

#[tokio::main]
async fn main() {
    // load config
    let Some(config) = settings::init() else {
        return;
    };

    // Init logger
    logger::init(&config);

    // Init DB
    let db = database::init(&config);

    // Setup CLA registry
    let cla_registry = cla::ClaRegistry::new(&config);

    // Prep graceful shutdown
    let mut task_set = tokio::task::JoinSet::new();
    let cancel_token = listen_for_cancel(&mut task_set);

    // Init async systems
    services::init(&config, cla_registry, &mut task_set, cancel_token);

    log::info!("{} started", built_info::PKG_NAME);

    // Wait for all tasks to finish
    while let Some(r) = task_set.join_next().await {
        r.log_expect("Task terminated unexpectedly")
    }

    log::info!("{} stopped", built_info::PKG_NAME);
}
